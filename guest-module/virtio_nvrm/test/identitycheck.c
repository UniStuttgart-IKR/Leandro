/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/* Exercise the identity and FREE-reply helpers used by the kernel module. */
#include <assert.h>
#include <stdio.h>

#include "../nvrm_identity.h"

static void put32(__u8 *bytes, size_t off, __u32 value)
{
	memcpy(bytes + off, &value, sizeof(value));
}

static void process_ids(void)
{
	__u32 last = 0, id = 0, first;

	assert(nvrm_next_process_id(&last, &id) == 0);
	assert(id == 1);
	first = id;
	/* Retiring a process has no effect on the allocation cursor. */
	assert(nvrm_next_process_id(&last, &id) == 0);
	assert(id != first && id == 2);

	last = NVRM_PROCESS_ID_MAX - 1;
	assert(nvrm_next_process_id(&last, &id) == 0);
	assert(id == NVRM_PROCESS_ID_MAX);
	assert(nvrm_next_process_id(&last, &id) == -ENOSPC);
	assert(nvrm_next_process_id(&last, &id) == -ENOSPC);
	assert(last == NVRM_PROCESS_ID_MAX && id == NVRM_PROCESS_ID_MAX);
}

static void free_replies(void)
{
	const struct nvrm_object_key objects[] = {
		{ .client = 1, .handle = 10 },
		{ .client = 1, .handle = 11 },
		{ .client = 2, .handle = 10 },
	};
	__u8 reply[NVRM_KSIZE_FREE] = { 0 };
	struct nvrm_object_key freed = { 99, 99 };
	size_t len;

	put32(reply, NVRM_NVOS00_HROOT_OFF, 1);
	put32(reply, NVRM_NVOS00_HOBJECTOLD_OFF, 10);
	for (len = 0; len < sizeof(reply); len++)
		assert(!nvrm_free_reply(reply, len, 0, &freed));
	assert(freed.client == 99 && freed.handle == 99);
	assert(!nvrm_free_reply(reply, sizeof(reply), -EIO, &freed));
	put32(reply, NVRM_NVOS00_STATUS_OFF, 0x1f);
	assert(!nvrm_free_reply(reply, sizeof(reply), 0, &freed));
	assert(freed.client == 99 && freed.handle == 99);

	put32(reply, NVRM_NVOS00_STATUS_OFF, 0);
	assert(nvrm_free_reply(reply, sizeof(reply), 0, &freed));
	assert(nvrm_object_freed(&objects[0], &freed));
	assert(!nvrm_object_freed(&objects[1], &freed));
	assert(!nvrm_object_freed(&objects[2], &freed));

	put32(reply, NVRM_NVOS00_HOBJECTOLD_OFF, 1);
	assert(nvrm_free_reply(reply, sizeof(reply), 0, &freed));
	assert(nvrm_object_freed(&objects[0], &freed));
	assert(nvrm_object_freed(&objects[1], &freed));
	assert(!nvrm_object_freed(&objects[2], &freed));
}

static void osdesc_replies(void)
{
	__u8 reply[NVRM_KSIZE_ALLOC] = { 0 };
	__u32 handle = 99;
	size_t len;

	put32(reply, NVRM_NVOS64_HOBJECTNEW_OFF, 12);
	/* Missing status bytes must not look like untouched NV_OK input. */
	for (len = 0; len < NVRM_NVOS64_STATUS_OFF + 4; len++)
		assert(nvrm_osdesc_reply(reply, len, 0, NVRM_NVOS64_STATUS_OFF,
					 NVRM_NVOS64_HOBJECTNEW_OFF,
					 &handle) == NVRM_OSDESC_UNKNOWN);
	assert(nvrm_osdesc_reply(reply, sizeof(reply), -EIO,
				 NVRM_NVOS64_STATUS_OFF,
				 NVRM_NVOS64_HOBJECTNEW_OFF,
				 &handle) == NVRM_OSDESC_UNKNOWN);
	assert(nvrm_osdesc_reply(reply, sizeof(reply), 0, ~(__u32)0,
				 NVRM_NVOS64_HOBJECTNEW_OFF,
				 &handle) == NVRM_OSDESC_UNKNOWN);
	assert(nvrm_osdesc_reply(reply, sizeof(reply), 0,
				 NVRM_NVOS64_STATUS_OFF, ~(__u32)0,
				 &handle) == NVRM_OSDESC_UNKNOWN);
	assert(handle == 99);

	put32(reply, NVRM_NVOS64_STATUS_OFF, 0x1f);
	assert(nvrm_osdesc_reply(reply, sizeof(reply), 0,
				 NVRM_NVOS64_STATUS_OFF,
				 NVRM_NVOS64_HOBJECTNEW_OFF,
				 &handle) == NVRM_OSDESC_REJECTED);
	assert(handle == 99);
	put32(reply, NVRM_NVOS64_STATUS_OFF, 0);
	assert(nvrm_osdesc_reply(reply, sizeof(reply), 0,
				 NVRM_NVOS64_STATUS_OFF,
				 NVRM_NVOS64_HOBJECTNEW_OFF,
				 &handle) == NVRM_OSDESC_CREATED);
	assert(handle == 12);
}

int main(void)
{
	process_ids();
	free_replies();
	osdesc_replies();
	puts("process IDs and native ownership replies: ok");
	return 0;
}
