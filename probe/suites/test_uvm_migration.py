# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Unified (managed) memory: allocate, write from the GPU, read back.

Allocates 256 MiB through cudaMallocManaged, has the GPU write into it and
reads one element back. The read-back is the point: it forces the migration
that the allocation only promises.

Needs: cupy.
"""

import cupy as cp


def uvm_page_fault_test():
    print("Starting UVM page fault & migration test...")
    size = 64 * 1024 * 1024        # 64M floats = 256 MiB
    bytes_size = size * 4

    print("Allocating unified managed memory via cudaMallocManaged...")
    ptr = cp.cuda.runtime.mallocManaged(bytes_size,
                                        cp.cuda.runtime.cudaMemAttachGlobal)

    mem = cp.cuda.UnownedMemory(ptr, bytes_size, owner=None)
    memptr = cp.cuda.MemoryPointer(mem, 0)
    gpu_arr = cp.ndarray((size,), dtype=cp.float32, memptr=memptr)

    print("GPU writes values...")
    gpu_arr.fill(42.0)
    gpu_arr += 10.0
    cp.cuda.Stream.null.synchronize()

    val = float(gpu_arr[0].get())
    print(f"result on GPU: {val} (expected: 52.0)")

    cp.cuda.runtime.free(ptr)
    # Checked rather than just printed: a fast wrong answer is no answer.
    if val != 52.0:
        raise SystemExit(f"FAIL: read back {val}, expected 52.0")
    print("UVM managed memory test ok")


if __name__ == "__main__":
    uvm_page_fault_test()
