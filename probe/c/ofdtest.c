// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
// Measures what RM binds a client to: the OFD it was created on, or just
// the handle.
//
// Test 1: create a client on fd1, control with that hClient on fd2 (same
//         process, different OFD).
// Test 2: the same from a child process with its own OFD.
// Test 3: control with a made-up client handle (the control case).
//
// Answer 0x0  -> no OFD binding, the handle alone suffices.
// Answer != 0 -> the binding holds (0x23 == NV_ERR_INVALID_CLIENT).

#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/wait.h>

#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_RM_ALLOC 0x2b
#define NV_ESC_RM_CONTROL 0x2a
#define NV_ESC_RM_FREE 0x29
#define NV_ESC_CARD_INFO 0xc8

#define NV01_ROOT_CLIENT 0x41
#define CMD_GET_ACCESS_RIGHTS 0xd03

// NVOS64_PARAMETERS, nvos.h:480-490
struct nvos64 {
    uint32_t hRoot, hObjectParent, hObjectNew, hClass;
    uint64_t pAllocParms, pRightsRequested;
    uint32_t paramsSize, flags, status, _pad;
};

// NVOS54_PARAMETERS
struct nvos54 {
    uint32_t hClient, hObject, cmd, flags;
    uint64_t params;
    uint32_t paramsSize, status;
};

// NV0000_CTRL_CLIENT_GET_ACCESS_RIGHTS_PARAMS, ctrl0000client.h:107-111
struct accrights {
    uint32_t hObject, hClient, maskResult;
};

static int open_ctl(void) {
    int fd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror("open /dev/nvidiactl"); }
    return fd;
}

static uint32_t alloc_client(int fd, int *ok) {
    struct nvos64 p;
    memset(&p, 0, sizeof p);
    p.hClass = NV01_ROOT_CLIENT;
    int r = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, struct nvos64), &p);
    *ok = (r == 0 && p.status == 0);
    if (!*ok) fprintf(stderr, "  alloc client: ret=%d status=0x%x\n", r, p.status);
    return p.hObjectNew;
}

// Control with hClient == hclient on fd. Prints (ret, status).
static void probe(const char *label, int fd, uint32_t hclient, uint32_t hobject) {
    struct accrights ap;
    memset(&ap, 0, sizeof ap);
    ap.hObject = hobject;
    ap.hClient = hclient;

    struct nvos54 c;
    memset(&c, 0, sizeof c);
    c.hClient = hclient;
    c.hObject = hclient;      // root-client controls go to the client itself
    c.cmd = CMD_GET_ACCESS_RIGHTS;
    c.params = (uint64_t)(uintptr_t)&ap;
    c.paramsSize = sizeof ap;

    int r = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_CONTROL, struct nvos54), &c);
    printf("  %-46s ret=%2d status=0x%02x mask=0x%08x\n",
           label, r, c.status, ap.maskResult);
}

static void free_client(int fd, uint32_t h) {
    struct nvos64 p;
    memset(&p, 0, sizeof p);
    p.hRoot = h; p.hObjectParent = h; p.hObjectNew = h;
    ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_FREE, struct nvos64), &p);
}

int main(void) {
    printf("sizeof nvos64=%zu (erwartet 48), nvos54=%zu (erwartet 32)\n",
           sizeof(struct nvos64), sizeof(struct nvos54));

    int fd1 = open_ctl(), fd2 = open_ctl();
    if (fd1 < 0 || fd2 < 0) return 1;

    int ok1 = 0, ok2 = 0;
    uint32_t c1 = alloc_client(fd1, &ok1);
    uint32_t c2 = alloc_client(fd2, &ok2);
    if (!ok1 || !ok2) { fprintf(stderr, "client alloc failed\n"); return 1; }
    printf("client1=0x%08x (on fd %d), client2=0x%08x (on fd %d)\n\n", c1, fd1, c2, fd2);

    printf("Same process:\n");
    probe("own client on own OFD (reference)", fd1, c1, c1);
    probe("own client on FOREIGN OFD", fd2, c1, c1);
    probe("erfundener Client-Handle (Kontrolle)", fd1, 0xc1d0dead, 0xc1d0dead);

    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        int fd3 = open_ctl();
        int ok3 = 0;
        uint32_t c3 = alloc_client(fd3, &ok3);
        printf("\nchild process (pid %d, own OFD, own client=0x%08x):\n",
               getpid(), ok3 ? c3 : 0);
        probe("PARENT's client on its own OFD", fd3, c1, c1);
        probe("PARENT's client on the INHERITED OFD (fd1)", fd1, c1, c1);
        probe("own client (reference)", fd3, c3, c3);
        fflush(stdout);
        _exit(0);
    }
    waitpid(pid, NULL, 0);

    free_client(fd1, c1);
    free_client(fd2, c2);
    return 0;
}
