// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * The commands the guest's graphics stack never asks, asked directly.
 *
 * OPEN-QUESTIONS number 52: every graphics probe issues the same handful of
 * commands natively and NONE of them in a guest. The consequence is exact --
 * those signatures are predicted to be carried and are exercised by nothing
 * on the other side of the boundary, so `predicted-green` stays untested for
 * them however many guest sweeps pass. A workload cannot settle that,
 * because the workloads are what stopped asking. This asks.
 *
 * It is deliberately the smallest program that can: open the control node,
 * build the object hierarchy by hand, and issue each command once. No CUDA,
 * no GL, no driver userspace at all -- so what it measures is the boundary
 * and never a library's opinion of it.
 *
 * WHAT IT ISSUES, in the order RM's own object model requires:
 *
 *   NV0000_CTRL_CMD_GPU_GET_PROBED_IDS   0x214   on the client
 *   NV0000_CTRL_CMD_GPU_ATTACH_IDS       0x215   on the client
 *   NV2080_CTRL_CMD_TIMER_GET_TIME       0x20800403 on a subdevice
 *   NV0073_CTRL_CMD_SYSTEM_GET_CAPS_V2   0x730101   on NV04_DISPLAY_COMMON
 *   NV0000_CTRL_CMD_GPU_DETACH_IDS       0x216   on the client
 *
 * ATTACH and DETACH are a PAIR and the pair is the point: attaching without
 * detaching leaks the attachment, and this is exactly the lifecycle every
 * GL and EGL probe performs natively on every run -- `glxinfo` does it once
 * per invocation. The sequence is not novel, only the fact that nothing in a
 * guest performs it.
 *
 * NV_ESC_RM_IDLE_CHANNELS is the sixth command of number 52 and is NOT here:
 * it needs a channel, which needs a GPFIFO allocation, a pushbuffer and a
 * VA space, and building those by hand is a different program. Stated rather
 * than quietly skipped -- five of six is what this probe covers.
 *
 * SOURCES for every constant and offset below. Sizes are stated because the
 * traces already show them, which is a cross-check that costs nothing:
 *   NV_IOCTL_MAGIC 'F', NV_ESC_RM_ALLOC 0x2b, NV_ESC_RM_CONTROL 0x2a
 *     -> kernel-open/common/inc/nv-ioctl-numbers.h
 *   NVOS64 / NVOS54                      -> nvos.h (as in probe/c/ctrlping.c)
 *   NV0000_CTRL_GPU_GET_PROBED_IDS_PARAMS { gpuIds[32]; excludedGpuIds[32];
 *     gpuFlags[32] } = 384                -> ctrl0000gpu.h, NV_MAX_DEVICES 32
 *   NV0000_CTRL_GPU_ATTACH_IDS_PARAMS { gpuIds[32]; failedId } = 132
 *   NV0000_CTRL_GPU_DETACH_IDS_PARAMS { gpuIds[32] } = 128
 *   NV0080_ALLOC_PARAMETERS { deviceId; hClientShare; hTargetClient;
 *     hTargetDevice; flags; u64 vaSpaceSize @24; u64 vaStartInternal @32;
 *     u64 vaLimitInternal @40; vaMode @48 } = 56   -> cl0080.h:54-64
 *   NV2080_ALLOC_PARAMETERS { subDeviceId } = 4    -> cl2080.h
 *   NV2080_CTRL_TIMER_GET_TIME_PARAMS { u64 time_nsec } = 8 -> ctrl2080tmr.h
 *   NV0073_CTRL_SYSTEM_GET_CAPS_V2_PARAMS { capsTbl[2] } = 2
 *                                                  -> ctrl0073system.h:72,97
 *   NV0000_CTRL_GPU_INVALID_ID 0xffffffff terminates the id arrays, which is
 *     what the native traces show: `00 2d 00 00 ff ff ff ff ...`
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define NV_IOCTL_MAGIC     'F'
#define NV_ESC_RM_ALLOC    0x2b
#define NV_ESC_RM_CONTROL  0x2a
/* The handshake every NVIDIA client performs before anything else -- it is
 * the FIRST ioctl of every trace in this tree. Without it RM answers
 * NV_ERR_INSUFFICIENT_PERMISSIONS to the device allocation, which is how
 * this probe found out it was needed. (nv-ioctl-numbers.h, nv-ioctl.h:100) */
