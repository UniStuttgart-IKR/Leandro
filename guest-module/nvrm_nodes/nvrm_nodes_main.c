// SPDX-License-Identifier: GPL-2.0-only
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * nvrm_nodes - guest kernel module.
 *
 * Replaces three crutches of a pure userspace setup:
 *
 *  1. mknod /dev/nvidiactl|nvidia0|nvidia-uvm  -> real character devices with
 *     the correct majors (195/235). devtmpfs creates the nodes itself.
 *  2. bind mount over /proc/devices            -> unnecessary, because a real
 *     chrdev registration shows up there anyway.
 *  3. tmpfs over /proc/driver plus a copy of params -> /proc/driver/nvidia/params
 *     comes from the module, filled from the REAL host file (lockstep rule:
 *     never guess) via NVRM_NODES_IOC_SET_PROC.
 *
 * Plus NVRM_NODES_IOC_VA2GPA, which resolves guest VAs into GPA
 * (guest-physical address) runs inside
 * the kernel. That way a CUDA process in the guest needs no root (pagemap
 * shows PFNs only with CAP_SYS_ADMIN) and the pages are genuinely held
 * against migration -- mlock did not do that. It was the point of this
 * module while a userspace shim carried the calls; today virtio_nvrm.ko pins
 * pages itself on the OS-descriptor path (NV01_MEMORY_SYSTEM_OS_DESCRIPTOR,
 * the NVIDIA allocation whose memory the driver pins rather than copies), and
 * this ioctl is exercised by the tool's self-test (`nvrm-nodes-tool gpa`)
 * rather than by the data path.
 *
 * The device nodes created here are PLACEHOLDERS: opening one returns -ENODEV
 * instead of misbehaving silently. Real forwarding lives in virtio_nvrm.ko,
 * which owns the nodes itself; next to it this module is loaded with
 * create_nodes=0 and supplies only /proc/driver/nvidia.
 *
 * Kernel pin: 6.8.0-136-generic (Ubuntu 24.04, GUEST_IMAGE).
 */

#include <linux/cdev.h>
#include <linux/device.h>
#include <linux/fs.h>
#include <linux/list.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/proc_fs.h>
#include <linux/seq_file.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/version.h>

#include "nvrm_nodes_uapi.h"

#define NV_FRONTEND_MAJOR 195
#define NV_UVM_MAJOR      235
#define NV_MINOR_GPU0     0
#define NV_MINOR_CTL      255

/* Pages per pin_user_pages_fast round: bounds the latency of a single call
 * and the size of one allocation, without capping the total length.
 * 4096 pages = 16 MiB. */
#define PIN_CHUNK_PAGES 4096

static bool create_nodes = true;
module_param(create_nodes, bool, 0444);
MODULE_PARM_DESC(create_nodes, "Create the NVIDIA placeholder nodes (default: yes)");

static unsigned int max_pin_mib = 1024;
module_param(max_pin_mib, uint, 0644);
MODULE_PARM_DESC(max_pin_mib, "Upper bound per VA2GPA call in MiB (default 1024)");

/* ------------------------------------------------------------------ *
 * Placeholder nodes
 * ------------------------------------------------------------------ */

static int placeholder_open(struct inode *inode, struct file *filp)
{
	pr_info_ratelimited(
		"nvrm_nodes: %d:%d opened -- this node is a placeholder and forwards nothing; "
		"the forwarding driver is virtio_nvrm.ko\n",
		imajor(inode), iminor(inode));
	return -ENODEV;
}

static const struct file_operations placeholder_fops = {
	.owner = THIS_MODULE,
	.open = placeholder_open,
};

/* One chrdev registration: (major, baseminor, count, name). Each produces its
 * own line in /proc/devices -- exactly like the real driver, which claims 195
 * several times under different names (cross-checked against the host's
 * /proc/devices: "195 nvidia", "195 nvidiactl", "235 nvidia-uvm"). */
struct chrdev_range {
	unsigned int major, baseminor, count;
	const char *name;
	bool registered;
};

