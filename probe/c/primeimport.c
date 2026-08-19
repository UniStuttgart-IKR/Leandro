// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * PRIME import, isolated: a display device allocates, NVIDIA imports.
 *
 *   primeimport [<exporter> <importer>]
 *   default: /dev/dri/card0 -> /dev/dri/card1 (nvidia-drm); use --find or
 *   pass the nodes, the numbers are not constant.
 *
 * This is the direction a PRIME desktop actually uses, and the reason it
 * can be tested on its own: the DISPLAY device allocates the scanout
 * buffer and the RENDER device imports it, exactly as an Optimus laptop
 * does. No X server, no glamor, no compositor. (Written while an interim
 * virtio-gpu display device sat beside nvidia-drm; that carrier is gone,
 * the direction under test is unchanged.)
 *
 * The import lands, RM-side, at NV01_MEMORY_SYSTEM_OS_DESCRIPTOR (class
 * 0x71) with descriptorType OS_DMA_BUF_PTR: nv_drm_gem_prime_import_sg_table
 * -> nvKms->getSystemMemoryHandleFromDmaBuf -> nvRmApiAlloc. That is the
 * same class virtio_nvrm already serves for guest memory, only the
 * descriptor is a kernel object instead of a user address.
 *
 * Build: make bin/primeimport  (plain DRM ioctls, no libdrm -- see probe/Makefile)
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <drm/drm.h>
#include <drm/drm_mode.h>

#define W 256
#define H 256

static int dumb_create(int fd, uint32_t *handle, uint64_t *size)
{
    struct drm_mode_create_dumb req;

    memset(&req, 0, sizeof(req));
    req.width = W;
    req.height = H;
    req.bpp = 32;
    if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &req)) {
        fprintf(stderr, "CREATE_DUMB: %s\n", strerror(errno));
        return -1;
    }
    *handle = req.handle;
    *size = req.size;
    return 0;
}

static int prime_export(int fd, uint32_t handle, int *dmabuf)
{
    struct drm_prime_handle req;

    memset(&req, 0, sizeof(req));
    req.handle = handle;
    req.flags = DRM_CLOEXEC | DRM_RDWR;
    if (ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &req)) {
        fprintf(stderr, "HANDLE_TO_FD: %s\n", strerror(errno));
        return -1;
    }
    *dmabuf = req.fd;
    return 0;
}

static int prime_import(int fd, int dmabuf, uint32_t *handle)
{
    struct drm_prime_handle req;

    memset(&req, 0, sizeof(req));
    req.fd = dmabuf;
    if (ioctl(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &req)) {
        fprintf(stderr, "FD_TO_HANDLE: %s\n", strerror(errno));
        return -1;
    }
    *handle = req.handle;
    return 0;
}

int main(int argc, char **argv)
{
    const char *exp_path = argc > 1 ? argv[1] : "/dev/dri/card0";
    const char *imp_path = argc > 2 ? argv[2] : "/dev/dri/card1";
    uint32_t src_handle, dst_handle;
    uint64_t size;
    int expfd, impfd, dmabuf;

    expfd = open(exp_path, O_RDWR | O_CLOEXEC);
    if (expfd < 0) {
        fprintf(stderr, "open %s: %s\n", exp_path, strerror(errno));
        return 1;
    }
    impfd = open(imp_path, O_RDWR | O_CLOEXEC);
    if (impfd < 0) {
        fprintf(stderr, "open %s: %s\n", imp_path, strerror(errno));
        return 1;
    }

    if (dumb_create(expfd, &src_handle, &size))
        return 1;
    printf("exporter %s: dumb buffer %ux%u, %llu bytes, handle %u\n",
           exp_path, W, H, (unsigned long long)size, src_handle);

    if (prime_export(expfd, src_handle, &dmabuf))
        return 1;
    printf("exported as dma-buf fd %d\n", dmabuf);

    if (prime_import(impfd, dmabuf, &dst_handle)) {
        fprintf(stderr, "IMPORT FAILED -- this is the measurement.\n"
                        "Check dmesg for virtio_nvrm's descriptorType line.\n");
        return 1;
    }
    printf("importer %s: handle %u\n", imp_path, dst_handle);
    printf("PRIME import OK\n");

    close(dmabuf);
    close(impfd);
    close(expfd);
    return 0;
}
