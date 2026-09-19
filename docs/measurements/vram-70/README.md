<!-- SPDX-License-Identifier: MIT -->
# Workloads under VRAM limits (question 70)

Historical sweep: fourteen guest capacities from 8192 to 384 MiB, one guest at a
time on an RTX 2070 with the host desktop active.

| Workload | Approximate VRAM | Purpose |
|---|---|---|
| Blender `bmw27` | 386 MB | Fits all but the smallest capacity |
| Blender `classroom` | 980 MB device-resident | Tests the capacity threshold |
| `ffmpeg h264_nvenc`, 1080p | Small | Encoder behavior across limits |
| PyTorch `convburn` | ~1 GB | Output compared bit for bit |
| `vrampress --max` | Available capacity | Allocation ceiling |

- `matrix/` contains the fourteen-capacity sweep. At 3072 and 1280 MiB, equivalent
  capacities are reached through `--vgpu-type`, `--vram-limit` and `--vram-profile`.
- `graceful/` tests larger pin budgets and `LEA_MANAGED_COMPAT=1`. Neither changed
  the observed degradation band. Pin settings: host 256→2048 MiB, guest 1024→4096 MiB.
- `glmark2` could not initialize GLX in this compute guest, including under Xvfb.
  Display-rig results are a different setup.
- The sixth recorded workload, `classroom4k`, duplicates `classroom`: its resolution
  option followed Blender's `--` and was passed to the script rather than applied.
  Keep this column as a documented measurement error, not an independent workload.
- `results.tsv` contains results. `classroom-*.txt` and `render-*.txt` retain the
  distinct GPU-memory and combined GPU/host-memory failure messages.