static struct chrdev_range ranges[] = {
	{ NV_FRONTEND_MAJOR, NV_MINOR_GPU0, 1, "nvidia" },
	{ NV_FRONTEND_MAJOR, NV_MINOR_CTL,  1, "nvidiactl" },
	{ NV_UVM_MAJOR,      0,             2, "nvidia-uvm" },
};

/* Nodes that devtmpfs is supposed to create. */
struct node_spec {
	unsigned int major, minor;
	const char *name;
	struct device *dev;
};

static struct node_spec nodes[] = {
	{ NV_FRONTEND_MAJOR, NV_MINOR_GPU0, "nvidia0" },
	{ NV_FRONTEND_MAJOR, NV_MINOR_CTL,  "nvidiactl" },
	{ NV_UVM_MAJOR,      0,             "nvidia-uvm" },
	{ NV_UVM_MAJOR,      1,             "nvidia-uvm-tools" },
};

static struct class *nvrm_nodes_class;

/* The real driver opens its nodes to everyone (on the host: crw-rw-rw-).
 * Without this the guest would need root or a udev rule after all. */
static char *nvrm_nodes_devnode(const struct device *dev, umode_t *mode)
{
	if (mode)
		*mode = 0666;
	return NULL;
}

/* ------------------------------------------------------------------ *
 * /proc/driver/nvidia/<file>
 * ------------------------------------------------------------------ */

static struct proc_dir_entry *proc_nvidia_dir;
static DEFINE_MUTEX(proc_lock);

struct proc_file {
	struct list_head node;
	char name[NVRM_NODES_PROC_NAME_MAX];
	char *data;
	size_t len;
	struct proc_dir_entry *pde;
};

static LIST_HEAD(proc_files);

static int proc_file_show(struct seq_file *m, void *v)
{
	struct proc_file *pf = m->private;

	mutex_lock(&proc_lock);
	if (pf->data)
		seq_write(m, pf->data, pf->len);
	mutex_unlock(&proc_lock);
	return 0;
}

static int proc_file_open(struct inode *inode, struct file *filp)
{
	return single_open(filp, proc_file_show, pde_data(inode));
}

static const struct proc_ops proc_file_ops = {
	.proc_open = proc_file_open,
	.proc_read = seq_read,
	.proc_lseek = seq_lseek,
	.proc_release = single_release,
};

/* Find the file or create it; replaces its content. The caller does NOT hold
 * proc_lock -- it is taken here. */
static int proc_file_set(const char *name, char *data, size_t len)
{
	struct proc_file *pf;
	char *old = NULL;

	mutex_lock(&proc_lock);
	list_for_each_entry(pf, &proc_files, node) {
		if (!strcmp(pf->name, name)) {
			old = pf->data;
			pf->data = data;
			pf->len = len;
			mutex_unlock(&proc_lock);
			kvfree(old);
			return 0;
		}
	}
	mutex_unlock(&proc_lock);

	pf = kzalloc(sizeof(*pf), GFP_KERNEL);
	if (!pf)
		return -ENOMEM;
	strscpy(pf->name, name, sizeof(pf->name));
	pf->data = data;
	pf->len = len;
	pf->pde = proc_create_data(pf->name, 0444, proc_nvidia_dir,
				   &proc_file_ops, pf);
	if (!pf->pde) {
		kfree(pf);
		return -ENOMEM;
	}
	mutex_lock(&proc_lock);
	list_add_tail(&pf->node, &proc_files);
	mutex_unlock(&proc_lock);
	return 0;
}

static void proc_files_free(void)
{
	struct proc_file *pf, *tmp;

	list_for_each_entry_safe(pf, tmp, &proc_files, node) {
		if (pf->pde)
			proc_remove(pf->pde);
		list_del(&pf->node);
		kvfree(pf->data);
		kfree(pf);
	}
}

/* ------------------------------------------------------------------ *
 * /dev/nvrm_nodes: VA2GPA plus provisioning
 * ------------------------------------------------------------------ */

