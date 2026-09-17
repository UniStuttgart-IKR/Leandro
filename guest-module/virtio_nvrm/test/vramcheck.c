/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * vramcheck -- the display path's VRAM arithmetic, in userspace.
 *
 *   cc -O2 -Wall -Wextra -Werror -o vramcheck vramcheck.c
 *   ./vramcheck        # exits non-zero on any breach
 *
 * Same translation unit as the kernel module (nvrm_vram.c is #included):
 * the scanout size against the buffers measured on 2026-09-17, the formula
 * against the display path's measured peak at each size, and the balloon:
 * how it is cut, what gives way to which refusal, and which allocations may
 * take its room at all.
 */

#include <stdio.h>
#include <stdlib.h>

#include "../nvrm_vram.c"

static int failures;

#define CHECK(what, ok)							\
	do {								\
		if (!(ok)) {						\
			fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, what); \
			failures++;					\
		}							\
	} while (0)

/* Measured on .23, 2026-09-17, sizes as asked of RM: the largest scanout
 * buffer at each display size, and the display side's highest point above
 * idle over an X11 fullscreen start, a Wayland fullscreen client, cursor
 * changes and a Moonlight connect (the table in nvrm_vram.c). */
static const struct {
	u32 w, h;
	u64 scanout;
	u64 peak;
	u32 reserve_mib;
} measured[] = {
	{ 1920, 1080, 0x0870000,  35701376,  44 },
	{ 2560, 1440, 0x0f00000,  41630336,  76 },
	{ 3840, 2160, 0x1fe0000, 156151104, 161 },
};

/* An NV_MEMORY_ALLOCATION_PARAMS with the three fields eligibility reads. */
static void ask(u8 *a, u32 flags, u32 attr, u32 attr2)
{
	memset(a, 0, NVRM_MEMALLOC_SIZE);
	nvrm_vram_wr32(a, NVRM_MEMALLOC_FLAGS_OFF, flags);
	nvrm_vram_wr32(a, NVRM_MEMALLOC_ATTR_OFF, attr);
	nvrm_vram_wr32(a, NVRM_MEMALLOC_ATTR2_OFF, attr2);
}

