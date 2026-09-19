<!-- SPDX-License-Identifier: MIT -->
# Admission and vGPU-mode traces (question 69)

Historical runs on one RTX 2070 with the host desktop active.

## Admission

- Four running `2Q` guests: a fifth is refused by the profile's instance limit.
- Running `4Q + 2Q + 2Q`: an additional `1Q` is refused for insufficient capacity.
- Both refusals precede VM creation. `admission/catalogue.txt` records the profiles.

## Mixed profiles

- One `4Q` and two `2Q` consume the 8192 MiB admission budget.
- All run `vrampress --max` for 120 s; observed allocation ceilings are
  3072 / 1280 / 1280 MiB, matching the individual guest limits.
- `hetero/p2-host-1hz.csv` records host samples; `p2-vrampress-vm*.csv` records
  guest results. This validates these admission/refusal cases, not GPU isolation.

## CUDA mode comparison

- The same binary runs in the same `2Q` guest with `LEA_VGPU_MEDIATE=mode` and `none`.
- The first 145 trace records match. At record 146, vGPU mode causes libcuda to
  allocate `0xa080` (`KEPLER_DEVICE_VGPU`); RM returns `NV_ERR_NOT_SUPPORTED`.
- Recorded vendor source rejects construction on `!IS_VIRTUAL(pGpu)` in
  `vgpuapiConstruct_IMPL` (`kernel/vgpu/vgpuapi.c:43`). The caller is `queryVirtMode`
  (`rmapi/nv_gpu_ops.c:7117`), through nvUvmInterface. `nvidia-smi` does not exercise it.
- `libcuda/analysis.txt` compares the complete traces: 4474 records without mode
  mediation, 164 with it. The stored `mode-off.jsonl` contains only the first 400
  records, including the divergence; `mode-on.jsonl` is complete.
- The tracer was copied into the compute guest by the historical `cudatrace.sh` runner.
- Trace format and license: [TRACES.md](libcuda/TRACES.md).
