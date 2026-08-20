// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * bindgen entry point. Declarations only, no logic -> no C code is compiled
 * into the build.
 *
 * The order matters: nvtypes before everything else, otherwise the class
 * headers do not know NvU32/NvHandle.
 *
 * Deliberately NOT included:
 *   - dev_vm.h (swref): consists almost entirely of DRF macros (NVIDIA's
 *     hi:lo bit-field notation), which
 *     bindgen does not emit. The three doorbell constants are written by
 *     hand in nvrm-abi::doorbell.
 */

#include <nvtypes.h>
#include <nvmisc.h>
#include <nvstatus.h>
#include <nvlimits.h>

/* ioctl level: escape numbers + the nv_ioctl_* structs */
#include <nv-ioctl-numbers.h>
#include <nv-ioctl.h>
#include <nv_escape.h>

/* RM level: NVOS21/33/46/54/64 etc. */
#include <nvos.h>

/* The classes the RM object graph is built from */
#include <class/cl0000.h>   /* NV01_ROOT / NV01_ROOT_CLIENT / NV01_NULL_OBJECT */
#include <class/cl0005.h>   /* NV01_EVENT                             */
#include <class/cl003e.h>   /* NV01_MEMORY_SYSTEM                     */
#include <class/cl50a0.h>   /* NV50_MEMORY_VIRTUAL                    */
#include <class/cl0071.h>   /* NV01_MEMORY_SYSTEM_OS_DESCRIPTOR        */
#include <class/cl0080.h>   /* NV01_DEVICE_0                          */
#include <class/cl2080.h>   /* NV20_SUBDEVICE_0                       */
#include <class/cl90f1.h>   /* FERMI_VASPACE_A                        */
#include <class/cl9067.h>   /* FERMI_CONTEXT_SHARE_A                  */
#include <class/cla06c.h>   /* KEPLER_CHANNEL_GROUP_A (TSG)           */
#include <class/clc361.h>   /* NVC361_NOTIFY_CHANNEL_PENDING + __SIZE */
#include <class/clc461.h>   /* TURING_USERMODE_A - the class number only,
                               the offsets live in clc361.h           */
#include <class/clc36f.h>   /* VOLTA_CHANNEL_GPFIFO_A (base class)    */
#include <class/clc46f.h>   /* TURING_CHANNEL_GPFIFO_A + methods      */
#include <class/cl2080_notification.h>  /* NV2080_ENGINE_TYPE_*                   */

/* Alloc parameters for channel, channel group and context share */
#include <alloc/alloc_channel.h>

/* Controls */
#include <class/cl0073.h>   /* NV04_DISPLAY_COMMON -- the class whose PRESENCE
                             keeps NVKMS out of the displayless path      */
#include <class/cla083.h>   /* NVA083_GRID_DISPLAYLESS -- the class NVKMS
                             takes when a GPU has no connectors of its own */
#include <ctrl/ctrla083.h>  /* the six controls the class defines;
                             NVKMS asks three of them                 */
#include <class/cl9010.h>   /* NV9010_VBLANK_CALLBACK -- pProc is a guest
                             kernel pointer, so the guest module services
                             the class itself (OPEN-QUESTIONS nr 7)      */
#include <ctrl/ctrl9010.h>  /* SET_VBLANK_NOTIFICATION -- the one control
                             the class exports                           */
#include <ctrl/ctrl00da.h>  /* NV_SEMAPHORE_SURFACE REGISTER/UNREGISTER_
                             WAITER -- nvidia-drm's fence waiters carry a
                             guest kernel callback pointer through these */
#include <ctrl/ctrl0080/ctrl0080gpu.h>  /* GET_CLASSLIST -- the entry point  */
#include <ctrl/ctrl0080/ctrl0080unix.h> /* VT_SWITCH / VT_GET_FB_INFO -- the
                                           console state of a card this guest
                                           has no console on              */
#include <ctrl/ctrl2080/ctrl2080unix.h> /* GC6_BLOCKER_REFCNT -- the call that
                                           gates nvAllocCoreChannelEvo      */
#include <ctrl/ctrla06c.h>  /* GPFIFO_SCHEDULE                        */
#include <ctrl/ctrlc36f.h>  /* GPFIFO_GET_WORK_SUBMIT_TOKEN           */
#include <ctrl/ctrl2080/ctrl2080gpu.h>  /* GET_GID_INFO - UUID for UVM_REGISTER_GPU */
#include <ctrl/ctrl2080/ctrl2080bus.h>  /* BUS_GET_INFO_V2 - where NVML reads the BDF */
#include <ctrl/ctrl0000/ctrl0000gpu.h>  /* GET_PROBED_IDS / GET_PCI_INFO -- what
                                           enumerate_gpus asks RM, so that the
                                           GPU list nvidia-drm sees is RM's and
                                           not this module's invention        */

/* NVKMS. /dev/nvidia-modeset is a userspace boundary of its own -- the GL
 * and Vulkan libraries call it directly -- and every one of its ioctls
 * carries the same number, with the real command in a field of this
 * struct. Only the indirection header is needed: the command NAMESPACE
 * (nvkms-api.h) is not decoded anywhere here. */
#include <nvkms-ioctl.h>

/* UVM. Needed because the semaphore-pool path talks to /dev/nvidia-uvm
 * directly. Command numbers there are RAW integers (UVM_IOCTL_BASE(i) == i,
 * uvm_ioctl.h:40), not _IOWR - only UVM_INITIALIZE carries the special value
 * 0x30000001 (uvm_linux_ioctl.h:32). */
#include <uvm_linux_ioctl.h>
