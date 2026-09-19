/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/* Translation-table parser shared by virtio_nvrm.c and the userspace tests.
 * The caller owns the input buffer and handles allocation and diagnostics. */

#ifdef __KERNEL__
#include <linux/string.h>
#include <linux/errno.h>
#else
#include <errno.h>
#include <stddef.h>
#include <string.h>
#endif

#include "nvrm_wire.h"

#define NVRM_XFER_HEADER_MAX 64u

struct nvrm_tables {
	void *blob; /* owned by the includer */
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

static int nvrm_range_valid(__u32 offset, __u32 size, __u32 total)
{
	return offset <= total && size <= total - offset;
}

/* Validate t->blob/t->len and initialize its table views.
 * On -EPROTO, *why describes the failure; no view may be used. */
static int nvrm_tables_parse(struct nvrm_tables *t, const char **why)
{
	__u64 need;
	__u32 i;

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
	need = sizeof(t->hdr) +
	       (__u64)t->hdr.n_ioctl * sizeof(struct nvrm_ioctl_desc) +
	       (__u64)t->hdr.n_class * sizeof(struct nvrm_class_desc) +
	       (__u64)t->hdr.n_ctrl * sizeof(struct nvrm_ctrl_desc) +
	       (__u64)t->hdr.n_nested * sizeof(struct nvrm_nested_row);
	if (need != t->len) {
		*why = "table counts do not match the stream length";
		return -EPROTO;
	}
	if (nvrm_fnv1a32((__u8 *)t->blob + sizeof(t->hdr),
			 t->len - sizeof(t->hdr)) != t->hdr.checksum) {
		*why = "wrong checksum";
		return -EPROTO;
	}
	if (t->hdr.max_nested > NVRM_MAX_NESTED) {
		*why = "host allows more nested pointers than the wire format carries";
		return -EPROTO;
	}
	if (t->hdr.max_inline > NVRM_MAX_PAYLOAD ||
	    t->hdr.max_ioctl_size > NVRM_MAX_PAYLOAD ||
	    t->hdr.max_aux > NVRM_MAX_AUX) {
		*why = "host payload limits exceed the wire format";
		return -EPROTO;
	}
	if (t->hdr.xfer_struct_len > NVRM_XFER_HEADER_MAX ||
	    !nvrm_range_valid(t->hdr.xfer_cmd_off, 4, t->hdr.xfer_struct_len) ||
	    !nvrm_range_valid(t->hdr.xfer_size_off, 4,
			      t->hdr.xfer_struct_len) ||
	    !nvrm_range_valid(t->hdr.xfer_ptr_off, 8, t->hdr.xfer_struct_len)) {
		*why = "XFER fields exceed the wrapper";
		return -EPROTO;
	}

	t->ioctls = (const struct nvrm_ioctl_desc *)((__u8 *)t->blob +
						     sizeof(t->hdr));
	t->classes =
		(const struct nvrm_class_desc *)(t->ioctls + t->hdr.n_ioctl);
	t->ctrls = (const struct nvrm_ctrl_desc *)(t->classes + t->hdr.n_class);
	t->nested = (const struct nvrm_nested_row *)(t->ctrls + t->hdr.n_ctrl);
	for (i = 0; i < t->hdr.n_ctrl; i++) {
		const struct nvrm_ctrl_desc *c = &t->ctrls[i];

		if (c->count > t->hdr.max_nested ||
		    !nvrm_range_valid(c->first, c->count, t->hdr.n_nested)) {
			*why = "control references invalid nested rows";
			return -EPROTO;
		}
	}
	return 0;
}

static const struct nvrm_ioctl_desc *find_ioctl(const struct nvrm_tables *t,
						__u32 dev_tag, __u32 nr)
{
	__u32 i;

	for (i = 0; i < t->hdr.n_ioctl; i++)
		if (t->ioctls[i].dev == dev_tag && t->ioctls[i].nr == nr)
			return &t->ioctls[i];
	return NULL;
}

static const struct nvrm_class_desc *find_class(const struct nvrm_tables *t,
						__u32 hclass)
{
	__u32 i;

	for (i = 0; i < t->hdr.n_class; i++)
		if (t->classes[i].hclass == hclass)
			return &t->classes[i];
	return NULL;
}

static const struct nvrm_ctrl_desc *find_ctrl(const struct nvrm_tables *t,
					      __u32 cmd)
{
	__u32 i;

	for (i = 0; i < t->hdr.n_ctrl; i++)
		if (t->ctrls[i].cmd == cmd)
			return &t->ctrls[i];
	return NULL;
}
