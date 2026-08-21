// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * vrampress - hold VRAM and keep churning it, for as long as asked.
 *
 * THE QUESTION IT EXISTS FOR (docs/OPEN-QUESTIONS.md 67 and 68). A guest
 * that freezes under memory pressure froze for one of two reasons that look
 * identical from inside it: its own per-tenant cap, or the card itself. To
 * tell a reservation policy from an accounting one, a run needs a load that
 * (a) presses on the limit for minutes rather than allocating once, (b)
 * CHURNS while it presses -- the host-side freeze detector counts DISTINCT
 * per-backend VRAM values per 30 s, so a load that holds a constant amount
 * is indistinguishable from a frozen guest -- and (c) can be pointed either
 * at the number the card reports or straight past it.
 *
 * oomprobe next door answers a different question: it allocates until the
 * first refusal, frees, and exits. That is the error PATH. This is the
 * error path under sustained load, which is where number 67 happens.
 *
 *   --fill PCT   grow until PCT of the free memory reported at startup is
 *                held, then churn. This is the WELL-BEHAVED tenant: it
 *                believes what the card tells it and sizes itself to it,
 *                which is what a streaming engine does (measured: CS2 takes
 *                4.8 GB uncapped and 3.1 GB under a 4 GiB cap without a
 *                single refusal).
 *   --max        grow until something refuses. This is the tenant that does
 *                NOT believe the card. It is the harsher test and it is not
 *                the reproduction of number 67 -- say which one a run used.
 *   --seconds N  how long to churn afterwards (default 300).
 *   --chunk MiB  the growth block (default 128).
 *   --churn MiB  the block allocated and freed once per iteration during
 *                the churn phase (default 64).
 *
 * One line per second on stdout, and it is a CSV: t,held_mib,free_mib,
 * total_mib,allocs,frees,fails. The summary line at the end starts with
 * "vrampress:" and states whether anything was refused, because "no refusal
 * at all" is a result and has to be visible without reading the table.
 *
 * Driver API only (libcuda, no runtime, no toolkit in the guest), exactly
 * like oomprobe. Build: make bin/vrampress
 */

#include <cuda.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define MAXN 512

