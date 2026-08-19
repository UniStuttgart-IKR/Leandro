#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Which UVM commands does a workload use -- and does it use managed memory?

Resolves the raw command numbers from the traces against uvm_ioctl.h rather
than guessing them. The question behind it: does the workload need
migrating UVM (cudaMallocManaged), or does it use UVM only as a page-table
mapper for memory RM allocated? That decides whether the "managed memory"
case has to be served at all.

  probe/python/uvmtax.py [trace.tsv ...]      # no argument: every trace under probe/traces/
"""
import collections
import glob
import os
import re
import sys

# Two levels up: this file sits in probe/python/, the vendor tree at the
# repository root. One level ("../vendor") named probe/vendor and raised
# FileNotFoundError on every run.
HEADER = os.path.join(
    os.path.dirname(__file__),
    "../../vendor/open-gpu-kernel-modules/kernel-open/nvidia-uvm/uvm_ioctl.h",
)

# Commands that mark migrating managed memory. If they do not appear, the
# workload is not using UVM as a memory manager.
MIGRATION = [
    "UVM_MIGRATE",
    "UVM_MIGRATE_RANGE_GROUP",
    "UVM_SET_PREFERRED_LOCATION",
    "UVM_UNSET_PREFERRED_LOCATION",
    "UVM_SET_ACCESSED_BY",
    "UVM_UNSET_ACCESSED_BY",
    "UVM_ENABLE_READ_DUPLICATION",
]


def names():
    src = open(HEADER).read()
    out = {}
    for m in re.finditer(r"#define\s+(UVM_\w+)\s+UVM_IOCTL_BASE\((\d+)\)", src):
        out[int(m.group(2))] = m.group(1)
    return out


def main(paths):
    nm = names()
    counts = collections.Counter()
    mmaps = collections.Counter()
    for f in paths:
        for line in open(f, errors="replace"):
            p = line.rstrip("\n").split("\t")
            if len(p) > 2 and p[0] == "ioctl" and p[1] == "uvm":
                counts[p[2]] += 1
            elif len(p) > 1 and p[0] == "mmap" and p[1] == "uvm":
                mmaps[os.path.basename(f)] += 1

    print(f"{'nr':>12} {'dec':>5} {'count':>6}  name")
    for nr, c in counts.most_common():
        try:
            d = int(nr, 16)
        except ValueError:
            continue
        print(f"{nr:>12} {d:>5} {c:>6}  {nm.get(d, '(no UVM_IOCTL_BASE)')}")

    print("\nCPU-visible UVM mappings per trace:")
    for f in paths:
        print(f"  {os.path.basename(f):<20} {mmaps.get(os.path.basename(f), 0)}")

    print("\nMigration commands (managed memory):")
    any_seen = False
    for want in MIGRATION:
        num = next((k for k, v in nm.items() if v == want), None)
        if num is None:
            continue
        seen = counts.get(hex(num), 0)
        if seen:
            any_seen = True
        # "seen 0x" was the German "0 mal", and in a tool whose other two
        # columns are hex it read as the number zero. Plain count.
        print(f"  {want:<32} nr {num:>3}  seen {seen}")
    print(
        "\n=> "
        + (
            "managed memory is in use."
            if any_seen
            else "NO managed memory: UVM serves only as a mapper for "
            "RM-allocated memory."
        )
    )


if __name__ == "__main__":
    # ../traces, for the reason HEADER gives: this file sits in
    # probe/python/, the traces in probe/traces/ -- which is what the
    # docstring above promises for a run without arguments.
    args = sys.argv[1:] or sorted(
        glob.glob(os.path.join(os.path.dirname(__file__), "../traces/*.tsv"))
    )
    main(args)
