/* SPDX-License-Identifier: GPL-2.0-only */
/* SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de> */
/* SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR */
/*
 * nvrm-nodes-tool - provisioning and self-test for the guest module.
 *
 *   nvrm-nodes-tool version
 *   nvrm-nodes-tool provision <name> <file>      # e.g. params params.txt
 *   nvrm-nodes-tool gpa <MiB> [hold seconds]     # check VA2GPA against pagemap
 *
 * `provision` needs root (CAP_SYS_ADMIN), `gpa` explicitly does NOT -- that is
 * the whole point of the module.
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include "nvrm_nodes_uapi.h"

static int open_dev(void)
{
	int fd = open(LEA_DEV_PATH, O_RDWR);

	if (fd < 0) {
		perror(LEA_DEV_PATH);
		fprintf(stderr, "  module loaded? (sudo insmod nvrm_nodes.ko)\n");
		exit(1);
	}
	return fd;
}

static int cmd_version(void)
{
	int fd = open_dev();
	uint32_t v = 0;

	if (ioctl(fd, NVRM_NODES_IOC_VERSION, &v)) {
		/* perror before close: close(2) may overwrite errno. */
		perror("VERSION");
		close(fd);
		return 1;
	}
	printf("nvrm-nodes-tool: module ABI %u (tool expects %u) %s\n",
	       v, NVRM_NODES_ABI_VERSION,
	       v == NVRM_NODES_ABI_VERSION ? "ok" : "MISMATCH");
	close(fd);
	return v != NVRM_NODES_ABI_VERSION;
}

static int cmd_provision(const char *name, const char *path)
{
	FILE *f = fopen(path, "rb");
	char *buf;
	long len;
	struct nvrm_nodes_proc_req req;
	int fd, rc;

	if (!f) { perror(path); return 1; }
	fseek(f, 0, SEEK_END);
	len = ftell(f);
	fseek(f, 0, SEEK_SET);
	if (len <= 0 || len > NVRM_NODES_PROC_DATA_MAX) {
		fprintf(stderr, "nvrm-nodes-tool: %s has unusable size %ld\n", path, len);
		fclose(f);
		return 1;
	}
	buf = malloc((size_t)len);
	if (!buf || fread(buf, 1, (size_t)len, f) != (size_t)len) {
		fprintf(stderr, "nvrm-nodes-tool: %s read incompletely\n", path);
		fclose(f);
		return 1;
	}
	fclose(f);

	memset(&req, 0, sizeof(req));
	snprintf(req.name, sizeof(req.name), "%s", name);
	req.data_ptr = (uint64_t)(uintptr_t)buf;
	req.data_len = (uint32_t)len;

	fd = open_dev();
	rc = ioctl(fd, NVRM_NODES_IOC_SET_PROC, &req);
	if (rc) {
		/* Both messages before close(2), which may overwrite errno. */
		perror("SET_PROC");
		fprintf(stderr, "  (needs root)\n");
	}
	close(fd);
	free(buf);
	if (rc)
		return 1;
	printf("nvrm-nodes-tool: /proc/driver/nvidia/%s set (%ld bytes)\n", name, len);
	return 0;
}

/* One pagemap entry per page of the RUNNING kernel: `ps` is sysconf's
 * answer, not 4096. A 16K or 64K page kernel (arm64 and ppc64 ship both)
 * would otherwise be read at the wrong entry -- silently, since any entry
 * decodes. */
static uint64_t pagemap_pfn(uint64_t va, size_t ps)
{
	int fd = open("/proc/self/pagemap", O_RDONLY);
	uint64_t ent = 0;

	if (fd < 0)
		return 0;
	if (pread(fd, &ent, 8, (va / ps) * 8) != 8)
		ent = 0;
	close(fd);
	if (!(ent & (1ull << 63)))
		return 0;
	return ent & ((1ull << 55) - 1);
}