/* One pinned range. Lives until the FD is closed -- that is, until the
 * CUDA process ends. Exactly the lifetime of the host arena (the
 * contiguous host-side buffer the backend assembles from the GPA runs),
 * and crash-proof: if
 * the process dies, the kernel releases the pages. */
struct pin_record {
	struct list_head node;
	struct page **pages;
	unsigned long npages;
};

struct nvrm_nodes_ctx {
	struct mutex lock;
	struct list_head pins;
};

static int nvrm_nodes_open(struct inode *inode, struct file *filp)
{
	struct nvrm_nodes_ctx *ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);

	if (!ctx)
		return -ENOMEM;
	mutex_init(&ctx->lock);
	INIT_LIST_HEAD(&ctx->pins);
	filp->private_data = ctx;
	return 0;
}

static int nvrm_nodes_release(struct inode *inode, struct file *filp)
{
	struct nvrm_nodes_ctx *ctx = filp->private_data;
	struct pin_record *pr, *tmp;

	list_for_each_entry_safe(pr, tmp, &ctx->pins, node) {
		unpin_user_pages(pr->pages, pr->npages);
		kvfree(pr->pages);
		list_del(&pr->node);
		kfree(pr);
	}
	mutex_destroy(&ctx->lock);
	kfree(ctx);
	return 0;
}

static long do_va2gpa(struct nvrm_nodes_ctx *ctx, void __user *uarg)
{
	struct nvrm_nodes_gpa_req req;
	struct nvrm_nodes_gpa_run *runs = NULL;
	struct page **pages = NULL;
	struct pin_record *rec = NULL;
	unsigned long npages, done = 0;
	u32 nruns = 0;
	unsigned long i;
	long ret;

	if (copy_from_user(&req, uarg, sizeof(req)))
		return -EFAULT;
	if (!req.len || (req.va & ~PAGE_MASK) || (req.len & ~PAGE_MASK))
		return -EINVAL;
	if (req.va + req.len < req.va)	/* wraps: not a range */
		return -EINVAL;
	if (req.len > (u64)max_pin_mib << 20)
		return -E2BIG;

	npages = req.len >> PAGE_SHIFT;
	pages = kvmalloc_array(npages, sizeof(*pages), GFP_KERNEL);
	if (!pages)
		return -ENOMEM;

	/* FOLL_WRITE breaks COW (otherwise a fresh anonymous page still points
	 * at the shared zero page, whose PFN is not the PFN of the eventual
	 * target). FOLL_LONGTERM prevents later migration; that is exactly what
	 * mlock cannot do, and without it the host arena could end up pointing
	 * at stale pages. */
	while (done < npages) {
		unsigned long want = min_t(unsigned long, PIN_CHUNK_PAGES, npages - done);
		long got = pin_user_pages_fast(req.va + (done << PAGE_SHIFT), want,
					       FOLL_WRITE | FOLL_LONGTERM,
					       pages + done);
		if (got <= 0) {
			ret = got ? got : -EFAULT;
			goto out_unpin;
		}
		done += got;
		if (fatal_signal_pending(current)) {
			ret = -EINTR;
			goto out_unpin;
		}
	}

	/* First pass: count the runs, so the capacity can be checked before
	 * anything is copied. */
	for (i = 0; i < npages; i++) {
		u64 gpa = (u64)page_to_pfn(pages[i]) << PAGE_SHIFT;

		if (i && gpa == ((u64)page_to_pfn(pages[i - 1]) << PAGE_SHIFT) + PAGE_SIZE)
			continue;
		nruns++;
	}

	if (nruns > req.runs_max) {
		/* Pin nothing, but report the requirement: the caller can come
		 * back with a larger buffer. */
		req.runs_len = nruns;
		ret = copy_to_user(uarg, &req, sizeof(req)) ? -EFAULT : -ENOSPC;
		goto out_unpin;
	}

	runs = kvmalloc_array(nruns, sizeof(*runs), GFP_KERNEL);
	if (!runs) {
		ret = -ENOMEM;
		goto out_unpin;
	}
	nruns = 0;
	for (i = 0; i < npages; i++) {
		u64 gpa = (u64)page_to_pfn(pages[i]) << PAGE_SHIFT;

		if (nruns && runs[nruns - 1].gpa + runs[nruns - 1].len == gpa) {
			runs[nruns - 1].len += PAGE_SIZE;
			continue;
		}
		runs[nruns].gpa = gpa;
		runs[nruns].len = PAGE_SIZE;
		nruns++;
	}

	rec = kzalloc(sizeof(*rec), GFP_KERNEL);
	if (!rec) {
		ret = -ENOMEM;
		goto out_free_runs;
	}

	if (copy_to_user(u64_to_user_ptr(req.runs_ptr), runs,
			 (size_t)nruns * sizeof(*runs))) {
		ret = -EFAULT;
		goto out_free_rec;
	}
	req.runs_len = nruns;
	if (copy_to_user(uarg, &req, sizeof(req))) {
		ret = -EFAULT;
		goto out_free_rec;
	}

	rec->pages = pages;
	rec->npages = npages;
	mutex_lock(&ctx->lock);
	list_add_tail(&rec->node, &ctx->pins);
	mutex_unlock(&ctx->lock);

	kvfree(runs);
	return 0;

out_free_rec:
	kfree(rec);
out_free_runs:
	kvfree(runs);
out_unpin:
	if (done)
		unpin_user_pages(pages, done);
	kvfree(pages);
	return ret;
}