static double now(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* Free/total as the CARD reports them to THIS guest -- which under a cap or
 * a profile is the mediated answer, not the physical card. That is the
 * point: --fill sizes itself to what the guest is told, so a run says
 * whether being told the truth is enough. */
static void meminfo(size_t *freeb, size_t *totalb)
{
    if (cuMemGetInfo(freeb, totalb) != CUDA_SUCCESS) {
        *freeb = 0;
        *totalb = 0;
    }
}

int main(int argc, char **argv)
{
    long fill = -1, seconds = 300, chunk_mib = 128, churn_mib = 64;
    int grow_max = 0;

    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--fill") && i + 1 < argc)        fill = atol(argv[++i]);
        else if (!strcmp(argv[i], "--max"))                    grow_max = 1;
        else if (!strcmp(argv[i], "--seconds") && i + 1 < argc) seconds = atol(argv[++i]);
        else if (!strcmp(argv[i], "--chunk") && i + 1 < argc)   chunk_mib = atol(argv[++i]);
        else if (!strcmp(argv[i], "--churn") && i + 1 < argc)   churn_mib = atol(argv[++i]);
        else {
            fprintf(stderr, "vrampress: unknown argument %s\n", argv[i]);
            return 2;
        }
    }
    if (fill < 0 && !grow_max) fill = 85;
    if (fill >= 0 && grow_max) {
        fprintf(stderr, "vrampress: --fill and --max are two different loads; pick one\n");
        return 2;
    }

    CUresult r;
    CUdevice dev;
    CUcontext ctx;
    if ((r = cuInit(0)) != CUDA_SUCCESS) {
        printf("vrampress: cuInit %d\n", (int)r);
        return 1;
    }
    if ((r = cuDeviceGet(&dev, 0)) != CUDA_SUCCESS) {
        printf("vrampress: cuDeviceGet %d\n", (int)r);
        return 1;
    }
#if CUDA_VERSION >= 12050
    CUctxCreateParams cp;
    memset(&cp, 0, sizeof cp);
    r = cuCtxCreate(&ctx, &cp, 0, dev);
#else
    r = cuCtxCreate(&ctx, 0, dev);
#endif
    if (r != CUDA_SUCCESS) {
        /* The context ITSELF is device memory (~106 MiB measured), so this
         * is the first thing a full card refuses. Say so plainly: it is a
         * result, not a broken run. */
        printf("vrampress: cuCtxCreate %d -- no context, the card refused before any allocation\n", (int)r);
        return 1;
    }

    size_t freeb, totalb;
    meminfo(&freeb, &totalb);
    printf("vrampress: start free %zu MiB of %zu MiB, mode %s, chunk %ld MiB, churn %ld MiB, %ld s\n",
           freeb >> 20, totalb >> 20, grow_max ? "max" : "fill", chunk_mib, churn_mib, seconds);
    fflush(stdout);

    CUdeviceptr held[MAXN];
    int n = 0;
    unsigned long allocs = 0, frees = 0, fails = 0;
    size_t chunk = (size_t)chunk_mib << 20;
    size_t churn = (size_t)churn_mib << 20;
    size_t target = grow_max ? (size_t)-1 : (size_t)((double)freeb * (double)fill / 100.0);
    size_t held_bytes = 0;
    double t0 = now();

    /* Grow. */
    while (n < MAXN && held_bytes < target) {
        CUdeviceptr p;
        r = cuMemAlloc(&p, chunk);
        if (r != CUDA_SUCCESS) {
            fails++;
            printf("vrampress: refused at %zu MiB held after %lu allocations (CUresult %d)\n",
                   held_bytes >> 20, allocs, (int)r);
            break;
        }
        held[n++] = p;
        held_bytes += chunk;
        allocs++;
    }
    meminfo(&freeb, &totalb);
    printf("vrampress: grown to %zu MiB held, %zu MiB free of %zu MiB\n",
           held_bytes >> 20, freeb >> 20, totalb >> 20);
    printf("t,held_mib,free_mib,total_mib,allocs,frees,fails\n");
    fflush(stdout);

    /* Churn: blocks in and out, at VARYING sizes, forever. The held blocks
     * stay held, so the pressure does not drain away while the churn runs.
     *
     * THE SIZES VARY ON PURPOSE, and the first version of this loop got it
     * wrong. A ring of equal blocks allocated and freed at the same rate
     * leaves the process footprint CONSTANT: measured on the host, 50
     * allocations and 43 frees per second moved `free` between exactly two
     * values. The detector this run is judged by counts DISTINCT
     * per-backend VRAM values per 30 s (number 67: frozen 3-6, healthy
     * 29-43), so a constant footprint would read as a freeze on a guest
     * that is working perfectly. A renderer churns unevenly; so does this.
     *
     * `rand` with a fixed seed rather than anything from the clock: two
     * guests in the same run should be doing the same thing, and a rerun
     * should be comparable to this one. */
    srand(1);
    int churn_n = 0;
    CUdeviceptr churn_p[8];
    size_t churn_b[8];
    size_t churn_held = 0;
    double next = now();
    while (now() - t0 < (double)seconds) {
        CUdeviceptr p;
        /* [churn/4, churn*2), rounded to 1 MiB and never zero. */
        size_t want = churn / 4 + ((size_t)rand() % (churn * 7 / 4));
        want = (want >> 20) << 20;
        if (want == 0) want = 1 << 20;
        r = cuMemAlloc(&p, want);
        if (r == CUDA_SUCCESS) {
            allocs++;
            if (churn_n == 8) {
                cuMemFree(churn_p[0]);
                frees++;
                churn_held -= churn_b[0];
                memmove(&churn_p[0], &churn_p[1], 7 * sizeof churn_p[0]);
                memmove(&churn_b[0], &churn_b[1], 7 * sizeof churn_b[0]);
                churn_n = 7;
            }
            churn_b[churn_n] = want;
            churn_p[churn_n++] = p;
            churn_held += want;
            /* And sometimes let one go early, so the footprint wanders
             * instead of settling on a sawtooth of one period. */
            if (churn_n > 2 && (rand() & 3) == 0) {
                cuMemFree(churn_p[0]);
                frees++;
                churn_held -= churn_b[0];
                memmove(&churn_p[0], &churn_p[1], (size_t)(churn_n - 1) * sizeof churn_p[0]);
                memmove(&churn_b[0], &churn_b[1], (size_t)(churn_n - 1) * sizeof churn_b[0]);
                churn_n--;
            }
        } else {
            fails++;
            /* Back off by one churn block rather than spinning on a
             * refusal: a load that only asks and never gives back is a
             * different experiment (and it is the one --max already ran). */
            if (churn_n > 0) {
                churn_n--;
                cuMemFree(churn_p[churn_n]);
                churn_held -= churn_b[churn_n];
                frees++;
            } else if (n > 0) {
                cuMemFree(held[--n]);
                held_bytes -= chunk;
                frees++;
            }
        }
        if (now() >= next) {
            meminfo(&freeb, &totalb);
            printf("%.0f,%zu,%zu,%zu,%lu,%lu,%lu\n", now() - t0,
                   (held_bytes + churn_held) >> 20,
                   freeb >> 20, totalb >> 20, allocs, frees, fails);
            fflush(stdout);
            next = now() + 1.0;
        }
        usleep(20000);
    }

    for (int i = 0; i < churn_n; i++) { cuMemFree(churn_p[i]); frees++; }
    churn_held = 0;
    for (int i = 0; i < n; i++)       { cuMemFree(held[i]);    frees++; }
    meminfo(&freeb, &totalb);

    /* Recovery probe: does an ordinary allocation still work after all
     * that? A cap that only ever counts up would fail here, and so would a
     * card that never got its memory back. */
    CUdeviceptr p;
    r = cuMemAlloc(&p, churn);
    if (r == CUDA_SUCCESS) cuMemFree(p);

    printf("vrampress: done -- %lu allocations, %lu frees, %lu refusals, "
           "free now %zu MiB of %zu MiB, recovery %s\n",
           allocs, frees, fails, freeb >> 20, totalb >> 20,
           r == CUDA_SUCCESS ? "ok" : "FAILED");
    cuCtxDestroy(ctx);
    return 0;
}
