#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""What a display-path frame copy costs, at frame sizes.

The question this answers: PRIME render offload, and any arrangement where
the rendering device is not the presenting device, has to move a finished
frame between them. An early display-path estimate put that at "~2 GB/s
for the raw frame against ~1 MB/s for an encoded bitstream" -- this
measures the
first number instead of asserting it.

Deliberately NOT a CUDA microbenchmark: the sizes are real framebuffers
(RGBA8 at common resolutions), and the figure reported is ms per frame and
the frame rate the link would sustain if it did nothing else.

WARNING: the PCIe link speed is a MOMENTARY value, not a rig property --
gen1/gen2/gen3 have all been seen within one hour on this machine.
It is therefore read before AND after the run, and the
result is only comparable against another run at the same speed.

  probe/python/framecopy.py [--iters N]
"""

import argparse
import subprocess
import time

import torch

# RGBA8 framebuffers. 4 bytes per pixel is what a compositor actually
# scans out; a 10-bit or HDR surface is larger and scales linearly.
RESOLUTIONS = [
    ("1920x1080", 1920, 1080),
    ("2560x1440", 2560, 1440),
    ("3840x2160", 3840, 2160),
]
BYTES_PER_PIXEL = 4


def link_state():
    """Current PCIe link, as the RIG line (showcase.sh state) reports it."""
    try:
        out = subprocess.run(
            ["nvidia-smi", "--query-gpu=pcie.link.gen.current,pcie.link.width.current",
             "--format=csv,noheader,nounits"],
            capture_output=True, text=True, timeout=10,
        ).stdout.strip()
        gen, width = (x.strip() for x in out.split(","))
        return f"gen{gen}x{width}"
    except Exception:
        return "unknown"


def bench(nbytes, iters, pinned):
    """Device -> host copy of one frame, repeated. Returns seconds per copy."""
    src = torch.empty(nbytes, dtype=torch.uint8, device="cuda")
    dst = torch.empty(nbytes, dtype=torch.uint8, device="cpu", pin_memory=pinned)

    # Warm up: the first copy carries allocation and context costs that no
    # steady-state frame would pay.
    for _ in range(5):
        dst.copy_(src, non_blocking=False)
    torch.cuda.synchronize()

    t0 = time.perf_counter()
    for _ in range(iters):
        dst.copy_(src, non_blocking=False)
    torch.cuda.synchronize()
    return (time.perf_counter() - t0) / iters


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iters", type=int, default=200)
    args = ap.parse_args()

    if not torch.cuda.is_available():
        raise SystemExit("framecopy: no CUDA device")

    before = link_state()
    print(f"framecopy: {torch.cuda.get_device_name(0)}, pcie={before} (before)")
    print(f"{'resolution':<12} {'MiB':>7} {'pinned':>18} {'pageable':>18}")
    print(f"{'':<12} {'':>7} {'ms   GB/s   fps':>18} {'ms   GB/s   fps':>18}")

    for name, w, h in RESOLUTIONS:
        nbytes = w * h * BYTES_PER_PIXEL
        cells = []
        for pinned in (True, False):
            sec = bench(nbytes, args.iters, pinned)
            gbps = nbytes / sec / 1e9
            cells.append(f"{sec * 1e3:5.2f} {gbps:6.2f} {1 / sec:6.0f}")
        print(f"{name:<12} {nbytes / (1 << 20):7.1f} {cells[0]:>18} {cells[1]:>18}")

    after = link_state()
    print(f"framecopy: pcie={after} (after)")
    if before != after:
        print("framecopy: WARNING -- the link changed speed during the run; "
              "the numbers above are not from one regime")


if __name__ == "__main__":
    main()
