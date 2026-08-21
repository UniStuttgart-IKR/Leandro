<!-- SPDX-License-Identifier: MIT -->
# The density benchmark for OPEN-QUESTIONS 69

Four VRAM policies against the same load on the same card, and a bisect of
the three vGPU-shaped ANSWERS that turned out to matter more than any of
them.

## The four policies, four VMs each

Fleet members `vm0..vm3` (thin overlays on the frozen torch base, 1536 MiB
of RAM and 2 vCPU each -- the host has 16 threads and ~20 GiB free, and the
runner refuses a configuration that would not leave it a 4096 MiB floor).
`probe/bin/vrampress --max --seconds 120` in every guest at once, host
sampled at 1 Hz.

All three capped rows give the guest **the same 1280 MiB**, so what differs
is the policy and not the budget:

| row | policy | what the guest is told |
|---|---|---|
| `n4-off` | none | 8192 MiB, the whole card |
| `n4-limit` | `LEA_VRAM_LIMIT_MIB=1280` | 1280; the card pays that plus RM's own overhead |
| `n4-profile` | `LEA_VRAM_PROFILE_MIB=1536` | 1280; the VM costs the card at most 1536 |
| `n4-grid` | `LEA_VGPU_TYPE=2Q` | 1280, quantised to 5 VMMU segments, admission-bounded, card named `Leandro RTX2070-2Q` |

## The bisect

`bisect/bisect.txt`: one guest, six times, one combination of
`LEA_VGPU_MEDIATE` each. It answers the question this branch was opened
for -- can the guest be made to believe it is a GRID vGPU -- and the answer
is yes, at a price nvidia-smi cannot see.

## Files

| | |
|---|---|
| `<row>/host-1hz.csv` | `ts,used_mib,free_mib,util,ram_avail_mib,vm0..vmN` |
| `<row>/vrampress-vm*.csv` | the load's own log per guest |
| `<row>/catalogue.txt` | what the card answered when the row started |
| `<row>/run.txt`, `<row>/policy.txt` | how the row was run, and the policy line each backend printed |
| `vgpubench.sh`, `benchsum.py` | the runner and the scorer |
