// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * managedprobe - managed memory (UVM acting as the memory manager), staged.
 *
 * The workloads measured so far use UVM only as a page-table mapper; none of
 * them calls cuMemAllocManaged. Managed memory across the VM boundary is
 * therefore supported only partially, and where it is not, it must fail
 * LOUDLY rather than compute a wrong answer. This probe checks both sides of
 * that: run natively it exercises the full surface (reference and trace); run
 * in the guest it establishes that the unsupported part fails visibly.
 *
 * Stages (NVMG_STAGE, default 3, cumulative):
 *   1  cuMemAllocManaged + CPU write + kernel (vecAdd) + CPU verify
 *      (migration in both directions via faults)
 *   2  + prefetch GPU/CPU + MemAdvise, verify again
 *   3  + oversubscription: more managed memory than VRAM, touch it all from
 *      the GPU, then spot-check from the CPU (NVMG_OVERSUB_MIB, default 9216)
 *
 * Environment: NVMG_STAGE, NVMG_OVERSUB_MIB, NVPROBE_PTX (kernels.ptx)
 * Build: make bin/managedprobe
 */

#include <cuda.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define MN (1 << 19)   /* floats per buffer in stages 1 and 2: 2 MiB */

static void ck(CUresult r, const char *what)
{
    if (r == CUDA_SUCCESS) return;
    const char *s = NULL;
    cuGetErrorString(r, &s);
    fprintf(stderr, "ERROR %s: %d %s\n", what, r, s ? s : "?");
    exit(1);
}

static char *slurp(const char *path)
{
    FILE *f = fopen(path, "rb");
    if (!f) { fprintf(stderr, "ERROR: cannot read %s\n", path); exit(1); }
    fseek(f, 0, SEEK_END);
    long len = ftell(f);
    fseek(f, 0, SEEK_SET);
    char *buf = malloc((size_t)len + 1);
    if (!buf || fread(buf, 1, (size_t)len, f) != (size_t)len) exit(1);
    buf[len] = 0;
    fclose(f);
    return buf;
}

