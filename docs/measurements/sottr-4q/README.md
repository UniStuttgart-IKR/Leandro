<!-- SPDX-License-Identifier: MIT -->
# Concurrent game benchmarks (questions 70–71)

Historical run: two `RTX2070-4Q` guests, 3072 MiB each, running Shadow of the
Tomb Raider's benchmark concurrently on one RTX 2070 through Sunshine/Moonlight.

| Measurement | 720p | 1080p |
|---|---|---|
| Card used, peak | 7411 MiB | 7771 MiB |
| Card free, minimum | 361 MiB | 1 MiB |
| Guest .15 charge, peak | 2919 MiB | 3130 MiB |
| Guest .17 charge, peak | 2928 MiB | 3184 MiB |
| Combined charge | 5840 / 6144 MiB | 6243 / 6144 MiB |
| Result | Both completed | Second scene failed to load |

- `refusals.csv` is empty: neither ledger returned `NV_ERR_NO_MEMORY`.
- The recorded per-guest charge exceeded the profile by about 110 MiB at 1080p.
  A profile did not bound total physical occupancy.
- Guest .15 kept encoding frames and reported 77–86% GPU use while its loading
  screen remained static. This does not reproduce the compositor freeze in question 67.
- Both guests recorded zero NVKMS allocation failures.
- Host RAM was also exhausted: 415 MiB free, 6.3 GB swap used and load 13 on 16
  threads, with two 8 GiB VMs. The run does not isolate one cause of the loading failure.
- 509 host samples at 1 Hz; `MARK-*.txt` delimit intervals. `sample.sh` documents columns.
