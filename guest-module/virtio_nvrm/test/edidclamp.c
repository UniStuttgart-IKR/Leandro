/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * edidclamp -- the clamping arithmetic of nvrm_edid_effective(), in userspace.
 *
 *   cc -O2 -Wall -Wextra -Werror -o edidclamp edidclamp.c
 *   ./edidclamp        # runs a fixed matrix, exits non-zero on any breach
 *
 * edidcheck.c proves that the EDID a size PRODUCES is self-consistent
 * (edid-decode reads it, and selfcheck() cross-reads the DTD against the
 * range limits). This binary proves the step BEFORE that: nvrm_edid_
 * effective() clamps a requested (width, height, rate) down to something the
 * EDID's fixed-width fields can express, and pacing the virtual display's
 * vblank hrtimer reads the SAME clamped rate (vblank_period() in
 * virtio_nvrm.c calls this function). If the clamp let a rate through that
 * the DTD's 16-bit pixel clock or the range limits' one-byte maxima could
 * not hold, the block would decode as a wildly wrong refresh -- 165 Hz once
 * decoded as 2 Hz -- with no error anywhere, and the compositor would pace
 * itself off a rate the panel does not really have.
 *
 * Same translation unit as the kernel module (nvrm_edid.c is #included), so
 * the arithmetic under test is exactly the arithmetic that ships. No kernel,
 * no VM, no edid-decode -- just the invariants the clamp must keep.
 */

#include <stdio.h>
#include <stdlib.h>

#include "../nvrm_edid.c"

static int failures;

static void check(const char *what, int ok, u32 w, u32 h, u32 hz)
{
	if (!ok) {
		fprintf(stderr, "FAIL %ux%u@%u: %s\n", w, h, hz, what);
		failures++;
	}
}

/* One requested mode through the clamp, with every invariant the fixed-size
 * EDID fields impose on the result. */
static void one(u32 w, u32 h, u32 hz)
{
	u32 ew, eh, ehz, ehblank;

	nvrm_edid_effective(w, h, hz, &ew, &eh, &ehz, &ehblank);

	/* Never zero: a mode with a zero dimension or rate is not a mode, and
	 * the timing/pixel-clock arithmetic downstream would divide by it. */
	check("effective width is zero", ew >= 1, w, h, hz);
	check("effective height is zero", eh >= 1, w, h, hz);
	check("effective rate is zero", ehz >= 1, w, h, hz);

	/* The DTD stores htotal/vtotal in 12 bits (8 + a shared nibble): the
	 * active pixels plus blanking must stay under 4096, or the value wraps
	 * on the way into the block. NVRM_DTD_MAX_ACTIVE is that bound. */
	check("htotal past the 12-bit DTD field",
	      (unsigned long long)ew + ehblank <= NVRM_DTD_MAX_ACTIVE, w, h, hz);
	check("vtotal past the 12-bit DTD field",
	      (unsigned long long)eh + NVRM_VBLANK <= NVRM_DTD_MAX_ACTIVE, w, h, hz);

	/* The clamp must not RAISE what was asked: a request is a ceiling, a
	 * larger answer would be inventing capability. */
	check("clamp raised the width", ew <= (w < 1 ? 1 : w), w, h, hz);
	check("clamp raised the height", eh <= (h < 1 ? 1 : h), w, h, hz);
	check("clamp raised the rate", ehz <= (hz < 1 ? 1 : hz), w, h, hz);

	/* The horizontal blanking is one of the two CVT-RB values, nothing
	 * else -- the timing builder keys off it. */
	check("blanking is neither RB v1 nor v2",
	      ehblank == NVRM_HBLANK || ehblank == NVRM_HBLANK_RB2, w, h, hz);

	/* The rate the DTD will carry, recomputed from the clamped mode the
	 * way nvrm_vtiming_for() does (pixel clock in 10 kHz units), must fit
	 * the DTD's 16-bit pixel-clock field -- that is the field a too-high
	 * rate silently wraps in. (The ceiling is an integer floor and caps at
	 * the range limits' one-byte maximum, so it may sit a hertz or two
	 * below the true limit; that headroom is deliberate, not a breach.) */
	{
		u64 htotal = (u64)ew + ehblank;
		u64 vtotal = (u64)eh + NVRM_VBLANK;
		u64 pclk_10khz = div_u64(htotal * vtotal * (u64)ehz, 10000ULL);

		check("pixel clock past the 16-bit DTD field", pclk_10khz <= 0xffff, w, h, hz);
	}
}

int main(void)
{
	/* A matrix that spans the ordinary and the deliberately absurd: the
	 * measured desktop sizes, NVIDIA's displayless maximum (2560x1600),
	 * high-rate small modes (240 Hz 1080p walks past the one-byte
	 * horizontal maximum), 4K and 8K (past the 12-bit active field), and a
	 * request of zero in each field. Every one must come back expressible. */
	const u32 sizes[][2] = {
		{ 640, 480 }, { 800, 600 }, { 1280, 720 }, { 1920, 1080 },
		{ 2560, 1440 }, { 2560, 1600 }, { 3840, 2160 }, { 7680, 4320 },
		{ 1, 1 }, { 0, 0 }, { 100000, 100000 },
	};
	const u32 rates[] = { 0, 1, 30, 60, 120, 144, 165, 240, 1000, 100000 };
	size_t i, j;

	for (i = 0; i < sizeof(sizes) / sizeof(sizes[0]); i++)
		for (j = 0; j < sizeof(rates) / sizeof(rates[0]); j++)
			one(sizes[i][0], sizes[i][1], rates[j]);

	/* Build one block end to end as well: it keeps nvrm_build_edid()
	 * (the rest of this translation unit) exercised, and a header magic or
	 * checksum that broke would show here without edid-decode. EDID 1.4:
	 * the eight-byte magic 00 FF..FF 00, and byte 127 is the checksum that
	 * makes the 128 bytes sum to 0 mod 256. */
	{
		u8 e[128];
		unsigned int sum = 0;
		int k;
		static const u8 magic[8] = { 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00 };

		nvrm_build_edid(e, 1920, 1080, 60);
		for (k = 0; k < 8; k++)
			check("EDID header magic", e[k] == magic[k], 1920, 1080, 60);
		for (k = 0; k < 128; k++)
			sum = (sum + e[k]) & 0xff;
		check("EDID checksum does not sum to zero", sum == 0, 1920, 1080, 60);
	}

	if (failures) {
		fprintf(stderr, "%d clamp invariant(s) breached\n", failures);
		return 1;
	}
	printf("edidclamp: %zu modes x %zu rates, every result expressible\n",
	       sizeof(sizes) / sizeof(sizes[0]), sizeof(rates) / sizeof(rates[0]));
	return 0;
}
