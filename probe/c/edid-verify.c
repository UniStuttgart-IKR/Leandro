// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * edid-verify -- read an EDID block with a parser that shares no code with
 * the thing that wrote it, and say whether it describes the mode we asked
 * for.
 *
 *   cc -O2 -Wall -Wextra -Werror -o edid-verify edid-verify.c
 *   ./edid-verify <file> [<width> <height>]
 *
 * Exit 0 = the block is well formed and (when a size was given) its
 * preferred timing is that size. 1 = it parsed but does not match, or a
 * structural check failed. 2 = usage or I/O.
 *
 * WHY IT EXISTS, and it is the whole point: the display gate's `edid` stage
 * compares the guest's connector bytes against
 * guest-module/virtio_nvrm/test/edidcheck.c, which is the module's OWN
 * builder (nvrm_edid.c) compiled for userspace. That comparison proves the
 * bytes ARRIVED INTACT and nothing more -- a change in nvrm_edid.c moves
 * both sides and the stage stays green. It was measured: mutating e[10] and
 * watching it pass.
 *
 * So this file re-implements the reading half from EDID 1.4 alone. Nothing
 * here is #included from the module. When the two disagree, one of them is
 * wrong -- which is the property the gate needs and could not have before.
 *
 * NOT a conformance checker. `edid-decode --check` is that, it knows the
 * spec far better than 150 lines can, and test.sh check already pipes the block
 * through it. This answers a narrower question that edid-decode cannot: is
 * the PREFERRED TIMING the size the module was asked for? edid-decode
 * happily prints a perfectly conformant block for the wrong resolution.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define BLOCK 128
#define MAX_BLOCKS 8

static const unsigned char MAGIC[8] = { 0x00, 0xff, 0xff, 0xff,
					0xff, 0xff, 0xff, 0x00 };

/* Every byte of a 128-byte block sums to 0 mod 256. A hand-written checksum
 * is the classic way an EDID silently fails to parse, so it is the first
 * thing worth asking. */
static int block_sum_ok(const unsigned char *b)
{
	unsigned sum = 0;
	int i;

	for (i = 0; i < BLOCK; i++)
		sum += b[i];
	return (sum & 0xff) == 0;
}

/* The three letters of the manufacturer id, five bits each, big endian. */
static void mfg_id(const unsigned char *e, char out[4])
{
	unsigned id = ((unsigned)e[8] << 8) | e[9];

	out[0] = (char)('A' + ((id >> 10) & 0x1f) - 1);
	out[1] = (char)('A' + ((id >> 5) & 0x1f) - 1);
	out[2] = (char)('A' + (id & 0x1f) - 1);
	out[3] = '\0';
}

/*
 * Descriptor 1 (offset 54) is the preferred timing when bit 1 of the feature
 * byte says so, which is what this block claims. The active pixel counts are
 * split: eight low bits in one byte, four high bits in the upper nibble of a
 * shared byte (EDID 1.4 section 3.10.2).
 */
static void dtd_size(const unsigned char *d, unsigned *w, unsigned *h,
		     unsigned *hz)
{
	unsigned pclk = d[0] | ((unsigned)d[1] << 8);	/* in 10 kHz */
	unsigned hblank = d[3] | (((unsigned)d[4] & 0x0f) << 8);
	unsigned vblank = d[6] | (((unsigned)d[7] & 0x0f) << 8);
	unsigned htotal, vtotal;

	*w = d[2] | (((unsigned)d[4] & 0xf0) << 4);
	*h = d[5] | (((unsigned)d[7] & 0xf0) << 4);

	htotal = *w + hblank;
	vtotal = *h + vblank;
	/* Recomputed, never read: the rate is not stored anywhere in a DTD.
	 * Rounded to nearest, because the clock is quantised to 10 kHz and a
	 * truncating divide reports 59 for a 60 Hz mode. */
	*hz = (htotal && vtotal)
	      ? (unsigned)(((unsigned long long)pclk * 10000ULL
			    + (unsigned long long)htotal * vtotal / 2)
			   / ((unsigned long long)htotal * vtotal))
	      : 0;
}

