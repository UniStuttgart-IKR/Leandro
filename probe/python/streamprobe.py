#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""streamprobe - several CUDA streams, events, overlapped copies.

rlprobe/torchprobe drive the default stream. Here: two streams with
kernels, cross-wise event sync, pinned H2D/D2H non_blocking alongside the
compute, and a correctness check against the sequential result at the end.

  NVSP_ITERS  Default 200
"""
import os
import time

import torch

iters = int(os.environ.get("NVSP_ITERS", "200"))
torch.manual_seed(3)
dev = torch.device("cuda")

s1 = torch.cuda.Stream()
s2 = torch.cuda.Stream()
a = torch.randn(2048, 2048, device=dev)
b = torch.randn(2048, 2048, device=dev)
h = torch.randn(4 << 20, pin_memory=True)         # 16 MiB pinned
ref = (a @ b).sum()                                # sequentielle Referenz
torch.cuda.synchronize()

ev1 = torch.cuda.Event()
ev2 = torch.cuda.Event()
t0 = time.monotonic()
for i in range(iters):
    with torch.cuda.stream(s1):
        x = a @ b
        ev1.record(s1)
    with torch.cuda.stream(s2):
        d = h.to(dev, non_blocking=True)           # copy alongside the matmul
        s2.wait_event(ev1)                         # then wait on s1
        y = x.sum() + d.sum() * 0                  # reads s1's result
        ev2.record(s2)
    s1.wait_event(ev2)                             # Kreuz-Sync zurueck
torch.cuda.synchronize()
dt = time.monotonic() - t0

ok = bool(torch.isclose(y, ref, rtol=1e-4).item())
print(f"streamprobe: iters={iters} time={dt:.3f}s {dt / iters * 1e3:.2f}ms/it "
      f"ergebnis={'korrekt' if ok else 'FALSCH'} y={y.item():.6e} ref={ref.item():.6e}")