static double now(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

int main(void)
{
    int stage = 3;
    const char *e = getenv("NVMG_STAGE");
    if (e) stage = atoi(e);

    CUdevice dev;
    CUcontext ctx;
    ck(cuInit(0), "cuInit");
    ck(cuDeviceGet(&dev, 0), "cuDeviceGet");

    int managed = 0, concurrent = 0, pageable = 0;
    cuDeviceGetAttribute(&managed, CU_DEVICE_ATTRIBUTE_MANAGED_MEMORY, dev);
    cuDeviceGetAttribute(&concurrent, CU_DEVICE_ATTRIBUTE_CONCURRENT_MANAGED_ACCESS, dev);
    cuDeviceGetAttribute(&pageable, CU_DEVICE_ATTRIBUTE_PAGEABLE_MEMORY_ACCESS, dev);
    printf("managedprobe: attrs managed=%d concurrent=%d pageable=%d\n",
           managed, concurrent, pageable);

#if CUDA_VERSION >= 12050
    CUctxCreateParams cp;
    memset(&cp, 0, sizeof cp);
    ck(cuCtxCreate(&ctx, &cp, 0, dev), "cuCtxCreate");
#else
    ck(cuCtxCreate(&ctx, 0, dev), "cuCtxCreate");
#endif

    /* ---- Stage 1: managed round trip through a real kernel ------------- */
    CUdeviceptr ma, mb, mc;
    ck(cuMemAllocManaged(&ma, MN * sizeof(float), CU_MEM_ATTACH_GLOBAL), "AllocManaged a");
    ck(cuMemAllocManaged(&mb, MN * sizeof(float), CU_MEM_ATTACH_GLOBAL), "AllocManaged b");
    ck(cuMemAllocManaged(&mc, MN * sizeof(float), CU_MEM_ATTACH_GLOBAL), "AllocManaged c");
    printf("managedprobe: stage1 alloc ok a=%#llx\n", (unsigned long long)ma);

    float *fa = (float *)(uintptr_t)ma, *fb = (float *)(uintptr_t)mb,
          *fc = (float *)(uintptr_t)mc;
    for (int i = 0; i < MN; i++) { fa[i] = (float)i; fb[i] = 2.0f * i; fc[i] = -1.0f; }

    const char *ptxpath = getenv("NVPROBE_PTX");
    if (!ptxpath || !*ptxpath) ptxpath = "kernels.ptx";
    CUmodule mod;
    CUfunction fadd;
    ck(cuModuleLoadData(&mod, slurp(ptxpath)), "cuModuleLoadData");
    ck(cuModuleGetFunction(&fadd, mod, "vecAdd"), "GetFunction vecAdd");
    int n = MN;
    void *args[] = { &ma, &mb, &mc, &n };
    ck(cuLaunchKernel(fadd, (MN + 255) / 256, 1, 1, 256, 1, 1, 0, 0, args, NULL),
       "cuLaunchKernel");
    ck(cuCtxSynchronize(), "cuCtxSynchronize");

    long bad = 0;
    for (int i = 0; i < MN; i++)
        if (fc[i] != fa[i] + fb[i]) bad++;
    printf("managedprobe: stage1 %s (bad=%ld)\n", bad ? "WRONG" : "ok, result correct", bad);
    if (bad) exit(1);
    if (stage < 2) { printf("DONE\n"); return 0; }

    /* ---- Stage 2: prefetch + advise ------------------------------------ */
#if CUDA_VERSION >= 13000
    CUmemLocation locd = { .type = CU_MEM_LOCATION_TYPE_DEVICE, .id = (int)dev };
    CUmemLocation loch = { .type = CU_MEM_LOCATION_TYPE_HOST, .id = 0 };
    ck(cuMemPrefetchAsync(ma, MN * sizeof(float), locd, 0, 0), "Prefetch->GPU");
    ck(cuMemAdvise(mb, MN * sizeof(float), CU_MEM_ADVISE_SET_READ_MOSTLY, locd), "Advise");
    ck(cuCtxSynchronize(), "sync prefetch");
    ck(cuMemPrefetchAsync(ma, MN * sizeof(float), loch, 0, 0), "Prefetch->CPU");
    ck(cuCtxSynchronize(), "sync prefetch2");
#else
    ck(cuMemPrefetchAsync(ma, MN * sizeof(float), dev, 0), "Prefetch->GPU");
    ck(cuMemAdvise(mb, MN * sizeof(float), CU_MEM_ADVISE_SET_READ_MOSTLY, dev), "Advise");
    ck(cuCtxSynchronize(), "sync prefetch");
    ck(cuMemPrefetchAsync(ma, MN * sizeof(float), CU_DEVICE_CPU, 0), "Prefetch->CPU");
    ck(cuCtxSynchronize(), "sync prefetch2");
#endif
    for (int i = 0; i < MN; i++) fc[i] = -1.0f;
    ck(cuLaunchKernel(fadd, (MN + 255) / 256, 1, 1, 256, 1, 1, 0, 0, args, NULL),
       "cuLaunchKernel 2");
    ck(cuCtxSynchronize(), "sync 2");
    bad = 0;
    for (int i = 0; i < MN; i++)
        if (fc[i] != fa[i] + fb[i]) bad++;
    printf("managedprobe: stage2 %s (prefetch/advise, bad=%ld)\n",
           bad ? "WRONG" : "ok, result correct", bad);
    if (bad) exit(1);
    if (stage < 3) { printf("DONE\n"); return 0; }

    /* ---- Stage 3: oversubscription --------------------------------------
     * More managed memory than there is VRAM. Touch all of it from the GPU
     * (cuMemsetD32 in 256 MiB chunks forces residency), then spot-check from
     * the CPU: every page migrates back when the CPU reads it.
     */
    size_t total_mib = 9216;
    e = getenv("NVMG_OVERSUB_MIB");
    if (e) total_mib = strtoull(e, NULL, 0);
    size_t vram_free = 0, vram_total = 0;
    cuMemGetInfo(&vram_free, &vram_total);
    printf("managedprobe: stage3 oversub %zu MiB (VRAM %zu MiB)\n",
           total_mib, vram_total >> 20);

    CUdeviceptr big;
    size_t bytes = total_mib << 20;
    ck(cuMemAllocManaged(&big, bytes, CU_MEM_ATTACH_GLOBAL), "AllocManaged big");
    double t0 = now();
    const size_t chunk = 256u << 20;
    for (size_t off = 0; off < bytes; off += chunk) {
        size_t len = bytes - off < chunk ? bytes - off : chunk;
        ck(cuMemsetD32(big + off, 0x40490fdb /* pi als float */, len / 4), "MemsetD32");
        ck(cuCtxSynchronize(), "sync memset");
    }
    double t1 = now();

    float *fb2 = (float *)(uintptr_t)big;
    size_t step = (bytes / 4) / 4096;   /* ~4096 samples spread across it */
    bad = 0;
    for (size_t i = 0; i < bytes / 4; i += step)
        if (fb2[i] != 3.14159274f) bad++;
    double t2 = now();
    printf("managedprobe: stage3 %s (GPU touch %.1fs, CPU samples %.1fs, bad=%ld)\n",
           bad ? "WRONG" : "ok, result correct", t1 - t0, t2 - t1, bad);
    if (bad) exit(1);

    cuMemFree(big);
    cuMemFree(ma); cuMemFree(mb); cuMemFree(mc);
    cuCtxDestroy(ctx);
    printf("DONE\n");
    return 0;
}
