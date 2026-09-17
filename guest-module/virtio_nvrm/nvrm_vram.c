/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * nvrm_vram.c -- the arithmetic of the display reserve.
 *
 * ONE translation unit for two worlds, pulled in via #include, exactly as
 * nvrm_edid.c is:
 *   - virtio_nvrm.c (kernel module): sizes the reserve from the virtual
 *     display and moves the sizes in RM's answers by it (display_reserve_mib).
 *   - test/vramcheck.c (userspace): the formula against the measured table,
 *     the rewrites against hand-built answers. test.sh check runs it.
 *
 * Hence ONLY integer arithmetic on buffers in here. Whoever reads module
 * parameters, takes locks or prints is the includer.
 */

#ifdef __KERNEL__
# include <linux/kernel.h>
# include <linux/string.h>
# include <linux/types.h>
#else
# include <stddef.h>
# include <stdint.h>
# include <string.h>
typedef uint8_t u8;
typedef uint32_t u32;
typedef uint64_t u64;
#endif

#include "nvrm_wire.h"

static inline u32 nvrm_vram_rd32(const u8 *p, u32 off)
{
	u32 v;

	memcpy(&v, p + off, sizeof(v));
	return v;
}

static inline u64 nvrm_vram_rd64(const u8 *p, u32 off)
{
	u64 v;

	memcpy(&v, p + off, sizeof(v));
	return v;
}

static inline void nvrm_vram_wr32(u8 *p, u32 off, u32 v)
{
	memcpy(p + off, &v, sizeof(v));
}

static inline void nvrm_vram_wr64(u8 *p, u32 off, u64 v)
{
	memcpy(p + off, &v, sizeof(v));
}

/*
 * One scanout buffer of a W x H head, in bytes, as NVIDIA's GBM backend
 * sizes it: XRGB8888, block-linear, rounded to 64 KiB.
 *
 *   align(4 W, 64) x align(H, 128), then align(.., 64 KiB)
 *
 * From libnvidia-allocator.so.610.57.04 (read 2026-09-17): the block height
 * is chosen as 128 lines for every display size, the pitch is a whole number
 * of 64-byte groups, and each plane is rounded up to 64 KiB. nouveau lays
 * out block-linear the same way (nvc0_miptree.c). Measured the same day as
 * the asked size of every mutter, Xwayland and nvidia-modeset buffer at the
 * three sizes below -- 0x870000 is the one refused in the 2026-09-17 freeze.
 *
 *   1920x1080  0x0870000   8.44 MiB
 *   2560x1440  0x0f00000  15.00 MiB
 *   3840x2160  0x1fe0000  31.88 MiB
 */
static u64 nvrm_scanout_bytes(u32 w, u32 h)
{
	u64 pitch = ((u64)w * 4 + 63) & ~(u64)63;
	u64 rows = ((u64)h + 127) & ~(u64)127;

	return (pitch * rows + 0xffff) & ~(u64)0xffff;
}

/*
 * How many of those the reserve holds, and the cursor beside them.
 *
 * Measured on .23 on 2026-09-17 (GNOME 46 on Wayland, mutter in simple KMS
 * mode, one head, 2816 MiB guest FB), with every VIDMEM allocation and free
 * of the guest traced: the display side -- nvidia-modeset (the kernel NVKMS
 * path, which carries Xwayland's and the cursor's gbm_bos), Xwayland and
 * gnome-shell -- above its idle sum, highest point per transition, in
 * scanout buffers S of that size:
 *
 *                     X11 fullscreen   Wayland fullscreen   cursor   Moonlight
 *   1920x1080  S  8.4    34.0 MiB 4.04 S   25.4 MiB 3.01 S   +0.6     +0.4
 *   2560x1440  S 15.0    38.6 MiB 2.57 S   39.7 MiB 2.65 S   +0.5     +0.3
 *   3840x2160  S 31.9   148.9 MiB 4.67 S   88.1 MiB 2.76 S   +1.3     +1.4
 *
 * The X11 case is the one that froze: Xwayland's window buffers for a
 * fullscreen window are three S through NVKMS, SCANOUT once mutter offers
 * direct scanout, plus one more while a window of the old size is still
 * out, plus gnome-shell's own transition copies. A Wayland client allocates
 * its swapchain itself; what grows is gnome-shell's fullscreen transition
 * (a colour and a depth buffer of S). mutter's own swapchain (four S in
 * gnome-shell, NVOS32) is allocated at session start and was never seen to
 * grow. Five S is the highest peak (4.67 S) in whole buffers. The cursor is
 * pitch linear, 256x256x4, and mutter's cursor manager holds up to four
 * (meta-kms-cursor-manager.c): 1 MiB.
 *
 *   1920x1080  44 MiB    2560x1440  76 MiB    3840x2160  161 MiB
 */
