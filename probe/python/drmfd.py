#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Sunshine's DRM-FD lookup, reproduced step by step.

The question this answers: Sunshine refuses to use NVENC in the guest with

    Couldn't open DRM FD for CUDA device: No such file or directory

and the message names the symptom, not the cause. The cause is a four-step
lookup in Sunshine's `src/platform/linux/cuda.cpp`
(`open_drm_fd_for_cuda_device`, commit 14ffa6f, lines 236-275):

    1. cuDeviceGet(0)
    2. cuDeviceGetPCIBusId  ->  "0000:2D:00.0"
    3. lowercase it, then list /sys/bus/pci/devices/<busid>/drm
    4. the first entry named card* is opened as /dev/dri/<that>

Every step is reproduced here, in the same order, with the same string
handling -- including the lowercasing, which matters, and including the
13-byte buffer, which truncates. The point is to say WHICH step fails and
what it saw, instead of relaying an errno.

No CUDA toolkit needed: libcuda.so.1 is called through ctypes, so this runs
in a guest that has the forwarded driver and nothing else.

  probe/python/drmfd.py [--json]

Runs natively too, and that is the counter-check that gives the guest
result its meaning -- a lookup that fails on both sides is not a border
problem.
"""

import argparse
import ctypes
import json
import os
import sys

# cuDeviceGetPCIBusId's buffer in Sunshine is std::array<char, 13>: twelve
# characters of "0000:2D:00.0" plus the NUL. Same size here, so that a
# truncation would show up here too.
PCI_BUS_ID_LEN = 13

CUDA_SUCCESS = 0


class Cuda:
    """The three driver-API entry points Sunshine's lookup uses."""

    def __init__(self):
        self.lib = ctypes.CDLL("libcuda.so.1")
        self.lib.cuInit.argtypes = [ctypes.c_uint]
        self.lib.cuDeviceGet.argtypes = [ctypes.POINTER(ctypes.c_int), ctypes.c_int]
        self.lib.cuDeviceGetPCIBusId.argtypes = [
            ctypes.c_char_p,
            ctypes.c_int,
            ctypes.c_int,
        ]
        self.lib.cuGetErrorName.argtypes = [
            ctypes.c_int,
            ctypes.POINTER(ctypes.c_char_p),
        ]

    def name(self, code):
        out = ctypes.c_char_p()
        if self.lib.cuGetErrorName(code, ctypes.byref(out)) != CUDA_SUCCESS:
            return f"CUDA_ERROR_{code}"
        return out.value.decode()

    def check(self, code, what):
        if code != CUDA_SUCCESS:
            raise RuntimeError(f"{what}: {self.name(code)}")

    def bus_id(self, index):
        self.check(self.lib.cuInit(0), "cuInit")
        dev = ctypes.c_int()
        self.check(self.lib.cuDeviceGet(ctypes.byref(dev), index), "cuDeviceGet")
        buf = ctypes.create_string_buffer(PCI_BUS_ID_LEN)
        self.check(
            self.lib.cuDeviceGetPCIBusId(buf, PCI_BUS_ID_LEN, dev),
            "cuDeviceGetPCIBusId",
        )
        return buf.value.decode()


def lookup(index=0):
    """Sunshine's four steps, each with its own verdict."""
    result = {
        "bus_id": None,
        "bus_id_lower": None,
        "sysfs_dir": None,
        "sysfs_exists": False,
        "sysfs_entries": [],
        "card_node": None,
        "opened": False,
        "open_errno": None,
        "fails_at": None,
    }

    try:
        result["bus_id"] = Cuda().bus_id(index)
    except (OSError, RuntimeError) as err:
        result["fails_at"] = f"cuda: {err}"
        return result

    # Step 3, and the lowercasing is Sunshine's: CUDA reports "0000:2D:00.0",
    # sysfs spells it "0000:2d:00.0".
    lower = result["bus_id"].lower()
    result["bus_id_lower"] = lower
    sysfs = f"/sys/bus/pci/devices/{lower}/drm"
    result["sysfs_dir"] = sysfs

    # Sunshine's directory_iterator throws here, and the catch logs "Failed
    # to read sysfs". Both a missing PCI device and a device without a drm/
    # subdirectory land in the same branch, so the two are separated here.
    if not os.path.isdir(sysfs):
        parent = f"/sys/bus/pci/devices/{lower}"
        result["fails_at"] = (
            "sysfs: no such PCI device in this namespace"
            if not os.path.isdir(parent)
            else "sysfs: PCI device present but has no drm/ subdirectory"
        )
        return result

    result["sysfs_exists"] = True
    result["sysfs_entries"] = sorted(os.listdir(sysfs))

    # Step 4: the first card* entry wins. Note it is the PRIMARY node, not
    # the render node -- Sunshine wants a GBM device on it.
    cards = [e for e in result["sysfs_entries"] if e.startswith("card")]
    if not cards:
        result["fails_at"] = "sysfs: drm/ holds no card* entry"
        return result
    result["card_node"] = f"/dev/dri/{cards[0]}"

    try:
        fd = os.open(result["card_node"], os.O_RDWR)
    except OSError as err:
        result["open_errno"] = err.strerror
        result["fails_at"] = f"open: {result['card_node']}: {err.strerror}"
        return result
    os.close(fd)
    result["opened"] = True
    return result


def dri_inventory():
    """What /dev/dri actually holds -- the other half of the picture."""
    out = {"nodes": [], "by_path": []}
    for name, key in (("/dev/dri", "nodes"), ("/dev/dri/by-path", "by_path")):
        try:
            out[key] = sorted(os.listdir(name))
        except OSError:
            pass
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--index", type=int, default=0, help="CUDA device index")
    ap.add_argument("--json", action="store_true", help="machine-readable")
    args = ap.parse_args()

    res = lookup(args.index)
    res["dri"] = dri_inventory()

    if args.json:
        print(json.dumps(res, indent=2))
        return 0 if res["opened"] else 1

    print(f"cuDeviceGetPCIBusId   {res['bus_id']}")
    print(f"lowercased            {res['bus_id_lower']}")
    print(f"sysfs dir             {res['sysfs_dir']}")
    print(f"  exists              {res['sysfs_exists']}")
    if res["sysfs_entries"]:
        print(f"  entries             {' '.join(res['sysfs_entries'])}")
    print(f"card node             {res['card_node']}")
    print(f"opened O_RDWR         {res['opened']}")
    print(f"/dev/dri              {' '.join(res['dri']['nodes'])}")
    print(f"/dev/dri/by-path      {' '.join(res['dri']['by_path'])}")
    if res["fails_at"]:
        print(f"FAILS AT              {res['fails_at']}")
        return 1
    print("RESULT                lookup succeeds -- Sunshine would get its FD")
    return 0


if __name__ == "__main__":
    sys.exit(main())
