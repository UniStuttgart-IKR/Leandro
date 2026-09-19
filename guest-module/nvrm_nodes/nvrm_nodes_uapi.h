/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * nvrm_nodes guest module - UAPI. Shared between the module and the
 * provisioning tool (nvrm-nodes-tool).
 */
#ifndef NVRM_NODES_UAPI_H
#define NVRM_NODES_UAPI_H

#ifdef __KERNEL__
#include <linux/types.h>
#include <linux/ioctl.h>
#else
#include <stdint.h>
#include <sys/ioctl.h>
typedef uint32_t __u32;
typedef uint64_t __u64;
#endif

#define LEA_DEV_PATH "/dev/nvrm_nodes"

/** One run of contiguous guest-physical memory. Pairs of (gpa, len), passed
 *  on unchanged by whoever forwards them. */
struct nvrm_nodes_gpa_run {
	__u64 gpa;
	__u64 len;
};

/**
 * VA2GPA pins the caller's page-aligned range and returns physical runs.
 * No CAP_SYS_ADMIN is required. FOLL_LONGTERM pins prevent migration; the
 * pages remain pinned until the file's final reference is released.
 * If runs_max is insufficient, return -ENOSPC with the required runs_len
 * and retain no pins.
 */
struct nvrm_nodes_gpa_req {
	__u64 va; /* IN,  page-aligned */
	__u64 len; /* IN,  multiple of the page size */
	__u64 runs_ptr; /* IN,  pointer to struct nvrm_nodes_gpa_run[] */
	__u32 runs_max; /* IN */
	__u32 runs_len; /* OUT */
};

/**
 * SET_PROC: set the content of a /proc/driver/nvidia/<file>.
 *
 * The content comes from the REAL file on the host (lockstep rule: never
 * guess). CAP_SYS_ADMIN only. `name` is "params" or "version".
 */
#define NVRM_NODES_PROC_NAME_MAX 32
#define NVRM_NODES_PROC_DATA_MAX (64 * 1024)

struct nvrm_nodes_proc_req {
	char name[NVRM_NODES_PROC_NAME_MAX]; /* IN, NUL-terminated */
	__u64 data_ptr; /* IN */
	__u32 data_len; /* IN */
	__u32 _pad;
};

#define NVRM_NODES_IOC_MAGIC 'S'
#define NVRM_NODES_IOC_VA2GPA \
	_IOWR(NVRM_NODES_IOC_MAGIC, 1, struct nvrm_nodes_gpa_req)
#define NVRM_NODES_IOC_SET_PROC \
	_IOW(NVRM_NODES_IOC_MAGIC, 2, struct nvrm_nodes_proc_req)
#define NVRM_NODES_IOC_VERSION _IOR(NVRM_NODES_IOC_MAGIC, 3, __u32)

/** Bumped whenever the meaning of the ioctls changes. */
#define NVRM_NODES_ABI_VERSION 1

#endif /* NVRM_NODES_UAPI_H */
