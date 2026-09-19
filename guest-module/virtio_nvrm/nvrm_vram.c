/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/* Display reserve sizing and balloon arithmetic.
 * Included by virtio_nvrm.c and test/vramcheck.c; no allocation or locking.
 */

#ifdef __KERNEL__
#include <linux/string.h>
#include <linux/types.h>
#else
#include <stddef.h>
#include <stdint.h>
#include <string.h>
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

/* Reserve five scanout buffers plus four 256x256x4 cursor buffers.
 * Measured 2026-09-17 on .23: GNOME 46, Wayland, simple KMS, one head,
 * 2816 MiB guest FB. Peak display allocations above idle:
 *
 *   size        X11 fullscreen   Wayland fullscreen   cursor   Moonlight
 *   1920x1080       34.0 MiB          25.4 MiB        +0.6       +0.4
 *   2560x1440       38.6 MiB          39.7 MiB        +0.5       +0.3
 *   3840x2160      148.9 MiB          88.1 MiB        +1.3       +1.4
 *
 * The largest peak is 4.67 scanouts, rounded up to five. Mutter holds up
 * to four cursors (meta-kms-cursor-manager.c). Rounded reserves are 44,
 * 76 and 161 MiB.
 */
#define NVRM_RESERVE_SCANOUTS 5
#define NVRM_RESERVE_CURSOR_BYTES (4u * 256 * 256 * 4)

/* The ceiling past which a size is not a display: the module's own maximum
 * (vdisplay_max_*) never gets near it, and it keeps the product in 64 bits
 * whatever a parameter says. */
#define NVRM_RESERVE_MAX_DIM 32768

static u32 nvrm_display_reserve_auto_mib(u32 w, u32 h)
{
	u64 b;

	if (!w || !h || w > NVRM_RESERVE_MAX_DIM || h > NVRM_RESERVE_MAX_DIM)
		return 0;
	b = NVRM_RESERVE_SCANOUTS * nvrm_scanout_bytes(w, h) +
	    NVRM_RESERVE_CURSOR_BYTES;
	return (u32)((b + (1u << 20) - 1) >> 20);
}

/*
 * ---- The balloon ----------------------------------------------------------
 *
 * R bytes held as `full` chunks of one scanout buffer S each and one `rest`
 * chunk (R - full x S, smaller than S). S because the buffer NVKMS is
 * refused IS one scanout buffer (0x870000 at 1920x1080, asked by
 * nvidia-modeset in every freeze of 2026-09-17), so one chunk given back is
 * exactly the room one refusal needs and nothing more leaves the balloon for
 * a game to take. The rest is the cursor's chunk: the auto R is five S plus
 * 1 MiB rounded up to a MiB, so the rest is at least the four 256x256 cursor
 * buffers mutter keeps (1.8 MiB at 1920x1080, 1.0 at 2560x1440, 1.6 at
 * 3840x2160).
 *
 * A fixed R of many S would make that many RM objects, so the chunk grows
 * past S once R needs more than NVRM_BALLOON_MAX_CHUNKS of them (1024 MiB at
 * 1920x1080 is 64 chunks of 16 MiB): a refusal then takes more room than it
 * needs, which is the price of a fixed value that large.
 */
#define NVRM_BALLOON_MAX_CHUNKS 64u
#define NVRM_BALLOON_ALIGN 0x10000ull /* 64 KiB, as a scanout buffer */

struct nvrm_balloon_shape {
	u64 chunk; /* bytes per full chunk */
	u32 full; /* how many of those */
	u64 rest; /* bytes of the last chunk, 0 = none */
};

static void nvrm_balloon_shape(u64 r, u64 s, struct nvrm_balloon_shape *b)
{
	u64 least = (r + NVRM_BALLOON_MAX_CHUNKS - 1) / NVRM_BALLOON_MAX_CHUNKS;

	least = (least + NVRM_BALLOON_ALIGN - 1) & ~(NVRM_BALLOON_ALIGN - 1);
	b->chunk = s > least ? s : least;
	b->full = b->chunk ? (u32)(r / b->chunk) : 0;
	b->rest = r - (u64)b->full * b->chunk;
}

/*
 * Which chunk gives way to a refused request that still needs `want` bytes:
 * the rest when it covers them alone (a cursor), else a full chunk (a
 * scanout buffer takes exactly one), else the rest. The caller asks again
 * until the request is covered or nothing is held.
 *
 * Returns 1 = a full chunk, 0 = the rest, -1 = nothing held.
 */
static int nvrm_balloon_pick(u32 full_held, int rest_held, u64 rest, u64 want)
{
	if (rest_held && rest >= want)
		return 0;
	if (full_held)
		return 1;
	return rest_held ? 0 : -1;
}

