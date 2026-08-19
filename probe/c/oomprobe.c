// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * oomprobe - exhaust VRAM deliberately, check the error path and recovery.
 *
 * The isolation test: one VM eats memory to OOM while another computes.
 * This is the eating side: cuMemAlloc in 256 MiB chunks until the first
 * failure, report the total and the error code, free everything, then a
 * small allocation as a recovery probe.
 *
 * Build: make bin/oomprobe
 */

#include <cuda.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CHUNK (256u << 20)
#define MAXN  64

int main(void)
{
    CUresult r;
    CUdevice dev;
    CUcontext ctx;
    CUdeviceptr chunks[MAXN];
    int n = 0;

    if ((r = cuInit(0))) { printf("oomprobe: cuInit %d\n", r); return 1; }
    cuDeviceGet(&dev, 0);
#if CUDA_VERSION >= 12050
    CUctxCreateParams cp;
    memset(&cp, 0, sizeof cp);
    r = cuCtxCreate(&ctx, &cp, 0, dev);
#else
    r = cuCtxCreate(&ctx, 0, dev);
#endif
    if (r) { printf("oomprobe: cuCtxCreate %d\n", r); return 1; }

    while (n < MAXN) {
        r = cuMemAlloc(&chunks[n], CHUNK);
        if (r != CUDA_SUCCESS) break;
        n++;
    }
    const char *s = NULL;
    cuGetErrorString(r, &s);
    printf("oomprobe: %d chunks = %u MiB, then error %d (%s)\n",
           n, n * 256u, r, s ? s : "?");
    fflush(stdout);

    /* NVOOM_HOLD=<s>: hold the occupancy, so that two VMs reliably
     * ueberlappen (Ueberbuchungstest). */
    const char *hold = getenv("NVOOM_HOLD");
    if (hold) sleep((unsigned)atoi(hold));

    for (int i = 0; i < n; i++) cuMemFree(chunks[i]);

    CUdeviceptr small;
    r = cuMemAlloc(&small, 1 << 20);
    printf("oomprobe: recovery 1 MiB: %d %s\n", r, r ? "ERROR" : "ok");
    if (!r) cuMemFree(small);
    cuCtxDestroy(ctx);
    return 0;
}
