#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""convburn - cuDNN under real load, with and without a per-iteration sync.

The question: does the wait path carry when things are TRULY async?
rlprobe/torchprobe sync per step (.item()); only without that sync does a
long chain of kernels stand in the channel before the guest first waits
on the semaphore pool.

  NVCB_SYNC=item   loss.item() per iteration  (the mode evidenced so far)
  NVCB_SYNC=end    accumulate on the GPU, ONE synchronize at the end
  NVCB_ITERS       Default 400
  NVCB_BATCH       Default 8

Determinism: fixed seed, deterministic cuDNN algorithms. The loss sum is
printed at full precision -- guest and native must be bit-identical.
"""
import os
import sys
import time

import torch

iters = int(os.environ.get("NVCB_ITERS", "400"))
batch = int(os.environ.get("NVCB_BATCH", "8"))
mode = os.environ.get("NVCB_SYNC", "end")

torch.manual_seed(7)
torch.backends.cudnn.deterministic = True
torch.backends.cudnn.benchmark = False

dev = torch.device("cuda")
model = torch.nn.Sequential(
    torch.nn.Conv2d(64, 128, 3, padding=1),
    torch.nn.BatchNorm2d(128),
    torch.nn.GELU(),
    torch.nn.Conv2d(128, 128, 3, padding=1),
).to(dev)
lin = torch.nn.Linear(128, 64).to(dev)
opt = torch.optim.SGD(list(model.parameters()) + list(lin.parameters()), lr=1e-3)
inp = torch.randn(batch, 64, 128, 128, device=dev)

acc = torch.zeros((), device=dev)
t0 = time.monotonic()
for i in range(iters):
    opt.zero_grad(set_to_none=True)
    y = model(inp)
    z = lin(y.mean(dim=(2, 3)))
    loss = (z * z).mean()
    loss.backward()
    opt.step()
    if mode == "item":
        acc += loss.detach()
        _ = loss.item()          # forces the sync per iteration
    else:
        acc += loss.detach()     # stays on the GPU, no sync
torch.cuda.synchronize()
dt = time.monotonic() - t0

print(f"convburn: sync={mode} iters={iters} batch={batch} "
      f"time={dt:.3f}s {dt / iters * 1e3:.2f}ms/it "
      f"acc={acc.item():.9e} "
      f"vram={torch.cuda.max_memory_allocated() // (1 << 20)}MiB")
