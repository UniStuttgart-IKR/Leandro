/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * nvrm_vram.c -- how much VRAM the guest's display path needs, measured.
 *
 * Plain integer arithmetic, in its own translation unit the way nvrm_edid.c
 * is, so test/vramcheck.c checks the formula against the measured table in
 * userspace. test.sh check runs it.
 */

#ifdef __KERNEL__
# include <linux/types.h>
#else
# include <stdint.h>
typedef uint32_t u32;
typedef uint64_t u64;
#endif

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
 * How many of those the display path needs above idle, and the cursor
 * beside them.
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