/*
 * Whether an NV04_ALLOC is a display buffer the balloon gives way to.
 * `alloc` is its NV_MEMORY_ALLOCATION_PARAMS, NVRM_MEMALLOC_SIZE bytes.
 *
 * NV01_MEMORY_LOCAL_USER in VIDMEM with attr2 ISO: what NVKMS asks for
 * NVKMS_KAPI_ALLOCATION_TYPE_SCANOUT (nvkms-kapi.c:835-860) -- the GEM
 * buffers nvidia-drm makes for Xwayland's windows and the cursor
 * (nvidia-drm-gem-nvkms-memory.c:654), dumb buffers (:480), NVKMS's own
 * surfaces and LUTs. Scanout memory has no system-memory fallback anywhere
 * in the stack (:657-673 has one only for NO_SCANOUT), so a refusal here is
 * a refusal of the picture. NO_SCANOUT and VIRTUAL are not display
 * buffers, and never eligible. The kernel path is the includer's to check:
 * only NVKMS's op() may take room from the balloon, a process never.
 */
static int nvrm_balloon_eligible(u32 hclass, const u8 *alloc)
{
	u32 flags, attr, attr2;

	/* The class first: only its params are NVRM_MEMALLOC_SIZE long. */
	if (hclass != NVRM_CLASS_MEMORY_LOCAL_USER)
		return 0;
	flags = nvrm_vram_rd32(alloc, NVRM_MEMALLOC_FLAGS_OFF);
	attr = nvrm_vram_rd32(alloc, NVRM_MEMALLOC_ATTR_OFF);
	attr2 = nvrm_vram_rd32(alloc, NVRM_MEMALLOC_ATTR2_OFF);
	return (attr & NVRM_NVOS32_ATTR_LOCATION_MASK) ==
		       NVRM_NVOS32_ATTR_LOCATION_VIDMEM &&
	       (attr2 & NVRM_NVOS32_ATTR2_ISO_YES) &&
	       !(flags & (NVRM_NVOS32_ALLOC_FLAGS_NO_SCANOUT |
			  NVRM_NVOS32_ALLOC_FLAGS_VIRTUAL));
}

/*
 * The block one chunk is asked with: NVKMS's own SCANOUT request, pitch
 * format, of `bytes`. The same attributes as the buffer that will take its
 * room, so the hole a freed chunk leaves on the card is one that buffer
 * fits: the host's PMA places by CONTIGUOUS and ignores ISO
 * (video_mem.c:190-345, 325-338), and the ledger only counts bytes.
 *
 * The owner is a tag of the balloon's own. RM refuses a client allocation
 * with owner 0, ~0 or one of its internal owners (NV_ERR_INVALID_OWNER,
 * standard_mem.c:78-82) -- measured on .23 as status 0x39 on every chunk.
 * NVKMS tags its buffers 0xDCBA (nvkms-types.h:116); "nvbl" keeps the
 * balloon's apart in an RM heap dump.
 */
#define NVRM_BALLOON_OWNER 0x6c62766eu /* "nvbl" */

static void nvrm_balloon_chunk_params(u8 *alloc, u64 bytes)
{
	memset(alloc, 0, NVRM_MEMALLOC_SIZE);
	nvrm_vram_wr32(alloc, NVRM_MEMALLOC_OWNER_OFF, NVRM_BALLOON_OWNER);
	nvrm_vram_wr32(alloc, NVRM_MEMALLOC_TYPE_OFF, NVRM_NVOS32_TYPE_PRIMARY);
	nvrm_vram_wr32(alloc, NVRM_MEMALLOC_FLAGS_OFF,
		       NVRM_NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE |
			       NVRM_NVOS32_ALLOC_FLAGS_FORCE_MEM_GROWS_UP);
	nvrm_vram_wr64(alloc, NVRM_MEMALLOC_ALIGNMENT_OFF,
		       NVRM_NV_EVO_SURFACE_ALIGNMENT);
	nvrm_vram_wr32(alloc, NVRM_MEMALLOC_ATTR_OFF,
		       NVRM_NVOS32_ATTR_LOCATION_VIDMEM |
			       NVRM_NVOS32_ATTR_PHYSICALITY_CONTIGUOUS);
	nvrm_vram_wr32(alloc, NVRM_MEMALLOC_ATTR2_OFF,
		       NVRM_NVOS32_ATTR2_ISO_YES |
			       NVRM_NVOS32_ATTR2_GPU_CACHEABLE_NO);
	nvrm_vram_wr64(alloc, NVRM_MEMALLOC_SIZE_OFF, bytes);
}
