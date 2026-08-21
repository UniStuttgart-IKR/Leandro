<!-- SPDX-License-Identifier: MIT -->
# The acceptance run for OPEN-QUESTIONS 68

Two runs, one rig, one variable: the VRAM policy. Everything else is held
still -- same two guests, same card, same load, same instruments, minutes
apart on 2026-08-21.

    reserved-2026-08-21/     --vram-profile 3072   (LEA_VRAM_PROFILE_MIB)
    accounting-2026-08-21/   --vram-limit   3072   (LEA_VRAM_LIMIT_MIB)

## What was run

    LEA_SUN_CAPTURE=kms scripts/showcase.sh up --name desktop  --index 5 \
        --session gnome --wayland <policy flag> --cpus 6 --mem 8192
    LEA_SUN_CAPTURE=kms scripts/showcase.sh up --name desktop2 --index 7 \
        --session gnome --wayland <policy flag> --cpus 6 --mem 8192

then in each guest, at the same moment:

  * `vkcube-wayland` on the GNOME Wayland session -- something has to
    ANIMATE or `fbprobe` reads `STATIC` on a guest that is perfectly
    healthy, which is the frozen signature. See the control below.
  * `probe/bin/vrampress --max --seconds 600` -- CUDA, 128 MiB blocks until
    something refuses, then blocks of varying size in and out at the
    ceiling for the rest of the run.

and on the host, at 1 Hz: `memory.used`, `memory.free`, `utilization.gpu`,
`encoder.stats.sessionCount/averageFps`, and per-backend `used_memory` from
`nvidia-smi --query-compute-apps`, resolved through each instance's
`nvrm.pid`.

## What this run is NOT

It is not the recorded 2026-08-21 workload. That one ran Steam, Shadow of
the Tomb Raider and a live 1080p Moonlight stream per guest; this one runs a
CUDA allocator and a spinning cube, with Sunshine up but no client attached
(`encoder.stats.sessionCount` is 0 throughout, and the ~595 MiB the two
Moonlight decoders cost the card is absent). So the card is under less total
demand here than it was then, and the comparison that carries is the one
between THESE TWO RUNS, not between either of them and the recorded freeze.

What the allocator does test, and the games did not, is the refusal path
under sustained pressure: both guests sit at their own limit for ten
minutes, being refused.

## The control that had to be added

The first attempt read `fbprobe` before starting any animation. All three
readers said `STATIC ... frame changed in 0/9 polls` and the verdict was
`AMBER` -- on two guests that were entirely healthy, which is exactly the
reading number 67 records for a FROZEN guest. `glxgears` is not installed in
this image and the session is Wayland; `vkcube-wayland` is, and is what the
frame limiter was measured with. **A `STATIC` reading means nothing unless
something in the guest is known to be drawing**, and both `before` readings
in `reserved-2026-08-21/` are kept as the demonstration of that.

## Where the two runs differ, apart from the policy

Two things, both from the control above being learned during the first run
rather than before it:

  * In `reserved-2026-08-21/` the animation was started about a minute AFTER
    the load, so its `before` reading is the no-animation control
    (`STATIC`/`AMBER`) rather than a healthy baseline. In
    `accounting-2026-08-21/` `vkcube-wayland` is up before the baseline is
    taken, and that reading is `GREEN`.
  * The first run's runner was edited while it was running and died in a
    `sleep` at 20:31 (see docs/llm.md, "Never edit a shell script while it is
    running"). The rig, the load and both guests carried on; the sampler was
    restarted within a minute and the remaining readings and the teardown
    were done by `salvage.sh`. `host-1hz.csv` therefore has a gap of about
    50 s at 20:31, and `driver.txt` records the failure rather than hiding
    it.

## What the two runs measured

| | reservation, `--vram-profile 3072` | accounting, `--vram-limit 3072` | the recorded freeze |
|---|---|---|---|
| what the guest is told it has | **2816 MiB** | 3072 MiB | 3072 MiB |
| combined charge to the card, peak | **5674 MiB** | **6186 MiB** | ~6494 MiB |
| against the sum of the two numbers, 6144 | inside it by 470 | **over it by 42** | over it |
| per backend, peak | 2841 / 2842 | 3099 / 3096 | 3242 / 3101 |
| card used, peak | 6664 MiB | 7178 MiB | -- |
| card free, MINIMUM | **1108 MiB** | 595 MiB | **1 MiB** |
| churn, distinct values per 30 s | 23-29 (medians 26 and 26, 17 windows per guest) | 20-29 (medians 25 and 26, 18 windows per guest) | 3-6 when frozen |
| `fbprobe` under load | `CONTENT`, frame changing | `CONTENT`, frame changing | `STATIC`, 0/8 |
| refusals logged by the backend | 259 / 278 | 268 / 258 | 29, one guest |
| `Failed to allocate NVKMS memory` | 0 and 0 | 0 and 0 | both guests |

Recomputed at any time with:

    LOAD_SECONDS=600 python3 analyse.py reserved-2026-08-21 3072
    LOAD_SECONDS=600 python3 analyse.py accounting-2026-08-21 3072

The four conditions were fixed before the runs. Under the reservation
policy: combined charge 5674 MiB inside the 6144 the two profiles
sum to; card free never below 1108 MiB against the 1 MiB the
recorded freeze reached; the freeze detector never fired (23-29 (medians 26 and 26, 17 windows per guest)
distinct values per 30 s, against 3-6 when frozen); and `fbprobe` read moving
content in both guests while they sat at their limit. Under the accounting
policy the same load put 6186 MiB on the card -- 42
MiB MORE than the two numbers the operator set -- and left 513
MiB less of it free.

Neither run reached the 1 MiB floor: this load is lighter on the card than
the recorded one. The clause that separates the policies is the arithmetic,
not the floor.

## Files

| | |
|---|---|
| `host-1hz.csv` | the host sampler: `ts,used_mib,free_mib,util,enc_sessions,enc_fps,desktop_mib,desktop2_mib` |
| `guest-desktop*.csv` | what each guest's own `nvidia-smi` reported, every 5 s, `ts,total,used,free`. **UTC**, where the host samples are local (+2) |
| `vrampress-desktop*.csv` | the load's own log, one line per second |
| `fbprobe-*-{before,during,late,after}.txt` | the scanout reader, per guest and per point in the run |
| `backend-desktop*.txt` | the backend log, including the policy line and every refusal |
| `dmesg-*.txt`, `nvkms-fail-*.txt` | the guest kernel, and the count of `Failed to allocate NVKMS memory for GEM object` |
| `up-*.txt`, `policy.txt`, `down.txt` | how the rig was brought up and taken down |
| `vramrun.sh`, `analyse.py` | the runner and the scorer, so the numbers below can be recomputed |
