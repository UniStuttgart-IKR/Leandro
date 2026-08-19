#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""convoom - cuDNN with a growing batch until VRAM OOM.

Does the guest tip cleanly when VRAM runs out (OOM propagates, the process
recovers) or does it take the stack down? One forward+backward with the
convburn model per batch; after the first OOM a recovery probe with batch
4.

  NVCO_BATCHES  Default "8,16,32,64,96,128,192,256"
"""
import os

import torch

batches = [int(b) for b in os.environ.get(
    "NVCO_BATCHES", "8,16,32,64,96,128,192,256").split(",")]

torch.manual_seed(7)
dev = torch.device("cuda")
model = torch.nn.Sequential(
    torch.nn.Conv2d(64, 128, 3, padding=1),
    torch.nn.BatchNorm2d(128),
    torch.nn.GELU(),
    torch.nn.Conv2d(128, 128, 3, padding=1),
).to(dev)

def step(b: int) -> str:
    inp = torch.randn(b, 64, 128, 128, device=dev)
    y = model(inp)
    loss = (y * y).mean()
    loss.backward()
    torch.cuda.synchronize()
    model.zero_grad(set_to_none=True)
    v = torch.cuda.max_memory_allocated() // (1 << 20)
    return f"loss={loss.item():.6e} vram={v}MiB"

oom = False
for b in batches:
    try:
        print(f"convoom: batch={b:4d} {step(b)}", flush=True)
    except torch.cuda.OutOfMemoryError as e:
        print(f"convoom: batch={b:4d} OOM (clean): {str(e).splitlines()[0][:100]}", flush=True)
        oom = True
        torch.cuda.empty_cache()
    except RuntimeError as e:
        print(f"convoom: batch={b:4d} ERROR {type(e).__name__}: {str(e).splitlines()[0][:120]}",
              flush=True)
        oom = True
        torch.cuda.empty_cache()

try:
    print(f"convoom: recovery batch=4 {step(4)}", flush=True)
except RuntimeError as e:
    print(f"convoom: recovery batch=4 ERROR: {str(e).splitlines()[0][:120]}", flush=True)
print(f"convoom: fertig oom_gesehen={oom}")
