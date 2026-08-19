/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * tabreject -- the C interpreter's REFUSALS, in userspace.
 *
 *   cc -O2 -Wall -Wextra -Werror -o tabreject tabreject.c
 *   ./tabreject <stream.bin>
 *
 * tabcheck proves that a good stream is read the way the Rust writer meant
 * it. This binary proves the other half: that a stream which is NOT good is
 * refused by `nvrm_tables_parse` (nvrm_tables.c, the very code the kernel
 * module runs) rather than believed. It takes the real stream, damages one
 * copy per case -- shorter than a header, wrong magic, wrong format
 * version, a total_len that lies, counts that do not add up to the length,
 * a bad checksum, more nested slots than the wire carries -- and expects
 * -EPROTO with a reason for each, and 0 for the untouched original. It also
 * asks the three lookups for keys that do not exist and expects NULL.
 *
 * Why this exists: those refusals guard the guest against a host it did not
 * expect, and until 2026-08-18 nothing had ever driven a single one of them.
 * test.sh check runs it right after tabcheck (step c-interpreter).
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../nvrm_tables.c"

static int failures;

/* Parse `buf`/`len`, expect `want` (0 or -EPROTO). */
static void expect(const char *name, unsigned char *buf, size_t len, int want)
{
	struct nvrm_tables t = { 0 };
	const char *why = NULL;
	int got;

	t.blob = buf;
	t.len = len;
	got = nvrm_tables_parse(&t, &why);
	if (got != want) {
		fprintf(stderr, "FAIL %s: parse returned %d (%s), expected %d\n",
			name, got, why ? why : "-", want);
		failures++;
		return;
	}
	if (want != 0 && !why) {
		fprintf(stderr, "FAIL %s: refused without naming a reason\n", name);
		failures++;
		return;
	}
	printf("ok   %-32s -> %d%s%s\n", name, got, why ? " " : "", why ? why : "");
}

/* A private copy of the stream, one per case. */
static unsigned char *copy_of(const unsigned char *src, size_t len)
{
	unsigned char *c = malloc(len);

	if (!c) {
		fprintf(stderr, "out of memory\n");
		exit(2);
	}
	memcpy(c, src, len);
	return c;
}

static void put32(unsigned char *buf, size_t off, __u32 v)
{
	memcpy(buf + off, &v, sizeof(v));
}

static __u32 get32(const unsigned char *buf, size_t off)
{
	__u32 v;

	memcpy(&v, buf + off, sizeof(v));
	return v;
}

int main(int argc, char **argv)
{
	unsigned char *buf, *c;
	struct nvrm_tables t = { 0 };
	const char *why = NULL;
	long n;
	FILE *f;
	size_t len;
	/* Header word offsets, from the field order in nvrm_wire.h. */
	const size_t off_magic = offsetof(struct nvrm_table_hdr, magic);
	const size_t off_version = offsetof(struct nvrm_table_hdr, table_version);
	const size_t off_total = offsetof(struct nvrm_table_hdr, total_len);
	const size_t off_checksum = offsetof(struct nvrm_table_hdr, checksum);
	const size_t off_n_ioctl = offsetof(struct nvrm_table_hdr, n_ioctl);
	const size_t off_max_nested = offsetof(struct nvrm_table_hdr, max_nested);

	if (argc != 2) {
		fprintf(stderr, "usage: %s <stream.bin>\n", argv[0]);
		return 2;
	}
	f = fopen(argv[1], "rb");
	if (!f) {
		perror(argv[1]);
		return 2;
	}
	if (fseek(f, 0, SEEK_END) != 0 || (n = ftell(f)) < 0 || fseek(f, 0, SEEK_SET) != 0) {
		perror("fseek/ftell");
		return 2;
	}
	len = (size_t)n;
	buf = malloc(len);
	if (!buf || fread(buf, 1, len, f) != len) {
		fprintf(stderr, "ERROR: %s not readable\n", argv[1]);
		return 2;
	}
	fclose(f);

	/* The original is accepted -- otherwise every refusal below would be
	 * meaningless. */
	expect("original stream", buf, len, 0);

	/* 1. Shorter than a header: not even the magic can be read. */
	c = copy_of(buf, len);
	expect("stream shorter than the header", c, sizeof(struct nvrm_table_hdr) - 1, -EPROTO);
	free(c);

	/* 2. Wrong magic. */
	c = copy_of(buf, len);
	put32(c, off_magic, get32(c, off_magic) ^ 0x1);
	expect("wrong magic", c, len, -EPROTO);
	free(c);

	/* 3. A table format version this interpreter does not know. */
	c = copy_of(buf, len);
	put32(c, off_version, get32(c, off_version) + 1);
	expect("unknown table version", c, len, -EPROTO);
	free(c);

	/* 4. total_len that does not match what arrived -- both directions. */
	c = copy_of(buf, len);
	put32(c, off_total, get32(c, off_total) + 4);
	expect("total_len larger than received", c, len, -EPROTO);
	free(c);
	c = copy_of(buf, len);
	put32(c, off_total, get32(c, off_total) - 4);
	expect("total_len smaller than received", c, len, -EPROTO);
	free(c);

	/* 5. Counts that do not add up to the length (one ioctl row too many),
	 *    with total_len still honest about the bytes. */
	c = copy_of(buf, len);
	put32(c, off_n_ioctl, get32(c, off_n_ioctl) + 1);
	expect("counts do not match the length", c, len, -EPROTO);
	free(c);

	/* 6. A flipped payload byte: the checksum catches what the length
	 *    checks cannot. */
	c = copy_of(buf, len);
	c[len - 1] ^= 0x80;
	expect("payload byte flipped (checksum)", c, len, -EPROTO);
	free(c);
	c = copy_of(buf, len);
	put32(c, off_checksum, get32(c, off_checksum) ^ 0x1);
	expect("checksum word altered", c, len, -EPROTO);
	free(c);

	/* 7. The host claims more nested slots than the wire format carries. */
	c = copy_of(buf, len);
	put32(c, off_max_nested, NVRM_MAX_NESTED + 1);
	expect("max_nested beyond the wire", c, len, -EPROTO);
	free(c);

	/* 8. Lookups for keys the stream does not contain answer NULL -- the
	 *    module's "not in the table" branches hang off exactly that. */
	t.blob = buf;
	t.len = len;
	if (nvrm_tables_parse(&t, &why) != 0) {
		fprintf(stderr, "FAIL: original stream rejected on second parse: %s\n", why);
		failures++;
	} else {
		if (find_ioctl(&t, 0xffffffffu, 0xffffffffu)) {
			fprintf(stderr, "FAIL: find_ioctl found a row for an impossible key\n");
			failures++;
		}
		if (find_class(&t, 0xffffffffu)) {
			fprintf(stderr, "FAIL: find_class found a row for hClass 0xffffffff\n");
			failures++;
		}
		if (find_ctrl(&t, 0xffffffffu)) {
			fprintf(stderr, "FAIL: find_ctrl found a row for cmd 0xffffffff\n");
			failures++;
		}
		printf("ok   %-32s -> NULL, NULL, NULL\n", "lookups of unknown keys");
	}

	free(buf);
	if (failures) {
		fprintf(stderr, "%d refusal(s) did not hold\n", failures);
		return 1;
	}
	return 0;
}
