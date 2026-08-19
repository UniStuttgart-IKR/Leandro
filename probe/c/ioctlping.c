// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/* ioctlping: round-trip latency of ONE cheap ioctl on /dev/nvidiactl, with
 * no CUDA around it. This is the figure for the transport itself: run it
 * natively (straight to the kernel) and in the guest (through virtio_nvrm)
 * and both measure exactly the same call.
 *
 * The call: NV_ESC_CHECK_VERSION_STR with cmd = QUERY, N times. All the host
 * does for it is a string compare and copy (rm_perform_version_check,
 * kernel-open/nvidia/nv.c:2668), so the measured time is almost entirely
 * path, not work.
 *
 * SOURCES of the constants:
 *   NV_IOCTL_MAGIC 'F', NV_IOCTL_BASE 200, NV_ESC_CHECK_VERSION_STR 210
 *     -> kernel-open/common/inc/nv-ioctl-numbers.h:33-40
 *   nv_ioctl_rm_api_version_t {u32 cmd; u32 reply; char versionString[64]}
 *     and NV_RM_API_VERSION_CMD_QUERY '2'
 *     -> kernel-open/common/inc/nv-ioctl.h:100-109
 *     (NV_RM_API_VERSION_STRING_LENGTH 64: nv-ioctl.h:97)
 *
 *   ./ioctlping [iters] [warmup] [pause_us]   (Default 10000, 200, 0)
 *
 * `pause_us` is the difference between a "hot loop" and what a real
 * program does. Measured: over the guest module the same ioctl costs
 * 15.2 us in a hot loop but 2.0 ms in the course of cuInit (strace -c -w).
 * If the pause produces the difference, it is not the call that is
 * expensive but the fact that the vCPU goes to sleep in between and waking
 * it costs milliseconds.
 *
 * Output: one line of statistics in us, median/p10/p90/p99 -- grep-friendly,
 * so scripts can collect them.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_CHECK_VERSION_STR 210
#define NV_RM_API_VERSION_CMD_QUERY '2'

typedef struct {
    uint32_t cmd;
    uint32_t reply;
    char versionString[64];
} nv_rm_api_version_t;

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

int main(int argc, char **argv)
{
    long iters = argc > 1 ? atol(argv[1]) : 10000;
    long warmup = argc > 2 ? atol(argv[2]) : 200;
    long pause_us = argc > 3 ? atol(argv[3]) : 0;
    if (iters <= 0) { fprintf(stderr, "iters > 0 noetig\n"); return 2; }

    int fd = open("/dev/nvidiactl", O_RDWR);
    if (fd < 0) { perror("open /dev/nvidiactl"); return 1; }

    nv_rm_api_version_t v;
    memset(&v, 0, sizeof v);
    v.cmd = NV_RM_API_VERSION_CMD_QUERY;
    if (ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_CHECK_VERSION_STR, nv_rm_api_version_t), &v) != 0) {
        perror("ioctl CHECK_VERSION_STR");
        return 1;
    }
    /* Show once that RM really answers -- a measurement against a path
     * that short-circuits the call locally would be worthless. */
    printf("version reply=%u str=%.64s\n", v.reply, v.versionString);

    for (long i = 0; i < warmup; i++) {
        v.cmd = NV_RM_API_VERSION_CMD_QUERY;
        ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_CHECK_VERSION_STR, nv_rm_api_version_t), &v);
    }

    uint64_t *ns = malloc(sizeof(uint64_t) * (size_t)iters);
    if (!ns) { fprintf(stderr, "malloc\n"); return 1; }
    long fails = 0;
    struct timespec pause = { .tv_sec = pause_us / 1000000,
                              .tv_nsec = (pause_us % 1000000) * 1000 };
    for (long i = 0; i < iters; i++) {
        if (pause_us > 0) nanosleep(&pause, NULL);
        v.cmd = NV_RM_API_VERSION_CMD_QUERY;
        uint64_t t0 = now_ns();
        int r = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_CHECK_VERSION_STR, nv_rm_api_version_t), &v);
        uint64_t t1 = now_ns();
        ns[i] = t1 - t0;
        if (r != 0) fails++;
    }
    close(fd);

    qsort(ns, (size_t)iters, sizeof(uint64_t), cmp_u64);
    uint64_t sum = 0;
    for (long i = 0; i < iters; i++) sum += ns[i];
#define PCT(p) (ns[(size_t)((p) * (iters - 1) / 100)] / 1000.0)
    printf("ioctlping n=%ld pause_us=%ld fails=%ld min_us=%.2f p10_us=%.2f p50_us=%.2f "
           "p90_us=%.2f p99_us=%.2f max_us=%.2f mean_us=%.2f\n",
           iters, pause_us, fails, ns[0] / 1000.0, PCT(10), PCT(50), PCT(90), PCT(99),
           ns[iters - 1] / 1000.0, (double)sum / iters / 1000.0);
    free(ns);
    return fails ? 1 : 0;
}
