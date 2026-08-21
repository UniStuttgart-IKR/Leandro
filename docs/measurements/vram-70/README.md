<!-- SPDX-License-Identifier: MIT -->
# What a VRAM limit costs (OPEN-QUESTIONS 70)

Fourteen guest framebuffer sizes from 8192 down to 384 MiB, six workloads
each, one guest at a time, on one RTX 2070 with the host desktop running.

## Why these workloads

| | VRAM | what it is here for |
|---|---|---|
| Blender `bmw27` | 386 MB | fits every size but the smallest -- the control |
| Blender `classroom` | 980 MB device-resident | the one that stops fitting |
| `ffmpeg h264_nvenc` 1080p | small | the encoder, which no size touched |
| torch `convburn` | ~1 GB | this project checks its output BIT FOR BIT |
| `vrampress --max` | all of it | the ceiling itself |

`glmark2` is deliberately absent: it answers "Could not initialize canvas"
in a compute guest, under Xvfb and `xvfb-run` alike, because there is no
NVIDIA GLX for it to bind. The repository's own glmark2 figure was measured
on the DISPLAY rig, which is a heavier setup than this sweep.

`classroom4k` in the matrix is a NO-OP and its numbers duplicate
`classroom`: the resolution argument was passed after `--`, which Blender
hands to the script rather than acting on. Kept rather than deleted,
because a column that quietly measured the same thing twice is worth
seeing.

## The two runs

  * `matrix/` -- the fourteen sizes. At 3072 and at 1280 MiB the same guest
    number is reached three ways (`--vgpu-type`, `--vram-limit`,
    `--vram-profile`) so that the policy and the number are separated.
  * `graceful/` -- whether the width of the degradation band is a knob we
    own. Two candidates tested, both negative: the pin budget
    (`LEA_MAX_PIN_MIB` 256 -> 2048, guest `max_pin_mib` 1024 -> 4096) and
    `LEA_MANAGED_COMPAT=1`.

## Files

`results.tsv` in each is the table; `classroom-*.txt` and `render-*.txt`
are Cycles' own output, including the exact wording of each failure --
"System is out of GPU memory" and "out of GPU and shared host memory" are
different errors and the difference is the finding.