int main(void)
{
	struct nvrm_balloon_shape b;
	u8 a[NVRM_MEMALLOC_SIZE];
	u32 i;

	for (i = 0; i < sizeof(measured) / sizeof(measured[0]); i++) {
		u32 mib = nvrm_display_reserve_auto_mib(measured[i].w, measured[i].h);

		printf("%ux%u: scanout %#llx, reserve %u MiB, measured peak %llu MiB\n",
		       measured[i].w, measured[i].h,
		       (unsigned long long)nvrm_scanout_bytes(measured[i].w, measured[i].h),
		       mib, (unsigned long long)(measured[i].peak >> 20));
		CHECK("scanout size is the measured one",
		      nvrm_scanout_bytes(measured[i].w, measured[i].h) == measured[i].scanout);
		CHECK("the reserve covers the measured peak",
		      ((u64)mib << 20) >= measured[i].peak);
		CHECK("the reserve is the documented one", mib == measured[i].reserve_mib);

		/* The balloon at the auto size: five whole scanout buffers, and
		 * a rest that holds mutter's four cursors. */
		nvrm_balloon_shape((u64)mib << 20, measured[i].scanout, &b);
		printf("  balloon: %u x %llu KiB + %llu KiB\n", b.full,
		       (unsigned long long)(b.chunk >> 10), (unsigned long long)(b.rest >> 10));
		CHECK("a chunk is one scanout buffer", b.chunk == measured[i].scanout);
		CHECK("five of them", b.full == 5);
		CHECK("the rest holds four cursors", b.rest >= NVRM_RESERVE_CURSOR_BYTES);
		CHECK("the rest is less than a buffer", b.rest < b.chunk);
		CHECK("the chunks add up to R", b.full * b.chunk + b.rest == (u64)mib << 20);
	}

	/* A fixed R too large for chunks of S: at most the cap, still R. */
	nvrm_balloon_shape(1024ull << 20, 0x870000, &b);
	CHECK("a large R is cut into at most the cap", b.full + (b.rest != 0) <= NVRM_BALLOON_MAX_CHUNKS);
	CHECK("a large R still adds up", b.full * b.chunk + b.rest == 1024ull << 20);
	CHECK("a large R's chunk is whole 64 KiB", !(b.chunk & 0xffff));
	nvrm_balloon_shape(4ull << 20, 0x870000, &b);
	CHECK("an R below one buffer is all rest", b.full == 0 && b.rest == 4ull << 20);
	nvrm_balloon_shape(44ull << 20, 0, &b);
	CHECK("no display size still cuts R", b.full * b.chunk + b.rest == 44ull << 20 && b.chunk);
	nvrm_balloon_shape(0, 0x870000, &b);
	CHECK("R 0 holds nothing", b.full == 0 && b.rest == 0);

	/* What gives way: the refusals of 2026-09-17 against the 1080p shape. */
	nvrm_balloon_shape(44ull << 20, 0x870000, &b);
	CHECK("a cursor takes the rest", nvrm_balloon_pick(5, 1, b.rest, 0x40000) == 0);
	CHECK("a scanout buffer takes a whole chunk", nvrm_balloon_pick(5, 1, b.rest, 0x870000) == 1);
	CHECK("a cursor takes a whole chunk once the rest is gone", nvrm_balloon_pick(5, 0, b.rest, 0x40000) == 1);
	CHECK("a scanout buffer takes the rest last", nvrm_balloon_pick(0, 1, b.rest, 0x870000) == 0);
	CHECK("an empty balloon gives nothing", nvrm_balloon_pick(0, 0, b.rest, 0x40000) == -1);

	/* Which allocations may take the room. The refused one, as the backend
	 * logged it (flags 0x102, attr 0x10020000), with the attr2 NVKMS sets
	 * for SCANOUT (ISO_YES, GPU_CACHEABLE_NO); the cursor is the same
	 * request in pitch format. */
	ask(a, 0x102, 0x10020000, 0x40008);
	CHECK("the refused scanout buffer", nvrm_balloon_eligible(NVRM_CLASS_MEMORY_LOCAL_USER, a));
	ask(a, 0x102, 0x10000000, 0x40008);
	CHECK("the cursor", nvrm_balloon_eligible(NVRM_CLASS_MEMORY_LOCAL_USER, a));
	/* Xwayland's NO_SCANOUT buffer as traced through NVKMS (flags 0x9002,
	 * attr 0x9821004, attr2 0x10000a): it has a system-memory fallback. */
	ask(a, 0x9002, 0x9821004, 0x10000a);
	CHECK("NO_SCANOUT is not a display buffer", !nvrm_balloon_eligible(NVRM_CLASS_MEMORY_LOCAL_USER, a));
	ask(a, 0x1102, 0x10020000, 0x40008);
	CHECK("NO_SCANOUT with ISO is still not", !nvrm_balloon_eligible(NVRM_CLASS_MEMORY_LOCAL_USER, a));
	ask(a, 0x102, 0x10020000, 0x8);
	CHECK("without ISO it is not", !nvrm_balloon_eligible(NVRM_CLASS_MEMORY_LOCAL_USER, a));
	ask(a, 0x102, 0x10020000 | (1u << 25), 0x40008);
	CHECK("system memory is not", !nvrm_balloon_eligible(NVRM_CLASS_MEMORY_LOCAL_USER, a));
	ask(a, 0x102 | NVRM_NVOS32_ALLOC_FLAGS_VIRTUAL, 0x10020000, 0x40008);
	CHECK("a virtual range is not", !nvrm_balloon_eligible(NVRM_CLASS_MEMORY_LOCAL_USER, a));
	ask(a, 0x102, 0x10020000, 0x40008);
	CHECK("another class is not", !nvrm_balloon_eligible(0x3e, a));

	/* A chunk looks exactly like what takes its room. */
	nvrm_balloon_chunk_params(a, 0x870000);
	CHECK("a chunk is a display buffer", nvrm_balloon_eligible(NVRM_CLASS_MEMORY_LOCAL_USER, a));
	CHECK("a chunk has the refused buffer's attr, in pitch format",
	      nvrm_vram_rd32(a, NVRM_MEMALLOC_ATTR_OFF) == 0x10000000);
	CHECK("a chunk has NVKMS's flags", nvrm_vram_rd32(a, NVRM_MEMALLOC_FLAGS_OFF) == 0x102);
	CHECK("a chunk has NVKMS's attr2", nvrm_vram_rd32(a, NVRM_MEMALLOC_ATTR2_OFF) == 0x40008);
	CHECK("a chunk is PRIMARY", nvrm_vram_rd32(a, NVRM_MEMALLOC_TYPE_OFF) == 8);
	/* standard_mem.c:78-82: 0, ~0 and RM's own 0xDEAF0000-0xDEAF0003 are
	 * refused with NV_ERR_INVALID_OWNER. */
	i = nvrm_vram_rd32(a, NVRM_MEMALLOC_OWNER_OFF);
	CHECK("a chunk has an owner RM accepts from a client",
	      i && i != 0xffffffffu && (i < 0xdeaf0000u || i > 0xdeaf0003u));
	CHECK("no display, no reserve", nvrm_display_reserve_auto_mib(0, 1080) == 0);
	CHECK("an absurd size is not a display", nvrm_display_reserve_auto_mib(1 << 30, 1 << 30) == 0);
	CHECK("the reserve grows with the display",
	      nvrm_display_reserve_auto_mib(2560, 1440) > nvrm_display_reserve_auto_mib(1920, 1080));

	if (failures)
		fprintf(stderr, "vramcheck: %d failure(s)\n", failures);
	return failures ? 1 : 0;
}
