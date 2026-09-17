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
 * the scanout size against the buffers measured on 2026-09-17, and the
 * formula against the display path's measured peak at each size.
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

int main(void)
{
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
	}
	CHECK("no display, no reserve", nvrm_display_reserve_auto_mib(0, 1080) == 0);
	CHECK("an absurd size is not a display", nvrm_display_reserve_auto_mib(1 << 30, 1 << 30) == 0);
	CHECK("the reserve grows with the display",
	      nvrm_display_reserve_auto_mib(2560, 1440) > nvrm_display_reserve_auto_mib(1920, 1080));

	if (failures)
		fprintf(stderr, "vramcheck: %d failure(s)\n", failures);
	return failures ? 1 : 0;
}