#define NV_ESC_CHECK_VERSION_STR    210
#define NV_RM_API_VERSION_CMD_QUERY '2'

#define NV01_ROOT_CLIENT      0x41
#define NV01_DEVICE_0         0x0080
#define NV20_SUBDEVICE_0      0x2080
#define NV04_DISPLAY_COMMON   0x0073

#define CMD_GPU_GET_PROBED_IDS  0x214
#define CMD_GPU_ATTACH_IDS      0x215
#define CMD_GPU_DETACH_IDS      0x216
#define CMD_TIMER_GET_TIME      0x20800403
#define CMD_SYSTEM_GET_CAPS_V2  0x730101
/* NV0080_CTRL_CMD_GR_GET_CAPS_V2. Not one of number 52's commands -- it is
 * here because number 61 is about it: in a guest, `nvdec`'s FIRST call of it
 * returns NV_OK and writes nothing while its second writes the same table
 * the native run produces. This probe calls it twice from a program with no
 * driver userspace in it, which is what tells a boundary behaviour apart
 * from something a library does. (ctrl0080gr.h; capsTbl is INLINE in V2 --
 * that is what V2 means -- with bCapsPopulated after it.) */
#define CMD_GR_GET_CAPS_V2      0x801109

#define MAX_GPUS   32
#define INVALID_ID 0xffffffffu

/* Handles this client picks for its own children. RM does not constrain
 * them beyond uniqueness within the client; the native traces show the
 * driver userspace picking unrelated values the same way. */
#define H_DEVICE    0x5ea00001u
#define H_SUBDEVICE 0x5ea00002u
#define H_DISPLAY   0x5ea00003u

typedef struct {                    /* nv_ioctl_rm_api_version_t, 72 bytes */
    uint32_t cmd;
    uint32_t reply;
    char versionString[64];
} nv_rm_api_version_t;

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

struct dev_params {                 /* NV0080_ALLOC_PARAMETERS, 56 bytes */
    uint32_t deviceId, hClientShare, hTargetClient, hTargetDevice, flags;
    uint32_t pad;
    uint64_t vaSpaceSize, vaStartInternal, vaLimitInternal;
    uint32_t vaMode, pad2;
};

/* NV0080_CTRL_GR_GET_CAPS_V2_PARAMS { NvU8 capsTbl[23]; NvBool
 * bCapsPopulated; ... } -- the traces show paramsSize 48 on every call, so
 * 48 is what a caller passes and 48 is what this passes. */
#define GR_CAPS_V2_LEN 48

struct probed_ids { uint32_t gpuIds[MAX_GPUS], excluded[MAX_GPUS], flags[MAX_GPUS]; };
struct attach_ids { uint32_t gpuIds[MAX_GPUS], failedId; };
struct detach_ids { uint32_t gpuIds[MAX_GPUS]; };

static int fd = -1;
static int failures;
/* Calls that returned NV_OK and wrote nothing. Counted apart from
 * `failures`: nothing failed, which is the whole point of number 61. */
static int unanswered;

/* One control. Reports and counts rather than exiting: a probe that stops at
 * the first refusal measures one command, and the point is to measure five. */
