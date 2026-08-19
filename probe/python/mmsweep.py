#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""mmsweep - cuBLAS sweep: matmul NxN over sizes, GFLOP/s and a checksum.

Where does the guest tip compared to native? Per size: warmup plus K timed
iterations with a sync at the end; the checksum (the sum of the result
matrix) makes guest and native comparable.

  NVMM_SIZES   comma list, default "1024,2048,4096,8192"
  NVMM_ITERS   Default 10
"""
import os
import time

import torch

sizes = [int(s) for s in os.environ.get("NVMM_SIZES", "1024,2048,4096,8192").split(",")]
iters = int(os.environ.get("NVMM_ITERS", "10"))

torch.manual_seed(11)
dev = torch.device("cuda")

for n in sizes:
    try:
        a = torch.randn(n, n, device=dev)
        b = torch.randn(n, n, device=dev)
        c = a @ b                      # warm-up + allocation
        torch.cuda.synchronize()
        t0 = time.monotonic()
        for _ in range(iters):
            c = a @ b
        torch.cuda.synchronize()
        dt = time.monotonic() - t0
        gflops = 2 * n**3 * iters / dt / 1e9
        print(f"mmsweep: N={n} {dt / iters * 1e3:8.2f}ms/mm {gflops:8.1f} GFLOP/s "
              f"chk={c.sum().item():.6e} "
              f"vram={torch.cuda.max_memory_allocated() // (1 << 20)}MiB", flush=True)
        del a, b, c
        torch.cuda.empty_cache()
    except RuntimeError as e:
        print(f"mmsweep: N={n} ERROR {type(e).__name__}: {e}", flush=True)
        break
