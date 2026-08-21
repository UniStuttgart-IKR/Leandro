#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Which allocation classes the descriptor table carries, and which any run
has actually allocated.

OPEN-QUESTIONS number 59 asks for exactly this number and says why: the 27
matrix probes are ENTRY PATHS, and the yield of any new-workload effort is
"which of the untouched classes did we reach" -- a diff that can be computed
BEFORE choosing a workload rather than discovered afterwards. This computes
it.

BOTH SIDES ARE DERIVED, neither is declared:

  * the table side is parsed out of the SERIALISED table stream that the
    guest module is built against (``nvrm-genhdr --dump-tables``), so it is
    the same 128 classes the boundary actually carries, checksum included --
    not a list re-typed from ``xlate.rs``;
  * the touched side is read out of the trace JSONL of runs that happened.
    A class counts as touched only if an allocation of it was RECORDED, and
    the status it came back with is reported beside it.

A class that is allocated but is NOT in the table is not an error. Classes
whose ``resource_list.h`` row is ``RS_NONE`` take no allocation parameters,
so their ``pAllocParms`` is NULL and there is nothing to size; ``xlate.rs``
names them where its class table ends. They are listed separately rather
than hidden, because "allocated 190 times, carried fine, in no table" is
worth being able to see.

Usage:
    probe/python/classcoverage.py TABLES TRACEDIR [--json]

TABLES is a file written by ``nvrm-genhdr --dump-tables``. TRACEDIR is
searched recursively for ``*.jsonl``; a path component ``guest`` marks a
trace as the guest side, everything else is native.
"""
import json
import os
import struct
import sys
from glob import glob

# nvrm_wire.h: nvrm_table_hdr is 96 bytes, nvrm_ioctl_desc 48, and
# nvrm_class_desc 24 with hclass first. Those four numbers carry
# _Static_asserts on the C side, so a layout change breaks the module build
# before it can quietly break this.
HDR_LEN, IOCTL_DESC_LEN, CLASS_DESC_LEN = 96, 48, 24
MAGIC = 0x5452564E  # "NVRT"


def read_table_classes(path):
    """-> (classes, checksum). Parsed, never assumed."""
    b = open(path, "rb").read()
    if len(b) < HDR_LEN:
        raise SystemExit(f"{path}: {len(b)} bytes, shorter than the header")
    magic, _ver, _total, checksum, n_ioctl, n_class = struct.unpack_from("<6I", b, 0)
    if magic != MAGIC:
        raise SystemExit(f"{path}: magic {magic:#x}, not a table stream")
    off = HDR_LEN + n_ioctl * IOCTL_DESC_LEN
    need = off + n_class * CLASS_DESC_LEN
    if len(b) < need:
        raise SystemExit(f"{path}: truncated -- {len(b)} bytes, {need} wanted")
    out = []
    for i in range(n_class):
        out.append(struct.unpack_from("<I", b, off + i * CLASS_DESC_LEN)[0])
    return out, checksum


def read_touched(tracedir):
    """-> {hclass: {"where": set, "status": {code: n}}} from recorded allocations."""
    touched = {}
    for f in glob(os.path.join(tracedir, "**", "*.jsonl"), recursive=True):
        side = "guest" if f"{os.sep}guest{os.sep}" in f else "native"
        probe = os.path.basename(f)[: -len(".jsonl")]
        for ln in open(f):
            # Cheap reject first: these files run to tens of thousands of
            # lines per probe and only the allocation records matter.
            if '"nvos64"' not in ln:
                continue
            try:
                r = json.loads(ln)
            except ValueError:
                continue
            if r.get("phase") != "out":
                continue
            try:
                c = int(r.get("hClass") or "0", 16)
            except ValueError:
                continue
            if not c:
                continue
            e = touched.setdefault(c, {"where": set(), "status": {}})
            e["where"].add(f"{probe}/{side}")
            st = r.get("status", "?")
            e["status"][st] = e["status"].get(st, 0) + 1
    return touched


def main(argv):
    if len(argv) < 3:
        raise SystemExit(__doc__.strip().splitlines()[-4].strip())
    tables, tracedir = argv[1], argv[2]
    as_json = "--json" in argv[3:]

    classes, checksum = read_table_classes(tables)
    table = set(classes)
    touched = read_touched(tracedir)

    hit = sorted(table & set(touched))
    miss = sorted(table - set(touched))
    extra = sorted(set(touched) - table)

    if as_json:
        json.dump(
            {
                "checksum": f"{checksum:#010x}",
                "n_table": len(table),
                "n_touched": len(hit),
                "n_missing": len(miss),
                "touched": [f"{c:#06x}" for c in hit],
                "missing": [f"{c:#06x}" for c in miss],
                "untabled": {
                    f"{c:#06x}": {
                        "allocations": sum(touched[c]["status"].values()),
                        "status": touched[c]["status"],
                    }
                    for c in extra
                },
            },
            sys.stdout,
            indent=1,
        )
        print()
        return 0

    pct = 100 * len(hit) // len(table) if table else 0
    print(f"descriptor table (checksum {checksum:#010x}): {len(table)} allocation classes")
    print(f"  allocated by some recorded run : {len(hit):>3}  ({pct}%)")
    print(f"  never allocated by any run     : {len(miss):>3}")
    print()
    print("NEVER ALLOCATED -- the target list number 59 asks to be aimed by.")
    print("READ IT WITH THE CARD IN MIND: this list is what THIS run's hardware")
    print("did not allocate, and a large part of it is engine classes of other")
    print("architectures (the 0xc7/0xc9/0xcd/0xce families are Ada, Hopper and")
    print("Blackwell) which a Turing card cannot allocate at all. Those are not")
    print("workload targets here -- they are targets for a run on that silicon,")
    print("which is why the same number from a second machine is worth having.")
    for i in range(0, len(miss), 8):
        print("   " + "  ".join(f"{c:#06x}" for c in miss[i : i + 8]))
    if extra:
        print()
        print("Allocated but carried by NO table entry. Not an error: an")
        print("RS_NONE class passes pAllocParms = NULL and has nothing to size.")
        for c in extra:
            e = touched[c]
            n = sum(e["status"].values())
            st = ", ".join(f"{k} x{v}" for k, v in sorted(e["status"].items()))
            print(f"   {c:#06x}  {n:>4} allocation(s)  status {st}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