/* The monitor-name descriptor (tag 0xfc), terminated by 0x0a and padded with
 * spaces. Empty when the block carries none. */
static void monitor_name(const unsigned char *e, char *out, size_t len)
{
	int off, i;

	out[0] = '\0';
	for (off = 54; off + 18 <= BLOCK; off += 18) {
		const unsigned char *d = e + off;
		size_t n = 0;

		/* A descriptor is a DISPLAY descriptor only when its first
		 * two bytes (the pixel clock of a timing descriptor) are
		 * zero. Skipping that test reads a DTD's timing numbers as a
		 * tag byte and occasionally finds 0xfc in them. */
		if (d[0] || d[1] || d[3] != 0xfc)
			continue;
		for (i = 5; i < 18 && n + 1 < len; i++) {
			if (d[i] == 0x0a)
				break;
			out[n++] = (char)d[i];
		}
		while (n > 0 && out[n - 1] == ' ')
			n--;
		out[n] = '\0';
		return;
	}
}

static int usage(const char *me)
{
	fprintf(stderr, "usage: %s <edid-file> [<width> <height>]\n", me);
	return 2;
}

int main(int argc, char **argv)
{
	unsigned char buf[BLOCK * MAX_BLOCKS];
	char mfg[4], name[32];
	unsigned w = 0, h = 0, hz = 0, want_w = 0, want_h = 0;
	int header_ok, sum_ok = 1, ext_ok, blocks, i, bad = 0;
	size_t got;
	FILE *f;

	if (argc != 2 && argc != 4)
		return usage(argv[0]);
	if (argc == 4) {
		want_w = (unsigned)strtoul(argv[2], NULL, 10);
		want_h = (unsigned)strtoul(argv[3], NULL, 10);
		if (!want_w || !want_h)
			return usage(argv[0]);
	}

	f = fopen(argv[1], "rb");
	if (!f) {
		perror(argv[1]);
		return 2;
	}
	got = fread(buf, 1, sizeof(buf), f);
	fclose(f);

	if (got < BLOCK || got % BLOCK) {
		fprintf(stderr, "%s: %zu bytes -- not a whole number of "
			"128-byte EDID blocks\n", argv[1], got);
		return 2;
	}
	blocks = (int)(got / BLOCK);

	header_ok = memcmp(buf, MAGIC, sizeof(MAGIC)) == 0;
	for (i = 0; i < blocks; i++)
		if (!block_sum_ok(buf + i * BLOCK))
			sum_ok = 0;
	/* Byte 126 counts the EXTENSION blocks, so the file should be one
	 * more than that. A block that promises extensions it does not carry
	 * makes a sink read past the end of what it was given. */
	ext_ok = (buf[126] + 1) == blocks;

	mfg_id(buf, mfg);
	monitor_name(buf, name, sizeof(name));
	dtd_size(buf + 54, &w, &h, &hz);

	if (!header_ok || !sum_ok || !ext_ok || !w || !h)
		bad = 1;
	if (want_w && (w != want_w || h != want_h))
		bad = 1;

	/* Exactly one line, whatever happened: a caller greps it into facts,
	 * and a tool that prints two lines on failure and one on success is a
	 * tool every caller parses twice. */
	printf("edid-verify %s bytes=%zu blocks=%d header=%s checksum=%s "
	       "extensions=%s mfg=%s monitor=\"%s\" preferred=%ux%u@%u",
	       argv[1], got, blocks, header_ok ? "ok" : "BAD",
	       sum_ok ? "ok" : "BAD", ext_ok ? "ok" : "BAD",
	       mfg, name, w, h, hz);
	if (want_w)
		printf(" want=%ux%u", want_w, want_h);
	printf(" %s\n", bad ? "MISMATCH" : "ok");

	return bad;
}
