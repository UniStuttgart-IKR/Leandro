/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * nvrm_tables.c -- the table interpreter: parse, check, look up.
 *
 * ONE translation unit for two worlds, pulled in via #include:
 *   - virtio_nvrm.c (kernel module): the production path.
 *   - test/tabcheck.c (userspace): the test binary that runs the same code
 *     over the same stream that `nvrm-abi::table::build()` produces
 *     (`nvrm-genhdr --dump-tables`).
 *
 * Hence ONLY this in here: memcpy, the wire structs from nvrm_wire.h and
 * error codes. No printk, no kvzalloc, no copy_from_user -- whoever obtains
 * memory or prints diagnostics is the includer. Why the code sits in its own
 * file: the kernel module is the ONLY interpreter of the xlate tables, and an
 * interpreter with zero tests would be the largest untested surface in the
 * system.
 */

#ifdef __KERNEL__
# include <linux/string.h>
# include <linux/errno.h>
#else
# include <errno.h>
# include <stddef.h>
# include <string.h>
#endif

#include "nvrm_wire.h"

struct nvrm_tables {
	void *blob;		/* owned by the includer */
	size_t len;
	struct nvrm_table_hdr hdr;
	const struct nvrm_ioctl_desc *ioctls;
	const struct nvrm_class_desc *classes;
	const struct nvrm_ctrl_desc *ctrls;
	const struct nvrm_nested_row *nested;
};

static __u32 nvrm_fnv1a32(const __u8 *b, size_t n)
{
	__u32 h = 0x811c9dc5u;
	size_t i;

	for (i = 0; i < n; i++) {
		h ^= b[i];
		h *= 0x01000193u;
	}
	return h;
}

/*
 * Check the blob and set the section pointers. `t->blob`/`t->len` are filled
 * in by the includer; everything else is set here. From here on the stream is
 * checked, not believed: it comes from the host, and the host could be a
 * different one than expected.
 *
 * Returns 0 or -EPROTO; on failure `*why` points at a static string for the
 * includer's diagnostic line.
 */
static int nvrm_tables_parse(struct nvrm_tables *t, const char **why)
{
	size_t need;

	*why = NULL;
	if (t->len < sizeof(t->hdr)) {
		*why = "stream shorter than the header";
		return -EPROTO;
	}
	memcpy(&t->hdr, t->blob, sizeof(t->hdr));
	if (t->hdr.magic != NVRM_TABLE_MAGIC) {
		*why = "wrong magic";
		return -EPROTO;
	}
	if (t->hdr.table_version != NVRM_TABLE_VERSION) {
		*why = "unknown table format version";
		return -EPROTO;
	}
	if (t->hdr.total_len != t->len) {
		*why = "total_len != received length";
		return -EPROTO;
	}
	need = sizeof(t->hdr)
	     + (size_t)t->hdr.n_ioctl * sizeof(struct nvrm_ioctl_desc)
	     + (size_t)t->hdr.n_class * sizeof(struct nvrm_class_desc)
	     + (size_t)t->hdr.n_ctrl * sizeof(struct nvrm_ctrl_desc)
	     + (size_t)t->hdr.n_nested * sizeof(struct nvrm_nested_row);
	if (need != t->len) {
		*why = "table counts do not match the stream length";
		return -EPROTO;
	}
	if (nvrm_fnv1a32((__u8 *)t->blob + sizeof(t->hdr), t->len - sizeof(t->hdr))
	    != t->hdr.checksum) {
		*why = "wrong checksum";
		return -EPROTO;
	}
	if (t->hdr.max_nested > NVRM_MAX_NESTED) {
		*why = "host allows more nested pointers than the wire format carries";
		return -EPROTO;
	}

	t->ioctls = (const struct nvrm_ioctl_desc *)((__u8 *)t->blob + sizeof(t->hdr));
	t->classes = (const struct nvrm_class_desc *)(t->ioctls + t->hdr.n_ioctl);
	t->ctrls = (const struct nvrm_ctrl_desc *)(t->classes + t->hdr.n_class);
	t->nested = (const struct nvrm_nested_row *)(t->ctrls + t->hdr.n_ctrl);
	return 0;
}

static const struct nvrm_ioctl_desc *find_ioctl(const struct nvrm_tables *t, __u32 dev_tag, __u32 nr)
{
	__u32 i;

	for (i = 0; i < t->hdr.n_ioctl; i++)
		if (t->ioctls[i].dev == dev_tag && t->ioctls[i].nr == nr)
			return &t->ioctls[i];
	return NULL;
}

static const struct nvrm_class_desc *find_class(const struct nvrm_tables *t, __u32 hclass)
{
	__u32 i;

	for (i = 0; i < t->hdr.n_class; i++)
		if (t->classes[i].hclass == hclass)
			return &t->classes[i];
	return NULL;
}

static const struct nvrm_ctrl_desc *find_ctrl(const struct nvrm_tables *t, __u32 cmd)
{
	__u32 i;

	for (i = 0; i < t->hdr.n_ctrl; i++)
		if (t->ctrls[i].cmd == cmd)
			return &t->ctrls[i];
	return NULL;
}
