/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * nvrm_edid.c -- the EDID this module invents for the virtual display.
 *
 * ONE translation unit for two worlds, pulled in via #include, exactly as
 * nvrm_tables.c is:
 *   - virtio_nvrm.c (kernel module): what NVA083_CTRL_CMD_VIRTUAL_DISPLAY_-
 *     GET_DEFAULT_EDID hands back.
 *   - test/edidcheck.c (userspace): dumps the same bytes so `edid-decode`
 *     can read them.
 *
 * Why it sits in its own file: an EDID is parsed by everything downstream --
 * NVKMS, X, the desktop -- and a broken one fails quietly, as a mode that is
 * simply not offered. This one had never been read by anything when it was
 * written, and two of its bytes were wrong. A parser that already knows the
 * spec is a better reviewer than another pair of eyes.
 *
 * Hence ONLY this in here: memset, memcpy and integer arithmetic. Whoever
 * prints or allocates is the includer.
 */

#ifdef __KERNEL__
# include <linux/kernel.h>
# include <linux/math64.h>
# include <linux/string.h>
# include <linux/types.h>
#else
# include <stdint.h>
# include <string.h>
typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef uint64_t u64;
# define div_u64(n, d) ((u64)(n) / (u64)(d))
# define min_t(type, a, b) ((type)(a) < (type)(b) ? (type)(a) : (type)(b))
#endif

#define NVRM_EDID_LEN 128

/* A CVT-reduced-blanking-ish timing for the requested size AND RATE. The
 * pixel clock is what NVKMS reads back as maxPixelClockKHz on this path
 * (nvkms-dpy.c:815), so it has to be at least the mode's own clock.
 *
 * The rate is a parameter because it used to be the constant 60 while
 * `vdisplay_vblank_hz` -- a SEPARATE module parameter -- drove the hrtimer
 * that delivers the vblank callbacks. Setting that to 120 gave a guest
 * whose display advertised 60 Hz and whose vblanks arrived at 120: the
 * compositor pacing itself off the mode would have been wrong by a factor
 * of two, and nothing in the path could have said so. One rate now feeds
 * both, so the two cannot disagree. */
struct nvrm_vtiming {
	u32 hactive, hblank, hsync_off, hsync_w;
	u32 vactive, vblank, vsync_off, vsync_w;
	u32 pclk_10khz;
};

/* Blanking is fixed (CVT-RB v1: VESA Coordinated Video Timings,
 * reduced blanking), so the totals are a pure function of the
 * size and both the clamp below and the timing need them. */
#define NVRM_HBLANK 160u
#define NVRM_VBLANK 46u
/* CVT reduced blanking v2 narrows the horizontal blanking to 80. Used ONLY
 * where the wide one does not fit, so every mode that worked before is
 * built from exactly the timing it was built from before.
 *
 * What it buys is not academic: 2560x1440 at 165 Hz needs 667 MHz with
 * 160 and 647 MHz with 80, and the ceiling of the DTD (Detailed Timing
 * Descriptor, the EDID's mode block) is 655.35 -- so this is
 * the difference between a monitor's advertised rate being reachable and
 * being clamped to 161. */
#define NVRM_HBLANK_RB2 80u

/* The DTD stores hactive/hblank and vactive/vblank in TWELVE bits each
 * (eight, plus four in a shared upper nibble), so 4095 is the largest
 * value that survives the trip. 7680 does not: 7680x4320 at 1 Hz
 * decoded as a 33 Hz display, because the width came back truncated and
 * every derived figure followed it. Far past NVIDIA's own displayless
 * limit (2560x1600) and therefore never reachable in practice -- which is
 * exactly why it must not be the thing that decides. */
#define NVRM_DTD_MAX_ACTIVE 4095u

