#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""torchprobe - staged PyTorch probe under the nvrm-trace tracer.

The counterpart to nvprobe.c, one level higher: the full PyTorch stack
(caching allocator, cuBLAS, optionally cuDNN, streams, events) instead of
the raw driver API. The question is the same: does the ioctl surface
saturate, and does a training step in steady state cost ioctls at all?

Stages (each includes the previous ones):
  0  cuInit                       (torch.cuda.init)
  1  context + first tensor
  2  pinned host buffer + H2D/D2H (the OS_DESCRIPTOR path)
  3  forward                      (cuBLAS; cuDNN with NVTORCH_CONV=1)
  4  one training step            (backward + SGD)
  5  N training steps, sync per step (saturation, NVTORCH_ITERS=100)

No DataLoader, no multiprocessing -- the tracer is LD_PRELOAD and sees only
this process. Threads are fine (libcuda's workers run that way too).

Teardown is explicit (empty_cache), so that the NV_ESC_RM_FREE order is in
the trace -- as with nvprobe. NVTORCH_NOCLEANUP=1 switches it off.
"""
import os
import sys
import time

level = int(sys.argv[1]) if len(sys.argv) > 1 else 5

import torch  # noqa: E402


def log(msg: str) -> None:
    print(f"torchprobe: {msg}", file=sys.stderr, flush=True)


def cleanup() -> None:
    if os.environ.get("NVTORCH_NOCLEANUP"):
        return
    g = globals()
    for name in ("loss", "out", "opt", "model", "inp", "tgt", "d", "h", "h2", "x"):
        g.pop(name, None)
    torch.cuda.synchronize()
    torch.cuda.empty_cache()  # returns caching-allocator blocks to the driver
    log("teardown ok (empty_cache)")


def gate(lvl: int) -> None:
    if level < lvl:
        cleanup()
        sys.exit(0)


# ---- stage 0: cuInit ------------------------------------------------------
assert torch.cuda.is_available(), "no CUDA device visible"
torch.cuda.init()
log(f"stage 0 ok (cuInit, torch {torch.__version__}, cuda {torch.version.cuda})")
gate(1)

# ---- stage 1: context + first tensor --------------------------------------
x = torch.zeros(1, device="cuda")
torch.cuda.synchronize()
log("stage 1 ok (context + first tensor)")
gate(2)

# ---- stage 2: pinned memory ----------------------------------------------
# By design cudaHostAlloc/cudaHostRegister is the only case that goes over
# NV01_MEMORY_SYSTEM_OS_DESCRIPTOR. The path never appeared in the
# vectorAdd trace -- this is its first measurement.
h = torch.randn(1 << 20, pin_memory=True)          # 4 MiB pinned
d = h.to("cuda", non_blocking=True)
h2 = torch.empty_like(h, pin_memory=True)
h2.copy_(d, non_blocking=True)
torch.cuda.synchronize()
assert torch.equal(h, h2), "H2D/D2H round trip broken"
log("stage 2 ok (pinned H2D/D2H, roundtrip correct)")
gate(3)

# ---- stage 3: forward -----------------------------------------------------
torch.manual_seed(0)
if os.environ.get("NVTORCH_CONV") == "1":
    # Conv pulls in cuDNN including heuristics/workspace -- its own surface.
    model = torch.nn.Sequential(
        torch.nn.Conv2d(3, 16, 3, padding=1), torch.nn.ReLU(),
        torch.nn.Conv2d(16, 16, 3, padding=1), torch.nn.ReLU(),
        torch.nn.Flatten(), torch.nn.Linear(16 * 32 * 32, 10),
    ).cuda()
    inp = torch.randn(8, 3, 32, 32, device="cuda")
    variant = "conv/cuDNN"
else:
    model = torch.nn.Sequential(
        torch.nn.Linear(512, 1024), torch.nn.ReLU(),
        torch.nn.Linear(1024, 10),
    ).cuda()
    inp = torch.randn(64, 512, device="cuda")
    variant = "mlp/cuBLAS"
tgt = torch.randn(inp.shape[0], 10, device="cuda")
out = model(inp)
torch.cuda.synchronize()
log(f"stage 3 ok (forward, {variant})")
gate(4)

# ---- stage 4: one training step -------------------------------------------
opt = torch.optim.SGD(model.parameters(), lr=1e-3)
loss = torch.nn.functional.mse_loss(out, tgt)
loss.backward()
opt.step()
opt.zero_grad(set_to_none=True)
torch.cuda.synchronize()
log("stage 4 ok (backward + step)")
gate(5)

# ---- stage 5: N steps -----------------------------------------------------
# The sync *inside* the loop is deliberate, as in nvprobe stage 4: without
# it the steps only go into the queue and the differential measurement
# measures nothing.
iters = int(os.environ.get("NVTORCH_ITERS", "100"))
t0 = time.monotonic()
for _ in range(iters):
    out = model(inp)
    loss = torch.nn.functional.mse_loss(out, tgt)
    loss.backward()
    opt.step()
    opt.zero_grad(set_to_none=True)
    torch.cuda.synchronize()
t1 = time.monotonic()
ms = 1e3 * (t1 - t0) / iters
log(f"stage 5 ok ({iters} steps, {ms:.3f} ms/step, {variant})")
print(f"ITERS={iters}", file=sys.stderr)
print(f"MSPERSTEP={ms:.3f}", file=sys.stderr)

cleanup()
