# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""NVRTC: compile a CUDA-C++ string at runtime and launch it raw.

Goes through libnvrtc and the PTX JIT rather than a pre-built module, then
launches with explicit grid/block dimensions. The read-back checks the
result instead of trusting that the launch returned.

Needs: cupy.
"""

import cupy as cp

cuda_code = r'''
extern "C" __global__
void custom_bitshift_kernel(unsigned int* data, int n) {
    int tid = blockIdx.x * blockDim.x + threadIdx.x;
    if (tid < n) {
        // Bitwise work, cheap to verify and hard to get accidentally right.
        data[tid] = (data[tid] ^ 0xDEADBEEF) + (tid << 3);
    }
}
'''


def nvrtc_raw_kernel_test():
    print("Starting NVRTC raw kernel compilation test...")
    n = 1000000
    data = cp.ones(n, dtype=cp.uint32)

    print("Compiling CUDA-C++ string to PTX on the fly...")
    module = cp.RawModule(code=cuda_code, options=('--std=c++11',))
    kernel = module.get_function('custom_bitshift_kernel')

    block_size = 256
    grid_size = (n + block_size - 1) // block_size

    print(f"Launching raw kernel (grid: {grid_size}, block: {block_size})...")
    kernel((grid_size,), (block_size,), (data, n))
    cp.cuda.Stream.null.synchronize()

    res = int(data[0].get())
    want = 1 ^ 0xDEADBEEF
    print(f"element 0: {hex(res)} (expected: {hex(want)})")
    if res != want:
        raise SystemExit(f"FAIL: {hex(res)} != {hex(want)}")
    print("NVRTC & raw kernel launch ok")


if __name__ == "__main__":
    nvrtc_raw_kernel_test()