static long do_set_proc(void __user *uarg)
{
	struct nvrm_nodes_proc_req req;
	char *data;
	int ret;

	if (!capable(CAP_SYS_ADMIN))
		return -EPERM;
	if (copy_from_user(&req, uarg, sizeof(req)))
		return -EFAULT;
	if (!req.data_len || req.data_len > NVRM_NODES_PROC_DATA_MAX)
		return -EINVAL;
	req.name[NVRM_NODES_PROC_NAME_MAX - 1] = 0;
	if (!req.name[0] || strchr(req.name, '/'))
		return -EINVAL;
	if (!proc_nvidia_dir)
		return -ENODEV;

	data = kvmalloc(req.data_len, GFP_KERNEL);
	if (!data)
		return -ENOMEM;
	if (copy_from_user(data, u64_to_user_ptr(req.data_ptr), req.data_len)) {
		kvfree(data);
		return -EFAULT;
	}
	ret = proc_file_set(req.name, data, req.data_len);
	if (ret)
		kvfree(data);
	return ret;
}

static long nvrm_abi(struct file *filp, unsigned int cmd, unsigned long arg)
{
	struct nvrm_nodes_ctx *ctx = filp->private_data;
	void __user *uarg = (void __user *)arg;
	u32 v = NVRM_NODES_ABI_VERSION;

	switch (cmd) {
	case NVRM_NODES_IOC_VA2GPA:
		return do_va2gpa(ctx, uarg);
	case NVRM_NODES_IOC_SET_PROC:
		return do_set_proc(uarg);
	case NVRM_NODES_IOC_VERSION:
		return copy_to_user(uarg, &v, sizeof(v)) ? -EFAULT : 0;
	}
	return -ENOTTY;
}

static const struct file_operations nvrm_nodes_fops = {
	.owner = THIS_MODULE,
	.open = nvrm_nodes_open,
	.release = nvrm_nodes_release,
	.unlocked_ioctl = nvrm_abi,
	.compat_ioctl = compat_ptr_ioctl,
	/* no_llseek was DELETED in 6.12 ("fs: remove no_llseek"), and with it
	 * the meaning of a NULL .llseek changed: up to 6.11 NULL meant
	 * default_llseek, from 6.12 it means exactly what no_llseek used to.
	 * So the field is set on old kernels and left out on new ones -- the
	 * same node semantics on both, which is why this is a version guard
	 * and not a deletion. Measured 2026-08-18: without it the module does
	 * not compile against 6.18.44 (nixpkgs' default kernel),
	 * "'no_llseek' undeclared here"; the Ubuntu guests run 6.8 and take
	 * the other branch.
	 */
#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 12, 0)
	.llseek = no_llseek,
#endif
};

