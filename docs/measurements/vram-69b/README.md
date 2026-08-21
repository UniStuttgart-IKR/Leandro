<!-- SPDX-License-Identifier: MIT -->
# Admission, mixed profiles, and what libcuda asks for (OPEN-QUESTIONS 69)

The three runs that closed number 69, on one RTX 2070 with the host desktop
running.

## `admission/` -- both refusals

Refused BEFORE the VM exists, which is the point of admitting rather than
letting an allocation fail at minute two:

  * four `2Q` running, a fifth asked for -> vGPU's own maxInstance error;
  * `4Q + 2Q + 2Q` running -> a `1Q` refused for NO ROOM, which is the
    refusal the old homogeneous rule could not phrase.

`catalogue.txt` is what the card said its profiles are.

## `hetero/` -- the mixed card under load

One `4Q` beside two `2Q`, admitted to exactly 8192 of 8192 MiB, then
`vrampress --max` in all three for 120 s. `p2-host-1hz.csv` is the host's
per-process view at 1 Hz; each `p2-vrampress-vm*.csv` ends in that guest's
own verdict, and the three ceilings in those lines (3072 / 1280 / 1280) are
the finding: **each guest is refused at its own limit, not at a shared
one.**

## `libcuda/` -- what the mode answer costs

The same CUDA binary in the same 2Q guest, traced by `crates/nvrm-trace`
twice: `LEA_VGPU_MEDIATE=mode` and `none`. The traces are identical for 145
records and diverge at 146, where libcuda -- having been told the GPU is a
vGPU -- tries to ALLOCATE class `0xa080` (`KEPLER_DEVICE_VGPU`,
`class/cla080.h:31`) and RM answers `NV_ERR_NOT_SUPPORTED` because
`vgpuapiConstruct_IMPL` refuses it on `!IS_VIRTUAL(pGpu)`
(`kernel/vgpu/vgpuapi.c:43`). The call site is `queryVirtMode`,
`rmapi/nv_gpu_ops.c:7117`, which is the nvUvmInterface layer -- the CUDA
path -- and is why `nvidia-smi` was unaffected.

`analysis.txt` is `tracediff.py` over the two, computed on the COMPLETE
traces. The shipped `mode-off.jsonl` is TRUNCATED to its first 400 records:
it is 4474 records to `mode-on`'s 164, and that ratio is itself the result
-- the mode answer stops libcuda before it enumerates anything -- but the
divergence is at record 146 and everything the finding rests on is inside
the window. Truncated rather than compressed because the licence gate reads
the first lines of every tracked file, and a .gz has no first lines.

The tracer is PUSHED into the guest by `cudatrace.sh` rather than staged:
the compute guest carries the probes and the driver libraries, and the
matrix's own staging is a path these runs must not take.
