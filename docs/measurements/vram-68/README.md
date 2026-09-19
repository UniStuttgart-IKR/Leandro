<!-- SPDX-License-Identifier: MIT -->
# VRAM policy comparison (question 68)

Historical run, 2026-08-21: two guests on one card under sustained CUDA allocation
pressure. Compare the two runs below; neither reproduces the earlier game freeze.

| Run | Policy |
|---|---|
| `reserved-2026-08-21/` | `--vram-profile 3072` |
| `accounting-2026-08-21/` | `--vram-limit 3072` |

## Setup

- GNOME Wayland guests, indices 5 and 7, each with 6 vCPUs and 8192 MiB RAM.
- `vkcube-wayland` supplies animation for framebuffer checks.
- Both guests run `vrampress --max --seconds 600`: fill with 128 MiB CUDA blocks,
  then vary allocations near the limit.
- Host samples at 1 Hz: card use/free/utilization, encoder statistics and per-backend use.
- Sunshine runs without clients. The earlier freeze involved games and live 1080p
  streams, including about 595 MiB of host decoder use absent here.

## Results

| Measurement | Profile | Limit | Earlier freeze |
|---|---|---|---|
| Guest-visible capacity | 2816 MiB | 3072 MiB | 3072 MiB |
| Combined charge, peak | 5674 MiB | 6186 MiB | ~6494 MiB |
| Per-backend peaks | 2841 / 2842 MiB | 3099 / 3096 MiB | 3242 / 3101 MiB |
| Card used, peak | 6664 MiB | 7178 MiB | Not recorded here |
| Card free, minimum | 1108 MiB | 595 MiB | 1 MiB |
| Distinct use values per 30 s | 23–29 | 20–29 | 3–6 when frozen |
| Framebuffer under load | Changing | Changing | Static |
| Backend refusals per guest | 259 / 278 | 268 / 258 | 29 in one guest |
| NVKMS allocation failures | 0 / 0 | 0 / 0 | Both guests |

- Profile combined charge stayed 470 MiB below the 6144 MiB total; limit charge
  exceeded it by 42 MiB. Neither run reached the earlier 1 MiB free-memory floor.
- Churn medians: 26/26 across 17 windows per profile guest; 25/26 across 18 windows
  per limit guest. Both framebuffers continued changing at their allocation limits.
- These results compare accounting under this workload; they do not prove physical
  reservation, tenant isolation or recovery from the earlier freeze.

## Measurement limitations

- A static framebuffer is inconclusive unless an application is known to animate.
  The profile run started animation about a minute after load, so its initial
  `STATIC`/`AMBER` results are an idle control. The limit baseline already had animation.
- Editing the active runner caused it to fail at 20:31. Load and guests continued;
  `salvage.sh` resumed sampling and teardown. The profile CSV has a roughly 50 s gap.
- Guest CSV timestamps are UTC; host samples use UTC+2.

Recompute from this directory:

```sh
LOAD_SECONDS=600 python3 analyse.py reserved-2026-08-21 3072
LOAD_SECONDS=600 python3 analyse.py accounting-2026-08-21 3072
```

| Files | Contents |
|---|---|
| `host-1hz.csv` | Card, encoder and per-backend samples |
| `guest-desktop*.csv` | Guest total/used/free VRAM every 5 s |
| `vrampress-desktop*.csv` | Allocator log every second |
| `fbprobe-*.txt` | Framebuffer checks before, during and after load |
| `backend-desktop*.txt` | Policy and refusal logs |
| `dmesg-*.txt`, `nvkms-fail-*.txt` | Kernel logs and allocation-failure counts |
| `up-*.txt`, `policy.txt`, `down.txt`, `driver.txt` | Setup, teardown and runner failure |
| `vramrun.sh`, `salvage.sh`, `analyse.py` | Historical runner, recovery and analysis |
