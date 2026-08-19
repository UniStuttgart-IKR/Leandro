// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * nvprobe - staged CUDA driver-API probe, meant to be run under the
 * nvrm-trace tracer.
 *
 * The question it answers is not "does CUDA work" but "which ioctl surface
 * does each step cost". Every stage is its own process run and therefore its
 * own trace file; the DIFFERENCE between two files is the result, not the
 * absolute numbers in either.
 *
 *   0  cuInit                          does it open /dev/nvidia-uvm?
 *   1  + device, context               building the object tree
 *   2  + cuMemAlloc, memcpy            the memory path
 *   3  + load PTX, launch kernel       the submission path (a plain vectorAdd)
 *   4  + long kernel in a loop         does "no ioctls per launch" still hold
 *                                      once the spin threshold is exceeded?
 *
 * Environment:
 *   NVPROBE_PTX      path to the PTX file        (default kernels.ptx)
 *   NVPROBE_SCHED    auto|spin|yield|blocking    (default auto)
 *   NVPROBE_CYCLES   cycles per spin kernel      (default 30000000, ~20 ms
 *                                                 on the Turing card here)
 *   NVPROBE_ITERS    launches in stage 4         (default 100)
 *   NVPROBE_NOCLEANUP  set -> skip the explicit teardown
 *
 * Build: make    (see Makefile; needs cuda.h and libcuda, not cudart)
 */

#include <cuda.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define N 4096

static void ck(CUresult r, const char *what)
{
    if (r == CUDA_SUCCESS) return;
    const char *s = NULL;
    cuGetErrorString(r, &s);
    fprintf(stderr, "ERROR %s: %d %s\n", what, r, s ? s : "?");
    exit(1);
}

static char *slurp(const char *path, long *len_out)
{
    FILE *f = fopen(path, "rb");
    if (!f) { fprintf(stderr, "ERROR: cannot read %s\n", path); exit(1); }
    fseek(f, 0, SEEK_END);
    long len = ftell(f);
    fseek(f, 0, SEEK_SET);
    char *buf = malloc((size_t)len + 1);
    if (!buf || fread(buf, 1, (size_t)len, f) != (size_t)len) {
        fprintf(stderr, "ERROR: short read on %s\n", path);
        exit(1);
    }
    buf[len] = 0;
    fclose(f);
    if (len_out) *len_out = len;
    return buf;
}

static unsigned int sched_flags(const char **name_out)
{
    const char *s = getenv("NVPROBE_SCHED");
    if (!s || !*s) s = "auto";
    *name_out = s;
    if (!strcmp(s, "blocking")) return CU_CTX_SCHED_BLOCKING_SYNC;
    if (!strcmp(s, "spin"))     return CU_CTX_SCHED_SPIN;
    if (!strcmp(s, "yield"))    return CU_CTX_SCHED_YIELD;
    return 0;  /* auto */
}

