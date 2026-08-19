/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * edidcheck -- write the module's EDID to a file so a real parser reads it.
 *
 *   cc -O2 -Wall -Wextra -Werror -o edidcheck edidcheck.c
 *   ./edidcheck <width> <height> [hz] <out.bin>
 *   edid-decode out.bin
 *
 * Runs EXACTLY the kernel module's builder (nvrm_edid.c, the same translation
 * unit). test.sh check pipes the result through edid-decode and fails on any
 * complaint, which is the only reviewer of this file that knows the spec.
 */

#include <stdio.h>
#include <stdlib.h>

#include "../nvrm_edid.c"

/* Does this EDID contradict itself?
 *
 * edid-decode reads the DTD and the range limits and never compares them,
 * which is stated in nvrm_edid.c and was true twice: the horizontal bound
 * was a constant 160 kHz that a 4K mode walks past, and the vertical bounds
 * were a constant 50-75 that any rate but 60 walks past. Both were correct
 * for as long as the thing they bounded was constant, and both became
 * wrong the moment it was not. So the comparison lives here, where a
 * resolution/rate matrix can run it.
 *
 * Returns 0 when the block is self-consistent, 1 when it is not.
 */
static int selfcheck(const unsigned char *e, u32 w, u32 h, u32 hz)
{
	const unsigned char *dtd = e + 54, *rl = e + 72;
	u32 pclk_10khz = dtd[0] | ((u32)dtd[1] << 8);
	u32 htotal = (dtd[2] | (((u32)dtd[4] & 0xf0) << 4)) +
		     (dtd[3] | (((u32)dtd[4] & 0x0f) << 8));
	u32 vtotal = (dtd[5] | (((u32)dtd[7] & 0xf0) << 4)) +
		     (dtd[6] | (((u32)dtd[7] & 0x0f) << 8));
	u32 vmin = rl[5], vmax = rl[6], hmin = rl[7], hmax = rl[8];
	u32 maxclk_10mhz = rl[9];
	u32 hkhz, vhz, sum = 0;
	int bad = 0, i;

	if (!htotal || !vtotal) {
		fprintf(stderr, "FAIL %ux%u@%u: DTD totals are zero\n", w, h, hz);
		return 1;
	}
	hkhz = (u32)(((unsigned long long)pclk_10khz * 10ULL) / htotal);
	vhz  = (u32)(((unsigned long long)pclk_10khz * 10000ULL) /
		     ((unsigned long long)htotal * vtotal));

	/* The three containment rules, each the one a real sink applies. */
	if (vhz + 1 < vmin || vhz > vmax) {
		fprintf(stderr, "FAIL %ux%u@%u: DTD is %u Hz, range limits say %u-%u Hz\n",
			w, h, hz, vhz, vmin, vmax);
		bad = 1;
	}
	if (hkhz + 1 < hmin || hkhz > hmax) {
		fprintf(stderr, "FAIL %ux%u@%u: DTD is %u kHz, range limits say %u-%u kHz\n",
			w, h, hz, hkhz, hmin, hmax);
		bad = 1;
	}
	if (pclk_10khz > maxclk_10mhz * 1000u) {
		fprintf(stderr, "FAIL %ux%u@%u: DTD clock %u.%02u MHz over the declared %u MHz\n",
			w, h, hz, pclk_10khz / 100, pclk_10khz % 100, maxclk_10mhz * 10);
		bad = 1;
	}
	/* And the mode really is the one that will be USED -- which is the
	 * request clamped to what the wire format holds, not the request.
	 * Asking the same function the builder asks is the point: if the two
	 * ever disagree, the vblank timer is pacing a mode nobody displays. */
	{
		u32 ew, eh, ehz, ehb;

		/* The pixel clock is stored in steps of 10 kHz, so the rate a
		 * sink recomputes is quantised by 10000/(htotal*vtotal) Hz.
		 * At 1920x1080 that is under 1 Hz and invisible; at a 1x1
		 * display it is two, and tightening the check there would
		 * only be measuring the format's granularity. */
		u32 tol = 1 + (u32)((10000u + htotal * vtotal - 1) / (htotal * vtotal));

		nvrm_edid_effective(w, h, hz, &ew, &eh, &ehz, &ehb);
		if (vhz + tol < ehz || vhz > ehz + tol) {
			fprintf(stderr, "FAIL %ux%u@%u: effective %ux%u@%u, DTD came out at %u Hz\n",
				w, h, hz, ew, eh, ehz, vhz);
			bad = 1;
		}
	}
	for (i = 0; i < NVRM_EDID_LEN; i++)
		sum += e[i];
	if (sum % 256u) {
		fprintf(stderr, "FAIL %ux%u@%u: checksum %u\n", w, h, hz, sum % 256u);
		bad = 1;
	}
	return bad;
}

int main(int argc, char **argv)
{
	unsigned char edid[NVRM_EDID_LEN];
	unsigned long w, h, hz = 60;
	const char *out;
	FILE *f;

	/* The rate is optional so every existing caller keeps working; it
	 * defaults to the module's own vdisplay_vblank_hz default. */
	if (argc != 4 && argc != 5) {
		fprintf(stderr, "usage: %s <width> <height> [hz] <out.bin>\n", argv[0]);
		return 2;
	}
	w = strtoul(argv[1], NULL, 10);
	h = strtoul(argv[2], NULL, 10);
	if (argc == 5)
		hz = strtoul(argv[3], NULL, 10);
	out = argv[argc - 1];

	nvrm_build_edid(edid, (u32)w, (u32)h, (u32)hz);

	f = fopen(out, "wb");
	if (!f) {
		perror(out);
		return 2;
	}
	if (fwrite(edid, 1, sizeof(edid), f) != sizeof(edid)) {
		fprintf(stderr, "ERROR: short write to %s\n", out);
		fclose(f);
		return 2;
	}
	fclose(f);
	return selfcheck(edid, (u32)w, (u32)h, (u32)hz);
}
