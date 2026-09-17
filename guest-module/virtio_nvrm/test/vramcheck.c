/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * vramcheck -- the display reserve's arithmetic, in userspace.
 *
 *   cc -O2 -Wall -Wextra -Werror -o vramcheck vramcheck.c
 *   ./vramcheck        # exits non-zero on any breach
 *
 * Same translation unit as the kernel module (nvrm_vram.c is #included), so
 * what is checked is what ships:
 *   - the scanout size against the buffers measured on 2026-09-17,
 *   - the reserve against the display path's measured peak at each size,
 *   - the FB_GET_INFO and NVOS32_FUNCTION_INFO rewrites: five sizes move
 *     together, Used does not, nothing else is touched, nothing wraps.
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

static void put_entry(u8 *list, u32 i, u32 index, u32 data)
{
	nvrm_vram_wr32(list, i * NVRM_FB_INFO_ENTRY_SIZE, index);
	nvrm_vram_wr32(list, i * NVRM_FB_INFO_ENTRY_SIZE + NVRM_FB_INFO_DATA_OFF, data);
}

static u32 data_at(const u8 *list, u32 i)
{
	return nvrm_vram_rd32(list, i * NVRM_FB_INFO_ENTRY_SIZE + NVRM_FB_INFO_DATA_OFF);
}

int main(void)
{
	u8 list[8 * NVRM_FB_INFO_ENTRY_SIZE];
	u8 nvos32[NVRM_NVOS32_SIZE];
	u32 i, r_kb;

	/* The formula against what was measured. */
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
	}
	CHECK("no display, no reserve", nvrm_display_reserve_auto_mib(0, 1080) == 0);
	CHECK("an absurd size is not a display", nvrm_display_reserve_auto_mib(1 << 30, 1 << 30) == 0);
	CHECK("the reserve grows with the display",
	      nvrm_display_reserve_auto_mib(2560, 1440) > nvrm_display_reserve_auto_mib(1920, 1080));

	/* FB_GET_INFO: the measured answer of a 2816 MiB guest, 240 MiB used. */
	memset(list, 0xee, sizeof(list));
	put_entry(list, 0, NVRM_FB_INFO_INDEX_HEAP_FREE, (2816 - 240) << 10);
	put_entry(list, 1, NVRM_FB_INFO_INDEX_TOTAL_RAM_SIZE, 2816 << 10);
	put_entry(list, 2, NVRM_FB_INFO_INDEX_HEAP_SIZE, 2816 << 10);
	put_entry(list, 3, NVRM_FB_INFO_INDEX_RAM_SIZE, 2816 << 10);
	put_entry(list, 4, NVRM_FB_INFO_INDEX_USABLE_RAM_SIZE, 2816 << 10);
	put_entry(list, 5, 0x1a, 0xf);			/* not a size */
	put_entry(list, 6, 0x12, 0x123456);		/* largest free region: RM's */
	r_kb = 64 << 10;
	CHECK("five sizes move", nvrm_fb_info_reserve(list, 7, r_kb) == 5);
	CHECK("free", data_at(list, 0) == (2816 - 240 - 64) << 10);
	CHECK("total", data_at(list, 1) == (2816 - 64) << 10);
	CHECK("heap", data_at(list, 2) == (2816 - 64) << 10);
	CHECK("ram", data_at(list, 3) == (2816 - 64) << 10);
	CHECK("usable", data_at(list, 4) == (2816 - 64) << 10);
	CHECK("used = heap - free does not move", data_at(list, 2) - data_at(list, 0) == 240 << 10);
	CHECK("other indices stay", data_at(list, 5) == 0xf && data_at(list, 6) == 0x123456);
	CHECK("past n nothing is read or written", nvrm_vram_rd32(list, 7 * NVRM_FB_INFO_ENTRY_SIZE) == 0xeeeeeeee);

	/* A free below the reserve clamps at 0 instead of wrapping to 4 TiB. */
	put_entry(list, 0, NVRM_FB_INFO_INDEX_HEAP_FREE, 10 << 10);
	nvrm_fb_info_reserve(list, 1, r_kb);
	CHECK("free clamps at 0", data_at(list, 0) == 0);
	/* Reserve 0 is off: byte for byte. */
	put_entry(list, 0, NVRM_FB_INFO_INDEX_HEAP_FREE, 1234);
	CHECK("reserve 0 moves nothing", nvrm_fb_info_reserve(list, 1, 0) == 0 && data_at(list, 0) == 1234);

	/* NVOS32_FUNCTION_INFO: bytes, and nothing but total and free. */
	memset(nvos32, 0xaa, sizeof(nvos32));
	nvrm_vram_wr64(nvos32, NVRM_NVOS32_TOTAL_OFF, 2816ull << 20);
	nvrm_vram_wr64(nvos32, NVRM_NVOS32_FREE_OFF, 30ull << 20);
	nvrm_heap_info_reserve(nvos32, 64ull << 20);
	CHECK("NVOS32 total", nvrm_vram_rd64(nvos32, NVRM_NVOS32_TOTAL_OFF) == (2816ull - 64) << 20);
	CHECK("NVOS32 free clamps at 0", nvrm_vram_rd64(nvos32, NVRM_NVOS32_FREE_OFF) == 0);
	for (i = 0; i < NVRM_NVOS32_SIZE; i++)
		if (i < NVRM_NVOS32_TOTAL_OFF || i >= NVRM_NVOS32_FREE_OFF + 8)
			CHECK("NVOS32: every other byte stays", nvos32[i] == 0xaa);

	if (failures)
		fprintf(stderr, "vramcheck: %d failure(s)\n", failures);
	return failures ? 1 : 0;
}
