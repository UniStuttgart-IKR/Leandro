/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * tabcheck -- the C interpreter in userspace, run against the real stream.
 *
 *   cc -O2 -Wall -Wextra -Werror -o tabcheck tabcheck.c
 *   ./tabcheck <stream.bin>
 *
 * Reads the table stream written by `nvrm-genhdr --dump-tables`, runs EXACTLY
 * the kernel module's parser over it (nvrm_tables.c, the same translation
 * unit) and prints every parsed row in a deterministic text format.
 * `nvrm-genhdr --expect-dump` produces the same format from the Rust WRITER'S
 * view (the source structures before serialization) -- so a `diff` of the two
 * outputs proves that the C reader understands the stream the way the Rust
 * writer means it. test.sh check runs exactly this diff (step c-interpreter).
 *
 * The values are deliberately raw (everything decimal, NONE = 4294967295):
 * any formatting logic would be surface for the two sides to diverge on.
 */

#include <stdio.h>
#include <stdlib.h>

#include "../nvrm_tables.c"

int main(int argc, char **argv)
{
	struct nvrm_tables t = { 0 };
	const char *why = NULL;
	unsigned char *buf;
	long n;
	FILE *f;
	__u32 i;

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
	buf = malloc((size_t)n);
	if (!buf || fread(buf, 1, (size_t)n, f) != (size_t)n) {
		fprintf(stderr, "ERROR: %s not readable\n", argv[1]);
		return 2;
	}
	fclose(f);

	t.blob = buf;
	t.len = (size_t)n;
	if (nvrm_tables_parse(&t, &why) != 0) {
		fprintf(stderr, "ERROR: tables rejected: %s\n", why);
		return 1;
	}

	/* The header, field by field in declaration order (nvrm_wire.h). */
	printf("hdr %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u %u\n",
	       t.hdr.magic, t.hdr.table_version, t.hdr.total_len, t.hdr.checksum,
	       t.hdr.n_ioctl, t.hdr.n_class, t.hdr.n_ctrl, t.hdr.n_nested,
	       t.hdr.xfer_nr, t.hdr.xfer_struct_len, t.hdr.xfer_cmd_off,
	       t.hdr.xfer_size_off, t.hdr.xfer_ptr_off, t.hdr.max_ioctl_size,
	       t.hdr.osdesc_class, t.hdr.osdesc_pmem_off, t.hdr.osdesc_limit_off,
	       t.hdr.osdesc_status_off, t.hdr.osdesc_handle_off,
	       t.hdr.max_inline, t.hdr.max_aux, t.hdr.max_nested);

	/* Every row goes through the parsed STRUCT pointers, not through raw
	 * offsets: that is precisely what proves the C struct layout and the
	 * section pointers hit the stream. Lookups go through find_*, so the
	 * search functions are covered too -- a row that find_ioctl cannot find
	 * again is an error, not a dump. */
	for (i = 0; i < t.hdr.n_ioctl; i++) {
		const struct nvrm_ioctl_desc *d = find_ioctl(&t, t.ioctls[i].dev, t.ioctls[i].nr);

		if (d != &t.ioctls[i]) {
			fprintf(stderr, "ERROR: find_ioctl(%u, %u) does not find row %u\n",
				t.ioctls[i].dev, t.ioctls[i].nr, i);
			return 1;
		}
		printf("ioctl %u %u %u %u %u %u %u %u %u %u %u %u\n",
		       d->dev, d->nr, d->size, d->fd_off, d->emb_ptr_off,
		       d->emb_len_kind, d->emb_len_off, d->cmd_off, d->rights_off,
		       d->rights_if_size, d->handle_off, d->flags);
	}
	for (i = 0; i < t.hdr.n_class; i++) {
		const struct nvrm_class_desc *c = find_class(&t, t.classes[i].hclass);

		if (c != &t.classes[i]) {
			fprintf(stderr, "ERROR: find_class(%u) does not find row %u\n",
				t.classes[i].hclass, i);
			return 1;
		}
		printf("class %u %u %u %u %u %u\n", c->hclass, c->param_size,
		       c->fd_off, c->flags, c->fd_if_off, c->fd_if_val);
	}
	for (i = 0; i < t.hdr.n_ctrl; i++) {
		const struct nvrm_ctrl_desc *c = find_ctrl(&t, t.ctrls[i].cmd);

		if (c != &t.ctrls[i]) {
			fprintf(stderr, "ERROR: find_ctrl(%u) does not find row %u\n",
				t.ctrls[i].cmd, i);
			return 1;
		}
		printf("ctrl %u %u %u %u %u\n", c->cmd, c->first, c->count, c->flags, c->fd_off);
	}
	for (i = 0; i < t.hdr.n_nested; i++)
		printf("nested %u %u %u %u\n",
		       t.nested[i].ptr_off, t.nested[i].len_kind,
		       t.nested[i].len_off, t.nested[i].elem);

	free(buf);
	return 0;
}
