// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
// fork without exec across the boundary.
//
// This is on record as "deliberately unsupported": the child inherits the
// socket FD and the seq counter, so two processes write into the same
// connection. What is measured here is WHAT then happens -- does it hang,
// does it deliver quiet garbage, or does it fail cleanly?
//
// Three stages:
//   0: only open() before the fork, the child opens its own  (the prepared case)
//   1: cuInit before the fork, the child makes a driver call
//   2: like 1, but parent and child call SIMULTANEOUSLY (barrier over a pipe)

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/wait.h>
#include <cuda.h>

/* WHY THIS IS DECLARED HERE AND NOT JUST CALLED.
 *
 * `cuCtxCreate` is a MACRO in cuda.h that aliases whichever versioned symbol
 * that header generation shipped, and the arity changed underneath it:
 *
 *   CUDA 13.3 (CUDA_VERSION 13030)  #define cuCtxCreate cuCtxCreate_v4
 *       cuCtxCreate_v4(CUcontext *, CUctxCreateParams *, unsigned, CUdevice)
 *   older headers                   #define cuCtxCreate cuCtxCreate_v2
 *       cuCtxCreate_v2(CUcontext *, unsigned, CUdevice)
 *
 * So a four-argument call builds on one machine and fails on another with
 * "too many arguments to function 'cuCtxCreate_v2'". Reported 2026-08-21 on a
 * host with the older header; this file had been written against 13.3.
 *
 * The versioned names are declared in cuda.h only under
 * __CUDA_API_VERSION_INTERNAL, so the fix is to declare the one we want.
 * `cuCtxCreate_v2` is exported by libcuda and by the stub we link against on
 * every version this project has seen, and its signature has been stable for
 * CUDA generations -- which a `#if CUDA_VERSION >= ...` would only approximate,
 * since it needs the exact release where v4 landed to be right.
 *
 * This probe wants a plain context and passed NULL for the params anyway, so
 * v2 is not a downgrade: it is the same call spelled portably.
 */
extern CUresult CUDAAPI cuCtxCreate_v2(CUcontext *pctx, unsigned int flags,
                                       CUdevice dev);

static const char *errstr(CUresult r) {
    const char *s = NULL;
    cuGetErrorString(r, &s);
    return s ? s : "?";
}

#define CHECK(who, expr) do {                                              \
    CUresult _r = (expr);                                                  \
    printf("  %-28s %-22s -> %d (%s)\n", who, #expr, _r, errstr(_r));      \
    fflush(stdout);                                                        \
} while (0)

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int level = argc > 1 ? atoi(argv[1]) : 0;
    printf("forkprobe stage %d\n", level);

    if (level == 0) {
        int fd = open("/dev/nvidiactl", O_RDWR);
        printf("  parent fd=%d\n", fd);
        pid_t p = fork();
        if (p == 0) {
            int fd2 = open("/dev/nvidiactl", O_RDWR);
            printf("  child  fd=%d (its own open after fork)\n", fd2);
            _exit(fd2 < 0 ? 1 : 0);
        }
        int st = 0; waitpid(p, &st, 0);
        printf("  child exit=%d\n", WEXITSTATUS(st));
        return 0;
    }

    CHECK("parent before fork", cuInit(0));
    CUdevice dev; int n = 0;
    CHECK("parent before fork", cuDeviceGetCount(&n));
    printf("  devices: %d\n", n);
    CHECK("parent before fork", cuDeviceGet(&dev, 0));

    int sync[2];
    if (pipe(sync) < 0) return 1;

    pid_t p = fork();
    if (p == 0) {
        /* Wait for the parent's go-ahead so both sides run at once. The
         * result is checked because -Wunused-result is an error waiting to
         * happen and a short read here would silently un-synchronise the
         * two processes, which is the whole point of level 2. */
        if (level == 2) { char c; if (read(sync[0], &c, 1) != 1) _exit(2); }
        int m = 0;
        CHECK("child after fork", cuDeviceGetCount(&m));
        printf("  child sees %d devices\n", m);
        CUcontext ctx;
        CHECK("child after fork", cuCtxCreate_v2(&ctx, 0, dev));
        _exit(0);
    }

    if (level == 2 && write(sync[1], "x", 1) != 1) return 1;
    int m = 0;
    CHECK("parent after fork", cuDeviceGetCount(&m));
    printf("  parent sees %d devices\n", m);

    int st = 0;
    // Do not wait forever: if it hangs, that is precisely the result.
    for (int i = 0; i < 100; i++) {
        pid_t r = waitpid(p, &st, WNOHANG);
        if (r == p) { printf("  child finished, exit=%d\n", WEXITSTATUS(st)); return 0; }
        usleep(100000);
    }
    printf("  CHILD HANGS (no exit within 10 s) -- SIGKILL\n");
    kill(p, 9); waitpid(p, &st, 0);
    return 2;
}