static int cmd_gpa(size_t mib, unsigned hold_s)
{
	long psl = sysconf(_SC_PAGESIZE);
	size_t ps, len = mib << 20, pages;
	struct nvrm_nodes_gpa_run *runs = NULL;
	struct nvrm_nodes_gpa_req req;
	unsigned char *buf = NULL;
	uint64_t total = 0, ref;
	int fd = -1, rc, ret = 1;

	/* No perror: sysconf reports "no limit" by returning -1 without
	 * touching errno, and a "Success" after a failure is worse than no
	 * message. */
	if (psl <= 0) {
		fprintf(stderr, "nvrm-nodes-tool: sysconf(_SC_PAGESIZE) gave %ld\n", psl);
		return 1;
	}
	ps = (size_t)psl;
	pages = len / ps;

	if (posix_memalign((void **)&buf, ps, len)) { perror("posix_memalign"); return 1; }
	memset(buf, 0x5a, len);
	runs = calloc(pages, sizeof(*runs));
	if (!runs)
		goto out;

	memset(&req, 0, sizeof(req));
	req.va = (uint64_t)(uintptr_t)buf;
	req.len = len;
	req.runs_ptr = (uint64_t)(uintptr_t)runs;
	req.runs_max = (uint32_t)pages;

	fd = open_dev();
	rc = ioctl(fd, NVRM_NODES_IOC_VA2GPA, &req);
	if (rc) { perror("VA2GPA"); goto out; }

	for (uint32_t i = 0; i < req.runs_len; i++)
		total += runs[i].len;

	printf("nvrm-nodes-tool: %zu MiB -> %u runs, sum %llu bytes %s\n",
	       mib, req.runs_len, (unsigned long long)total,
	       total == len ? "ok" : "WRONG");

	/* Cross-check: first run against pagemap (comparable as root; as an
	 * ordinary user pagemap shows 0 -- which is exactly why this module
	 * exists). */
	ref = pagemap_pfn((uint64_t)(uintptr_t)buf, ps);
	if (ref) {
		/* PFN -> address is a multiplication by the page size, not a
		 * shift by 12. */
		printf("nvrm-nodes-tool: first GPA %#llx, pagemap %#llx %s\n",
		       (unsigned long long)runs[0].gpa,
		       (unsigned long long)(ref * ps),
		       runs[0].gpa == ref * ps ? "ok" : "MISMATCH");
		if (runs[0].gpa != ref * ps) goto out;
	} else {
		printf("nvrm-nodes-tool: pagemap shows 0 (non-root) -- module reports %#llx\n",
		       (unsigned long long)runs[0].gpa);
		if (!runs[0].gpa) goto out;
	}

	/* A capacity failure must report cleanly instead of pinning. */
	req.runs_max = 0;
	rc = ioctl(fd, NVRM_NODES_IOC_VA2GPA, &req);
	printf("nvrm-nodes-tool: runs_max=0 -> rc=%d needed=%u %s\n",
	       rc, req.runs_len, (rc < 0 && req.runs_len > 0) ? "ok" : "WRONG");

	/* Hold, so it can be checked from outside what happens to the pinned
	 * pages on a kill (the module releases them in release()). */
	if (hold_s) {
		printf("nvrm-nodes-tool: holding %u s with %zu MiB pinned (PID %d)\n",
		       hold_s, mib, (int)getpid());
		fflush(stdout);
		sleep(hold_s);
	}

	ret = total != len;
out:
	/* One exit, and it matters more here than the memory does: the module
	 * keeps the pages pinned until the device is released, so a return
	 * that skipped this left MiB of guest RAM pinned for as long as the
	 * process lived -- and `gpa` is the command that holds on purpose. */
	if (fd >= 0)
		close(fd);
	free(runs);
	free(buf);
	return ret;
}

int main(int argc, char **argv)
{
	if (argc >= 2 && !strcmp(argv[1], "version"))
		return cmd_version();
	if (argc == 4 && !strcmp(argv[1], "provision"))
		return cmd_provision(argv[2], argv[3]);
	if ((argc == 3 || argc == 4) && !strcmp(argv[1], "gpa"))
		return cmd_gpa(strtoul(argv[2], NULL, 0),
			       argc == 4 ? (unsigned)strtoul(argv[3], NULL, 0) : 0);

	fprintf(stderr,
		"usage:\n"
		"  nvrm-nodes-tool version\n"
		"  nvrm-nodes-tool provision <name> <file>\n"
		"  nvrm-nodes-tool gpa <MiB> [hold seconds]\n");
	return 2;
}
