// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/* ctrlping: does transport latency scale with PAYLOAD SIZE?
 *
 * probe/c/ioctlping.c measures the round trip at a fixed 72 bytes. This probe
 * measures the same ioctl with a GROWING payload, turning the question into
 * a curve: a slope means the per-request cost is dominated by moving the
 * payload (the module allocates kvzalloc buffers per call and builds a
 * scatter list), a flat line means the cost sits elsewhere.
 *
 * Uses NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION (0x101) -- the same command
 * nvidia-smi uses for its "KMD Version" line. It is the only call available
 * whose payload size is freely CHOOSABLE: its three buffers are
 * `SizeOfStrings` bytes each.
 *
 * SOURCES:
 *   NV_IOCTL_MAGIC 'F', NV_ESC_RM_ALLOC 0x2b, NV_ESC_RM_CONTROL 0x2a
 *     -> kernel-open/common/inc/nv-ioctl-numbers.h:33 ff., nvgpu.rs:505
 *   NVOS64 (alloc): hRoot @0, hObjectParent @4, hObjectNew @8, hClass @12,
 *     pAllocParms @16, pRightsRequested @24, paramsSize @32, flags @36,
 *     status @40 -> nvos.h:480-490
 *   NVOS54 (Control): hClient @0, hObject @4, cmd @8, flags @12,
 *     params @16, paramsSize @24, status @28 -> nvos.h, share.rs:88-91
 *   NV01_ROOT_CLIENT = 0x41 (not 0x0 -- that is the privileged path)
 *     -> nvgpu.rs:517-520
 *   NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS: SizeOfStrings u32 @0,
 *     pDriverVersionBuffer @8, pVersionBuffer @16, pTitleBuffer @24,
 *     ChangelistNumber @32, OfficialChangelistNumber @36 = 40 bytes
 *     -> ctrl0000system.h, the same annotation as in xlate::nested_ptrs
 *
 *   ./ctrlping [iters] [size...]         (default 2000; 64 512 4096 16384)
 *
 * Output: one grep-friendly line per size, median and p99 in us.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_RM_ALLOC 0x2b
#define NV_ESC_RM_CONTROL 0x2a
#define NV01_ROOT_CLIENT 0x41
#define CMD_SYSTEM_GET_BUILD_VERSION 0x101

struct nvos64 {
    uint32_t hRoot, hObjectParent, hObjectNew, hClass;
    uint64_t pAllocParms, pRightsRequested;
    uint32_t paramsSize, flags, status;
};

struct nvos54 {
    uint32_t hClient, hObject, cmd, flags;
    uint64_t params;
    uint32_t paramsSize, status;
};

struct build_version {
    uint32_t sizeOfStrings;
    uint32_t pad;
    uint64_t pDriverVersionBuffer;
    uint64_t pVersionBuffer;
    uint64_t pTitleBuffer;
    uint32_t changelist;
    uint32_t officialChangelist;
};

static int cmp_u64(const void *a, const void *b)
{
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return (x > y) - (x < y);
}

static uint64_t now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + ts.tv_nsec;
}

/* One measurement series for one payload size. `quiet` = warm up only,
 * do not print. 0 = ok. */
static int reihe(int fd, uint32_t hclient, uint32_t size, long iters, int quiet)
{
    char *b1 = calloc(size ? size : 1, 1);
    char *b2 = calloc(size ? size : 1, 1);
    char *b3 = calloc(size ? size : 1, 1);
    uint64_t *ns = malloc(sizeof(uint64_t) * (size_t)iters);
    long fails = 0;

    if (!b1 || !b2 || !b3 || !ns) {
        fprintf(stderr, "malloc\n");
        return 1;
    }

    for (long i = 0; i < iters; i++) {
        struct build_version p;
        struct nvos54 c;

        memset(&p, 0, sizeof p);
        p.sizeOfStrings = size;
        p.pDriverVersionBuffer = (uint64_t)(uintptr_t)b1;
        p.pVersionBuffer = (uint64_t)(uintptr_t)b2;
        p.pTitleBuffer = (uint64_t)(uintptr_t)b3;

        memset(&c, 0, sizeof c);
        c.hClient = hclient;
        c.hObject = hclient;
        c.cmd = CMD_SYSTEM_GET_BUILD_VERSION;
        c.params = (uint64_t)(uintptr_t)&p;
        c.paramsSize = sizeof p;

        uint64_t t0 = now_ns();
        int r = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_CONTROL, struct nvos54), &c);
        uint64_t t1 = now_ns();
        ns[i] = t1 - t0;
        if (r != 0 || c.status != 0) {
            if (fails == 0)
                fprintf(stderr, "ctrlping: size=%u ret=%d status=%#x\n", size, r, c.status);
            fails++;
        }
    }

    qsort(ns, (size_t)iters, sizeof(uint64_t), cmp_u64);
    uint64_t sum = 0;
    for (long i = 0; i < iters; i++)
        sum += ns[i];
    if (!quiet)
        printf("ctrlping bytes=%u n=%ld fails=%ld p50_us=%.2f p90_us=%.2f p99_us=%.2f mean_us=%.2f\n",
           /* three buffers plus the 40-byte struct cross the boundary */
           size * 3 + (unsigned)sizeof(struct build_version), iters, fails,
           ns[(size_t)(50 * (iters - 1) / 100)] / 1000.0,
           ns[(size_t)(90 * (iters - 1) / 100)] / 1000.0,
           ns[(size_t)(99 * (iters - 1) / 100)] / 1000.0,
           (double)sum / iters / 1000.0);

    free(b1); free(b2); free(b3); free(ns);
    return fails ? 1 : 0;
}

int main(int argc, char **argv)
{
    long iters = argc > 1 ? atol(argv[1]) : 2000;
    uint32_t defaults[] = { 64, 512, 4096, 16384 };
    int n_sizes = argc > 2 ? argc - 2 : 4;

    if (iters <= 0) {
        fprintf(stderr, "iters > 0 noetig\n");
        return 2;
    }

    int fd = open("/dev/nvidiactl", O_RDWR);
    if (fd < 0) {
        perror("open /dev/nvidiactl");
        return 1;
    }

    /* One RM client. hClass 0x41, not 0x0: 0x0 is the privileged path and
     * gives INSUFFICIENT_PERMISSIONS to an ordinary user. */
    struct nvos64 a;
    memset(&a, 0, sizeof a);
    a.hClass = NV01_ROOT_CLIENT;
    if (ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, struct nvos64), &a) != 0 || a.status != 0) {
        fprintf(stderr, "NV01_ROOT_CLIENT: status=%#x errno=%d\n", a.status, errno);
        return 1;
    }

    int bad = 0;
    for (int i = 0; i < n_sizes; i++) {
        uint32_t size = argc > 2 ? (uint32_t)strtoul(argv[2 + i], NULL, 0) : defaults[i];
        /* Warmup: the first call at a size carries the allocation cost of
         * the caller's buffer, not that of the transport. */
        reihe(fd, a.hObjectNew, size, iters < 50 ? iters : 50, 1);
        bad |= reihe(fd, a.hObjectNew, size, iters, 0);
    }
    close(fd);
    return bad;
}
