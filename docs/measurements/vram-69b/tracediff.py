#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
"""OPEN-QUESTIONS 69(a): what libcuda does after the mode answer.

Reads the two guest traces (mode answered / not answered) and prints what
follows GET_VIRTUALIZATION_MODE in each, plus the first record where the
two runs stop agreeing."""
import json, sys, collections

MODE = 0x800289


def load(p):
    out = []
    for line in open(p, errors="replace"):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    return out


def key(r):
    """What makes two records 'the same call' across runs -- deliberately
    not the fd or the return, which legitimately differ."""
    t = r.get("t")
    if t == "ioctl":
        return (t, r.get("dev"), r.get("nr"), r.get("sub"), r.get("size"))
    return (t, r.get("dev"))


def label(r):
    t = r.get("t")
    if t != "ioctl":
        return f'{t} dev={r.get("dev")}'
    sub = r.get("sub")
    s = f'ioctl {r.get("dev")} nr={r.get("nr")}'
    if sub not in (None, "null"):
        s += f" sub={sub}"
    return s + f' size={r.get("size")} ret={r.get("ret")} status={r.get("status")}'


def is_mode(r):
    sub = r.get("sub")
    if not sub:
        return False
    try:
        return int(str(sub), 16 if str(sub).startswith("0x") else 10) == MODE
    except ValueError:
        return False


a_p, b_p = sys.argv[1], sys.argv[2]
A, B = load(a_p), load(b_p)
print(f"mode ON : {len(A)} records from {a_p}")
print(f"mode OFF: {len(B)} records from {b_p}")

for tag, R in (("ON", A), ("OFF", B)):
    idx = [i for i, r in enumerate(R) if is_mode(r)]
    print(f"\n=== mode {tag}: GET_VIRTUALIZATION_MODE at {idx or 'NEVER ASKED'} of {len(R)} ===")
    for i in idx[:2]:
        print(f"  [{i}] {label(R[i])}")
        for j in range(i + 1, min(i + 16, len(R))):
            print(f"    +{j-i:<2} {label(R[j])}")

# where the two runs part company
print("\n=== first divergence ===")
n = min(len(A), len(B))
for i in range(n):
    if key(A[i]) != key(B[i]):
        print(f"  at record {i}:")
        for k in range(max(0, i - 4), min(i + 12, n)):
            m = "  " if key(A[k]) == key(B[k]) else "->"
            print(f"   {m} [{k}] ON  {label(A[k])}")
            print(f"   {m}      OFF {label(B[k])}")
        break
else:
    print(f"  identical for all {n} shared records; ON has {len(A)}, OFF has {len(B)}")

# and the overall shape
print("\n=== control commands, by count ===")
for tag, R in (("ON", A), ("OFF", B)):
    c = collections.Counter(r.get("sub") for r in R if r.get("t") == "ioctl" and r.get("sub"))
    print(f"  {tag}: {len(c)} distinct; top: {', '.join(f'{k}x{v}' for k, v in c.most_common(8))}")

# what only one side ever asked
ka = collections.Counter(key(r) for r in A)
kb = collections.Counter(key(r) for r in B)
only_a = [k for k in ka if k not in kb]
only_b = [k for k in kb if k not in ka]
print(f"\n=== only with the mode answered ({len(only_a)}) ===")
for k in only_a[:15]:
    print("   ", k, f"x{ka[k]}")
print(f"=== only without it ({len(only_b)}) ===")
for k in only_b[:15]:
    print("   ", k, f"x{kb[k]}")