static int ctrl(uint32_t hclient, uint32_t hobject, uint32_t cmd,
                void *params, uint32_t len, const char *what)
{
    struct nvos54 c;
    memset(&c, 0, sizeof c);
    c.hClient = hclient;
    c.hObject = hobject;
    c.cmd = cmd;
    c.params = (uint64_t)(uintptr_t)params;
    c.paramsSize = len;

    int r = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_CONTROL, struct nvos54), &c);
    if (r != 0 || c.status != 0) {
        printf("  %-22s cmd=%#010x FAIL ret=%d status=%#x errno=%d\n",
               what, cmd, r, c.status, errno);
        failures++;
        return -1;
    }
    printf("  %-22s cmd=%#010x ok status=0x0 paramsSize=%u\n", what, cmd, len);
    return 0;
}

/* One allocation. `parms` may be NULL: several classes take none. */
static int alloc(uint32_t hroot, uint32_t hparent, uint32_t hnew, uint32_t hclass,
                 void *parms, const char *what)
{
    struct nvos64 a;
    memset(&a, 0, sizeof a);
    a.hRoot = hroot;
    a.hObjectParent = hparent;
    a.hObjectNew = hnew;
    a.hClass = hclass;
    a.pAllocParms = (uint64_t)(uintptr_t)parms;

    int r = ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, struct nvos64), &a);
    if (r != 0 || a.status != 0) {
        printf("  alloc %-16s class=%#06x FAIL ret=%d status=%#x errno=%d\n",
               what, hclass, r, a.status, errno);
        failures++;
        return -1;
    }
    printf("  alloc %-16s class=%#06x ok handle=%#x\n", what, hclass, a.hObjectNew);
    return 0;
}

