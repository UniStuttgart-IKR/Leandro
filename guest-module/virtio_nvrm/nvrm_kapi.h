/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * nvrm_kapi.h -- the interface nvidia-modeset.ko expects from nvidia.ko.
 *
 * That is NVKMS (NVIDIA's modesetting kernel module) asking RM (the Resource
 * Manager inside nvidia.ko) for a function table instead of opening a device
 * node -- the "kernel path" virtio_nvrm.c serves. virtio_nvrm.ko takes
 * nvidia.ko's place, so it has to offer exactly this shape.
 *
 * MIRRORED, not included. The definitive copies live in the vendor tree at
 * NVIDIA_VERSION 610.43.03:
 *
 *   kernel-open/common/inc/nv-modeset-interface.h        nvidia_modeset_rm_ops_t
 *                                                        nvidia_modeset_callbacks_t
 *   kernel-open/common/inc/nv-gpu-info.h                 nv_gpu_info_t
 *   src/nvidia/arch/nvalloc/unix/include/
 *       nv-kernel-rmapi-ops.h                            op codes carried by op()
 *
 * Why mirrored and not included: virtio_nvrm.ko builds IN THE GUEST out of
 * ~/guest-module, against kernel headers and nothing else -- the same reason
 * nvrm_wire.h is checked in instead of generated there. The guest has no
 * NVIDIA source tree, and requiring one would make the module unbuildable
 * exactly where it has to be built.
 *
 * A transcribed layout is what goes stale, so it is checked rather than
 * trusted: the kapi-abi step of `scripts/test.sh check` compiles this header
 * side by side with the vendor originals and fails on any disagreement in
 * size, alignment or offset.
 */

#ifndef _NVRM_KAPI_H_
#define _NVRM_KAPI_H_

#include <linux/types.h>
#include <linux/stddef.h>

/* NV_STATUS is NvU32; NvBool is NvU8 (nvtypes.h:272, nvstatus.h:33). */
#define NVRM_NV_OK		0x00000000u
#define NVRM_NV_ERR_GENERIC	0x0000FFFFu

/* nv-gpu-info.h */
#define NVRM_NV_MAX_GPUS	32

struct nvrm_gpu_info {
	__u32 gpu_id;
	struct {
		__u32 domain;
		__u8  bus, slot, function;
	} pci_info;
	__u8 needs_numa_setup;		/* NvBool */
	__u8 is_soc_disp;		/* NvBool */
	/* On Linux: the GPU's `struct device *`. */
	void *os_device_ptr;
};

/*
 * Callbacks from the RM side INTO nvidia-modeset. NVKMS registers them at
 * load; on the host they fire when a GPU is probed, suspended or removed.
 *
 * Nothing in this module calls them: a virtqueue is strictly
 * request/response, and there is no path from the host back into the guest
 * kernel. They are stored so that set_callbacks() keeps its contract and so
 * that the day a back channel exists, the table is already here.
 */
struct nvrm_modeset_callbacks {
	void (*suspend)(__u32 gpu_id);
	void (*resume)(__u32 gpu_id);
	void (*remove)(__u32 gpu_id);
	void (*probe)(const struct nvrm_gpu_info *gpu_info);
};

/* The table nvidia_get_rm_ops() fills in. Field ORDER is the ABI. */
struct nvrm_modeset_rm_ops {
	const char *version_string;
	struct {
		__u8 allow_write_combining;	/* NvBool */
	} system_info;
	int  (*alloc_stack)(void **sp);
	void (*free_stack)(void *sp);
	__u32 (*enumerate_gpus)(struct nvrm_gpu_info *gpu_info);
	int  (*open_gpu)(__u32 gpu_id, void *sp, __u8 reset_aware);
	void (*close_gpu)(__u32 gpu_id, void *sp, __u8 reset_aware);
	void (*op)(void *sp, void *ops_cmd);
	int  (*set_callbacks)(const struct nvrm_modeset_callbacks *cb);
};

/*
 * nv-kernel-rmapi-ops.h: the block op() actually receives.
 *
 *   typedef struct { NvU32 op; union { ...NVOS structs... } params; }
 *
 * The op NUMBERS and the escape each maps onto live in nvrm_wire.h
 * as NVRM_KOP_x, NVRM_KESC_x and NVRM_KSIZE_x -- generated from the vendor headers
 * via bindgen, because a size typed in by hand is the one that goes stale.
 * Only the offset of the union is an ABI fact of THIS struct, and it is
 * asserted rather than assumed: NvU32 op at 0, union aligned to 8.
 */
#define NVRM_KAPI_PARAMS_OFF	8u

/*
 * The one symbol nvidia-modeset.ko links against. Measured: `nm -u` on the
 * vendor-built nvidia-modeset.ko lists 104 undefined symbols, and the other
 * 103 are the kernel's own.
 *
 * Hidden from the ABI check on purpose. That check includes NVIDIA's header
 * and this one in the SAME translation unit, and both declare this name --
 * with parameter types that are, by construction, two different struct tags
 * for one layout. C calls that a conflict no matter how well the layouts
 * agree, so the declaration steps aside and lets the assertions do the work:
 * that step proves the struct layouts identical field by field and
 * NV_STATUS to be an unsigned 32-bit type, which is the same statement
 * without the name collision.
 */
#ifndef NVRM_KAPI_ABI_CHECK
__u32 nvidia_get_rm_ops(struct nvrm_modeset_rm_ops *rm_ops);
#endif

#endif /* _NVRM_KAPI_H_ */
