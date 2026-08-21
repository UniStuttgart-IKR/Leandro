#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Summarise the density benchmark: one line per row of the matrix."""
import csv, os, re, sys, glob

BASE = sys.argv[1] if len(sys.argv) > 1 else '.'

def row(d):
    tag = os.path.basename(d)
    hz = os.path.join(d, 'host-1hz.csv')
    if not os.path.exists(hz):
        return None
    rows = []
    for r in csv.DictReader(l for l in open(hz) if not l.startswith('#')):
        rows.append(r)
    if not rows:
        return None
    cols = [k for k in rows[0] if k and k.startswith('vm')]
    used = [int(r['used_mib']) for r in rows if r['used_mib'].isdigit()]
    free = [int(r['free_mib']) for r in rows if r['free_mib'].isdigit()]
    ram = [int(r['ram_avail_mib']) for r in rows if r.get('ram_avail_mib','').isdigit()]
    util = [int(r['util']) for r in rows if r.get('util','').isdigit()]
    # per-backend peak, and the combined peak over the same instants
    peaks, comb = {}, 0
    for c in cols:
        v = [int(r[c]) for r in rows if r.get(c) and r[c].isdigit()]
        peaks[c] = max(v) if v else 0
    for r in rows:
        s = sum(int(r[c]) for c in cols if r.get(c) and r[c].isdigit())
        comb = max(comb, s)
    # what each guest managed to hold, and who got nothing
    held, starved, refusals = [], 0, 0
    for f in sorted(glob.glob(os.path.join(d, 'vrampress-vm*.csv'))):
        txt = open(f, errors='replace').read()
        m = re.search(r'grown to (\d+) MiB held', txt)
        h = int(m.group(1)) if m else 0
        if 'cuCtxCreate' in txt and 'no context' in txt:
            h = -1                      # could not even make a context
        held.append(h)
        if h <= 0:
            starved += 1
        m = re.search(r'(\d+) refusals', txt)
        if m:
            refusals += int(m.group(1))
    policy = ''
    run = os.path.join(d, 'run.txt')
    if os.path.exists(run):
        m = re.search(r'policy=(\S+) count=(\d+)', open(run).read())
        if m:
            policy, n = m.group(1), m.group(2)
    ups = re.search(r'up: (\d+) of (\d+)', open(run).read()) if os.path.exists(run) else None
    return dict(tag=tag, policy=policy, up=ups.group(0)[4:] if ups else '?',
                peak_used=max(used) if used else 0, min_free=min(free) if free else 0,
                comb=comb, peaks=peaks, held=held, starved=starved, refusals=refusals,
                ram_min=min(ram) if ram else 0, util=sum(util)//len(util) if util else 0,
                guests=len(held))

print(f"{'row':<12} {'policy':<13} {'up':>6} {'peak used':>10} {'min free':>9} "
      f"{'combined':>9} {'guests':>7} {'starved':>8} {'held MiB each':<28} {'util%':>6} {'RAM free':>9}")
for d in sorted(glob.glob(os.path.join(BASE, '*'))):
    if not os.path.isdir(d):
        continue
    r = row(d)
    if not r:
        print(f"{os.path.basename(d):<12} (no samples)")
        continue
    held = ' '.join(str(h) for h in r['held'])
    print(f"{r['tag']:<12} {r['policy']:<13} {r['up']:>6} {r['peak_used']:>10} {r['min_free']:>9} "
          f"{r['comb']:>9} {r['guests']:>7} {r['starved']:>8} {held:<28} {r['util']:>6} {r['ram_min']:>9}")
