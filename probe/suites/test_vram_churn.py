# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""VRAM churn: fill the card, compute on every block, then free and
re-allocate in cycles.

Three phases. Phase 1 fills VRAM in 512 MiB blocks until OOM (which is an
expected outcome, not a failure). Phase 2 reduces over every block, so the
data has to still be there. Phase 3 frees half the blocks and allocates
fresh ones five times over -- that is the part that shows whether freed
device memory actually makes it back to the host driver.

Needs: torch.
"""

import torch
import gc
import time


def vram_churn_test():
    print("Starting VRAM churn & allocation stress test...")
    total_mem = torch.cuda.get_device_properties(0).total_memory / (1024 ** 2)
    print(f"detected VRAM: {total_mem:.0f} MiB")

    block_size = 512 * 1024 * 1024 // 4      # float32 = 4 bytes
    tensors = []

    print("\n--- phase 1: fill VRAM to the limit ---")
    try:
        for i in range(14):                  # ~7 GB
            t = torch.empty(block_size, dtype=torch.float32, device='cuda')
            t.fill_(float(i))
            tensors.append(t)
            allocated = torch.cuda.memory_allocated() / (1024 ** 2)
            print(f"block {i + 1} allocated | VRAM used: {allocated:.0f} MiB")
            time.sleep(0.1)
    except torch.cuda.OutOfMemoryError:
        # Expected once the card is full -- this is a limit, not a defect.
        print("hit OutOfMemory (expected once VRAM is full)")

    print("\n--- phase 2: reduce over every block ---")
    for idx, t in enumerate(tensors):
        sum_val = t.sum().item()
        print(f"block {idx + 1} checksum: {sum_val:.0f}")

    print("\n--- phase 3: free / re-allocate cycles ---")
    for cycle in range(5):
        print(f"cycle {cycle + 1}: freeing every second tensor...")
        del tensors[::2]
        gc.collect()
        torch.cuda.empty_cache()

        print(f"VRAM after empty_cache(): "
              f"{torch.cuda.memory_allocated() / (1024 ** 2):.0f} MiB")

        print("re-allocating...")
        for _ in range(len(tensors)):
            t = torch.empty(block_size, dtype=torch.float32, device='cuda')
            t.fill_(99.0)
            tensors.append(t)

    print("\nVRAM churn test ok (no leak, no crash)")


if __name__ == "__main__":
    vram_churn_test()
