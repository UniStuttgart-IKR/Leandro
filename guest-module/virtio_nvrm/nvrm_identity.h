/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
#ifndef NVRM_IDENTITY_H
#define NVRM_IDENTITY_H

#ifdef __KERNEL__
#include <linux/errno.h>
#include <linux/string.h>
#else
#include <errno.h>
#include <stddef.h>
#include <string.h>
#endif

#include "nvrm_wire.h"

#define NVRM_PROCESS_ID_MAX 0x7fffffffu

/* Caller serializes allocation. Failed teardown must not permit ID reuse. */
static inline int nvrm_next_process_id(__u32 *last, __u32 *id)
{
	if (*last >= NVRM_PROCESS_ID_MAX)
		return -ENOSPC;
	*id = ++*last;
	return 0;
}

struct nvrm_object_key {
	__u32 client;
	__u32 handle;
};

/* Decode only an acknowledged NVOS00 FREE, never untouched request bytes. */
static inline int nvrm_free_reply(const void *reply, size_t len, int ret,
				  struct nvrm_object_key *key)
{
	const __u8 *bytes = reply;
	__u32 status;

	if (ret || len < NVRM_KSIZE_FREE)
		return 0;
	memcpy(&status, bytes + NVRM_NVOS00_STATUS_OFF, sizeof(status));
	if (status != 0)
		return 0;
	memcpy(&key->client, bytes + NVRM_NVOS00_HROOT_OFF,
	       sizeof(key->client));
	memcpy(&key->handle, bytes + NVRM_NVOS00_HOBJECTOLD_OFF,
	       sizeof(key->handle));
	return 1;
}

enum nvrm_osdesc_result {
	NVRM_OSDESC_UNKNOWN,
	NVRM_OSDESC_REJECTED,
	NVRM_OSDESC_CREATED,
};

/* Only returned status bytes establish allocation failure or ownership. */
static inline enum nvrm_osdesc_result
nvrm_osdesc_reply(const void *reply, size_t len, int ret, __u32 status_off,
		  __u32 handle_off, __u32 *handle)
{
	const __u8 *bytes = reply;
	__u32 status;

	if (ret || len < sizeof(status) || status_off > len - sizeof(status) ||
	    handle_off > len - sizeof(*handle))
		return NVRM_OSDESC_UNKNOWN;
	memcpy(&status, bytes + status_off, sizeof(status));
	if (status)
		return NVRM_OSDESC_REJECTED;
	memcpy(handle, bytes + handle_off, sizeof(*handle));
	return NVRM_OSDESC_CREATED;
}

/* A client-root FREE also removes that client's children. */
static inline int nvrm_object_freed(const struct nvrm_object_key *object,
				    const struct nvrm_object_key *freed)
{
	return object->client == freed->client &&
	       (object->handle == freed->handle ||
		freed->handle == freed->client);
}

#endif