/* The rate ceiling for one choice of horizontal blanking. */
static u32 nvrm_hz_ceiling(u32 w, u32 h, u32 hblank)
{
	u64 htotal = (u64)w + hblank;
	u64 vtotal = (u64)h + NVRM_VBLANK;
	u64 by_clock, by_hfreq;

	/* div_u64 takes a 32-bit divisor, which both terms fit. */
	/* pclk_10khz = htotal * vtotal * hz / 10000 must fit in 16 bits. */
	by_clock = div_u64(65535ULL * 10000ULL, (u32)(htotal * vtotal));
	/* hkhz = vtotal * hz / 1000 must fit in one byte of range limits. */
	by_hfreq = div_u64(255ULL * 1000ULL, (u32)vtotal);
	if (by_clock > by_hfreq)
		by_clock = by_hfreq;
	/* And the range limits' vertical maximum is one byte too. */
	if (by_clock > 254)
		by_clock = 254;
	return by_clock < 1 ? 1 : (u32)by_clock;
}

/*
 * The size and rate this EDID can actually EXPRESS, which is not always the
 * one that was asked for.
 *
 * Not a theoretical bound -- the fields are small and they wrap in
 * silence. The DTD carries the pixel clock in TWO bytes of 10 kHz, so
 * 655.35 MHz is the ceiling, and 2560x1440 at 165 Hz needs 667 MHz: the
 * block then decoded as a 2 Hz display, with no error anywhere. That is
 * inside NVIDIA's own displayless limit (2560x1600), so it is an ordinary
 * setting, not an exotic one. The range-limits horizontal maximum is a
 * single byte of kHz and saturates at 255, which 1920x1080 at 240 Hz
 * (270 kHz) walks past the same way.
 *
 * Clamping rather than refusing: a guest that asks for more than the wire
 * format holds still gets a working display, at the fastest rate that is
 * honest. The caller logs it, and the vblank timer takes the SAME number,
 * so the mode and the callbacks cannot drift apart.
 */
static void nvrm_edid_effective(u32 w, u32 h, u32 hz, u32 *ew, u32 *eh, u32 *ehz,
			 u32 *ehblank)
{
	u32 hblank = NVRM_HBLANK, cap;

	if (w < 1)
		w = 1;
	if (h < 1)
		h = 1;
	if (w > NVRM_DTD_MAX_ACTIVE - NVRM_HBLANK)
		w = NVRM_DTD_MAX_ACTIVE - NVRM_HBLANK;
	if (h > NVRM_DTD_MAX_ACTIVE - NVRM_VBLANK)
		h = NVRM_DTD_MAX_ACTIVE - NVRM_VBLANK;

	if (hz < 1)
		hz = 1;

	/* Wide blanking first, because it is what every measured mode has
	 * used; narrow only where that is the thing standing in the way. */
	cap = nvrm_hz_ceiling(w, h, hblank);
	if (hz > cap) {
		u32 cap2 = nvrm_hz_ceiling(w, h, NVRM_HBLANK_RB2);

		if (cap2 > cap) {
			hblank = NVRM_HBLANK_RB2;
			cap = cap2;
		}
	}
	if (hz > cap)
		hz = cap;

	*ew = w;
	*eh = h;
	*ehz = hz;
	*ehblank = hblank;
}

static void nvrm_vtiming_for(u32 w, u32 h, u32 hz, u32 hblank,
			     struct nvrm_vtiming *t)
{
	/* CVT reduced blanking v1: fixed 160 px horizontal blanking and a
	 * fixed 46-line vertical blanking, which is what every 1080p60 RB
	 * panel reports. Derived rather than tabulated so a different size
	 * still yields a self-consistent EDID. */
	t->hactive   = w;
	t->hblank    = hblank;
	t->hsync_off = 48;
	t->hsync_w   = 32;
	t->vactive   = h;
	t->vblank    = NVRM_VBLANK;
	t->vsync_off = 3;
	t->vsync_w   = 5;
	/* clock = total pixels * total lines * rate, in 10 kHz units. */
	t->pclk_10khz = (u32)div_u64((u64)(w + t->hblank) * (h + t->vblank) *
				     (u64)hz, 10000ULL);
}

