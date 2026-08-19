// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * hostregprobe - a targeted probe for the pinned / 0x71 path.
 *
 * The question: which escapes does libcuda issue for host-registered
 * memory? Run it natively under the tracer and count the nvos02 lines
 * (hClass=0x71); hold the same in the guest against the host backend log.
 *
 * Steps (all in one run, one result per line):
 *   A  cuInit + device + context
 *   B  cuMemHostRegister(flags=0)            + GetDevicePointer + Unregister
 *   C  cuMemHostRegister(DEVICEMAP)          + GetDevicePointer
 *   D  memcpy round trip H2D/D2H over the registered buffer (correctness)
 *   E  cuMemHostAlloc(DEVICEMAP)             + GetDevicePointer + Roundtrip
 *
 * Environment: NVHR_SIZE  buffer size in bytes (default 2 MiB, page-aligned)
 *
 * Build: make bin/hostregprobe
 */

#include <cuda.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const char *es(CUresult r)
{
    const char *s = NULL;
    cuGetErrorString(r, &s);
    return s ? s : "?";
}

int main(void)
{
    size_t sz = 2u << 20;
    const char *env = getenv("NVHR_SIZE");
    if (env) sz = strtoull(env, NULL, 0);

    CUresult r;
    CUdevice dev;
    CUcontext ctx;

    r = cuInit(0);
    printf("A cuInit                : %d %s\n", r, r ? es(r) : "ok");
    if (r) return 1;
    r = cuDeviceGet(&dev, 0);
    if (r) { printf("A cuDeviceGet ERROR %d\n", r); return 1; }
    /* Since CUDA 12.5, cuda.h maps cuCtxCreate to cuCtxCreate_v4 (as in nvprobe). */
#if CUDA_VERSION >= 12050
    CUctxCreateParams cparams;
    memset(&cparams, 0, sizeof cparams);
    r = cuCtxCreate(&ctx, &cparams, 0, dev);
#else
    r = cuCtxCreate(&ctx, 0, dev);
#endif
    printf("A cuCtxCreate           : %d %s\n", r, r ? es(r) : "ok");
    if (r) return 1;

    void *buf;
    if (posix_memalign(&buf, 4096, sz)) { perror("posix_memalign"); return 1; }
    memset(buf, 0xa5, sz);

    /* B: Flag 0 */
    r = cuMemHostRegister(buf, sz, 0);
    printf("B HostRegister(0)       : %d %s\n", r, r ? es(r) : "ok");
    if (r == CUDA_SUCCESS) {
        CUdeviceptr dptr = 0;
        CUresult r2 = cuMemHostGetDevicePointer(&dptr, buf, 0);
        printf("B GetDevicePointer      : %d %s dptr=%#llx\n",
               r2, r2 ? es(r2) : "ok", (unsigned long long)dptr);
        r2 = cuMemHostUnregister(buf);
        printf("B Unregister            : %d %s\n", r2, r2 ? es(r2) : "ok");
    }

    /* C: DEVICEMAP */
    r = cuMemHostRegister(buf, sz, CU_MEMHOSTREGISTER_DEVICEMAP);
    printf("C HostRegister(DEVMAP)  : %d %s\n", r, r ? es(r) : "ok");
    CUdeviceptr dreg = 0;
    if (r == CUDA_SUCCESS) {
        CUresult r2 = cuMemHostGetDevicePointer(&dreg, buf, 0);
        printf("C GetDevicePointer      : %d %s dptr=%#llx\n",
               r2, r2 ? es(r2) : "ok", (unsigned long long)dreg);
    }

    /* D: round trip over the registered buffer */
    if (r == CUDA_SUCCESS) {
        CUdeviceptr d = 0;
        CUresult r2 = cuMemAlloc(&d, sz);
        if (r2 == CUDA_SUCCESS) {
            unsigned char *p = buf;
            for (size_t i = 0; i < sz; i++) p[i] = (unsigned char)(i * 131 + 7);
            r2 = cuMemcpyHtoD(d, buf, sz);
            printf("D Memcpy HtoD (reg)     : %d %s\n", r2, r2 ? es(r2) : "ok");
            memset(buf, 0, sz);
            r2 = cuMemcpyDtoH(buf, d, sz);
            size_t bad = 0;
            for (size_t i = 0; i < sz; i++)
                if (p[i] != (unsigned char)(i * 131 + 7)) bad++;
            printf("D Roundtrip             : %d %s bad=%zu %s\n",
                   r2, r2 ? es(r2) : "ok", bad, bad ? "FALSCH" : "korrekt");
            cuMemFree(d);
        }
        cuMemHostUnregister(buf);
    }

    /* E: cuMemHostAlloc(DEVICEMAP) */
    void *hbuf = NULL;
    r = cuMemHostAlloc(&hbuf, sz, CU_MEMHOSTALLOC_DEVICEMAP);
    printf("E HostAlloc(DEVMAP)     : %d %s p=%p\n", r, r ? es(r) : "ok", hbuf);
    if (r == CUDA_SUCCESS) {
        CUdeviceptr dptr = 0;
        CUresult r2 = cuMemHostGetDevicePointer(&dptr, hbuf, 0);
        printf("E GetDevicePointer      : %d %s dptr=%#llx\n",
               r2, r2 ? es(r2) : "ok", (unsigned long long)dptr);
        CUdeviceptr d = 0;
        if (cuMemAlloc(&d, sz) == CUDA_SUCCESS) {
            unsigned char *p = hbuf;
            for (size_t i = 0; i < sz; i++) p[i] = (unsigned char)(i * 17 + 3);
            cuMemcpyHtoD(d, hbuf, sz);
            memset(hbuf, 0, sz);
            r2 = cuMemcpyDtoH(hbuf, d, sz);
            size_t bad = 0;
            for (size_t i = 0; i < sz; i++)
                if (p[i] != (unsigned char)(i * 17 + 3)) bad++;
            printf("E Roundtrip             : %d %s bad=%zu %s\n",
                   r2, r2 ? es(r2) : "ok", bad, bad ? "FALSCH" : "korrekt");
            cuMemFree(d);
        }
        cuMemFreeHost(hbuf);
    }

    cuCtxDestroy(ctx);
    printf("FERTIG\n");
    return 0;
}