static struct miscdevice nvrm_nodes_misc = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "nvrm_nodes",
	.fops = &nvrm_nodes_fops,
	.mode = 0666,
};

/* ------------------------------------------------------------------ *
 * Setup and teardown
 * ------------------------------------------------------------------ */

static void nvrm_nodes_teardown(void)
{
	size_t i;

	for (i = 0; i < ARRAY_SIZE(nodes); i++) {
		if (nodes[i].dev) {
			device_destroy(nvrm_nodes_class, MKDEV(nodes[i].major, nodes[i].minor));
			nodes[i].dev = NULL;
		}
	}
	if (nvrm_nodes_class) {
		class_destroy(nvrm_nodes_class);
		nvrm_nodes_class = NULL;
	}
	for (i = 0; i < ARRAY_SIZE(ranges); i++) {
		if (ranges[i].registered) {
			__unregister_chrdev(ranges[i].major, ranges[i].baseminor,
					    ranges[i].count, ranges[i].name);
			ranges[i].registered = false;
		}
	}
	proc_files_free();
	if (proc_nvidia_dir) {
		proc_remove(proc_nvidia_dir);
		proc_nvidia_dir = NULL;
	}
}

static int __init nvrm_nodes_init(void)
{
	size_t i;
	int ret;

	ret = misc_register(&nvrm_nodes_misc);
	if (ret) {
		pr_err("nvrm_nodes: misc_register: %d\n", ret);
		return ret;
	}

	proc_nvidia_dir = proc_mkdir("driver/nvidia", NULL);
	if (!proc_nvidia_dir)
		pr_warn("nvrm_nodes: cannot create /proc/driver/nvidia -- params will be missing\n");

	if (create_nodes) {
		for (i = 0; i < ARRAY_SIZE(ranges); i++) {
			ret = __register_chrdev(ranges[i].major, ranges[i].baseminor,
						ranges[i].count, ranges[i].name,
						&placeholder_fops);
			if (ret) {
				pr_err("nvrm_nodes: chrdev %u:%u '%s': %d (is a real NVIDIA driver loaded?)\n",
				       ranges[i].major, ranges[i].baseminor,
				       ranges[i].name, ret);
				goto err;
			}
			ranges[i].registered = true;
		}

		nvrm_nodes_class = class_create("nvrm_nodes");
		if (IS_ERR(nvrm_nodes_class)) {
			ret = PTR_ERR(nvrm_nodes_class);
			nvrm_nodes_class = NULL;
			pr_err("nvrm_nodes: class_create: %d\n", ret);
			goto err;
		}
		nvrm_nodes_class->devnode = nvrm_nodes_devnode;

		for (i = 0; i < ARRAY_SIZE(nodes); i++) {
			nodes[i].dev = device_create(nvrm_nodes_class, NULL,
						     MKDEV(nodes[i].major, nodes[i].minor),
						     NULL, "%s", nodes[i].name);
			if (IS_ERR(nodes[i].dev)) {
				ret = PTR_ERR(nodes[i].dev);
				nodes[i].dev = NULL;
				pr_err("nvrm_nodes: device_create %s: %d\n", nodes[i].name, ret);
				goto err;
			}
		}
	}

	pr_info("nvrm_nodes: ready (ABI %d, nodes %s, max_pin %u MiB)\n",
		NVRM_NODES_ABI_VERSION, create_nodes ? "on" : "off", max_pin_mib);
	return 0;

err:
	nvrm_nodes_teardown();
	misc_deregister(&nvrm_nodes_misc);
	return ret;
}

static void __exit nvrm_nodes_exit(void)
{
	nvrm_nodes_teardown();
	misc_deregister(&nvrm_nodes_misc);
	pr_info("nvrm_nodes: unloaded\n");
}

module_init(nvrm_nodes_init);
module_exit(nvrm_nodes_exit);

MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("leandro nvrm_nodes: NVIDIA node placeholders, /proc/driver/nvidia, VA->GPA");
MODULE_AUTHOR("leandro");
MODULE_VERSION("1");
