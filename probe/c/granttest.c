// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
// Measures whether the DUP_OBJECT grant (RS_SHARE_TYPE_OS_SECURITY_TOKEN)
// this project sets makes an object dupable by a FOREIGN process.
//
// NV0000_CTRL_CMD_CLIENT_GET_ACCESS_RIGHTS exists, per the header,
// precisely to query rights on FOREIGN objects ("does not have to be
// owned by the client calling the command"). The caller uses its
// OWN client on its OWN OFD (open file description) -- exactly the
// situation of a second guest that knows the first one's handles.
//
// Mask bits (rs_access.h:59-62): 0=DUP_OBJECT 1=NICE 2=DEBUG 3=PERFMON
//
// Two pipes, one per direction. With a single pipe the child reads back
// its own sync byte and measures before the grant.

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

#define NV01_ROOT_CLIENT 0x41
#define CMD_GET_ACCESS_RIGHTS 0xd03
#define CMD_SHARE_OBJECT 0xd06

struct nvos64 {
    uint32_t hRoot, hObjectParent, hObjectNew, hClass;
    uint64_t pAllocParms, pRightsRequested;
    uint32_t paramsSize, flags, status, _pad;
};
struct nvos54 {
    uint32_t hClient, hObject, cmd, flags;
    uint64_t params;
    uint32_t paramsSize, status;
};
struct accrights { uint32_t hObject, hClient, maskResult; };

// RS_SHARE_POLICY, rs_access.h:268-273
struct sharepolicy {
    uint32_t target;
    uint32_t accessMask;   // limbs[1], RsAccessLimb == NvU32
    uint16_t type;
    uint8_t  action;
    uint8_t  _pad;
};
// NV0000_CTRL_CLIENT_SHARE_OBJECT_PARAMS, ctrl0000client.h:159-162
struct shareobj { uint32_t hObject; struct sharepolicy policy; };

#define SHARE_TYPE_ALL 1
#define SHARE_TYPE_OS_SECURITY_TOKEN 2
#define SHARE_ACTION_COMPOSE (1u << 2)

static int open_ctl(void) { return open("/dev/nvidiactl", O_RDWR | O_CLOEXEC); }

static uint32_t alloc_client(int fd) {
    struct nvos64 p; memset(&p, 0, sizeof p);
    p.hClass = NV01_ROOT_CLIENT;
    int r = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, struct nvos64), &p);
    if (r || p.status) { fprintf(stderr, "alloc client ret=%d st=0x%x\n", r, p.status); return 0; }
    return p.hObjectNew;
}

static int ctrl(int fd, uint32_t hclient, uint32_t hobject, uint32_t cmd,
                void *params, uint32_t psize, uint32_t *status_out) {
    struct nvos54 c; memset(&c, 0, sizeof c);
    c.hClient = hclient; c.hObject = hobject; c.cmd = cmd;
    c.params = (uint64_t)(uintptr_t)params; c.paramsSize = psize;
    int r = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_CONTROL, struct nvos54), &c);
    *status_out = c.status;
    return r;
}

static void query(const char *label, int fd, uint32_t self, uint32_t oclient, uint32_t oobj) {
    struct accrights ap; memset(&ap, 0, sizeof ap);
    ap.hClient = oclient; ap.hObject = oobj;
    uint32_t st;
    int r = ctrl(fd, self, self, CMD_GET_ACCESS_RIGHTS, &ap, sizeof ap, &st);
    printf("  %-46s ret=%d status=0x%02x mask=0x%08x  DUP=%s\n",
           label, r, st, ap.maskResult, (ap.maskResult & 1) ? "JA" : "nein");
}

static void grant(int fd, uint32_t hclient, uint32_t hobject, uint16_t type, const char *tn) {
    struct shareobj so; memset(&so, 0, sizeof so);
    so.hObject = hobject;
    so.policy.accessMask = 1u << 0;              // RS_ACCESS_DUP_OBJECT
    so.policy.type = type;
    so.policy.action = SHARE_ACTION_COMPOSE;
    uint32_t st;
    int r = ctrl(fd, hclient, hclient, CMD_SHARE_OBJECT, &so, sizeof so, &st);
    printf("  [A gibt 0x%08x frei, type=%s: ret=%d status=0x%02x]\n", hobject, tn, r, st);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("sizeof sharepolicy=%zu (erwartet 12), shareobj=%zu (erwartet 16)\n\n",
           sizeof(struct sharepolicy), sizeof(struct shareobj));

    int fdA = open_ctl();
    if (fdA < 0) { perror("open"); return 1; }
    uint32_t cA = alloc_client(fdA);
    if (!cA) return 1;

    int ab[2], ba[2];                 // ab: A->B, ba: B->A
    if (pipe(ab) < 0 || pipe(ba) < 0) return 1;

    pid_t pid = fork();
    if (pid == 0) {
        close(fdA); close(ab[1]); close(ba[0]);
        int fdB = open_ctl();
        uint32_t cB = alloc_client(fdB);
        printf("A client=0x%08x, B client=0x%08x, getrennte OFDs\n\n", cA, cB);
        char s;

        printf("Phase 1 - without a grant:\n");
        query("B fragt Rechte an A's Client", fdB, cB, cA, cA);
        query("B fragt Rechte an eigenem Client (Referenz)", fdB, cB, cB, cB);
        write(ba[1], "x", 1); read(ab[0], &s, 1);

        printf("\nPhase 2 - after OS_SECURITY_TOKEN (same euid):\n");
        query("B fragt Rechte an A's Client", fdB, cB, cA, cA);
        write(ba[1], "x", 1); read(ab[0], &s, 1);

        printf("\nPhase 3 - after SHARE_TYPE_ALL (the counter-check):\n");
        query("B fragt Rechte an A's Client", fdB, cB, cA, cA);
        write(ba[1], "x", 1);
        _exit(0);
    }
    close(ab[0]); close(ba[1]);
    char s;

    read(ba[0], &s, 1);
    printf("\n");
    grant(fdA, cA, cA, SHARE_TYPE_OS_SECURITY_TOKEN, "OS_SECURITY_TOKEN");
    write(ab[1], "x", 1);

    read(ba[0], &s, 1);
    printf("\n");
    grant(fdA, cA, cA, SHARE_TYPE_ALL, "ALL");
    write(ab[1], "x", 1);

    read(ba[0], &s, 1);
    waitpid(pid, NULL, 0);
    printf("\n[A raeumt ab]\n");
    struct nvos64 f; memset(&f, 0, sizeof f);
    f.hRoot = cA; f.hObjectParent = cA; f.hObjectNew = cA;
    ioctl(fdA, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_FREE, struct nvos64), &f);
    return 0;
}
