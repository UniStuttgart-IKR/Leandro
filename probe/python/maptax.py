#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Taxonomy of the mmap regions in a trace.

Answers the core question: what kind of memory does each of the mappings
libcuda creates consist of? That decides which can come out of guest RAM
and which cannot -- the latter need a host-visible window, i.e. the
cloud-hypervisor SHMEM patch.

Correlated over three of the tracer's line types:
  mmap      dev, fd, length, offset, result
  nvos33in  RM_MAP_MEMORY -- which hMemory is registered on which fd
  nvos64in  RM_ALLOC      -- with which hClass hMemory was created

The driver remembers the pending mapping PER FD (the mmap offset is 0 on
the frontend nodes), hence the attribution over the fd number.

  probe/python/maptax.py probe/traces/lvl4blocking.tsv
"""
import collections
import sys

# hClass -> plain text. Evidence in the vendor tree:
#   0xc461 clc461.h:27, 0x40 cl0040.h:34, 0x3e cl003e.h, 0xde cl00de.h
NAMES = {
    "0xc461": "TURING_USERMODE_A (doorbell, MMIO on the card)",
    "0x40": "NV01_MEMORY_LOCAL_USER (Vidmem)",
    "0x3e": "NV01_MEMORY_SYSTEM (Sysmem)",
    "0xde": "NV01_MEMORY_DEVICELESS / 0xde",
}
# Can this kind of memory come out of guest RAM?
FROM_GUEST_RAM = {"0x3e": True, "0x40": False, "0xc461": False, "0xde": False}


def kv(fields):
    d = {}
    for f in fields:
        if "=" in f:
            k, v = f.split("=", 1)
            d[k] = v
    return d


def main(path):
    rows = [l.rstrip("\n").split("\t") for l in open(path)]

    cls = {}
    for r in rows:
        if r and r[0] == "nvos64in":
            d = kv(r[1:])
            cls[d.get("hNew")] = d.get("hClass")

    pending, out = {}, []
    for r in rows:
        if not r:
            continue
        if r[0] == "nvos33in":
            d = kv(r[1:])
            pending[d.get("fd")] = d
        elif r[0] == "mmap" and len(r) >= 4:
            d = pending.get(r[2], {})
            hm = d.get("hMemory")
            out.append((r[1], int(r[3]), hm, cls.get(hm), d.get("flags", "?")))

    print(f"{'#':>3} {'node':<5} {'length':>10}  {'hMemory':<12} class")
    for i, (dev, ln, hm, c, fl) in enumerate(out, 1):
        name = NAMES.get(c, "UVM-managed (no MAP_MEMORY before it)" if c is None
                         else f"hClass {c}")
        print(f"{i:>3} {dev:<5} {ln:>10}  {hm or '-':<12} {name}")

    agg = collections.defaultdict(lambda: [0, 0])
    for dev, ln, hm, c, fl in out:
        key = NAMES.get(c, "UVM-managed" if c is None else f"hClass {c}")
        agg[key][0] += 1
        agg[key][1] += ln

    print(f"\n{'category':<48} {'n':>4} {'bytes':>12}  from guest RAM?")
    guest = 0
    for k, (n, b) in sorted(agg.items(), key=lambda x: -x[1][1]):
        c = next((c for c, nm in NAMES.items() if nm == k), None)
        ok = FROM_GUEST_RAM.get(c)
        if ok:
            guest += b
        print(f"{k:<48} {n:>4} {b / 2**20:>9.2f} MiB  "
              f"{'yes' if ok else 'no' if ok is False else 'open'}")
    tot = sum(v[1] for v in agg.values())
    print(f"{'TOTAL':<48} {sum(v[0] for v in agg.values()):>4} {tot / 2**20:>9.2f} MiB")
    print(f"\nof which in principle from guest RAM: {guest / 2**20:.2f} MiB "
          f"({100 * guest / tot:.1f} %)")
    if out:
        first = out[0]
        print(f"FIRST mapping of all: {first[1]} bytes, "
              f"{NAMES.get(first[3], first[3])}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "probe/traces/lvl4blocking.tsv")