int main(int argc, char **argv)
{
    int level = argc > 1 ? atoi(argv[1]) : 3;

    /* ---- Stage 0 --------------------------------------------------------
     * cuInit on its own. Whatever ioctls accumulate here are the unavoidable
     * base cost -- the guest pays it on every process start.
     */
    ck(cuInit(0), "cuInit");
    fprintf(stderr, "stage 0 ok (cuInit)\n");
    if (level < 1) return 0;

    /* ---- Stage 1: device and context ----------------------------------- */
    const char *schedname;
    unsigned int cflags = sched_flags(&schedname);

    CUdevice dev;
    CUcontext ctx;
    ck(cuDeviceGet(&dev, 0), "cuDeviceGet");

    /* From CUDA 12.5 on, cuda.h maps cuCtxCreate to cuCtxCreate_v4 taking
     * CUctxCreateParams. Zero-initialised means no green context and no exec
     * affinity -- i.e. the same behaviour as the older signature. */
#if CUDA_VERSION >= 12050
    CUctxCreateParams cparams;
    memset(&cparams, 0, sizeof cparams);
    ck(cuCtxCreate(&ctx, &cparams, cflags, dev), "cuCtxCreate");
#else
    ck(cuCtxCreate(&ctx, cflags, dev), "cuCtxCreate");
#endif
    fprintf(stderr, "stage 1 ok (context, sched=%s)\n", schedname);
    if (level < 2) return 0;

    /* ---- Stage 2: memory ----------------------------------------------- */
    float *ha = malloc(N * sizeof(float));
    float *hb = malloc(N * sizeof(float));
    float *hc = malloc(N * sizeof(float));
    if (!ha || !hb || !hc) { fprintf(stderr, "ERROR: malloc\n"); return 1; }
    for (int i = 0; i < N; i++) { ha[i] = (float)i; hb[i] = 2.0f * (float)i; }

    CUdeviceptr da, db, dc;
    ck(cuMemAlloc(&da, N * sizeof(float)), "cuMemAlloc a");
    ck(cuMemAlloc(&db, N * sizeof(float)), "cuMemAlloc b");
    ck(cuMemAlloc(&dc, N * sizeof(float)), "cuMemAlloc c");
    ck(cuMemcpyHtoD(da, ha, N * sizeof(float)), "cuMemcpyHtoD a");
    ck(cuMemcpyHtoD(db, hb, N * sizeof(float)), "cuMemcpyHtoD b");
    fprintf(stderr, "stage 2 ok (memory)\n");
    if (level < 3) return 0;

    /* ---- Stage 3: load PTX, launch kernel -------------------------------
     * PTX on purpose, not a precompiled cubin: the JIT is part of the
     * proprietary userspace that has to run unchanged in the guest.
     * Precompiling here would measure everything except it.
     */
    const char *ptxpath = getenv("NVPROBE_PTX");
    if (!ptxpath || !*ptxpath) ptxpath = "kernels.ptx";
    char *ptx = slurp(ptxpath, NULL);

    CUmodule mod;
    CUfunction fadd;
    ck(cuModuleLoadData(&mod, ptx), "cuModuleLoadData");
    ck(cuModuleGetFunction(&fadd, mod, "vecAdd"), "cuModuleGetFunction vecAdd");

    int n = N;
    void *args[] = { &da, &db, &dc, &n };
    int threads = 256, blocks = (N + threads - 1) / threads;
    ck(cuLaunchKernel(fadd, blocks, 1, 1, threads, 1, 1, 0, 0, args, NULL),
       "cuLaunchKernel vecAdd");
    ck(cuCtxSynchronize(), "cuCtxSynchronize");
    ck(cuMemcpyDtoH(hc, dc, N * sizeof(float)), "cuMemcpyDtoH c");

    for (int i = 0; i < N; i++) {
        if (hc[i] != ha[i] + hb[i]) {
            fprintf(stderr, "WRONG at %d: %f != %f\n", i, hc[i], ha[i] + hb[i]);
            return 1;
        }
    }
    fprintf(stderr, "stage 3 ok (kernel, result correct)\n");

    /* ---- Stage 4: long kernel -------------------------------------------
     * The point of the whole probe. In stage 3 the kernel finishes before
     * libcuda stops spinning, so "no ioctls per launch" could be an artefact
     * of how short the kernel is. Here it runs long enough to break the spin
     * threshold, forcing cuCtxSynchronize to actually block.
     *
     * The cuCtxSynchronize *inside* the loop is essential: without it the
     * launches would merely queue up and nothing would be measured.
     */
    if (level < 4) goto cleanup;
    {
        CUfunction fspin;
        ck(cuModuleGetFunction(&fspin, mod, "spin"), "cuModuleGetFunction spin");

        const char *e;
        long long cycles = (e = getenv("NVPROBE_CYCLES")) ? atoll(e) : 30000000LL;
        int iters        = (e = getenv("NVPROBE_ITERS"))  ? atoi(e)  : 100;
        if (iters < 1) iters = 1;
        void *sargs[] = { &cycles, &dc };

        /* One warmup outside the measurement: the very first launch drags
         * module relocation and channel setup along with it and would skew
         * the average. */
        ck(cuLaunchKernel(fspin, 1, 1, 1, 32, 1, 1, 0, 0, sargs, NULL), "spin warmup");
        ck(cuCtxSynchronize(), "spin warmup sync");

        struct timespec t0, t1;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        for (int i = 0; i < iters; i++) {
            ck(cuLaunchKernel(fspin, 1, 1, 1, 32, 1, 1, 0, 0, sargs, NULL), "cuLaunchKernel spin");
            ck(cuCtxSynchronize(), "cuCtxSynchronize spin");
        }
        clock_gettime(CLOCK_MONOTONIC, &t1);

        double ms = ((double)(t1.tv_sec - t0.tv_sec) * 1e3
                   + (double)(t1.tv_nsec - t0.tv_nsec) / 1e6) / iters;
        fprintf(stderr, "stage 4 ok (%d launches, %.3f ms/launch, sched=%s)\n",
                iters, ms, schedname);
        /* machine-readable for `trace.sh analyse` */
        fprintf(stderr, "ITERS=%d\n", iters);
        fprintf(stderr, "MSPERLAUNCH=%.3f\n", ms);
    }

cleanup:
    /* Explicit teardown, because it should be visible in the trace: the
     * NV_ESC_RM_FREE order is exactly the information the object tree in
     * nvrm-client has to reproduce. On a plain exit() RM's client teardown
     * cleans up and nothing is visible at all.
     */
    if (!getenv("NVPROBE_NOCLEANUP")) {
        if (level >= 3) cuModuleUnload(mod);
        if (level >= 2) { cuMemFree(da); cuMemFree(db); cuMemFree(dc); }
        if (level >= 1) cuCtxDestroy(ctx);
    }
    free(ha); free(hb); free(hc);
    if (level >= 3) free(ptx);
    return 0;
}
