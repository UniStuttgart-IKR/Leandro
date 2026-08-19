// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * E2 - does libcuda tolerate UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE?
 *
 * LD_PRELOAD shim that hooks ONLY ioctl and does ONLY one thing: set flag
 * 0x2 (uvm_types.h:67) in every UVM_INITIALIZE (0x30000001,
 * uvm_linux_ioctl.h:32) before it reaches the driver. Nothing else is
 * touched - the point is to isolate the one question the driver source
 * cannot answer: does libcuda cope with NV_WARN_NOTHING_TO_DO from
 * UVM_MM_INITIALIZE and PAGEABLE_MEM_ACCESS == FALSE?
 *
 * The request value 0x30000001 cannot collide with RM escapes: those are
 * _IOWR('F', ...) encodings carrying 0x46 in bits 8..15, this one has 0x00
 * there. UVM_INITIALIZE_PARAMS starts with the NvU64 flags field
 * (uvm_linux_ioctl.h:35-38), so the first 8 bytes are the flags.
 *
 * Build: make bin/uvminit_shim.so    Run: LD_PRELOAD=$PWD/bin/uvminit_shim.so ./bin/nvprobe 3
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>

#define UVM_INITIALIZE_REQ 0x30000001UL
#define UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE 0x2ULL

static int (*real_ioctl)(int, unsigned long, ...);
static long n_hit;

int ioctl(int fd, unsigned long request, ...)
{
    va_list ap;
    void *arg;

    va_start(ap, request);
    arg = va_arg(ap, void *);
    va_end(ap);

    if (!real_ioctl)
        real_ioctl = dlsym(RTLD_NEXT, "ioctl");

    if (request == UVM_INITIALIZE_REQ && arg) {
        uint64_t *flags = arg;
        *flags |= UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE;
        n_hit++;
        fprintf(stderr, "uvminit-shim: UVM_INITIALIZE #%ld flags=0x%llx\n",
                n_hit, (unsigned long long)*flags);
    }

    return real_ioctl(fd, request, arg);
}