#define NVRM_RESERVE_SCANOUTS		5
#define NVRM_RESERVE_CURSOR_BYTES	(4u * 256 * 256 * 4)

/* The ceiling past which a size is not a display: the module's own maximum
 * (vdisplay_max_*) never gets near it, and it keeps the product in 64 bits
 * whatever a parameter says. */
#define NVRM_RESERVE_MAX_DIM		32768

static u32 nvrm_display_reserve_auto_mib(u32 w, u32 h)
{
	u64 b;

	if (!w || !h || w > NVRM_RESERVE_MAX_DIM || h > NVRM_RESERVE_MAX_DIM)
		return 0;
	b = NVRM_RESERVE_SCANOUTS * nvrm_scanout_bytes(w, h) + NVRM_RESERVE_CURSOR_BYTES;
	return (u32)((b + (1u << 20) - 1) >> 20);
}

/*
 * Take `r_kb` off the five size indices of one NV2080_CTRL_FB_INFO list, in
 * place, clamped at 0. TOTAL, RAM, USABLE, HEAP and HEAP_FREE move together,
 * so Used (HEAP - HEAP_FREE) -- what nvidia-smi and NVML report -- stays
 * what the host said; only a free smaller than the reserve clamps, and then
 * Used reads as the whole advertised heap, which is true.
 *
 * `list` holds `n` entries of NVRM_FB_INFO_ENTRY_SIZE bytes; the caller has
 * already bounded `n` by the buffer. Returns how many entries moved.
 */
static u32 nvrm_fb_info_reserve(u8 *list, u32 n, u32 r_kb)
{
	u32 i, touched = 0;

	for (i = 0; r_kb && i < n; i++) {
		u32 e = i * NVRM_FB_INFO_ENTRY_SIZE;
		u32 d = e + NVRM_FB_INFO_DATA_OFF;
		u32 v;

		switch (nvrm_vram_rd32(list, e)) {
		case NVRM_FB_INFO_INDEX_RAM_SIZE:
		case NVRM_FB_INFO_INDEX_TOTAL_RAM_SIZE:
		case NVRM_FB_INFO_INDEX_USABLE_RAM_SIZE:
		case NVRM_FB_INFO_INDEX_HEAP_SIZE:
		case NVRM_FB_INFO_INDEX_HEAP_FREE:
			v = nvrm_vram_rd32(list, d);
			nvrm_vram_wr32(list, d, v > r_kb ? v - r_kb : 0);
			touched++;
			break;
		default:
			break;
		}
	}
	return touched;
}

/*
 * The same move on an NVOS32_FUNCTION_INFO answer: `total` and `free` in
 * bytes, in the NVOS32 block itself. `nvos32` holds NVRM_NVOS32_SIZE bytes
 * and is an INFO answer with status OK -- the caller checked both.
 */
static void nvrm_heap_info_reserve(u8 *nvos32, u64 r)
{
	u64 v;

	v = nvrm_vram_rd64(nvos32, NVRM_NVOS32_TOTAL_OFF);
	nvrm_vram_wr64(nvos32, NVRM_NVOS32_TOTAL_OFF, v > r ? v - r : 0);
	v = nvrm_vram_rd64(nvos32, NVRM_NVOS32_FREE_OFF);
	nvrm_vram_wr64(nvos32, NVRM_NVOS32_FREE_OFF, v > r ? v - r : 0);
}
