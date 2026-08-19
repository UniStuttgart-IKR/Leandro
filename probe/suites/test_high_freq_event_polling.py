# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""High-frequency CUDA events: record 5000 timed event pairs back to back.

Events are the cheap synchronisation primitive, and each one costs a
submission. Recording thousands in a tight loop puts pressure on the
submission path without doing real work.

Needs: torch.
"""

import torch
import time


def high_freq_event_test(num_events=5000):
    print(f"Starting high-frequency event test ({num_events} events)...")

    start_events = [torch.cuda.Event(enable_timing=True)
                    for _ in range(num_events)]
    end_events = [torch.cuda.Event(enable_timing=True)
                  for _ in range(num_events)]

    x = torch.randn(1024, 1024, device='cuda')

    print("Recording events in a tight loop...")
    t0 = time.perf_counter()
    for i in range(num_events):
        start_events[i].record()
        x = x + 1.0
        end_events[i].record()

    torch.cuda.synchronize()
    t1 = time.perf_counter()

    print(f"event submission: {(t1 - t0) * 1000:.2f} ms")

    # Read one back: recording is only half the path, querying is the other.
    elapsed = start_events[100].elapsed_time(end_events[100])
    print(f"single kernel time (event 100): {elapsed:.4f} ms")
    print("high-frequency event test ok")


if __name__ == "__main__":
    high_freq_event_test()