int main(void)
{
    fd = open("/dev/nvidiactl", O_RDWR);
    if (fd < 0) {
        perror("open /dev/nvidiactl");
        return 1;
    }

    /* The version handshake, first, exactly as every driver client does it.
     * QUERY rather than STRICT: this probe is not checking that its own
     * build matches, it is telling RM that a client is present. */
    nv_rm_api_version_t ver;
    memset(&ver, 0, sizeof ver);
    ver.cmd = NV_RM_API_VERSION_CMD_QUERY;
    if (ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_CHECK_VERSION_STR, nv_rm_api_version_t),
              &ver) != 0) {
        perror("NV_ESC_CHECK_VERSION_STR");
        close(fd);
        return 1;
    }
    printf("  driver version: %.64s\n", ver.versionString);

    /* The GPU node, open for as long as the client lives. RM binds a client's
     * device to the state behind this node; with only the control node open
     * the allocation below is refused. */
    int gfd = open("/dev/nvidia0", O_RDWR);
    if (gfd < 0)
        printf("  /dev/nvidia0: %s -- the device allocation will likely be refused\n",
               strerror(errno));

    /* The client. hClass 0x41 and not 0x0: 0x0 is the privileged path and
     * answers INSUFFICIENT_PERMISSIONS to an ordinary user. */
    struct nvos64 root;
    memset(&root, 0, sizeof root);
    root.hClass = NV01_ROOT_CLIENT;
    if (ioctl(fd, _IOWR(NV_IOCTL_MAGIC, NV_ESC_RM_ALLOC, struct nvos64), &root) != 0
        || root.status != 0) {
        fprintf(stderr, "NV01_ROOT_CLIENT: status=%#x errno=%d\n", root.status, errno);
        close(fd);
        return 1;
    }
    uint32_t hc = root.hObjectNew;
    printf("  alloc %-16s class=%#06x ok handle=%#x\n", "root client", NV01_ROOT_CLIENT, hc);

    /* 1. Which GPUs exist. */
    struct probed_ids probed;
    memset(&probed, 0, sizeof probed);
    int have_probed = ctrl(hc, hc, CMD_GPU_GET_PROBED_IDS,
                           &probed, (uint32_t)sizeof probed, "GPU_GET_PROBED_IDS") == 0;

    unsigned n_ids = 0;
    if (have_probed)
        while (n_ids < MAX_GPUS && probed.gpuIds[n_ids] != INVALID_ID)
            n_ids++;
    printf("  probed ids: %u\n", n_ids);

    /* 2. Attach them. The id array is terminated by INVALID_ID, which is
     *    what the native traces carry. */
    int attached = 0;
    if (n_ids > 0) {
        struct attach_ids at;
        memset(&at, 0xff, sizeof at);        /* INVALID_ID in every slot */
        for (unsigned i = 0; i < n_ids; i++)
            at.gpuIds[i] = probed.gpuIds[i];
        at.failedId = 0;
        attached = ctrl(hc, hc, CMD_GPU_ATTACH_IDS,
                        &at, (uint32_t)sizeof at, "GPU_ATTACH_IDS") == 0;
    } else {
        printf("  %-22s skipped -- no probed id to attach\n", "GPU_ATTACH_IDS");
    }

    /* 3. A device and a subdevice, so that a subdevice control has a home. */
    struct dev_params dp;
    memset(&dp, 0, sizeof dp);
    dp.deviceId = 0;
    int have_dev = alloc(hc, hc, H_DEVICE, NV01_DEVICE_0, &dp, "device") == 0;

    if (have_dev) {
        uint32_t sub = 0;   /* NV2080_ALLOC_PARAMETERS { subDeviceId } */
        if (alloc(hc, H_DEVICE, H_SUBDEVICE, NV20_SUBDEVICE_0, &sub, "subdevice") == 0) {
            uint64_t t = 0;
            ctrl(hc, H_SUBDEVICE, CMD_TIMER_GET_TIME,
                 &t, (uint32_t)sizeof t, "TIMER_GET_TIME");
            printf("  gpu time: %llu ns\n", (unsigned long long)t);
        }

        /* 3b. NUMBER 61's REPRODUCER. Twice, on the device, with a buffer
         *     filled with a pattern RM cannot plausibly write, so that
         *     "did RM answer" is a question this program can answer for
         *     itself rather than one for the trace. */
        for (int k = 0; k < 2; k++) {
            unsigned char caps[GR_CAPS_V2_LEN];
            memset(caps, 0xa5, sizeof caps);
            if (ctrl(hc, H_DEVICE, CMD_GR_GET_CAPS_V2,
                     caps, (uint32_t)sizeof caps, "GR_GET_CAPS_V2") == 0) {
                int touched = 0;
                for (size_t j = 0; j < sizeof caps; j++)
                    if (caps[j] != 0xa5) { touched = 1; break; }
                printf("  GR_GET_CAPS_V2 call %d: RM %s the buffer (first bytes %02x %02x %02x)\n",
                       k, touched ? "WROTE" : "did NOT write",
                       caps[0], caps[1], caps[2]);
                if (!touched)
                    unanswered++;
            }
        }

        /* 4. The display object. It takes no allocation parameters. */
        if (alloc(hc, H_DEVICE, H_DISPLAY, NV04_DISPLAY_COMMON, NULL, "display") == 0) {
            uint8_t caps[2] = { 0, 0 };
            ctrl(hc, H_DISPLAY, CMD_SYSTEM_GET_CAPS_V2,
                 caps, (uint32_t)sizeof caps, "SYSTEM_GET_CAPS_V2");
            printf("  display caps: %02x %02x\n", caps[0], caps[1]);
        }
    }

    /* 5. Detach, which is the other half of step 2 and not optional. */
    if (attached) {
        struct detach_ids dt;
        memset(&dt, 0xff, sizeof dt);
        for (unsigned i = 0; i < n_ids; i++)
            dt.gpuIds[i] = probed.gpuIds[i];
        ctrl(hc, hc, CMD_GPU_DETACH_IDS,
             &dt, (uint32_t)sizeof dt, "GPU_DETACH_IDS");
    }

    /* Closing the nodes frees the whole hierarchy; RM does the teardown. */
    if (gfd >= 0)
        close(gfd);
    close(fd);

    printf("COMMANDS_FAILED=%d\n", failures);
    printf("COMMANDS_UNANSWERED=%d\n", unanswered);
    return failures ? 1 : 0;
}