static void nvrm_edid_dtd(u8 *d, const struct nvrm_vtiming *t)
{
	/* Detailed Timing Descriptor, EDID 1.4 section 3.10.2. */
	d[0]  = t->pclk_10khz & 0xff;
	d[1]  = (t->pclk_10khz >> 8) & 0xff;
	d[2]  = t->hactive & 0xff;
	d[3]  = t->hblank & 0xff;
	d[4]  = ((t->hactive >> 8) << 4) | ((t->hblank >> 8) & 0xf);
	d[5]  = t->vactive & 0xff;
	d[6]  = t->vblank & 0xff;
	d[7]  = ((t->vactive >> 8) << 4) | ((t->vblank >> 8) & 0xf);
	d[8]  = t->hsync_off & 0xff;
	d[9]  = t->hsync_w & 0xff;
	d[10] = ((t->vsync_off & 0xf) << 4) | (t->vsync_w & 0xf);
	d[11] = (((t->hsync_off >> 8) & 0x3) << 6) |
		(((t->hsync_w >> 8) & 0x3) << 4) |
		(((t->vsync_off >> 4) & 0x3) << 2) |
		((t->vsync_w >> 4) & 0x3);
	/* Physical size: a 16:9 panel of 531 x 299 mm, i.e. 24 inches. It is
	 * only used for DPI, and a zero here makes some clients compute
	 * nonsense. */
	d[12] = 531 & 0xff;
	d[13] = 299 & 0xff;
	d[14] = ((531 >> 8) << 4) | ((299 >> 8) & 0xf);
	d[15] = 0;	/* h border */
	d[16] = 0;	/* v border */
	/* Digital separate sync, both polarities positive. */
	d[17] = 0x1e;
}

/*
 * A complete EDID 1.4 base block for a display this module invents.
 *
 * Every number here is a statement about hardware that does not exist.
 * The checksum is COMPUTED rather than typed, because a hand-written one is
 * the classic way an EDID silently fails to parse.
 */
