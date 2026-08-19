// SPDX-License-Identifier: MIT
/*
 * Kernels for nvprobe.
 *
 * extern "C" is mandatory: nvprobe looks the functions up by name through
 * cuModuleGetFunction, and C++ mangling would turn vecAdd into
 * _Z6vecAddPKfS0_Pfi.
 *
 * Compiled to PTX, not to cubin -- see the Makefile. The JIT is part of
 * the proprietary userspace that has to run unchanged in the guest;
 * precompiling would mean not measuring it.
 */

extern "C" __global__ void vecAdd(const float *a, const float *b, float *c, int n)
{
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) c[i] = a[i] + b[i];
}

/*
 * Runtime-controlled spin, for stage 4.
 *
 * clock64() is an SM-local cycle counter and is not optimised away, so no
 * volatile tricks are needed. One block, one warp: the card stays
 * essentially idle, because this is about duration, not load.
 *
 * The store at the end depends on the loop and stops the optimiser from
 * discarding the whole kernel as having no effect.
 *
 * On Turing the counter runs at about 1.6 GHz, so 30e6 cycles is roughly
 * 20 ms. Do not rely on that -- the clock moves with boost. nvprobe
 * measures and prints wall time; calibrate against that.
 */
extern "C" __global__ void spin(long long cycles, float *out)
{
    long long t0 = clock64();
    long long now = t0;
    while (now - t0 < cycles) {
        now = clock64();
    }
    if (threadIdx.x == 0 && blockIdx.x == 0) {
        *out = (float)(now - t0);
    }
}
