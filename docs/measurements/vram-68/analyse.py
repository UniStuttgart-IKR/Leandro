#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
"""Score one acceptance run of OPEN-QUESTIONS 68 against its four conditions.

   analyse.py <run-dir> <profile-mib> [--label NAME]

Reads host-1hz.csv (the host sampler), the vrampress logs and the fbprobe
verdicts, and prints one block per condition. It states the numbers it
judged on; a condition it cannot judge says so rather than passing.
"""
import sys, csv, os, re, glob

d = sys.argv[1]
prof = int(sys.argv[2])
label = sys.argv[4] if len(sys.argv) > 4 else os.path.basename(d.rstrip('/'))

rows = []
with open(os.path.join(d, 'host-1hz.csv')) as f:
    # The committed copies carry a leading SPDX comment; the gate wants one
    # and a reader has to step over it.
    for r in csv.DictReader(l for l in f if not l.startswith('#')):
        try:
            rows.append({k: (int(v) if v and v.strip('-').isdigit() else v)
                         for k, v in r.items()})
        except Exception:
            pass

print(f"== {label}: {len(rows)} samples of 1 Hz, profile {prof} MiB per VM ==")
if not rows:
    sys.exit("no samples")

# Only the window in which the load actually ran: both backends charged.
live = [r for r in rows if r['desktop_mib'] and r['desktop2_mib']]
print(f"   both backends charged in {len(live)} of {len(rows)} samples")

comb = [r['desktop_mib'] + r['desktop2_mib'] for r in live]
free = [r['free_mib'] for r in live]
used = [r['used_mib'] for r in live]
print()
print("1. COMBINED CHARGE AND THE FLOOR")
print(f"   sum of profiles           {2*prof} MiB")
print(f"   combined charge  max      {max(comb)} MiB  (mean {sum(comb)//len(comb)})")
print(f"   per backend      max      desktop {max(r['desktop_mib'] for r in live)}, "
      f"desktop2 {max(r['desktop2_mib'] for r in live)}")
print(f"   card used        max      {max(used)} MiB")
print(f"   card free        MIN      {min(free)} MiB   (the 2026-08-21 run reached 1)")
print(f"   -> combined <= sum of profiles: {'PASS' if max(comb) <= 2*prof else 'FAIL'}")
print(f"   -> free never at the floor:     {'PASS' if min(free) > 1 else 'FAIL'} (min {min(free)})")

print()
print("2. THE FREEZE DETECTOR (distinct per-backend values per 30 s)")
print("   frozen in the recorded run: 3-6.  healthy: 29-43.")
worst = {}
for name in ('desktop_mib', 'desktop2_mib'):
    vals = [r[name] for r in live]
    win = []
    for i in range(0, max(1, len(vals) - 29), 30):
        chunk = vals[i:i+30]
        if len(chunk) == 30:
            win.append(len(set(chunk)))
    if win:
        worst[name] = min(win)
        print(f"   {name:13s} windows {len(win)}  distinct min {min(win)}  "
              f"median {sorted(win)[len(win)//2]}  max {max(win)}")
    else:
        print(f"   {name:13s} not enough samples for a 30 s window")
if worst:
    ok = all(v > 10 for v in worst.values())
    print(f"   -> churn stayed above 10 in every window: {'PASS' if ok else 'FAIL'} "
          f"(worst {min(worst.values())})")

print()
print("3. FBPROBE")
for f in sorted(glob.glob(os.path.join(d, 'fbprobe-*.txt'))):
    if f.endswith('build.txt'):
        continue
    txt = open(f, errors='replace').read()
    ways = re.findall(r'^\s+(mmap|gl|cuda)\s+(\S+)(?:.*?frame changed in (\d+)/(\d+))?', txt, re.M)
    verdict = re.findall(r'READER\s+(\w+)', txt)
    pretty = '  '.join(f"{w}={v}{(' ' + c + '/' + t) if c else ''}" for w, v, c, t in ways)
    print(f"   {os.path.basename(f):34s} {verdict}  {pretty or 'no reader line'}")

print()
print("4. THE LOAD ITSELF")
for f in sorted(glob.glob(os.path.join(d, 'vrampress-*.csv'))):
    txt = open(f, errors='replace').read().splitlines()
    head = [l for l in txt if l.startswith('vrampress:')]
    data = [l for l in txt if re.match(r'^\d+,', l)]
    print(f"   {os.path.basename(f)}: {len(data)} samples")
    for l in head:
        print(f"      {l}")

print()
print("5. WHAT THE GUEST BELIEVED IT HAD")
for f in sorted(glob.glob(os.path.join(d, 'guest-desktop*.csv'))):
    lines = [l for l in open(f, errors='replace').read().splitlines()
             if ',' in l and not l.startswith('#')]
    if lines:
        print(f"   {os.path.basename(f)}: first {lines[0]}  last {lines[-1]}")

print()
print("6. REFUSALS AND NVKMS")
for f in sorted(glob.glob(os.path.join(d, 'backend-*.txt'))):
    txt = open(f, errors='replace').read()
    print(f"   {os.path.basename(f)}: {len(re.findall('VRAM cap reached', txt))} refusal lines, "
          f"policy: {'; '.join(set(re.findall(r'VRAM (?:cap|profile) [0-9]+ MiB[^.]*', txt)))[:110]}")
for f in sorted(glob.glob(os.path.join(d, 'nvkms-fail-*.txt'))):
    print(f"   {os.path.basename(f)}: {open(f).read().strip()} NVKMS allocation failures in dmesg")

print()
print("7. THE OVERHEAD THE POLICY IS SIZED AGAINST")
print("   host charge (nvidia-smi, per backend process) minus what the guest")
print("   reports used, at the same second. Guest clocks are UTC, host samples")
print("   are local (+2), and the guest samples every 5 s.")
host = {r['ts']: r for r in rows}
for name, col in (('desktop', 'desktop_mib'), ('desktop2', 'desktop2_mib')):
    f = os.path.join(d, f'guest-{name}.csv')
    if not os.path.exists(f):
        continue
    diffs = []
    for line in open(f, errors='replace'):
        if line.startswith('#'):
            continue
        p = line.strip().split(',')
        if len(p) != 4 or not p[1].isdigit():
            continue
        hh, mm, ss = p[0].split(':')
        local = f"{(int(hh)+2)%24:02d}:{mm}:{ss}"
        h = host.get(local)
        if not h or not h[col]:
            continue
        guest_used = int(p[2])
        if guest_used < 100:      # before the load: nothing to compare
            continue
        diffs.append((local, h[col], guest_used, h[col] - guest_used))
    if diffs:
        vals = [x[3] for x in diffs]
        print(f"   {name}: {len(diffs)} paired samples, host-minus-guest "
              f"min {min(vals)} mean {sum(vals)//len(vals)} max {max(vals)} MiB")
        print(f"      peak host charge {max(x[1] for x in diffs)} MiB against a "
              f"profile of {prof} MiB")
    else:
        print(f"   {name}: no paired samples")