static void nvrm_build_edid(u8 *e, u32 w, u32 h, u32 hz)
{
	struct nvrm_vtiming t;
	static const char name[] = "Leandro vDisp";
	u32 sum = 0;
	u8 *d;
	int i;

	/* The clamp lives here as well as at the caller: this is the only
	 * function that knows the wire format, and a block that contradicts
	 * itself must not be constructible at all. */
	{
		u32 hblank;

		nvrm_edid_effective(w, h, hz, &w, &h, &hz, &hblank);
		nvrm_vtiming_for(w, h, hz, hblank, &t);
	}
	memset(e, 0, NVRM_EDID_LEN);

	/* Header. */
	e[0] = 0x00;
	memset(e + 1, 0xff, 6);
	e[7] = 0x00;
	/* Manufacturer "LEA", five bits per letter, big endian. */
	{
		u16 id = ((12u & 0x1f) << 10) | ((5u & 0x1f) << 5) | (1u & 0x1f);

		e[8] = id >> 8;
		e[9] = id & 0xff;
	}
	e[10] = 0x01;	/* product code */
	e[11] = 0x00;
	/* serial 12..15 stays 0, week 16 = 1, year 17 = 2026 - 1990 */
	e[16] = 1;
	e[17] = 36;
	e[18] = 1;	/* EDID 1.4 */
	e[19] = 4;
	/* Digital input, 8 bpc, DisplayPort. Bits 6:4 are the colour bit
	 * depth and they are NOT the depth itself: 1 = 6 bpc, 2 = 8 bpc
	 * (EDID 1.4 table 3.14). This said 8 bpc and encoded 6. */
	e[20] = 0x80 | (0x2 << 4) | 0x5;
	e[21] = 53;	/* max h image size, cm */
	e[22] = 30;	/* max v image size, cm */
	e[23] = 120;	/* gamma 2.2 */
	/* Feature support. Bit 1: the preferred timing mode is the first DTD.
	 * Bit 2: sRGB is the default colour space -- it has to be SIGNALLED,
	 * not merely implied by the chromaticities below, or edid-decode
	 * calls the block non-conformant for saying sRGB in one place and
	 * nothing in the other. Bit 0 (continuous frequency) stays clear:
	 * this display offers a mode list, not a range. */
	e[24] = 0x02 | 0x04;
	/* Chromaticity, sRGB-ish. */
	e[25] = 0xee; e[26] = 0x91; e[27] = 0xa3; e[28] = 0x54; e[29] = 0x4c;
	e[30] = 0x99; e[31] = 0x26; e[32] = 0x0f; e[33] = 0x50; e[34] = 0x54;
	/* Established timings: 640x480@60 only, so the list is never empty. */
	e[35] = 0x20; e[36] = 0x00; e[37] = 0x00;
	/* Standard timings: all unused. */
	for (i = 38; i < 54; i += 2) {
		e[i] = 0x01;
		e[i + 1] = 0x01;
	}

	/* Descriptor 1: the mode itself. */
	nvrm_edid_dtd(e + 54, &t);

	/* Descriptor 2: range limits. Every bound has to CONTAIN the one mode
	 * this EDID offers, so the horizontal maximum is derived rather than
	 * typed: a fixed 160 kHz is right up to about 4K and a contradiction
	 * above it -- 7680x4320 at 60 Hz needs 262 kHz, and the block would
	 * then advertise a range that excludes its own preferred timing.
	 * Nothing catches that today: edid-decode reads the DTD and the
	 * range limits without comparing them.
	 *
	 * The field is one byte of kHz, so it saturates at 255 kHz and
	 * anything past ~7400x4160 is under-declared again. EDID has an
	 * offset mechanism for that (descriptor byte 4); it is not built,
	 * because GRID_DISPLAYLESS_LINUX_MAX_HRES is 2560 and the host would
	 * refuse the mode long before. */
	{
		u32 hkhz = (u32)div_u64((u64)t.pclk_10khz * 10ULL,
					(u64)(t.hactive + t.hblank));
		/* The VERTICAL bounds are derived for exactly the reason above,
		 * and they were not always: 50 and 75 stood here as constants
		 * while the rate was the constant 60, and both stayed correct
		 * together. The moment the rate became a parameter they became
		 * the very contradiction the comment warns about -- a 120 Hz
		 * DTD inside a block advertising 50-75 Hz. Kept wide when the
		 * rate is ordinary, so the descriptor still reads like a
		 * monitor's rather than like one mode with a fence around it. */
		u32 vmin = hz > 50 ? 50 : (hz > 1 ? hz - 1 : 1);
		u32 vmax = hz < 75 ? 75 : hz + 1;

		d = e + 72;
		d[3] = 0xfd;
		d[5] = (u8)min_t(u32, 255, vmin);	/* min vertical Hz */
		d[6] = (u8)min_t(u32, 255, vmax);	/* max vertical Hz */
		/* Derived for the same reason as the vertical pair, and it was
		 * a constant 30 for the same reason too: at 24 Hz a 640x480
		 * mode runs at 12 kHz and fell below its own floor. */
		d[7] = (u8)(hkhz < 30 ? (hkhz > 1 ? hkhz - 1 : 1) : 30);
		d[8] = (u8)min_t(u32, 255, hkhz > 159 ? hkhz + 1 : 160);
	}
	/* Max pixel clock, and the field counts in 10 MHz while pclk_10khz
	 * counts in 10 kHz -- so the divisor is 1000, not 100. With 100 this
	 * declared 1410 MHz for a 140.5 MHz mode: too generous to break
	 * anything, and enough for edid-decode to call the block wrong. */
	d[9] = (u8)min_t(u32, 255, t.pclk_10khz / 1000 + 1);
	/* Video timing support: 0x01 = range limits only, no timing formula.
	 * NOT 0x00, which means "default GTF" -- deprecated in EDID 1.4, and
	 * a claim this display cannot honour anyway: GTF means the sink will
	 * accept any timing the formula produces, which requires the
	 * continuous-frequency bit in e[24] that is deliberately clear. */
	d[10] = 0x01;
	d[11] = 0x0a;
	memset(d + 12, 0x20, 6);

	/* Descriptor 3: the name, so the guest's tools show what this is. */
	d = e + 90;
	d[3] = 0xfc;
	memcpy(d + 5, name, min_t(size_t, sizeof(name) - 1, 13));
	if (sizeof(name) - 1 < 13) {
		d[5 + sizeof(name) - 1] = 0x0a;
		memset(d + 5 + sizeof(name), 0x20, 13 - (sizeof(name) - 1) - 1);
	}

	/* Descriptor 4: dummy. */
	e[108 + 3] = 0x10;

	e[126] = 0;	/* no extension blocks */
	for (i = 0; i < NVRM_EDID_LEN - 1; i++)
		sum += e[i];
	e[127] = (u8)(256 - (sum & 0xff));
}
