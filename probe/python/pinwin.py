#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""pinwin - measure out the pinned window: where does the 0x71/MAP path tip?

The limits a large pin can hit are the two pin caps (the module's
max_pin_mib, cumulative, and the backend's LEA_MAX_PIN_MIB, per arena) --
the host-visible window itself is 8 GiB and no longer the first wall.
Per size: allocate pinned, write a pattern, H2D + D2H (non_blocking,
then sync), check the round trip, free. Errors should propagate cleanly and
not crash.

  NVPW_SIZES  comma list in MiB, default "16,64,128,192,224,240,256,272,320,512"
  NVPW_HOLD=1 do NOT free the buffers (probes the cumulative limit instead of the single-pin size)
"""
import os

import torch

sizes = [int(s) for s in os.environ.get(
    "NVPW_SIZES", "16,64,128,192,224,240,256,272,320,512").split(",")]
hold = bool(os.environ.get("NVPW_HOLD"))

dev = torch.device("cuda")
torch.zeros(1, device=dev)  # bring the context up
kept = []

for mib in sizes:
    n = mib * (1 << 20) // 4
    try:
        h = torch.empty(n, dtype=torch.float32, pin_memory=True)
        h.copy_(torch.arange(n, dtype=torch.float32) % 977)
        d = h.to(dev, non_blocking=True)
        torch.cuda.synchronize()
        back = d.cpu()           # blocking D2H copy, compare only after it
        ok = bool(torch.equal(h, back))
        print(f"pinwin: {mib:5d} MiB pinned ok roundtrip={'correct' if ok else 'WRONG'}"
              f"{' (gehalten)' if hold else ''}", flush=True)
        del d, back
        if hold:
            kept.append(h)
        else:
            del h
        torch.cuda.empty_cache()
    except (RuntimeError, MemoryError) as e:
        print(f"pinwin: {mib:5d} MiB ERROR {type(e).__name__}: {e}", flush=True)

# Recovery: does a small allocation still go through after a failure?
try:
    n = 16 * (1 << 20) // 4
    h = torch.empty(n, dtype=torch.float32, pin_memory=True)
    h.fill_(3.0)
    d = h.to(dev, non_blocking=True)
    torch.cuda.synchronize()
    ok = bool(torch.equal(h, d.cpu()))
    print(f"pinwin: recovery 16 MiB {'correct' if ok else 'WRONG'}")
except (RuntimeError, MemoryError) as e:
    print(f"pinwin: recovery 16 MiB ERROR {type(e).__name__}: {e}")

print(f"pinwin: fertig, gehalten={sum(t.numel() * 4 for t in kept) // (1 << 20)} MiB")
