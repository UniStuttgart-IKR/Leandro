# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""CUDA IPC across processes: hand a device tensor to a spawned child.

Passing a CUDA tensor to a 'spawn'-started process makes torch export a
CUDA IPC handle; the child imports it and writes through it. Both processes
therefore hold their own client state against the same allocation.

Needs: torch.
"""

import torch
import torch.multiprocessing as mp


def worker(tensor):
    print("[worker] received CUDA IPC tensor, running op...")
    tensor.add_(100.0)


def multiprocessing_ipc_test():
    print("Starting CUDA multiprocessing / IPC test...")
    mp.set_start_method('spawn', force=True)

    parent_tensor = torch.ones(1024, 1024, device='cuda', dtype=torch.float32)
    torch.cuda.synchronize()

    p = mp.Process(target=worker, args=(parent_tensor,))
    p.start()
    p.join()

    torch.cuda.synchronize()
    val = parent_tensor[0, 0].item()
    print(f"[parent] value after worker: {val} (expected: 101.0)")

    # Checked rather than just printed: the original only printed a message
    # on mismatch and still exited 0, so a broken IPC path looked green.
    if val != 101.0:
        raise SystemExit(f"FAIL: IPC modification did not land ({val} != 101.0)")
    print("CUDA IPC test ok")


if __name__ == "__main__":
    multiprocessing_ipc_test()
