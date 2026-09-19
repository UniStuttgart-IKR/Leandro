// SPDX-License-Identifier: GPL-2.0-only
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/* Forward NVIDIA RM open/ioctl/mmap calls over virtio-nvrm; guest userspace
 * stays unmodified and driver ABIs must match.
 * GET_TABLES supplies ioctl layouts keyed by (device type, ioctl number).
 * nvrm_wire.h supplies generated ABI constants.
 * With vdisplay=1, serve NVIDIA's displayless class and guest callbacks
 * through nvidia_get_rm_ops(). nvrm_nodes.ko supplies procfs with
 * create_nodes=0; this module owns the device nodes. See
 * docs/ARCHITECTURE.md and docs/DISPLAY.md. */

#include <linux/build_bug.h>
#include <linux/cdev.h>
#include <linux/device.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/highmem.h>
#include <linux/hrtimer.h>
#include <linux/kref.h>
#include <linux/list.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/mutex.h>
#include <linux/pci.h>
#include <linux/pid.h>
#include <linux/poll.h>
#include <linux/sched.h>
#include <linux/dma-buf.h>
#include <linux/scatterlist.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/version.h>
#include <linux/virtio.h>
#include <linux/virtio_config.h>
#include <linux/virtio_ring.h>
#include <linux/vmalloc.h>
#include <linux/wait.h>
#include <linux/workqueue.h>
#include <linux/xarray.h>

#include "nvrm_kapi.h"
#include "nvrm_wire.h"
#include "nvrm_identity.h"

/* Modern virtio-PCI supports device types 0..63 (PCI IDs 0x1040..0x107f).
 * Type 60 is the project's unassigned ID; keep it equal to the backend's ID
 * in crates/vhost-user-nvrm/src/nvrm.rs. */
#define VIRTIO_ID_NVRM 60

/* The shmid under which the host-visible window (the shared-memory
 * region the host places this guest's mappings into) lives. The Cloud
 * Hypervisor patch (patches/0001-generic-vhost-user-shmem.patch) assigns
 * shmids by region-list index; 1 matches the id virtio-gpu uses for its
 * HOST_VISIBLE region. */
#define NVRM_SHM_ID_HOST_VISIBLE 1

/* Device major/minor numbers expected by NVIDIA userspace: 195 for
 * nvidia/nvidiactl, 235 for nvidia-uvm on the measured host. */
#define NV_FRONTEND_MAJOR 195
#define NV_UVM_MAJOR 235
#define NV_MINOR_CTL 255
#define NV_MAX_GPUS 8

/* Pages per pin_user_pages_fast round: bounds the latency and allocation
 * size of a single call without capping the total length. */
#define PIN_CHUNK_PAGES 4096

/* Bound noninterruptible teardown waits, including vm_ops->close. On
 * timeout, the callback retains any submitted request. */
#define NVRM_TEARDOWN_TIMEOUT (10 * HZ)

static bool create_nodes = true;
module_param(create_nodes, bool, 0444);
MODULE_PARM_DESC(
	create_nodes,
	"Create the NVIDIA nodes (default: yes). Off when nvrm_nodes.ko holds them");

static unsigned int gpu_count = 1;
module_param(gpu_count, uint, 0444);
MODULE_PARM_DESC(gpu_count, "Number of /dev/nvidiaN nodes (default 1)");

/* Report the mediating PCI function's guest BDF instead of the host GPU's
 * BDF. Rewrite gpuId consistently: ((domain & 0xffff) << 16) | (bus << 8) |
 * device (gpuGenerate32BitId, gpu.c:292).
 * Read the address from the parent PCI function; the virtio device type
 * determines its device ID, not its bus address.
 * Default off: lazy host-ID discovery breaks the first nvidia-smi
 * enumeration after load (2026-08-08). The display rig enables mediation;
 * eager discovery at probe remains needed. */
static unsigned int bdf_mediation;
module_param(bdf_mediation, uint, 0644);
MODULE_PARM_DESC(
	bdf_mediation,
	"report the guest's own PCI address for the card in every RM answer (default 0 = off, see the comment)");

/* Diagnostic scan of all control replies for the host gpuId. Disabled by
 * default because it reads every params buffer. */
static unsigned int bdf_debug;
module_param(bdf_debug, uint, 0644);
MODULE_PARM_DESC(
	bdf_debug,
	"log every control reply that still carries the host's gpuId (default 0)");

/* Enable kernel RM operations needed by NVKMS/nvidia-drm. The display rig
 * and display gate set display=1; compute guests leave it disabled. */
static unsigned int display;
module_param(display, uint, 0644);
MODULE_PARM_DESC(
	display,
	"serve the kernel-path RM operations a display needs (0 = off, 1 = on, 2 = on and verbose)");

/* Serve NVA083_GRID_DISPLAYLESS locally: head count, resolution limits and
 * EDID describe a virtual monitor with no host display state.
 * Replace NV04_DISPLAY_COMMON in GET_CLASSLIST so nvRmAllocDisplays selects
 * this path (nvkms-rm.c:1819). The class count stays unchanged. Requires
 * display=1. */
static unsigned int vdisplay;
module_param(vdisplay, uint, 0644);
MODULE_PARM_DESC(
	vdisplay,
	"present a virtual display to NVKMS via NVA083_GRID_DISPLAYLESS (0 = off, 1 = on). Needs display=1: the kernel-path RM operations a display uses are refused without it");

static unsigned int vdisplay_width = 1920;
module_param(vdisplay_width, uint, 0644);
MODULE_PARM_DESC(vdisplay_width, "width of the virtual display (default 1920)");

static unsigned int vdisplay_height = 1080;
module_param(vdisplay_height, uint, 0644);
MODULE_PARM_DESC(vdisplay_height,
		 "height of the virtual display (default 1080)");

/* GET_MAX_RESOLUTION bounds every NVKMS surface, including offscreen
 * buffers (nvkms-rm.c:1409). Keep these limits separate from the offered
 * mode.
 * Defaults match unlicensed Linux passthrough (objgriddisplayless.c:38-54).
 * maxPixels is independent: 4096000 permits 2560x1600, not 4K. Raise both
 * bounds when needed. */
static unsigned int vdisplay_max_width = 2560;
module_param(vdisplay_max_width, uint, 0644);
MODULE_PARM_DESC(
	vdisplay_max_width,
	"maximum width NVKMS may use (default 2560, NVIDIA's Linux displayless limit)");

static unsigned int vdisplay_max_height = 1600;
module_param(vdisplay_max_height, uint, 0644);
MODULE_PARM_DESC(
	vdisplay_max_height,
	"maximum height NVKMS may use (default 1600, NVIDIA's Linux displayless limit)");

static unsigned int vdisplay_max_pixels = 4096000;
module_param(vdisplay_max_pixels, uint, 0644);
MODULE_PARM_DESC(
	vdisplay_max_pixels,
	"maximum pixel count, a bound of its own next to the resolution (default 4096000 = 2560x1600)");

/* The raster rate of a raster generator that does not exist. The EDID this
 * module invents advertises 60 Hz, and the callbacks served from it (see the
 * vblank section) fire at this rate. Writable for experiments. */
static unsigned int vdisplay_vblank_hz = 60;
module_param(vdisplay_vblank_hz, uint, 0644);
MODULE_PARM_DESC(vdisplay_vblank_hz,
		 "rate of the virtual display's vblank callbacks (default 60)");

/* Hold VRAM inside the guest's quota; release chunks and retry when NVKMS
 * gets NV_ERR_NO_MEMORY for a display buffer.
 * Measured 2026-09-17 on .23 (4Q, 2816 MiB FB): 7/9 SotTR runs stalled
 * after refusal of an 8.4 MiB Xwayland scanout. Scanout has no sysmem
 * fallback (nvidia-drm-gem-nvkms-memory.c:654-673).
 * -1: auto reserve with display enabled, five scanouts plus 1 MiB of
 * cursors. Measured peaks: 44/76/161 MiB at 1080p/1440p/4K; see
 * nvrm_vram.c.
 * 0: disabled. Positive values: explicit MiB. Runtime writes release and
 * refill the balloon, also applying changed display dimensions. Allocations
 * use a separate nvrm-balloon client and remain charged by the host ledger. */
static int display_reserve_mib = -1;
static int display_reserve_set(const char *val, const struct kernel_param *kp);
static const struct kernel_param_ops display_reserve_ops = {
	.set = display_reserve_set,
	.get = param_get_int,
};
module_param_cb(display_reserve_mib, &display_reserve_ops, &display_reserve_mib,
		0644);
MODULE_PARM_DESC(
	display_reserve_mib,
	"MiB of VRAM the module holds and gives back when NVKMS is refused a display buffer (-1 = auto from vdisplay_width/height when display is on, 0 = off, >0 = fixed; default -1)");

/* Scan replies for advertised FB size and optional physical-card size, in
 * KiB and bytes. Log once per (command, offset, form, process);
 * vram_debug=2 logs every hit with rate limiting. Also log
 * NVOS32_FUNCTION_INFO total/free, which bypass FB_GET_INFO rewriting.
 * Hits are diagnostic candidates, not proof of a size field. Disabled by
 * default because every reply is scanned. */
static unsigned int vram_debug;
module_param(vram_debug, uint, 0644);
MODULE_PARM_DESC(
	vram_debug,
	"log every RM answer that carries the advertised FB size (1 = once per command, offset and process, 2 = every hit; default 0)");

static unsigned int vram_debug_card_mib;
module_param(vram_debug_card_mib, uint, 0644);
MODULE_PARM_DESC(
	vram_debug_card_mib,
	"with vram_debug: also look for this physical card size in MiB (default 0 = do not)");

static unsigned long stat_vblank_fired;
module_param(stat_vblank_fired, ulong, 0444);
MODULE_PARM_DESC(stat_vblank_fired,
		 "vblank callback invocations served from the virtual display");

/* Queue-1 event counters: registered counts 0x7e callback slots; delivered
 * counts 0x79 wakes and 0x7e calls; dropped counts overflow,
 * missing/mismatched owners, and unsupported notifiers. delivered should
 * advance during vkprobe --present. */
static unsigned long stat_events_delivered;
module_param(stat_events_delivered, ulong, 0444);
MODULE_PARM_DESC(
	stat_events_delivered,
	"host events handed on: fd wake-ups plus kernel-callback invocations");

static unsigned long stat_events_dropped;
module_param(stat_events_dropped, ulong, 0444);
MODULE_PARM_DESC(
	stat_events_dropped,
	"host events with nowhere to go, all reasons (see the four below)");
/* Separate drop reasons distinguish ring pressure from intentionally
 * filtered host-display events, such as DP_IRQ. */
static unsigned long stat_events_drop_ringfull;
module_param(stat_events_drop_ringfull, ulong, 0444);
MODULE_PARM_DESC(
	stat_events_drop_ringfull,
	"dropped: guest ring full before the workqueue drained it -- the one that costs frames");
static unsigned long stat_events_drop_filtered;
module_param(stat_events_drop_filtered, ulong, 0444);
MODULE_PARM_DESC(
	stat_events_drop_filtered,
	"dropped: DP_IRQ/HDMI/LPWR notifiers of the HOST display, filtered on purpose");
static unsigned long stat_events_drop_noslot;
module_param(stat_events_drop_noslot, ulong, 0444);
MODULE_PARM_DESC(
	stat_events_drop_noslot,
	"dropped: no callback slot for (client, hEvent) -- registration missed or freed");
static unsigned long stat_events_drop_class;
module_param(stat_events_drop_class, ulong, 0444);
MODULE_PARM_DESC(stat_events_drop_class,
		 "dropped: class 0x78 or unknown -- not callable in the guest");

static unsigned long stat_events_registered;
module_param(stat_events_registered, ulong, 0444);
MODULE_PARM_DESC(stat_events_registered,
		 "kernel-callback events (0x7e) this module holds a slot for");

/* Count open struct files, including those retained by VMAs after their FDs
 * close. Compare this with backend mirrors; per-process FD counts alone
 * undercount guest ownership (OPEN-QUESTIONS 31). */
static unsigned long stat_ctx_open;
module_param(stat_ctx_open, ulong, 0444);
MODULE_PARM_DESC(stat_ctx_open,
		 "device-node contexts (struct file) currently open");
static unsigned long stat_ctx_opened;
module_param(stat_ctx_opened, ulong, 0444);
MODULE_PARM_DESC(stat_ctx_opened, "device-node contexts ever opened");
static unsigned long stat_ctx_closed;
module_param(stat_ctx_closed, ulong, 0444);
MODULE_PARM_DESC(stat_ctx_closed,
		 "device-node contexts ever released (KIND_CLOSE sent)");

/* Semaphore-surface waiter counters: waiters should return to idle after
 * compositor exit; fired should advance once per frame. */
static unsigned long stat_semsurf_waiters;
module_param(stat_semsurf_waiters, ulong, 0444);
MODULE_PARM_DESC(
	stat_semsurf_waiters,
	"semaphore-surface waiter slots currently armed (control 0xda0003)");
/* Cancellation refusals after an RM waiter fired. semsurf_after_control
 * retires these slots to suppress stale callbacks. */
static unsigned long stat_semsurf_late_unreg;
module_param(stat_semsurf_late_unreg, ulong, 0444);
MODULE_PARM_DESC(
	stat_semsurf_late_unreg,
	"unregisters that lost the race with the firing (slot retired anyway)");
static unsigned long stat_semsurf_fired;
module_param(stat_semsurf_fired, ulong, 0444);
MODULE_PARM_DESC(stat_semsurf_fired,
		 "semaphore-surface waiter callbacks invoked");

static unsigned int max_pin_mib = 1024;
module_param(max_pin_mib, uint, 0644);
MODULE_PARM_DESC(
	max_pin_mib,
	"Upper bound on concurrently pinned guest memory in MiB (default 1024)");

/* Read-only pin/pool counters in /sys/module/virtio_nvrm/parameters/. */
static unsigned long stat_pinned_kib;
module_param(stat_pinned_kib, ulong, 0444);
MODULE_PARM_DESC(stat_pinned_kib, "Guest memory currently pinned, in KiB");

static unsigned long stat_osdesc_pins;
module_param(stat_osdesc_pins, ulong, 0444);
MODULE_PARM_DESC(stat_osdesc_pins,
		 "OS descriptors whose pages were resolved here");

static unsigned long stat_pool_pages;
module_param(stat_pool_pages, ulong, 0444);
MODULE_PARM_DESC(stat_pool_pages, "Pages owned by this module for UVM pools");

#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 11, 0)
#define nvrm_fd_file(f) ((f).file)
#else
#define nvrm_fd_file(f) fd_file(f)
#endif

/* Parse and validate host tables. nvrm_tables.c is shared with userspace C
 * tests; tools/check.sh compares its interpretation with the real
 * Rust-generated table stream. */

#include "nvrm_tables.c"

/* Device */

/* Receive buffers pre-posted on the event queue. The host writes ONE
 * `struct nvrm_req` per firing (KIND_EVENT_FIRED) and never waits: no free
 * buffer on its side means the event is dropped and counted there. */
#define NVRM_EVQ_BUFS 256u
/* Pending event ring, power-of-two sized. Overflow drops a firing and
 * leaves the caller's 10 ms fallback poll.
 * Measured 2026-08-15: 128 slots lost 106k events in CS2; 1024 handled
 * 34,500 events/s during play but lost about 10k during session startup.
 * 8192 x 160 B = 1.25 MiB absorbs that startup burst. */
#define NVRM_EV_RING 8192u

struct nvrm_dev {
	struct kref ref;
	struct virtio_device *vdev;
	bool stopping;
	unsigned int vq_free;
	struct mutex quarantine_lock;
	struct list_head quarantined_pins;
	struct list_head quarantined_pools;
	struct virtqueue *vq;
	struct workqueue_struct *release_wq;
	/* Protects the virtqueue AND every request's done/abandoned fields.
	 * Spinlock, because the callback arrives from interrupt context. */
	spinlock_t vq_lock;

	/* Queue 1 delivers host RM events. NULL in one-queue mode, which
	 * has no event-driven fd wakes or kernel callbacks. */
	struct virtqueue *evq;
	/* Protect evq and its pending ring with irqsave in both IRQ and
	 * workqueue context. Queue 0 uses the separate vq_lock. */
	spinlock_t evq_lock;
	struct nvrm_req ev_ring[NVRM_EV_RING];
	unsigned int ev_head,
		ev_tail; /* under evq_lock; tail - head = filled */
	/* Hands the ring on in process context. Nothing is CALLED from the
	 * IRQ callback: neither NVKMS' callbacks nor a wait-queue lookup. */
	struct work_struct events_work;
	/* Index userspace contexts by (proc id << 32 | token). Tokens are
	 * session-local; the 64-bit XArray key includes their owner. */
	struct xarray ctx_xa;
	/* Waiters for a free descriptor slot. */
	wait_queue_head_t vq_space;
	atomic_t seq;
	/* Inflight request count used by remove(). */
	atomic_t inflight;
	wait_queue_head_t drain;

	/* Host-visible window: guest-physical memory, filled by the host. */
	u64 win_base;
	u64 win_len;
	unsigned long *win_bitmap; /* one page per bit */
	struct mutex win_lock;

	struct nvrm_tables tbl;

	/* Guest processes with this device open, each with one host session. */
	struct list_head procs;
	struct mutex proc_lock;

	/* Learn the host gpuId from enumeration. Disable BDF mediation if a
	 * second GPU ID appears: one virtio PCI function cannot represent
	 * two GPU addresses. */
	u32 bdf_guest_id; /* 0 = no PCI parent, mediation impossible */
	u32 bdf_host_id; /* 0 = not learned yet */
	bool bdf_disabled; /* more than one GPU seen */
	u16 bdf_domain;
	u8 bdf_bus, bdf_slot, bdf_func;

	/* The FB size the host advertises this guest, in KB, as the last
	 * FB_GET_INFO answer said. 0 = not seen yet. Only vram_debug reads it. */
	u32 vram_fb_kb;
};

/* One host session per struct pid identity. vnr is the guest-visible PID. */
struct nvrm_proc {
	struct nvrm_dev *dev;
	struct list_head node;
	struct pid *pid;
	u32 id;
	u32 vnr;
	char comm[TASK_COMM_LEN];
	refcount_t ref;
};

/* One virtio device owns the global /dev/nvidia* nodes. */
static struct nvrm_dev *nvrm;
static DEFINE_MUTEX(nvrm_device_lock);
static bool nvrm_bound;

static void nvrm_dev_release(struct kref *ref)
{
	struct nvrm_dev *dev = container_of(ref, struct nvrm_dev, ref);

	WARN_ON(!list_empty(&dev->procs));
	WARN_ON(!xa_empty(&dev->ctx_xa));
	kvfree(dev->tbl.blob);
	bitmap_free(dev->win_bitmap);
	xa_destroy(&dev->ctx_xa);
	mutex_destroy(&dev->proc_lock);
	mutex_destroy(&dev->win_lock);
	mutex_destroy(&dev->quarantine_lock);
	put_device(&dev->vdev->dev);
	kfree(dev);
}

static void nvrm_dev_put(struct nvrm_dev *dev)
{
	if (dev)
		kref_put(&dev->ref, nvrm_dev_release);
}

/* Publication and reference acquisition share this lock with remove. */
static struct nvrm_dev *nvrm_dev_get(void)
{
	struct nvrm_dev *dev;

	mutex_lock(&nvrm_device_lock);
	dev = nvrm;
	if (dev)
		kref_get(&dev->ref);
	mutex_unlock(&nvrm_device_lock);
	return dev;
}

/* Preserve session identity across virtio rebind within one module load.
 * Module reload still requires a fresh backend session; IDs restart then. */
static DEFINE_MUTEX(proc_id_lock);
static u32 last_proc_id;

static int nvrm_proc_alloc_id(u32 *id)
{
	int ret;

	mutex_lock(&proc_id_lock);
	ret = nvrm_next_process_id(&last_proc_id, id);
	mutex_unlock(&proc_id_lock);
	return ret;
}

/* One quota covers pinned application pages and module-owned pool pages
 * across all contexts. */
static atomic_long_t held_pages = ATOMIC_LONG_INIT(0);
static atomic_long_t quarantined_pages = ATOMIC_LONG_INIT(0);

static int nvrm_quarantined_pages_get(char *buf, const struct kernel_param *kp)
{
	return scnprintf(buf, PAGE_SIZE, "%ld\n",
			 atomic_long_read(&quarantined_pages));
}

static const struct kernel_param_ops quarantine_param_ops = {
	.get = nvrm_quarantined_pages_get,
};
module_param_cb(stat_quarantined_pages, &quarantine_param_ops, NULL, 0444);
MODULE_PARM_DESC(
	stat_quarantined_pages,
	"Charged pages retained after uncertain host completion; cleared by guest restart");

/* No native final-release notification exists. Retain exceptional backing,
 * its device and this module until guest restart, under the existing quota. */
static void nvrm_quarantine(struct nvrm_dev *dev, struct list_head *node,
			    struct list_head *list, unsigned long npages)
{
	kref_get(&dev->ref);
	mutex_lock(&dev->quarantine_lock);
	list_add_tail(node, list);
	mutex_unlock(&dev->quarantine_lock);
	atomic_long_add(npages, &quarantined_pages);
	pr_warn_ratelimited(
		"virtio_nvrm: retained %lu uncertain backing pages until guest restart (%ld total)\n",
		npages, atomic_long_read(&quarantined_pages));
}

/* Charge the quota. 0 = ok, otherwise -ENOMEM (with a message naming the knob). */
static int nvrm_charge(unsigned long npages)
{
	long limit = (long)max_pin_mib << (20 - PAGE_SHIFT);

	if (atomic_long_add_return(npages, &held_pages) > limit) {
		atomic_long_sub(npages, &held_pages);
		pr_warn_ratelimited(
			"virtio_nvrm: %lu pages would blow the limit of %u MiB -- knob: module parameter max_pin_mib\n",
			npages, max_pin_mib);
		return -ENOMEM;
	}
	return 0;
}

static void nvrm_uncharge(unsigned long npages)
{
	atomic_long_sub(npages, &held_pages);
}

/* One round trip over the virtqueue */

struct nvrm_xfer {
	struct nvrm_dev *dev;
	struct work_struct release_work;
	int transport_error;
	void *req;
	size_t req_cap;
	size_t req_len;
	void *rsp;
	size_t rsp_cap;
	unsigned int rsp_len;
	struct scatterlist *sg_req;
	struct scatterlist *sg_rsp;
	unsigned int n_req, n_rsp;
	bool done;
	bool abandoned;
	wait_queue_head_t wq;
};

static void nvrm_xfer_free(struct nvrm_xfer *x)
{
	if (!x)
		return;
	kvfree(x->req);
	kvfree(x->rsp);
	kfree(x->sg_req);
	kfree(x->sg_rsp);
	nvrm_dev_put(x->dev);
	kfree(x);
}

/* kvfree and final device release require process context. */
static void nvrm_xfer_release_work(struct work_struct *work)
{
	nvrm_xfer_free(container_of(work, struct nvrm_xfer, release_work));
}

/* Maximum scatterlist entries; vmalloc-backed buffers split at page
 * boundaries. */
static unsigned int nvrm_sg_max(size_t len)
{
	return (unsigned int)(len / PAGE_SIZE) + 2;
}

/* Build a scatterlist over a kvmalloc buffer. Returns the number of entries
 * used. */
static unsigned int nvrm_sg_fill(struct scatterlist *sg, unsigned int max,
				 void *buf, size_t len)
{
	unsigned int n = 0;

	sg_init_table(sg, max);
	if (!is_vmalloc_addr(buf)) {
		/* kmalloc: physically contiguous, one entry is enough. */
		sg_set_buf(&sg[0], buf, len);
		n = 1;
	} else {
		size_t done = 0;

		while (done < len) {
			size_t off = offset_in_page((char *)buf + done);
			size_t take = min(len - done, PAGE_SIZE - off);

			if (WARN_ON_ONCE(n >= max))
				break;
			sg_set_page(&sg[n], vmalloc_to_page((char *)buf + done),
				    take, off);
			n++;
			done += take;
		}
	}
	/* Allocation and request validation guarantee a nonempty buffer.
	 * Guard sg[n - 1] if another caller violates that contract. */
	if (WARN_ON_ONCE(!n))
		return 0;
	sg_mark_end(&sg[n - 1]);
	return n;
}

static struct nvrm_xfer *nvrm_xfer_alloc(size_t req_cap, size_t rsp_cap)
{
	struct nvrm_xfer *x;

	if (req_cap > NVRM_MAX_MSG || rsp_cap > NVRM_MAX_MSG)
		return ERR_PTR(-EMSGSIZE);
	if (req_cap < sizeof(struct nvrm_req) ||
	    rsp_cap < sizeof(struct nvrm_rsp))
		return ERR_PTR(-EINVAL);

	x = kzalloc(sizeof(*x), GFP_KERNEL);
	if (!x)
		return ERR_PTR(-ENOMEM);
	init_waitqueue_head(&x->wq);
	INIT_WORK(&x->release_work, nvrm_xfer_release_work);
	x->req_cap = req_cap;
	x->rsp_cap = rsp_cap;
	x->req = kvzalloc(req_cap, GFP_KERNEL);
	x->rsp = kvzalloc(rsp_cap, GFP_KERNEL);
	x->sg_req = kmalloc_array(nvrm_sg_max(req_cap), sizeof(*x->sg_req),
				  GFP_KERNEL);
	x->sg_rsp = kmalloc_array(nvrm_sg_max(rsp_cap), sizeof(*x->sg_rsp),
				  GFP_KERNEL);
	if (!x->req || !x->rsp || !x->sg_req || !x->sg_rsp) {
		nvrm_xfer_free(x);
		return ERR_PTR(-ENOMEM);
	}
	x->req_len = req_cap;
	return x;
}

static void nvrm_vq_cb(struct virtqueue *vq)
{
	struct nvrm_dev *dev = vq->vdev->priv;
	struct nvrm_xfer *x;
	unsigned int len;
	unsigned long flags;
	bool woke = false;

	spin_lock_irqsave(&dev->vq_lock, flags);
	while ((x = virtqueue_get_buf(vq, &len)) != NULL) {
		x->rsp_len = len;
		WRITE_ONCE(x->done, true);
		if (x->abandoned)
			queue_work(dev->release_wq, &x->release_work);
		else
			wake_up(&x->wq);
		atomic_dec(&dev->inflight);
		woke = true;
	}
	WRITE_ONCE(dev->vq_free, vq->num_free);
	spin_unlock_irqrestore(&dev->vq_lock, flags);
	if (woke) {
		wake_up(&dev->vq_space);
		wake_up(&dev->drain);
	}
}

/* Defined in the event section, below the vblank engine it is modelled on. */
static void nvrm_events_work(struct work_struct *work);

/* Queue-1 IRQ callback, under evq_lock. Copy firings into the ring and
 * immediately repost the existing inbuf with GFP_ATOMIC.
 * Workqueue processing performs XArray lookups, fd wakes and NVKMS
 * callbacks outside IRQ context and the virtqueue lock. Ring overflow drops
 * and counts events; this callback never waits. */
static void nvrm_evq_cb(struct virtqueue *vq)
{
	struct nvrm_dev *dev = vq->vdev->priv;
	struct scatterlist sg;
	unsigned long flags;
	unsigned int len;
	void *buf;
	bool queued = false;

	spin_lock_irqsave(&dev->evq_lock, flags);
	while ((buf = virtqueue_get_buf(vq, &len)) != NULL) {
		const struct nvrm_req *r = buf;

		if (len >= sizeof(*r) && r->kind == NVRM_KIND_EVENT_FIRED) {
			if (dev->ev_tail - dev->ev_head < NVRM_EV_RING) {
				dev->ev_ring[dev->ev_tail & (NVRM_EV_RING - 1)] =
					*r;
				dev->ev_tail++;
				queued = true;
			} else {
				stat_events_dropped++;
				stat_events_drop_ringfull++;
			}
		}
		/* Repost the same inbuf. Free it if reposting fails;
		 * remove() only detaches buffers still owned by the queue. */
		sg_init_one(&sg, buf, sizeof(struct nvrm_req));
		if (virtqueue_add_inbuf(vq, &sg, 1, buf, GFP_ATOMIC) < 0)
			kfree(buf);
	}
	virtqueue_kick(vq);
	spin_unlock_irqrestore(&dev->evq_lock, flags);

	if (queued)
		queue_work(system_highpri_wq, &dev->events_work);
}

/* Queue and wait. User calls wait killably; kernel calls have a timeout.
 * On abandonment, clear *owned before the callback can free the request.
 * On every other return, the caller still owns and must free *owned. */
static int nvrm_xfer_run(struct nvrm_dev *dev, struct nvrm_xfer **owned,
			 bool interruptible, bool *submitted)
{
	struct nvrm_xfer *x = *owned;
	struct scatterlist *sgs[2];
	unsigned long flags;
	int err;

	if (submitted)
		*submitted = false;
	if (x->req_len < sizeof(struct nvrm_req) || x->req_len > x->req_cap)
		return -EINVAL;

	x->n_req = nvrm_sg_fill(x->sg_req, nvrm_sg_max(x->req_cap), x->req,
				x->req_len);
	x->n_rsp = nvrm_sg_fill(x->sg_rsp, nvrm_sg_max(x->rsp_cap), x->rsp,
				x->rsp_cap);
	sgs[0] = x->sg_req;
	sgs[1] = x->sg_rsp;

	for (;;) {
		spin_lock_irqsave(&dev->vq_lock, flags);
		if (dev->stopping) {
			spin_unlock_irqrestore(&dev->vq_lock, flags);
			return -ENODEV;
		}
		err = virtqueue_add_sgs(dev->vq, sgs, 1, 1, x, GFP_ATOMIC);
		WRITE_ONCE(dev->vq_free, dev->vq->num_free);
		if (!err) {
			kref_get(&dev->ref);
			x->dev = dev;
			if (submitted)
				*submitted = true;
			atomic_inc(&dev->inflight);
			virtqueue_kick(dev->vq);
		}
		spin_unlock_irqrestore(&dev->vq_lock, flags);
		if (err != -ENOSPC)
			break;
		/* Queue full: wait until a slot frees up. */
		if (interruptible) {
			if (wait_event_interruptible(
				    dev->vq_space,
				    READ_ONCE(dev->stopping) ||
					    READ_ONCE(dev->vq_free) > 0))
				return -ERESTARTSYS;
		} else if (!wait_event_timeout(dev->vq_space,
					       READ_ONCE(dev->stopping) ||
						       READ_ONCE(dev->vq_free) >
							       0,
					       NVRM_TEARDOWN_TIMEOUT)) {
			return -ETIMEDOUT;
		}
	}
	if (err)
		return err;

	if (interruptible) {
		/* The host may already have executed this request. Ordinary signals
		 * must not restart the ioctl: ATTACH_GPUS_TO_FD, for example,
		 * rejects a second attach (nv.c, nvlfp->num_attached_gpus).
		 * Only fatal signals may abandon a submitted user request.
		 * The queue-full wait remains interruptible because it submits
		 * nothing before returning. See docs/OPEN-QUESTIONS.md, item 45. */
		if (wait_event_killable(x->wq, READ_ONCE(x->done))) {
			/* Transfer ownership only while the submitted
			 * request is unfinished. */
			spin_lock_irqsave(&dev->vq_lock, flags);
			if (!x->done) {
				x->abandoned = true;
				*owned = NULL;
				spin_unlock_irqrestore(&dev->vq_lock, flags);
				return -ERESTARTSYS;
			}
			spin_unlock_irqrestore(&dev->vq_lock, flags);
			/* Lost the race: the reply did arrive after all. */
		}
	} else if (!wait_event_timeout(x->wq, READ_ONCE(x->done),
				       NVRM_TEARDOWN_TIMEOUT)) {
		spin_lock_irqsave(&dev->vq_lock, flags);
		if (!x->done) {
			x->abandoned = true;
			*owned = NULL;
			spin_unlock_irqrestore(&dev->vq_lock, flags);
			pr_warn("virtio_nvrm: host does not answer -- teardown request abandoned\n");
			return -ETIMEDOUT;
		}
		spin_unlock_irqrestore(&dev->vq_lock, flags);
	}

	/* done may be visible before the callback finishes using x and its wait
	 * queue. Wait for that critical section before returning ownership. */
	spin_lock_irqsave(&dev->vq_lock, flags);
	spin_unlock_irqrestore(&dev->vq_lock, flags);

	if (x->transport_error)
		return x->transport_error;
	if (x->rsp_len < sizeof(struct nvrm_rsp) || x->rsp_len > x->rsp_cap) {
		pr_warn_ratelimited(
			"virtio_nvrm: reply length %u outside [%zu, %zu]\n",
			x->rsp_len, sizeof(struct nvrm_rsp), x->rsp_cap);
		return -EIO;
	}
	return 0;
}

/* Prepare a request header. `guest_proc` is the dense id of the guest process
 * doing the talking (0 = device-wide request without an owner, e.g. HELLO or
 * GET_TABLES). */
static struct nvrm_req *nvrm_req_init(struct nvrm_dev *dev, struct nvrm_xfer *x,
				      u32 kind, u32 guest_proc)
{
	struct nvrm_req *r = x->req;

	memset(r, 0, sizeof(*r));
	r->seq = (u32)atomic_inc_return(&dev->seq);
	r->kind = kind;
	r->guest_proc = guest_proc;
	r->fd_field_off = NVRM_NONE_U32;
	r->fd_field_token = NVRM_NONE_U64;
	/* Explicit, like aux_fd_field_proc below: memset would leave 0, and 0
	 * is a live session id. */
	r->fd_field_proc = NVRM_NONE_U32;
	r->embedded_ptr_off = NVRM_NONE_U32;
	r->aux_fd_field_off = NVRM_NONE_U32;
	r->aux_fd_field_token = NVRM_NONE_U64;
	/* Explicit: the memset above would leave 0, and 0 is a session id. */
	r->aux_fd_field_proc = NVRM_NONE_U32;
	return r;
}

/* A small request with at most a short payload (`info`, currently only the
 * process description sent on open); the reply is the header alone. */
static int nvrm_simple_info(struct nvrm_dev *dev, u32 kind, u32 dev_tag,
			    u32 ioctl_nr, u64 target_token, u64 addr,
			    u64 map_len, u64 *token_out, bool interruptible,
			    u32 guest_proc, const struct nvrm_proc_info *info)
{
	struct nvrm_xfer *x;
	struct nvrm_req *r;
	struct nvrm_rsp *rsp;
	size_t len = sizeof(*r) + (info ? sizeof(*info) : 0);
	int ret;

	x = nvrm_xfer_alloc(len, sizeof(*rsp));
	if (IS_ERR(x))
		return PTR_ERR(x);
	r = nvrm_req_init(dev, x, kind, guest_proc);
	r->dev_tag = dev_tag;
	r->ioctl_nr = ioctl_nr;
	r->target_token = target_token;
	r->addr = addr;
	r->map_len = map_len;
	if (info) {
		memcpy((u8 *)x->req + sizeof(*r), info, sizeof(*info));
		r->inline_len = sizeof(*info);
	}
	x->req_len = len;

	ret = nvrm_xfer_run(dev, &x, interruptible, NULL);
	if (ret) {
		nvrm_xfer_free(x);
		return ret;
	}

	rsp = x->rsp;
	ret = rsp->ret;
	if (token_out)
		*token_out = rsp->token;
	nvrm_xfer_free(x);
	return ret;
}

/* A small request without payload; the reply is the header alone. */
static int nvrm_simple(struct nvrm_dev *dev, u32 kind, u32 dev_tag,
		       u32 ioctl_nr, u64 target_token, u64 addr, u64 map_len,
		       u64 *token_out, bool interruptible, u32 guest_proc)
{
	return nvrm_simple_info(dev, kind, dev_tag, ioctl_nr, target_token,
				addr, map_len, token_out, interruptible,
				guest_proc, NULL);
}

/* Fetch and parse the tables */

static int nvrm_fetch_tables(struct nvrm_dev *dev)
{
	struct nvrm_tables *t = &dev->tbl;
	const size_t chunk = 4096;
	u64 total = 0;
	size_t got = 0;
	int ret = 0;

	do {
		struct nvrm_xfer *x;
		struct nvrm_req *r;
		struct nvrm_rsp *rsp;
		size_t n;

		x = nvrm_xfer_alloc(sizeof(*r), sizeof(*rsp) + chunk);
		if (IS_ERR(x)) {
			ret = PTR_ERR(x);
			goto fail;
		}
		r = nvrm_req_init(dev, x, NVRM_KIND_GET_TABLES, 0);
		r->addr = got;
		r->map_len = chunk;
		x->req_len = sizeof(*r);

		ret = nvrm_xfer_run(dev, &x, false, NULL);
		if (ret) {
			nvrm_xfer_free(x);
			goto fail;
		}
		rsp = x->rsp;
		if (rsp->ret != 0) {
			pr_err("virtio_nvrm: GET_TABLES rejected: %d\n",
			       rsp->ret);
			ret = -EIO;
			nvrm_xfer_free(x);
			goto fail;
		}
		if (!total) {
			total = rsp->token;
			if (total < sizeof(struct nvrm_table_hdr) ||
			    total > SZ_1M) {
				pr_err("virtio_nvrm: implausible table length %llu\n",
				       total);
				ret = -EPROTO;
				nvrm_xfer_free(x);
				goto fail;
			}
			t->blob = kvzalloc(total, GFP_KERNEL);
			if (!t->blob) {
				ret = -ENOMEM;
				nvrm_xfer_free(x);
				goto fail;
			}
			t->len = total;
		}
		n = rsp->inline_len;
		if (!n || got + n > total || sizeof(*rsp) + n > x->rsp_len) {
			pr_err("virtio_nvrm: table chunk unusable (%zu @%zu of %llu)\n",
			       n, got, total);
			ret = -EPROTO;
			nvrm_xfer_free(x);
			goto fail;
		}
		memcpy((u8 *)t->blob + got, (u8 *)x->rsp + sizeof(*rsp), n);
		got += n;
		nvrm_xfer_free(x);
	} while (got < total);

	/* Validate the complete stream using the parser shared with
	 * userspace tests. */
	{
		const char *why;

		ret = nvrm_tables_parse(t, &why);
		if (ret) {
			pr_err("virtio_nvrm: tables rejected: %s\n", why);
			goto fail;
		}
	}

	pr_info("virtio_nvrm: tables v%u accepted -- %zu bytes, checksum %#010x (%u ioctls, %u classes, %u controls, %u nested)\n",
		t->hdr.table_version, t->len, t->hdr.checksum, t->hdr.n_ioctl,
		t->hdr.n_class, t->hdr.n_ctrl, t->hdr.n_nested);
	return 0;

fail:
	kvfree(t->blob);
	memset(t, 0, sizeof(*t));
	return ret;
}

/* The window: the guest manages the space, the host fills it */

static long win_alloc(struct nvrm_dev *dev, size_t len)
{
	unsigned long npages = len >> PAGE_SHIFT;
	unsigned long total = dev->win_len >> PAGE_SHIFT;
	unsigned long start;

	if (!dev->win_bitmap || !npages)
		return -ENOMEM;
	mutex_lock(&dev->win_lock);
	start = bitmap_find_next_zero_area(dev->win_bitmap, total, 0, npages,
					   0);
	if (start >= total) {
		unsigned long used = bitmap_weight(dev->win_bitmap, total);

		mutex_unlock(&dev->win_lock);
		/* Refusal ledger: this one was SILENT and cost CS2 its life
		 * (2026-08-15: 938 MiB in a 1 GiB window, then a 32 MiB ask). */
		pr_warn_ratelimited(
			"virtio_nvrm: window full: %s[%d] asked %zu KiB, %lu of %lu MiB in use -- ENOSPC\n",
			current->comm, task_pid_nr(current), len >> 10,
			(used << PAGE_SHIFT) >> 20,
			(total << PAGE_SHIFT) >> 20);
		return -ENOSPC;
	}
	bitmap_set(dev->win_bitmap, start, npages);
	mutex_unlock(&dev->win_lock);
	return (long)(start << PAGE_SHIFT);
}

static void win_free(struct nvrm_dev *dev, u64 off, size_t len)
{
	if (!dev->win_bitmap)
		return;
	mutex_lock(&dev->win_lock);
	bitmap_clear(dev->win_bitmap, off >> PAGE_SHIFT, len >> PAGE_SHIFT);
	mutex_unlock(&dev->win_lock);
}

/* One context per struct file. The host token names its mirrored file
 * within the dense guest_proc session, not a Linux PID. */

/* Pinned OS-descriptor memory, retained by hMemory until successful RM_FREE
 * or context release. A context may allocate and free many such objects. */
struct nvrm_pin {
	struct list_head node;
	struct nvrm_object_key object;
	struct page **pages;
	unsigned long npages;
	/* PRIME pages belong to the exporter. Release the dma-buf
	 * attachment; never unpin pages this module did not pin. */
	struct dma_buf *dmabuf;
	struct dma_buf_attachment *attach;
	struct sg_table *sgt;
};

struct nvrm_ctx {
	struct nvrm_dev *dev;
	/* The opening process owns the host session. Inheritance or
	 * SCM_RIGHTS transfer does not change that owner. */
	struct nvrm_proc *proc;
	u32 dev_tag;
	u32 gpu_index;
	u64 token;
	/* Serializes the ioctls of this FD. The device works the queue in order
	 * anyway, so the mutex costs nothing and makes the write-back path
	 * unambiguous. */
	struct mutex lock;
	spinlock_t pin_lock;
	struct list_head pins;

	/* OS-event firings set events_pending and wake this queue; poll
	 * consumes the flag. */
	wait_queue_head_t events_wq;
	atomic_t events_pending;
	/* Userspace ctx_xa entry. NVKMS sessions deliver events through
	 * callback slots instead. */
	bool indexed;
	bool uncertain; /* a submitted request has no trustworthy outcome */
};

/* Key of dev->ctx_xa. Tokens are per session, so the process id is half of
 * the key. A token above 32 bits (never seen; the Mirror counts from 1) is
 * not indexed at all rather than aliased. */
static bool nvrm_ctx_key(u32 proc_id, u64 token, unsigned long *key)
{
	if (token > U32_MAX)
		return false;
	*key = ((unsigned long)proc_id << 32) | (unsigned long)token;
	return true;
}

static bool ctx_is_uvm(const struct nvrm_ctx *c)
{
	return c->dev_tag == NVRM_DEV_UVM || c->dev_tag == NVRM_DEV_UVM_TOOLS;
}

static const struct file_operations nvrm_node_fops;

/* Translate by struct file identity: fdget() holds the file while f_op
 * validates our node and ctx supplies its token and owning session. dup()
 * and fork() preserve that identity.
 * Tokens are session-local; send the owner with cross-process references.
 * proc_out may be NULL for an escape-level FD already scoped to the caller. */
static int nvrm_token_of_fd(int n, u64 *tok, u32 *proc_out)
{
	struct fd f = fdget(n);
	struct file *file = nvrm_fd_file(f);
	int ret = -EBADF;

	if (file && file->f_op == &nvrm_node_fops) {
		struct nvrm_ctx *c = file->private_data;

		if (c) {
			*tok = c->token;
			/* fdget holds the immutable ctx->proc reference
			 * until fdput; proc_lock is unnecessary here. */
			if (proc_out)
				*proc_out = c->proc ? c->proc->id : 0;
			ret = 0;
		}
	}
	fdput(f);
	return ret;
}

/* Pinning with accounting */

static void nvrm_unpin(struct nvrm_pin *p)
{
	if (p->attach) {
		/* Exporter-owned pages are not unpinned or dirtied here. */
		dma_buf_unmap_attachment_unlocked(p->attach, p->sgt,
						  DMA_BIDIRECTIONAL);
		dma_buf_detach(p->dmabuf, p->attach);
		dma_buf_put(p->dmabuf);
		nvrm_uncharge(p->npages);
		kvfree(p->pages);
		kfree(p);
		module_put(THIS_MODULE);
		return;
	}
	/* dirty_lock(..., true): the pin used FOLL_WRITE, the GPU may have
	 * written into these pages. */
	unpin_user_pages_dirty_lock(p->pages, p->npages, true);
	nvrm_uncharge(p->npages);
	stat_pinned_kib -= p->npages << (PAGE_SHIFT - 10);
	kvfree(p->pages);
	kfree(p);
	module_put(THIS_MODULE);
}

/* Build GPA runs from a dma-buf scatterlist for PRIME imports. The pages
 * are guest RAM; no user VA needs pinning here.
 * The attachment device must support DMA. Use the virtio PCI parent, not
 * struct virtio_device (see kapi_enumerate_gpus). */
static struct nvrm_pin *nvrm_pin_dmabuf(struct dma_buf *dmabuf,
					struct device *dev)
{
	struct dma_buf_attachment *attach;
	struct scatterlist *sg;
	struct sg_table *sgt;
	struct nvrm_pin *p;
	unsigned long npages = 0, n = 0;
	unsigned int i;
	int err;

	if (!dmabuf || !dev)
		return ERR_PTR(-EINVAL);

	attach = dma_buf_attach(dmabuf, dev);
	if (IS_ERR(attach))
		return ERR_CAST(attach);

	sgt = dma_buf_map_attachment_unlocked(attach, DMA_BIDIRECTIONAL);
	if (IS_ERR(sgt)) {
		dma_buf_detach(dmabuf, attach);
		return ERR_CAST(sgt);
	}

	for_each_sgtable_sg(sgt, sg, i) {
		if (!sg_page(sg) || (sg->length & ~PAGE_MASK)) {
			/* No page behind an entry means the exporter handed out
			 * device memory; a partial page means an alignment this
			 * wire cannot express. Both are refusals, not guesses. */
			err = -EOPNOTSUPP;
			goto fail;
		}
		npages += sg->length >> PAGE_SHIFT;
	}
	if (!npages) {
		err = -EINVAL;
		goto fail;
	}

	p = kzalloc(sizeof(*p), GFP_KERNEL);
	if (!p) {
		err = -ENOMEM;
		goto fail;
	}
	p->pages = kvmalloc_array(npages, sizeof(*p->pages), GFP_KERNEL);
	if (!p->pages) {
		kfree(p);
		err = -ENOMEM;
		goto fail;
	}
	err = nvrm_charge(npages);
	if (err) {
		kvfree(p->pages);
		kfree(p);
		goto fail;
	}
	if (!try_module_get(THIS_MODULE)) {
		nvrm_uncharge(npages);
		kvfree(p->pages);
		kfree(p);
		err = -ENODEV;
		goto fail;
	}
	get_dma_buf(dmabuf);
	p->npages = npages;
	p->dmabuf = dmabuf;
	p->attach = attach;
	p->sgt = sgt;

	for_each_sgtable_sg(sgt, sg, i) {
		struct page *first = sg_page(sg);
		unsigned long k, cnt = sg->length >> PAGE_SHIFT;

		for (k = 0; k < cnt; k++)
			p->pages[n++] = nth_page(first, k);
	}
	return p;

fail:
	dma_buf_unmap_attachment_unlocked(attach, sgt, DMA_BIDIRECTIONAL);
	dma_buf_detach(dmabuf, attach);
	return ERR_PTR(err);
}

static struct nvrm_pin *nvrm_pin_range(unsigned long va, size_t len)
{
	struct nvrm_pin *p;
	unsigned long npages, done = 0;
	int err;

	if (!len || (va & ~PAGE_MASK) || (len & ~PAGE_MASK))
		return ERR_PTR(-EINVAL);
	if (va + len < va) /* overflow */
		return ERR_PTR(-EINVAL);

	npages = len >> PAGE_SHIFT;
	err = nvrm_charge(npages);
	if (err)
		return ERR_PTR(err);

	p = kzalloc(sizeof(*p), GFP_KERNEL);
	if (!p) {
		nvrm_uncharge(npages);
		return ERR_PTR(-ENOMEM);
	}
	p->pages = kvmalloc_array(npages, sizeof(*p->pages), GFP_KERNEL);
	if (!p->pages) {
		kfree(p);
		nvrm_uncharge(npages);
		return ERR_PTR(-ENOMEM);
	}
	p->npages = npages;

	/* FOLL_WRITE breaks COW; FOLL_LONGTERM requests pins suitable for
	 * long-lived DMA. */
	while (done < npages) {
		unsigned long want =
			min_t(unsigned long, PIN_CHUNK_PAGES, npages - done);
		long got = pin_user_pages_fast(va + (done << PAGE_SHIFT), want,
					       FOLL_WRITE | FOLL_LONGTERM,
					       p->pages + done);
		if (got <= 0) {
			long errno_got = got ? got : -EFAULT;

			if (done)
				unpin_user_pages(p->pages, done);
			kvfree(p->pages);
			kfree(p);
			nvrm_uncharge(npages);
			return ERR_PTR(errno_got);
		}
		done += got;
		if (fatal_signal_pending(current)) {
			unpin_user_pages(p->pages, done);
			kvfree(p->pages);
			kfree(p);
			nvrm_uncharge(npages);
			return ERR_PTR(-EINTR);
		}
	}
	/* Reserve a module reference before this backing can be submitted or
	 * quarantined. Taking one during module_exit would be too late. */
	if (!try_module_get(THIS_MODULE)) {
		unpin_user_pages(p->pages, npages);
		kvfree(p->pages);
		kfree(p);
		nvrm_uncharge(npages);
		return ERR_PTR(-ENODEV);
	}
	stat_pinned_kib += npages << (PAGE_SHIFT - 10);
	stat_osdesc_pins++;
	return p;
}

/* GPA runs from a page list. Returns the number of runs. */
static u32 nvrm_runs_from_pages(struct page **pages, unsigned long npages,
				struct nvrm_gpa_run *runs, u32 max)
{
	unsigned long i;
	u32 n = 0;

	for (i = 0; i < npages; i++) {
		u64 gpa = (u64)page_to_pfn(pages[i]) << PAGE_SHIFT;

		if (n && runs[n - 1].gpa + runs[n - 1].len == gpa) {
			runs[n - 1].len += PAGE_SIZE;
			continue;
		}
		if (n >= max)
			return 0;
		runs[n].gpa = gpa;
		runs[n].len = PAGE_SIZE;
		n++;
	}
	return n;
}

/* open / release */

/* Resolve the node type from major/minor numbers. */
static int node_dev_tag(unsigned int major, unsigned int minor, u32 *tag,
			u32 *idx)
{
	*idx = 0;
	if (major == NV_FRONTEND_MAJOR) {
		if (minor == NV_MINOR_CTL) {
			*tag = NVRM_DEV_CTL;
		} else {
			*tag = NVRM_DEV_GPU;
			*idx = minor;
		}
		return 0;
	}
	if (major == NV_UVM_MAJOR) {
		*tag = minor ? NVRM_DEV_UVM_TOOLS : NVRM_DEV_UVM;
		return 0;
	}
	return -ENODEV;
}

/* Find or create this process identity; return with a reference held. */
static struct nvrm_proc *nvrm_proc_get(struct nvrm_dev *dev)
{
	struct pid *pid = get_task_pid(current, PIDTYPE_TGID);
	struct nvrm_proc *p, *fresh = NULL;
	int ret;

	if (!pid)
		return ERR_PTR(-ESRCH);

	mutex_lock(&dev->proc_lock);
	list_for_each_entry(p, &dev->procs, node) {
		if (p->pid == pid) {
			refcount_inc(&p->ref);
			mutex_unlock(&dev->proc_lock);
			put_pid(pid);
			return p;
		}
	}

	fresh = kzalloc(sizeof(*fresh), GFP_KERNEL);
	if (!fresh) {
		mutex_unlock(&dev->proc_lock);
		put_pid(pid);
		return ERR_PTR(-ENOMEM);
	}
	ret = nvrm_proc_alloc_id(&fresh->id);
	if (ret) {
		mutex_unlock(&dev->proc_lock);
		kfree(fresh);
		put_pid(pid);
		return ERR_PTR(ret);
	}
	kref_get(&dev->ref);
	fresh->dev = dev;
	fresh->pid = pid; /* the reference passes to the entry */
	fresh->vnr = (u32)pid_vnr(pid);
	get_task_comm(fresh->comm, current);
	refcount_set(&fresh->ref, 1);
	list_add(&fresh->node, &dev->procs);
	mutex_unlock(&dev->proc_lock);
	return fresh;
}

/* Remove the process from lookup before host teardown. IDs are never reused,
 * including when PROC_GONE times out and its reply arrives later. */
static void nvrm_proc_put(struct nvrm_dev *dev, struct nvrm_proc *p)
{
	u32 id;

	if (!p)
		return;
	mutex_lock(&dev->proc_lock);
	if (!refcount_dec_and_test(&p->ref)) {
		mutex_unlock(&dev->proc_lock);
		return;
	}
	list_del(&p->node);
	id = p->id;
	mutex_unlock(&dev->proc_lock);

	/* Wait outside proc_lock. Teardown also runs after SIGKILL, so use
	 * the bounded noninterruptible path. */
	nvrm_simple(dev, NVRM_KIND_PROC_GONE, 0, 0, 0, 0, 0, NULL, false, id);

	put_pid(p->pid);
	nvrm_dev_put(p->dev);
	kfree(p);
}

static int nvrm_node_open(struct inode *inode, struct file *filp)
{
	struct nvrm_dev *dev = nvrm_dev_get();
	struct nvrm_ctx *ctx;
	struct nvrm_proc_info info;
	u64 token = 0;
	int ret;

	if (!dev)
		return -ENODEV;

	ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);
	if (!ctx) {
		nvrm_dev_put(dev);
		return -ENOMEM;
	}
	mutex_init(&ctx->lock);
	spin_lock_init(&ctx->pin_lock);
	INIT_LIST_HEAD(&ctx->pins);
	init_waitqueue_head(&ctx->events_wq);
	atomic_set(&ctx->events_pending, 0);
	ctx->dev = dev;

	ret = node_dev_tag(imajor(inode), iminor(inode), &ctx->dev_tag,
			   &ctx->gpu_index);
	if (ret)
		goto err;

	ctx->proc = nvrm_proc_get(dev);
	if (IS_ERR(ctx->proc)) {
		ret = PTR_ERR(ctx->proc);
		ctx->proc = NULL;
		goto err;
	}

	/* The opener identifies itself to the host. The payload is optional:
	 * a session that never sends it behaves as if no process information
	 * had been given. */
	memset(&info, 0, sizeof(info));
	info.pid = ctx->proc->vnr;
	memcpy(info.comm, ctx->proc->comm,
	       min(sizeof(info.comm), sizeof(ctx->proc->comm)));
	info.comm[sizeof(info.comm) - 1] = '\0';

	ret = nvrm_simple_info(dev, NVRM_KIND_OPEN, ctx->dev_tag,
			       ctx->gpu_index, 0, 0, 0, &token, true,
			       ctx->proc->id, &info);
	if (ret < 0)
		goto err;
	ctx->token = token;

	/* Index the token for event wakes. Index failure leaves a usable fd
	 * without asynchronous wakes. */
	{
		unsigned long key;

		if (!nvrm_ctx_key(ctx->proc->id, token, &key)) {
			pr_warn_once(
				"virtio_nvrm: token %llu does not fit the event index -- fd will not wake\n",
				(unsigned long long)token);
		} else if (xa_err(xa_store(&dev->ctx_xa, key, ctx,
					   GFP_KERNEL))) {
			pr_warn_ratelimited(
				"virtio_nvrm: event index full for proc %u token %llu -- fd will not wake\n",
				ctx->proc->id, (unsigned long long)token);
		} else {
			ctx->indexed = true;
		}
	}
	filp->private_data = ctx;
	nvrm_dev_put(dev);
	stat_ctx_opened++;
	stat_ctx_open++;
	return 0;

err:
	if (ctx->proc)
		nvrm_proc_put(dev, ctx->proc);
	mutex_destroy(&ctx->lock);
	kfree(ctx);
	nvrm_dev_put(dev);
	return ret;
}

static void nvrm_ctx_release_pins(struct nvrm_ctx *ctx, int close_ret)
{
	struct nvrm_pin *p, *tmp;

	list_for_each_entry_safe(p, tmp, &ctx->pins, node) {
		list_del(&p->node);
		if (close_ret || ctx->uncertain ||
		    READ_ONCE(ctx->dev->stopping))
			nvrm_quarantine(ctx->dev, &p->node,
					&ctx->dev->quarantined_pins, p->npages);
		else
			nvrm_unpin(p);
	}
}

static int nvrm_node_release(struct inode *inode, struct file *filp)
{
	struct nvrm_ctx *ctx = filp->private_data;
	int ret;

	if (!ctx)
		return 0;

	/* Erase under the XArray lock used by events_work. No firing can
	 * retain ctx after xa_erase returns. */
	if (ctx->indexed) {
		unsigned long key;

		if (nvrm_ctx_key(ctx->proc->id, ctx->token, &key))
			xa_erase(&ctx->dev->ctx_xa, key);
		ctx->indexed = false;
	}

	/* Native copies can outlive a successful source close. Normal shared
	 * backing reclamation still requires the planned host ownership ledger. */
	ret = nvrm_simple(ctx->dev, NVRM_KIND_CLOSE, ctx->dev_tag, 0,
			  ctx->token, 0, 0, NULL, false,
			  ctx->proc ? ctx->proc->id : 0);
	nvrm_ctx_release_pins(ctx, ret);
	stat_ctx_closed++;
	if (stat_ctx_open)
		stat_ctx_open--;
	/* Close the node before dropping the process identity; the last
	 * reference sends PROC_GONE. */
	nvrm_proc_put(ctx->dev, ctx->proc);
	mutex_destroy(&ctx->lock);
	kfree(ctx);
	return 0;
}

/* Sleep on the file's wait queue until a host OS event arrives. Without
 * .poll, DEFAULT_POLLMASK would make libcuda spin.
 * Consume the pending flag as native nvidia_poll does (nv.c:2320): Vulkan
 * may poll without GET_EVENT_DATA. Each firing re-arms it; a successful
 * GET_EVENT_DATA with MoreEvents also re-arms it. Spurious wakes are
 * allowed. */
static __poll_t nvrm_node_poll(struct file *filp, struct poll_table_struct *pt)
{
	struct nvrm_ctx *ctx = filp->private_data;

	if (!ctx)
		return EPOLLERR;
	poll_wait(filp, &ctx->events_wq, pt);
	if (READ_ONCE(ctx->dev->stopping))
		return EPOLLERR | EPOLLHUP;
	if (atomic_xchg(&ctx->events_pending, 0))
		return EPOLLIN | EPOLLPRI;
	return 0;
}

/* Shared ioctl interpreter. NVOS parameter layouts and hClass-specific
 * metadata come from host tables and nvrm_wire.h. */

/* Everything one call needs as intermediate state. */
struct call {
	struct nvrm_ctx *ctx;
	struct nvrm_dev *dev;
	const struct nvrm_tables *t;
	const struct nvrm_ioctl_desc *desc;

	u32 nr;
	u32 size;
	u64 addr; /* where the inline block is written back */
	/* Kernel callers use memcpy through call_in/call_out instead of
	 * user-memory accessors. */
	bool kern;

	u8 *inl;
	/* the out-of-line block beside inl: embedded-pointer payloads */
	u8 *aux;
	size_t aux_len;
	size_t params_len; /* the params buffer only, without nested */

	u32 emb_off;
	u64 saved_ptr; /* guest address of the params buffer */

	u32 fd_off;
	u64 fd_token;
	/* Which guest process owns fd_token. Same rule as aux_fd_proc below:
	 * NVRM_NONE_U32 = not stated, never 0. A token is minted per session,
	 * so the host cannot resolve it without being told whose it is. */
	u32 fd_proc;
	u32 fd_orig; /* the application's own fd number */

	u32 aux_fd_off;
	u64 aux_fd_token;
	/* Which guest process owns aux_fd_token. NVRM_NONE_U32 = not stated;
	 * never 0, which is a live session id (the "not stated" caller). */
	u32 aux_fd_proc;
	u8 aux_fd_orig[8];
	u32 aux_fd_len; /* 8 for an NvP64 (alloc), 4 for an NvS32 (control) */

	u32 n_nested;
	struct nvrm_nested_desc nested[NVRM_MAX_NESTED];
	u64 nested_gva[NVRM_MAX_NESTED];

	/* OS descriptor: pinned pages instead of a guest VA */
	struct nvrm_pin *pin;
	u64 pmem_orig;
	u64 limit_orig;
	u32 run_count;
};

static u32 rd32(const u8 *p, u32 off)
{
	u32 v;

	memcpy(&v, p + off, sizeof(v));
	return v;
}

static u64 rd64(const u8 *p, u32 off)
{
	u64 v;

	memcpy(&v, p + off, sizeof(v));
	return v;
}

static void wr64(u8 *p, u32 off, u64 v)
{
	memcpy(p + off, &v, sizeof(v));
}

static u16 rd16(const u8 *p, u32 off)
{
	u16 v;

	memcpy(&v, p + off, sizeof(v));
	return v;
}

static void wr32(u8 *p, u32 off, u32 v)
{
	memcpy(p + off, &v, sizeof(v));
}

/* BDF mediation rewrites both directions: requests may name guest gpuIds,
 * while replies contain host gpuIds and PCI addresses. */

/* The guest's own address, as gpuGenerate32BitId() would encode it. */
static void bdf_init(struct nvrm_dev *dev)
{
	struct pci_dev *pdev;

	dev->bdf_guest_id = 0;
	dev->bdf_host_id = 0;
	dev->bdf_disabled = false;

	if (!dev->vdev->dev.parent || !dev_is_pci(dev->vdev->dev.parent))
		return;

	pdev = to_pci_dev(dev->vdev->dev.parent);
	dev->bdf_domain = (u16)pci_domain_nr(pdev->bus);
	dev->bdf_bus = pdev->bus->number;
	dev->bdf_slot = PCI_SLOT(pdev->devfn);
	dev->bdf_func = PCI_FUNC(pdev->devfn);
	dev->bdf_guest_id = ((u32)dev->bdf_domain << 16) |
			    ((u32)dev->bdf_bus << 8) | dev->bdf_slot;

	pr_info("virtio_nvrm: BDF mediation: guest %04x:%02x:%02x.%u -> gpu id %#x\n",
		dev->bdf_domain, dev->bdf_bus, dev->bdf_slot, dev->bdf_func,
		dev->bdf_guest_id);
}

static bool bdf_on(const struct nvrm_dev *dev)
{
	return bdf_mediation && dev && dev->bdf_guest_id && !dev->bdf_disabled;
}

/* Learn the host's id from an answer. Everything downstream keys off this,
 * and it is the one place that decides mediation is not representable. */
static void bdf_learn(struct nvrm_dev *dev, u32 host_id)
{
	if (host_id == NVRM_GPU_INVALID_ID || host_id == 0)
		return;
	/* The mediated guest ID can return in later requests; do not treat
	 * it as a second host GPU. */
	if (host_id == dev->bdf_guest_id)
		return;
	if (!dev->bdf_host_id) {
		dev->bdf_host_id = host_id;
		pr_info("virtio_nvrm: BDF mediation: host gpu id %#x -> guest %#x\n",
			host_id, dev->bdf_guest_id);
		return;
	}
	if (dev->bdf_host_id != host_id && !dev->bdf_disabled) {
		dev->bdf_disabled = true;
		pr_warn("virtio_nvrm: BDF mediation OFF -- RM names a second GPU (%#x beside %#x). One virtio device cannot stand for two addresses; the guest now sees the host's addresses again.\n",
			host_id, dev->bdf_host_id);
	}
}

/* Translate host IDs to guest IDs. Learn only from GET_PROBED_IDS and
 * GET_ATTACHED_IDS; other fields may contain derived values that resemble
 * IDs and would falsely disable single-GPU mediation. */
static u32 bdf_to_guest(struct nvrm_dev *dev, u32 id, bool learn)
{
	if (id == NVRM_GPU_INVALID_ID || id == 0)
		return id;
	if (learn)
		bdf_learn(dev, id);
	if (!bdf_on(dev) || id != dev->bdf_host_id)
		return id;
	return dev->bdf_guest_id;
}

/* guest -> host, for ids the guest names in a question. */
static u32 bdf_to_host(struct nvrm_dev *dev, u32 id)
{
	if (!bdf_on(dev) || !dev->bdf_host_id || id != dev->bdf_guest_id)
		return id;
	return dev->bdf_host_id;
}

/* BDF scalar/array offsets are generated from SDK structs by
 * nvrm-genhdr.rs. */
struct bdf_scalar {
	u32 cmd;
	u32 off;
};
struct bdf_array {
	u32 cmd;
	u32 off;
	u32 count;
	u32 stride;
};

static const struct bdf_scalar bdf_scalars[] = { NVRM_BDF_SCALARS };
static const struct bdf_array bdf_arrays[] = { NVRM_BDF_ARRAYS };

static const struct bdf_array *bdf_find_array(u32 cmd)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(bdf_arrays); i++)
		if (bdf_arrays[i].cmd == cmd)
			return &bdf_arrays[i];
	return NULL;
}

/* NV_ESC_CARD_INFO returns an inline array of nv_ioctl_card_info_t. Rewrite
 * each BDF and gpuId; this escape bypasses control-reply mediation. */
static void bdf_rewrite_card_info(struct call *c)
{
	struct nvrm_dev *dev = c->dev;
	u32 n, i;

	if (!bdf_on(dev) || !NVRM_CARD_INFO_ENTRY)
		return;

	n = c->size / NVRM_CARD_INFO_ENTRY;
	for (i = 0; i < n; i++) {
		u32 base = i * NVRM_CARD_INFO_ENTRY;
		u32 pci = base + NVRM_CARD_INFO_PCI_OFF;
		u32 id;

		if (!rd32(c->inl, base + NVRM_CARD_INFO_VALID_OFF))
			continue;
		id = rd32(c->inl, base + NVRM_CARD_INFO_GPUID_OFF);
		if (id != dev->bdf_host_id)
			continue;
		wr32(c->inl, base + NVRM_CARD_INFO_GPUID_OFF,
		     dev->bdf_guest_id);
		wr32(c->inl, pci + NVRM_PCI_DOMAIN_OFF, dev->bdf_domain);
		c->inl[pci + NVRM_PCI_BUS_OFF] = dev->bdf_bus;
		c->inl[pci + NVRM_PCI_SLOT_OFF] = dev->bdf_slot;
		c->inl[pci + NVRM_PCI_FUNC_OFF] = dev->bdf_func;
	}
}

/* ATTACH_GPUS_TO_FD carries an inline gpuId array and has no control
 * descriptor. Derive its count from _IOC_SIZE, as the host does
 * (nv.c:2605).
 * Translate both request and reply: write_back returns the same buffer to
 * userspace. Zero slots mean no GPU and pass through unchanged. */
static void bdf_rewrite_attach_gpus(struct call *c, bool to_host)
{
	struct nvrm_dev *dev = c->dev;
	u32 off;

	/* Restrict to the control node: raw UVM command numbers overlap
	 * escape numbers (NV_CTL_DEVICE_ONLY, nv.c:2608). */
	if (c->ctx->dev_tag != NVRM_DEV_CTL ||
	    c->nr != NVRM_ESC_ATTACH_GPUS_TO_FD || !bdf_on(dev))
		return;

	for (off = 0; off + 4 <= c->size; off += 4) {
		u32 id = rd32(c->inl, off);

		wr32(c->inl, off,
		     to_host ? bdf_to_host(dev, id) :
			       bdf_to_guest(dev, id, false));
	}
}

/* The question side: the guest names the card by the address IT can see,
 * and RM knows only its own. Called with the params buffer fetched, before
 * the request goes out. */
static void bdf_rewrite_request(struct call *c, u32 cmd)
{
	const struct bdf_scalar *sc;
	const struct bdf_array *ar;
	u32 i;

	if (!bdf_mediation)
		return;
	if (!bdf_on(c->dev) || !c->dev->bdf_host_id)
		return;

	/* EVERY matching entry, not the first: one command can carry the same
	 * id in two fields (GET_ID_INFO does, in gpuId and boardId). */
	sc = NULL;
	for (i = 0; i < ARRAY_SIZE(bdf_scalars); i++) {
		if (bdf_scalars[i].cmd != cmd)
			continue;
		sc = &bdf_scalars[i];
		if (c->params_len < 4 || sc->off > c->params_len - 4)
			continue;
		wr32(c->aux, sc->off,
		     bdf_to_host(c->dev, rd32(c->aux, sc->off)));
	}
	if (sc)
		return;
	/* Rewrite every matching array row; P2P_CAPS_MATRIX carries two
	 * arrays. */
	for (ar = bdf_arrays; ar < bdf_arrays + ARRAY_SIZE(bdf_arrays); ar++) {
		if (ar->cmd != cmd)
			continue;
		for (i = 0; i < ar->count; i++) {
			u32 off = ar->off + i * ar->stride;

			if (c->params_len < 4 || off > c->params_len - 4)
				break;
			wr32(c->aux, off,
			     bdf_to_host(c->dev, rd32(c->aux, off)));
		}
	}
}

/* Rewrite replies in c->aux before copy-out. Every control passes here;
 * keep unrelated commands allocation-free and lock-free, using only the
 * scalar/array table scans. */
static void bdf_rewrite_reply(struct call *c, u32 cmd)
{
	struct nvrm_dev *dev = c->dev;
	const struct bdf_scalar *sc;
	const struct bdf_array *ar;
	u32 i;

	/* Skip all scans on the common disabled path. */
	if (!bdf_mediation && !bdf_debug)
		return;

	/* Scan before rewriting, including known commands: an additional
	 * host-ID field may be missing from the table. */
	if (bdf_debug && dev->bdf_host_id && c->params_len >= 4) {
		u32 hbus = (dev->bdf_host_id >> 8) & 0xff;
		u32 off;

		for (off = 0; off + 4 <= c->params_len; off += 4) {
			u32 v = rd32(c->aux, off);

			/* Level 2 also scans the bare bus number. */
			if (v == dev->bdf_host_id)
				pr_info_ratelimited(
					"virtio_nvrm: bdf_debug: control %#x carries host id %#x at +%u (params %zu)\n",
					cmd, dev->bdf_host_id, off,
					c->params_len);
			else if (bdf_debug > 1 && v == hbus)
				pr_info_ratelimited(
					"virtio_nvrm: bdf_debug: control %#x carries host bus %#x at +%u (params %zu)\n",
					cmd, hbus, off, c->params_len);
		}
		/* Level 3 scans formatted PCI busId strings. */
		if (bdf_debug > 2 && c->params_len >= 5) {
			char want[8];
			size_t k;

			scnprintf(want, sizeof(want), "%02x:%02x", hbus,
				  (dev->bdf_host_id) & 0xff);
			for (k = 0; k + 5 <= c->params_len; k++)
				if (!strncasecmp((char *)c->aux + k, want, 5)) {
					pr_info_ratelimited(
						"virtio_nvrm: bdf_debug: control %#x carries the printed host address at +%zu (params %zu)\n",
						cmd, k, c->params_len);
					break;
				}
		}
	}

	/* Rewrite NVML PCI address fields encoded as (index, data) pairs. */
	if (cmd == NVRM_CTRL_BUS_GET_INFO_V2 || cmd == NVRM_CTRL_BUS_GET_INFO) {
		u32 n;

		if (!bdf_on(dev) || c->params_len < NVRM_BUS_INFO_LIST_OFF + 4)
			return;
		n = rd32(c->aux, 0); /* busInfoListSize */
		if (n > NVRM_BUS_INFO_MAX_LIST)
			n = NVRM_BUS_INFO_MAX_LIST;
		for (i = 0; i < n; i++) {
			u32 e = NVRM_BUS_INFO_LIST_OFF +
				i * NVRM_BUS_INFO_ENTRY_SIZE;
			u32 d = e + NVRM_BUS_INFO_DATA_OFF;
			u32 val;

			if (d + 4 > c->params_len)
				return;
			switch (rd32(c->aux, e)) {
			case NVRM_BUS_INFO_INDEX_BUS:
				val = dev->bdf_bus;
				break;
			case NVRM_BUS_INFO_INDEX_DEVICE:
				val = dev->bdf_slot;
				break;
			case NVRM_BUS_INFO_INDEX_DOMAIN:
				val = dev->bdf_domain;
				break;
			default:
				continue;
			}
			wr32(c->aux, d, val);
		}
		return;
	}

	ar = bdf_find_array(cmd);
	if (ar) {
		/* Every row naming this cmd (P2P_CAPS_MATRIX has two). */
		for (; ar < bdf_arrays + ARRAY_SIZE(bdf_arrays); ar++) {
			if (ar->cmd != cmd)
				continue;
			for (i = 0; i < ar->count; i++) {
				u32 off = ar->off + i * ar->stride;

				if (c->params_len < 4 ||
				    off > c->params_len - 4)
					break;
				wr32(c->aux, off,
				     bdf_to_guest(
					     dev, rd32(c->aux, off),
					     cmd == NVRM_CTRL_GPU_GET_PROBED_IDS ||
						     cmd == NVRM_CTRL_GPU_GET_ATTACHED_IDS));
			}
		}
		return;
	}

	sc = NULL;
	for (i = 0; i < ARRAY_SIZE(bdf_scalars); i++) {
		if (bdf_scalars[i].cmd != cmd)
			continue;
		sc = &bdf_scalars[i];
		if (c->params_len < 4 || sc->off > c->params_len - 4)
			continue;
		wr32(c->aux, sc->off,
		     bdf_to_guest(dev, rd32(c->aux, sc->off), false));
	}
	if (!sc)
		return;

	/* The address itself, and it has to agree with the id above or the
	 * two answers contradict each other. */
	if (cmd != NVRM_CTRL_GPU_GET_PCI_INFO || !bdf_on(dev))
		return;
	if (c->params_len < NVRM_SIZE_PCI_INFO)
		return;
	wr32(c->aux, NVRM_PCI_INFO_DOMAIN_OFF, dev->bdf_domain);
	{
		u16 v = dev->bdf_bus;

		memcpy(c->aux + NVRM_PCI_INFO_BUS_OFF, &v, sizeof(v));
		v = dev->bdf_slot;
		memcpy(c->aux + NVRM_PCI_INFO_SLOT_OFF, &v, sizeof(v));
	}
}

/* VRAM reply diagnostics. Inspect replies before copy-out without modifying
 * them. */

/* Locate the FB_INFO list: inline for V2, or a nested buffer identified by
 * pointer offset for V1. */
static u8 *vram_fb_list(struct call *c, u32 cmd, u32 *n)
{
	u32 want, fits, i;

	if (c->params_len < 4)
		return NULL;
	if (cmd == NVRM_CTRL_FB_GET_INFO_V2) {
		if (c->params_len <
		    NVRM_FB_INFO_V2_LIST_OFF + NVRM_FB_INFO_ENTRY_SIZE)
			return NULL;
		want = rd32(c->aux, NVRM_FB_INFO_V2_COUNT_OFF);
		fits = (c->params_len - NVRM_FB_INFO_V2_LIST_OFF) /
		       NVRM_FB_INFO_ENTRY_SIZE;
		*n = min3(want, fits, NVRM_FB_INFO_MAX_LIST);
		return c->aux + NVRM_FB_INFO_V2_LIST_OFF;
	}
	if (cmd != NVRM_CTRL_FB_GET_INFO)
		return NULL;
	for (i = 0; i < c->n_nested; i++) {
		const struct nvrm_nested_desc *d = &c->nested[i];

		if (d->ptr_off != NVRM_FB_INFO_V1_LIST_PTR_OFF ||
		    d->aux_off > c->aux_len || d->len > c->aux_len - d->aux_off)
			continue;
		want = rd32(c->aux, NVRM_FB_INFO_V1_COUNT_OFF);
		fits = d->len / NVRM_FB_INFO_ENTRY_SIZE;
		*n = min3(want, fits, NVRM_FB_INFO_MAX_LIST);
		return c->aux + d->aux_off;
	}
	return NULL;
}

/* The forms vram_debug looks for: KB, the unit of FB_GET_INFO, and bytes,
 * the unit of NVOS32 and NVML. Not MiB: 2816 or 8192 as a bare number is a
 * handle, an 8 KiB size or a table index far more often than a card, and
 * the first census (2026-09-17) drowned in exactly those. */
enum vram_form {
	VRAM_FB_KB,
	VRAM_FB_BYTES,
	VRAM_CARD_KB,
	VRAM_CARD_BYTES,
	VRAM_HEAP_INFO,
};
static const char *const vram_form_name[] = {
	[VRAM_FB_KB] = "the advertised FB size in KB",
	[VRAM_FB_BYTES] = "the advertised FB size in bytes",
	[VRAM_CARD_KB] = "the card size in KB",
	[VRAM_CARD_BYTES] = "the card size in bytes",
	[VRAM_HEAP_INFO] = "NVOS32_FUNCTION_INFO",
};

/* Track (command, offset, form, process) once. After the table fills, log
 * further hits with rate limiting. */
static struct {
	u32 cmd, off;
	u8 form;
	char comm[TASK_COMM_LEN];
} vram_seen[128];
static unsigned int vram_seen_n;
static DEFINE_SPINLOCK(vram_seen_lock);

/* Log first sightings directly; rate-limit level 2 and overflow. */
#define vram_debug_say(first, fmt, ...)                          \
	do {                                                     \
		if ((first) && READ_ONCE(vram_debug) < 2)        \
			pr_info(fmt, ##__VA_ARGS__);             \
		else                                             \
			pr_info_ratelimited(fmt, ##__VA_ARGS__); \
	} while (0)

static bool vram_debug_first(u32 cmd, u32 off, enum vram_form form)
{
	char comm[TASK_COMM_LEN];
	unsigned long flags;
	unsigned int i;
	bool first = true;

	if (READ_ONCE(vram_debug) > 1)
		return true;
	get_task_comm(comm, current);
	spin_lock_irqsave(&vram_seen_lock, flags);
	for (i = 0; i < vram_seen_n; i++)
		if (vram_seen[i].cmd == cmd && vram_seen[i].off == off &&
		    vram_seen[i].form == form &&
		    !strcmp(vram_seen[i].comm, comm)) {
			first = false;
			break;
		}
	if (first && vram_seen_n < ARRAY_SIZE(vram_seen)) {
		vram_seen[vram_seen_n].cmd = cmd;
		vram_seen[vram_seen_n].off = off;
		vram_seen[vram_seen_n].form = form;
		memcpy(vram_seen[vram_seen_n].comm, comm, sizeof(comm));
		vram_seen_n++;
	}
	spin_unlock_irqrestore(&vram_seen_lock, flags);
	return first;
}

/* Which form, if any, the value at `off` is. The advertised size first: on a
 * card whose guest FB is the whole card the two coincide, and the question
 * the census answers is what the GUEST is told. */
static int vram_debug_form(const struct nvrm_dev *dev, const u8 *p, size_t len,
			   u32 off)
{
	u32 fb_kb = dev->vram_fb_kb;
	u64 card = (u64)READ_ONCE(vram_debug_card_mib) << 20;
	u32 v = rd32(p, off);
	u64 w = off + 8 <= len ? rd64(p, off) : 0;

	if (fb_kb && v == fb_kb)
		return VRAM_FB_KB;
	if (fb_kb && w == (u64)fb_kb << 10)
		return VRAM_FB_BYTES;
	if (card && v == card >> 10)
		return VRAM_CARD_KB;
	if (card && w == card)
		return VRAM_CARD_BYTES;
	return -1;
}

/* A control's params and nested buffers. The FB_GET_INFO entries are named
 * too, so one census answers both "who reads the size" and "where else it
 * travels". */
static void vram_debug_control(struct call *c, u32 cmd)
{
	u32 off;

	for (off = 0; off + 4 <= c->aux_len; off += 4) {
		int form = vram_debug_form(c->dev, c->aux, c->aux_len, off);

		if (form < 0 || !vram_debug_first(cmd, off, form))
			continue;
		vram_debug_say(
			vram_seen_n < ARRAY_SIZE(vram_seen),
			"virtio_nvrm: vram_debug: control %#x carries %s at %s+%u (params %zu, with nested %zu; %s %s[%d])\n",
			cmd, vram_form_name[form],
			off < c->params_len ? "params" : "nested",
			off < c->params_len ? off : off - (u32)c->params_len,
			c->params_len, c->aux_len,
			c->kern ? "kernel path in" : "process", current->comm,
			task_tgid_nr(current));
	}
}

/* An escape's inline block, as the backend answered it.
 * NVOS32_FUNCTION_INFO is named on its own, with its values: `total` and `free` come from an FB_GET_INFO_V2 the host RM makes
 * internally (rmapi_deprecated_vidheapctrl.c:340-383), so they are the host
 * card's unless the backend caps that door as well. */
static void vram_debug_inline(struct call *c)
{
	u32 off;

	if (ctx_is_uvm(c->ctx))
		return;
	if (c->nr == NVRM_ESC_RM_VID_HEAP_CONTROL &&
	    c->size >= NVRM_NVOS32_SIZE &&
	    rd32(c->inl, NVRM_NVOS32_FUNCTION_OFF) ==
		    NVRM_NVOS32_FUNCTION_INFO &&
	    vram_debug_first(c->nr, NVRM_NVOS32_FUNCTION_OFF, VRAM_HEAP_INFO))
		vram_debug_say(
			vram_seen_n < ARRAY_SIZE(vram_seen),
			"virtio_nvrm: vram_debug: NVOS32_FUNCTION_INFO answers total %llu MiB, free %llu MiB (%s %s[%d])\n",
			rd64(c->inl, NVRM_NVOS32_TOTAL_OFF) >> 20,
			rd64(c->inl, NVRM_NVOS32_FREE_OFF) >> 20,
			c->kern ? "kernel path in" : "process", current->comm,
			task_tgid_nr(current));
	for (off = 0; off + 4 <= c->size; off += 4) {
		int form = vram_debug_form(c->dev, c->inl, c->size, off);

		if (form < 0 ||
		    !vram_debug_first(c->nr | 0x80000000u, off, form))
			continue;
		vram_debug_say(
			vram_seen_n < ARRAY_SIZE(vram_seen),
			"virtio_nvrm: vram_debug: escape %#x inline carries %s at +%u (size %u; %s %s[%d])\n",
			c->nr, vram_form_name[form], off, c->size,
			c->kern ? "kernel path in" : "process", current->comm,
			task_tgid_nr(current));
	}
}

/* Inspect FB_GET_INFO before nested-buffer copy-out; V1 keeps its list
 * there. Unrelated replies only compare command IDs and load the debug
 * setting. */
static void vram_debug_reply(struct call *c, u32 cmd)
{
	u32 i, n = 0;
	u8 *list;

	if (cmd != NVRM_CTRL_FB_GET_INFO_V2 && cmd != NVRM_CTRL_FB_GET_INFO &&
	    !READ_ONCE(vram_debug))
		return;

	list = vram_fb_list(c, cmd, &n);
	/* The size the host advertises. */
	for (i = 0; list && i < n; i++) {
		u32 e = i * NVRM_FB_INFO_ENTRY_SIZE;

		if (rd32(list, e) == NVRM_FB_INFO_INDEX_TOTAL_RAM_SIZE &&
		    rd32(list, e + NVRM_FB_INFO_DATA_OFF))
			WRITE_ONCE(c->dev->vram_fb_kb,
				   rd32(list, e + NVRM_FB_INFO_DATA_OFF));
	}
	if (READ_ONCE(vram_debug))
		vram_debug_control(c, cmd);
}

/* User and kernel callers share the interpreter. Only fetch/write-back
 * differ: copy_from_user/copy_to_user for processes, memcpy for NVKMS. */
static int call_in(const struct call *c, void *dst, u64 src, size_t n)
{
	if (!n)
		return 0;
	if (c->kern) {
		memcpy(dst, (void *)(uintptr_t)src, n);
		return 0;
	}
	return copy_from_user(dst, (void __user *)(uintptr_t)src, n) ? -EFAULT :
								       0;
}

static int call_out(const struct call *c, u64 dst, const void *src, size_t n)
{
	if (!n)
		return 0;
	if (c->kern) {
		memcpy((void *)(uintptr_t)dst, src, n);
		return 0;
	}
	return copy_to_user((void __user *)(uintptr_t)dst, src, n) ? -EFAULT :
								     0;
}

/* Resolve the userspace XFER wrapper before sizing the payload. Kernel ops
 * already supply their command and size directly. */
static int resolve_xfer(struct call *c, void __user *arg)
{
	const struct nvrm_table_hdr *h = &c->t->hdr;
	u8 hdrbuf[NVRM_XFER_HEADER_MAX];
	u32 real_nr, real_size;
	u64 real_ptr;

	if (h->xfer_struct_len > sizeof(hdrbuf))
		return -EPROTO;
	if (copy_from_user(hdrbuf, arg, h->xfer_struct_len))
		return -EFAULT;
	real_nr = rd32(hdrbuf, h->xfer_cmd_off);
	real_size = rd32(hdrbuf, h->xfer_size_off);
	real_ptr = rd64(hdrbuf, h->xfer_ptr_off);
	if (!real_size || real_size > h->max_ioctl_size || !real_ptr)
		return -EINVAL;
	c->nr = real_nr;
	c->size = real_size;
	c->addr = real_ptr;
	c->desc = find_ioctl(c->t, c->ctx->dev_tag, real_nr);
	return 0;
}

/* Gather the embedded params buffer and its nested pointers. */
static int gather_embedded(struct call *c)
{
	const struct nvrm_ioctl_desc *d = c->desc;
	const struct nvrm_class_desc *cls = NULL;
	u32 plen = 0;
	u32 i;

	if (!d || d->emb_ptr_off == NVRM_NONE_U32)
		return 0;
	if (!nvrm_range_valid(d->emb_ptr_off, 8, c->size))
		return -EINVAL;

	/* Reject table-marked blocked controls early. The host
	 * independently enforces the block. */
	if (d->cmd_off != NVRM_NONE_U32 &&
	    nvrm_range_valid(d->cmd_off, 4, c->size)) {
		const struct nvrm_ctrl_desc *blk =
			find_ctrl(c->t, rd32(c->inl, d->cmd_off));

		if (blk && (blk->flags & NVRM_CF_BLOCK)) {
			pr_warn_ratelimited(
				"virtio_nvrm: control %#x is blocked -- not forwarded\n",
				rd32(c->inl, d->cmd_off));
			return -EPERM;
		}
	}

	/* Check for NULL before class lookup: parameterless classes may
	 * have no descriptor row. */
	c->saved_ptr = rd64(c->inl, d->emb_ptr_off);
	if (!c->saved_ptr)
		return 0;

	switch (d->emb_len_kind) {
	case NVRM_EMB_LEN_FIELD:
		if (!nvrm_range_valid(d->emb_len_off, 4, c->size))
			return -EINVAL;
		plen = rd32(c->inl, d->emb_len_off);
		break;
	case NVRM_EMB_LEN_CLASS:
		if (!nvrm_range_valid(d->emb_len_off, 4, c->size))
			return -EINVAL;
		cls = find_class(c->t, rd32(c->inl, d->emb_len_off));
		if (!cls) {
			/* Reject unknown classes; guessing a params length
			 * could expose an out-of-bounds host read. */
			pr_warn_ratelimited(
				"virtio_nvrm: hClass %#x unknown -- EOPNOTSUPP instead of a guess (%s[%d])\n",
				rd32(c->inl, d->emb_len_off), current->comm,
				task_pid_nr(current));
			return -EOPNOTSUPP;
		}
		plen = cls->param_size;
		/* Reject unsupported non-NULL pRightsRequested. */
		if (d->rights_off != NVRM_NONE_U32 &&
		    c->size == d->rights_if_size) {
			if (!nvrm_range_valid(d->rights_off, 8, c->size))
				return -EINVAL;
			if (rd64(c->inl, d->rights_off))
				return -EOPNOTSUPP;
		}
		break;
	case NVRM_EMB_LEN_FIXED:
		/* The constant length is stored in emb_len_off, as for
		 * NVOS41.pEvent. Marshal that output buffer instead of
		 * forwarding its guest pointer. */
		plen = d->emb_len_off;
		break;
	default:
		return 0;
	}

	if (!plen)
		return 0; /* length 0: nothing to take along */
	if (plen > c->t->hdr.max_aux)
		return -EMSGSIZE;

	c->aux = kvzalloc(plen, GFP_KERNEL);
	if (!c->aux)
		return -ENOMEM;
	if (call_in(c, c->aux, c->saved_ptr, plen)) {
		kvfree(c->aux);
		c->aux = NULL;
		return -EFAULT;
	}
	c->aux_len = plen;
	c->params_len = plen;
	c->emb_off = d->emb_ptr_off;

	/* Resolve second-level pointers and lengths from the control table. */
	if (d->cmd_off != NVRM_NONE_U32 &&
	    nvrm_range_valid(d->cmd_off, 4, c->size)) {
		const struct nvrm_ctrl_desc *ct =
			find_ctrl(c->t, rd32(c->inl, d->cmd_off));

		if (ct && ct->count) {
			struct {
				u32 ptr_off;
				u64 gva;
				u32 len;
			} plan[NVRM_MAX_NESTED];
			u32 n = 0;
			size_t total = c->params_len;
			u8 *bigger;

			if (ct->count > NVRM_MAX_NESTED ||
			    !nvrm_range_valid(ct->first, ct->count,
					      c->t->hdr.n_nested))
				return -EPROTO;

			/* Read pointer metadata before reallocating the
			 * params buffer. */
			for (i = 0; i < ct->count; i++) {
				const struct nvrm_nested_row *row =
					&c->t->nested[ct->first + i];
				u64 gva;
				u32 nlen;

				if (!nvrm_range_valid(row->ptr_off, 8,
						      c->params_len))
					continue;
				gva = rd64(c->aux, row->ptr_off);
				if (row->len_kind == NVRM_NLEN_FIXED) {
					nlen = row->len_off;
				} else {
					if (!nvrm_range_valid(row->len_off, 4,
							      c->params_len))
						return -EINVAL;
					if (check_mul_overflow(
						    rd32(c->aux, row->len_off),
						    row->elem, &nlen))
						return -EINVAL;
				}
				if (!gva || !nlen)
					continue;
				if (nlen > c->t->hdr.max_aux ||
				    total + nlen > c->t->hdr.max_aux)
					return -EMSGSIZE;
				plan[n].ptr_off = row->ptr_off;
				plan[n].gva = gva;
				plan[n].len = nlen;
				total += nlen;
				n++;
			}
			if (n) {
				bigger = kvzalloc(total, GFP_KERNEL);
				if (!bigger)
					return -ENOMEM;
				memcpy(bigger, c->aux, c->params_len);
				kvfree(c->aux);
				c->aux = bigger;
				c->aux_len = c->params_len;
				for (i = 0; i < n; i++) {
					if (call_in(c, c->aux + c->aux_len,
						    plan[i].gva, plan[i].len))
						return -EFAULT;
					c->nested[i].ptr_off = plan[i].ptr_off;
					c->nested[i].aux_off = (u32)c->aux_len;
					c->nested[i].len = plan[i].len;
					c->nested[i].pad = 0;
					c->nested_gva[i] = plan[i].gva;
					c->aux_len += plan[i].len;
				}
				c->n_nested = n;
			}
		}
	}

	/* An fd INSIDE the params buffer (NV0005.data for NV01_EVENT_OS_EVENT):
	 * RM compares it as a NUMBER against the registered events, and the
	 * host holds those under ITS numbers. So translate it. */
	if (cls && cls->fd_off != NVRM_NONE_U32) {
		s64 val;

		if (c->params_len < 8 || cls->fd_off > c->params_len - 8)
			return -EINVAL;
		/* NV0005.data is an FD only when its inner hClass is
		 * NV01_EVENT_OS_EVENT; kernel-callback classes store a
		 * pointer there. The outer allocation class alone cannot
		 * distinguish them. */
		if (cls->fd_if_off != NVRM_NONE_U32) {
			if (c->params_len < 4 ||
			    cls->fd_if_off > c->params_len - 4)
				return -EINVAL;
			if (rd32(c->aux, cls->fd_if_off) != cls->fd_if_val)
				goto no_alloc_fd;
		}
		memcpy(c->aux_fd_orig, c->aux + cls->fd_off, 8);
		memcpy(&val, c->aux_fd_orig, 8);
		c->aux_fd_off = cls->fd_off;
		c->aux_fd_len = 8;
		if (val < 0) {
			c->aux_fd_token = NVRM_NONE_U64;
		} else if (c->kern) {
			/* See the control-fd branch below: a kernel caller has
			 * no fd table of its own, and current's is somebody
			 * else's. */
			pr_warn_ratelimited(
				"virtio_nvrm: kernel path names fd %lld in alloc params\n",
				(long long)val);
			return -EBADF;
		} else {
			u64 tok;

			if (nvrm_token_of_fd((int)val, &tok, &c->aux_fd_proc)) {
				pr_warn_ratelimited(
					"virtio_nvrm: alloc params name foreign fd %lld\n",
					(long long)val);
				return -EBADF;
			}
			c->aux_fd_token = tok;
		}
	}
no_alloc_fd:

	/* Control FD fields use NVRM_CF_FD table offsets. Copy four bytes:
	 * these are NvS32, unlike class FD fields encoded as NvP64. An
	 * eight-byte copy would overwrite the adjacent flags in
	 * EXPORT_OBJECT_TO_FD. */
	if (d->cmd_off != NVRM_NONE_U32 &&
	    nvrm_range_valid(d->cmd_off, 4, c->size)) {
		const struct nvrm_ctrl_desc *ct =
			find_ctrl(c->t, rd32(c->inl, d->cmd_off));

		if (ct && (ct->flags & NVRM_CF_FD) &&
		    ct->fd_off != NVRM_NONE_U32) {
			s32 val;

			/* Written so it cannot wrap: fd_off is a u32 out of
			 * the table, and `fd_off + 4` would overflow to a
			 * small number for a value near U32_MAX. */
			if (c->params_len < 4 || ct->fd_off > c->params_len - 4)
				return -EINVAL;
			/* Two fd fields in one call have never occurred and
			 * there is only one slot on the wire. Refuse rather
			 * than silently translate one of them. */
			if (c->aux_fd_off != NVRM_NONE_U32)
				return -EOPNOTSUPP;
			memcpy(c->aux_fd_orig, c->aux + ct->fd_off, 4);
			memcpy(&val, c->aux_fd_orig, 4);
			c->aux_fd_off = ct->fd_off;
			c->aux_fd_len = 4;
			if (val < 0) {
				c->aux_fd_token = NVRM_NONE_U64;
			} else if (c->kern && (current->flags & PF_KTHREAD)) {
				/* IMPORT_OBJECT_FROM_FD names the exporting
				 * userspace session even on the kernel
				 * path. NVKMS executes it in the calling
				 * process's ioctl context, so resolve its
				 * FD through current. Reject kthreads,
				 * whose FD table is unrelated.
				 * nvrm_token_of_fd also requires one of our
				 * nodes. */
				pr_warn_ratelimited(
					"virtio_nvrm: kthread names fd %d in control %#x (comm %s, pid %d) -- no table to read it against\n",
					val, rd32(c->inl, d->cmd_off),
					current->comm, current->pid);
				return -EBADF;
			} else {
				u64 tok;

				if (nvrm_token_of_fd((int)val, &tok,
						     &c->aux_fd_proc)) {
					pr_warn_ratelimited(
						"virtio_nvrm: control %#x names foreign fd %d\n",
						rd32(c->inl, d->cmd_off), val);
					return -EBADF;
				}
				c->aux_fd_token = tok;
			}
		}
	}

	/* The guest names the card by the address IT can see; RM knows only
	 * its own. See bdf_mediation. */
	if (d->cmd_off != NVRM_NONE_U32 &&
	    nvrm_range_valid(d->cmd_off, 4, c->size))
		bdf_rewrite_request(c, rd32(c->inl, d->cmd_off));

	return 0;
}

/* Kernel OS-descriptor allocation carries params followed by GPA runs in
 * aux. params_len separates them. The host recognizes this form by
 * NV_ESC_RM_ALLOC; older hosts reject it instead of treating params as page
 * runs. */
static int gather_osdesc_kern(struct call *c)
{
	const struct nvrm_table_hdr *h = &c->t->hdr;
	struct nvrm_gpa_run *runs;
	struct nvrm_pin *pin;
	struct dma_buf *dmabuf;
	struct device *dmadev;
	u8 *merged;
	u64 desc, limit;
	u32 dtype, nruns, max_runs;
	size_t plen = NVRM_OSDESC_PARAMS_SIZE;

	if (c->params_len < plen)
		return -EINVAL;

	dtype = rd32(c->aux, NVRM_OSDESC_TYPE_OFF);
	desc = rd64(c->aux, NVRM_OSDESC_DESCRIPTOR_OFF);
	limit = rd64(c->aux, NVRM_OSDESC_LIMIT_OFF);

	if (display > 1)
		pr_info("virtio_nvrm: osdesc(kernel): descriptorType %u, descriptor %#llx, limit %#llx\n",
			dtype, desc, limit);

	switch (dtype) {
	case NVRM_OSDESC_OS_DMA_BUF_PTR:
		dmabuf = (struct dma_buf *)(uintptr_t)desc;
		break;
	case NVRM_OSDESC_OS_SGT_PTR:
		/* Use the dma_buf exporter; a foreign sg_table has no
		 * lifetime guarantee here. PRIME import normally uses
		 * descriptor type 5 (DMA_BUF). Type 6 (SGT) appears only
		 * after that import failed (nvidia-drm-gem-dma-buf.c:155);
		 * investigate the earlier failure first. */
		pr_warn_ratelimited(
			"virtio_nvrm: OS descriptor type %u (sg_table) is not built; the dma-buf import above failed first -- look there\n",
			dtype);
		return -EOPNOTSUPP;
	default:
		pr_warn_ratelimited(
			"virtio_nvrm: OS descriptor from the kernel path with descriptorType %u -- not built\n",
			dtype);
		return -EOPNOTSUPP;
	}
	if (!dmabuf)
		return -EINVAL;

	/* The importing device, and it must be able to do DMA. */
	dmadev = NULL;
	if (c->dev && c->dev->vdev && c->dev->vdev->dev.parent &&
	    dev_is_pci(c->dev->vdev->dev.parent))
		dmadev = c->dev->vdev->dev.parent;
	if (!dmadev) {
		pr_warn_ratelimited(
			"virtio_nvrm: no DMA-capable device for a PRIME import\n");
		return -EOPNOTSUPP;
	}

	pin = nvrm_pin_dmabuf(dmabuf, dmadev);
	if (IS_ERR(pin)) {
		pr_warn_ratelimited(
			"virtio_nvrm: PRIME import: page walk failed: %ld\n",
			PTR_ERR(pin));
		return PTR_ERR(pin);
	}
	c->pin = pin;
	c->pin->object.client = rd32(c->inl, NVRM_NVOS64_HROOT_OFF);
	c->pmem_orig = desc;
	c->limit_orig = limit;

	max_runs = (u32)pin->npages;
	if (plen + (size_t)max_runs * sizeof(*runs) > h->max_aux)
		max_runs = (u32)((h->max_aux - plen) / sizeof(*runs));
	merged = kvzalloc(plen + (size_t)max_runs * sizeof(*runs), GFP_KERNEL);
	if (!merged)
		return -ENOMEM;
	memcpy(merged, c->aux, plen);
	runs = (struct nvrm_gpa_run *)(merged + plen);
	nruns = nvrm_runs_from_pages(pin->pages, pin->npages, runs, max_runs);
	if (!nruns) {
		kvfree(merged);
		return -EMSGSIZE;
	}
	kvfree(c->aux);
	c->aux = merged;
	c->aux_len = plen + (size_t)nruns * sizeof(*runs);
	c->params_len = plen;
	c->run_count = nruns;
	if (display > 1)
		pr_info("virtio_nvrm: PRIME import: %lu pages in %u runs, params %zu bytes\n",
			pin->npages, nruns, plen);
	return 0;
}

/* Resolve OS-descriptor memory to pinned guest pages and send GPA runs.
 * Guest virtual addresses are not valid host pointers. */
static int gather_osdesc(struct call *c)
{
	const struct nvrm_table_hdr *h = &c->t->hdr;
	const struct nvrm_ioctl_desc *d = c->desc;
	struct nvrm_gpa_run *runs;
	unsigned long pmem;
	u64 limit, dlen;
	u32 nruns, max_runs;

	/* Userspace sends NVOS02 through RM_ALLOC_MEMORY, marked
	 * NVRM_F_OSDESC. NVKMS sends NVOS64 through RM_ALLOC with class
	 * 0x71 and an NV_OS_DESC_MEMORY_ALLOCATION_PARAMS buffer; detect
	 * that form separately. */
	if (c->kern && d && c->nr == NVRM_KESC_ALLOC &&
	    NVRM_NVOS64_HCLASS_OFF + 4 <= c->size &&
	    rd32(c->inl, NVRM_NVOS64_HCLASS_OFF) == h->osdesc_class)
		return gather_osdesc_kern(c);

	if (!d || !(d->flags & NVRM_F_OSDESC)) {
		/* Emit OS-descriptor diagnostics only for allocation calls. */
		if (display > 1 && c->kern && c->nr == NVRM_KESC_ALLOC)
			pr_info("virtio_nvrm: osdesc: alloc nr %#x has no OSDESC flag\n",
				c->nr);
		return 0;
	}
	if (!nvrm_range_valid(d->emb_len_off, 4, c->size))
		return -EINVAL;
	if (display > 1 && c->kern)
		pr_info("virtio_nvrm: osdesc: class at +%u is %#x, looking for %#x\n",
			d->emb_len_off, rd32(c->inl, d->emb_len_off),
			h->osdesc_class);
	if (rd32(c->inl, d->emb_len_off) != h->osdesc_class)
		return 0; /* other memory class: forward normally */
	/* Never pass a kernel VA to nvrm_pin_range, which resolves
	 * userspace addresses. Kernel descriptors require their own
	 * dma-buf/sg-table path. */
	if (c->kern) {
		u32 dtype = NVRM_NONE_U32;

		if (NVRM_OSDESC_TYPE_OFF + 4 <= c->size)
			dtype = rd32(c->inl, NVRM_OSDESC_TYPE_OFF);
		/* Log the descriptor type: user VA=0, dma-buf=5,
		 * sg-table=6. */
		pr_warn_ratelimited(
			"virtio_nvrm: OS descriptor from the kernel path is not supported (descriptorType %u)\n",
			dtype);
		return -EOPNOTSUPP;
	}
	/* Validate all OS-descriptor fields, including reply status/handle,
	 * against the caller-supplied ioctl size. */
	if (!nvrm_range_valid(NVRM_NVOS02_HROOT_OFF, 4, c->size) ||
	    !nvrm_range_valid(h->osdesc_pmem_off, 8, c->size) ||
	    !nvrm_range_valid(h->osdesc_limit_off, 8, c->size) ||
	    !nvrm_range_valid(h->osdesc_status_off, 4, c->size) ||
	    !nvrm_range_valid(h->osdesc_handle_off, 4, c->size))
		return -EINVAL;

	pmem = (unsigned long)rd64(c->inl, h->osdesc_pmem_off);
	limit = rd64(c->inl, h->osdesc_limit_off);
	dlen = limit + 1;
	if (!pmem || !dlen || (dlen & ~PAGE_MASK) || (pmem & ~PAGE_MASK))
		return -EINVAL;

	c->pin = nvrm_pin_range(pmem, dlen);
	if (IS_ERR(c->pin)) {
		int err = PTR_ERR(c->pin);

		c->pin = NULL;
		return err;
	}
	c->pmem_orig = pmem;
	c->pin->object.client = rd32(c->inl, NVRM_NVOS02_HROOT_OFF);
	c->limit_orig = limit;

	/* The userspace form carries only GPA runs in aux, with no params
	 * prefix. */
	max_runs = (u32)(c->pin->npages);
	if ((size_t)max_runs * sizeof(*runs) > h->max_aux)
		max_runs = h->max_aux / sizeof(*runs);
	runs = kvzalloc((size_t)max_runs * sizeof(*runs), GFP_KERNEL);
	if (!runs)
		return -ENOMEM;
	nruns = nvrm_runs_from_pages(c->pin->pages, c->pin->npages, runs,
				     max_runs);
	if (!nruns) {
		kvfree(runs);
		return -EMSGSIZE;
	}
	kvfree(c->aux);
	c->aux = (u8 *)runs;
	c->aux_len = (size_t)nruns * sizeof(*runs);
	c->params_len = 0;
	c->run_count = nruns;
	return 0;
}

/* Perform the write-back to the application. */
static int write_back(struct call *c, const struct nvrm_rsp *rsp,
		      const u8 *body)
{
	size_t il = min_t(size_t, rsp->inline_len, c->size);
	size_t al = min_t(size_t, rsp->aux_len, c->aux_len);
	u32 i;

	if (il)
		memcpy(c->inl, body, il);
	if (al)
		memcpy(c->aux, body + rsp->inline_len, al);

	if (c->emb_off != NVRM_NONE_U32 && c->saved_ptr) {
		/* 0. vram_debug. Before step 1, because FB_GET_INFO (V1) keeps
		 *    its list in a nested buffer that step 1 copies out. */
		if (c->desc->cmd_off != NVRM_NONE_U32 &&
		    nvrm_range_valid(c->desc->cmd_off, 4, c->size))
			vram_debug_reply(c, rd32(c->inl, c->desc->cmd_off));
		/* 1. nested buffers back to their guest addresses */
		for (i = 0; i < c->n_nested; i++) {
			if (call_out(c, c->nested_gva[i],
				     c->aux + c->nested[i].aux_off,
				     c->nested[i].len))
				return -EFAULT;
			/* 2. Restore the nested guest pointer inside
			 * params. */
			wr64(c->aux, c->nested[i].ptr_off, c->nested_gva[i]);
		}
		/* 3. restore the application's own fd number */
		if (c->aux_fd_off != NVRM_NONE_U32)
			memcpy(c->aux + c->aux_fd_off, c->aux_fd_orig,
			       c->aux_fd_len);
		/* 3b. Restore guest BDFs before copy-out. */
		if (c->desc->cmd_off != NVRM_NONE_U32 &&
		    nvrm_range_valid(c->desc->cmd_off, 4, c->size))
			bdf_rewrite_reply(c, rd32(c->inl, c->desc->cmd_off));
		/* 4. Copy params_len bytes; nested buffers follow the
		 * params block. */
		if (call_out(c, c->saved_ptr, c->aux, c->params_len))
			return -EFAULT;
		/* 5. Restore the inline guest pointer. */
		wr64(c->inl, c->emb_off, c->saved_ptr);
	}

	if (c->nr == NVRM_ESC_CARD_INFO)
		bdf_rewrite_card_info(c);
	bdf_rewrite_attach_gpus(c, false);
	if (READ_ONCE(vram_debug))
		vram_debug_inline(c);

	/* Diagnose host IDs still present after all rewrites. */
	if (bdf_debug && c->dev->bdf_host_id) {
		u32 off;

		for (off = 0; off + 4 <= c->size; off += 4)
			if (rd32(c->inl, off) == c->dev->bdf_host_id) {
				pr_info_ratelimited(
					"virtio_nvrm: bdf_debug: ioctl %u inline still names host %#x at +%u (size %u)\n",
					c->nr, c->dev->bdf_host_id, off,
					c->size);
				break;
			}
	}

	/* Restore the caller FD and OS-descriptor address. */
	if (c->fd_off != NVRM_NONE_U32 &&
	    nvrm_range_valid(c->fd_off, 4, c->size))
		memcpy(c->inl + c->fd_off, &c->fd_orig, 4);
	/* Restore NVOS02 pMemory/limit only for userspace. Kernel
	 * OS-descriptor calls use NVOS64, where the same offsets hold
	 * rights and size/flags; the host already restores that form's
	 * params. */
	if (c->run_count && !c->kern) {
		wr64(c->inl, c->t->hdr.osdesc_pmem_off, c->pmem_orig);
		wr64(c->inl, c->t->hdr.osdesc_limit_off, c->limit_orig);
	}

	if (call_out(c, c->addr, c->inl, c->size))
		return -EFAULT;
	return 0;
}

/* Process VIDMEM allocations hold balloon_gate for reading through
 * completion. The balloon holds it for writing while freeing chunks and
 * retrying NVKMS, so a process cannot consume the released quota between
 * those operations. */
static DECLARE_RWSEM(balloon_gate);

static bool balloon_gated(const struct call *c)
{
	if (c->kern || ctx_is_uvm(c->ctx))
		return false;
	/* NVRM_KESC_ALLOC is NV_ESC_RM_ALLOC: the op table names the escape
	 * both doors use. hClass sits at +12 in NVOS64 and in NVOS21 alike. */
	if (c->nr == NVRM_KESC_ALLOC)
		return c->size >= NVRM_NVOS64_HCLASS_OFF + 4 &&
		       rd32(c->inl, NVRM_NVOS64_HCLASS_OFF) ==
			       NVRM_CLASS_MEMORY_LOCAL_USER;
	if (c->nr == NVRM_ESC_RM_VID_HEAP_CONTROL)
		return c->size >= NVRM_NVOS32_FUNCTION_OFF + 4 &&
		       rd32(c->inl, NVRM_NVOS32_FUNCTION_OFF) ==
			       NVRM_NVOS32_FUNCTION_ALLOC_SIZE;
	return false;
}

/* Shared marshalling and transport for process ioctls and kernel RM ops.
 * c->kern selects only how caller memory is accessed. */
static long nvrm_call_run(struct call *c)
{
	struct nvrm_ctx *ctx = c->ctx;
	struct nvrm_dev *dev = c->dev;
	struct nvrm_xfer *x = NULL;
	struct nvrm_req *r;
	struct nvrm_rsp *rsp;
	struct nvrm_object_key freed;
	bool gated = false, submitted = false;
	long ret;
	u32 i;

	mutex_lock(&ctx->lock);

	/* (1) Fetch the inline block. */
	if (c->size) {
		c->inl = kvzalloc(c->size, GFP_KERNEL);
		if (!c->inl) {
			ret = -ENOMEM;
			goto out;
		}
		if (call_in(c, c->inl, c->addr, c->size)) {
			ret = -EFAULT;
			goto out;
		}
	}

	/* (1b) A process's VIDMEM allocation passes the balloon's gate. */
	if (balloon_gated(c)) {
		if (down_read_killable(&balloon_gate)) {
			ret = -EINTR;
			goto out;
		}
		gated = true;
	}

	/* (2) Resolve userspace FD fields by file identity. Kernel callers
	 * already set their token in kapi_forward; never replace it with a
	 * lookup against current. */
	if (!c->kern && c->desc && c->desc->fd_off != NVRM_NONE_U32) {
		s32 n;

		if (!nvrm_range_valid(c->desc->fd_off, 4, c->size)) {
			ret = -EINVAL;
			goto out;
		}
		memcpy(&n, c->inl + c->desc->fd_off, 4);
		c->fd_off = c->desc->fd_off;
		memcpy(&c->fd_orig, &n, 4);
		if (n < 0) {
			c->fd_token =
				NVRM_NONE_U64; /* -1: pass through unchanged */
		} else if (nvrm_token_of_fd(n, &c->fd_token, &c->fd_proc)) {
			pr_warn_ratelimited(
				"virtio_nvrm: fd field points at foreign fd %d\n",
				n);
			ret = -EBADF;
			goto out;
		}
	}

	/* (3) Embedded pointer plus the pointers inside it. */
	ret = gather_embedded(c);
	if (ret)
		goto out;

	/* (4) OS descriptor: pin pages instead of sending a VA. */
	ret = gather_osdesc(c);
	if (ret)
		goto out;

	/* (5) Rewrite inline gpuIds after all gathering. */
	bdf_rewrite_attach_gpus(c, true);

	/* (6) Build the request. */
	x = nvrm_xfer_alloc(sizeof(*r) + c->size + c->aux_len,
			    sizeof(*rsp) + c->size + c->aux_len);
	if (IS_ERR(x)) {
		ret = PTR_ERR(x);
		x = NULL;
		goto out;
	}
	r = nvrm_req_init(dev, x, NVRM_KIND_IOCTL,
			  ctx->proc ? ctx->proc->id : 0);
	r->dev_tag = ctx->dev_tag;
	r->ioctl_nr = c->nr;
	r->target_token = ctx->token;
	r->inline_len = c->size;
	r->aux_len = (u32)c->aux_len;
	r->fd_field_off = c->fd_off;
	r->fd_field_token = c->fd_token;
	r->fd_field_proc = c->fd_proc;
	r->embedded_ptr_off = c->emb_off;
	r->aux_fd_field_off = c->aux_fd_off;
	r->aux_fd_field_token = c->aux_fd_token;
	r->aux_fd_field_proc = c->aux_fd_proc;
	r->gpa_run_count = c->run_count;
	r->nested_count = c->n_nested;
	for (i = 0; i < c->n_nested; i++)
		r->nested[i] = c->nested[i];
	if (c->size)
		memcpy((u8 *)x->req + sizeof(*r), c->inl, c->size);
	if (c->aux_len)
		memcpy((u8 *)x->req + sizeof(*r) + c->size, c->aux, c->aux_len);
	x->req_len = sizeof(*r) + c->size + c->aux_len;

	/* (7) User requests may be abandoned on fatal signals. Kernel
	 * callers use NVRM_TEARDOWN_TIMEOUT so module teardown can finish
	 * when the backend stops answering. */
	ret = nvrm_xfer_run(dev, &x, !c->kern, &submitted);
	if (ret) {
		ctx->uncertain |= submitted;
		goto out;
	}
	rsp = x->rsp;
	if (sizeof(*rsp) + (size_t)rsp->inline_len + rsp->aux_len >
	    x->rsp_len) {
		pr_warn_ratelimited(
			"virtio_nvrm: reply claims %u+%u bytes, but only %u arrived\n",
			rsp->inline_len, rsp->aux_len, x->rsp_len);
		ret = -EIO;
		ctx->uncertain = true;
		goto out;
	}

	/* Record native ownership before copy_to_user can fail. A malformed
	 * reply or transport error cannot prove that native allocation failed. */
	if (c->pin) {
		u32 handle;
		enum nvrm_osdesc_result result = nvrm_osdesc_reply(
			(u8 *)x->rsp + sizeof(*rsp), rsp->inline_len, rsp->ret,
			c->t->hdr.osdesc_status_off,
			c->t->hdr.osdesc_handle_off, &handle);

		if (result == NVRM_OSDESC_CREATED) {
			c->pin->object.handle = handle;
			spin_lock(&ctx->pin_lock);
			list_add_tail(&c->pin->node, &ctx->pins);
			spin_unlock(&ctx->pin_lock);
			c->pin = NULL;
		} else if (result == NVRM_OSDESC_REJECTED) {
			nvrm_unpin(c->pin);
			c->pin = NULL;
		} else {
			ctx->uncertain = true;
		}
	}

	/* (8) Write back to the application. */
	ret = write_back(c, rsp, (u8 *)x->rsp + sizeof(*rsp));
	if (ret)
		goto out;
	ret = rsp->ret;

	/* (8b) Successful NVOS41 with MoreEvents re-arms poll after
	 * write_back updates c->inl. Status/MoreEvents offsets come from
	 * nvrm_wire.h. */
	if (!c->kern && c->nr == NVRM_ESC_RM_GET_EVENT_DATA && ret == 0 &&
	    c->size >= NVRM_NVOS41_SIZE &&
	    rd32(c->inl, NVRM_NVOS41_STATUS_OFF) == NVRM_NV_OK &&
	    rd32(c->inl, NVRM_NVOS41_MOREEVENTS_OFF))
		atomic_set(&ctx->events_pending, 1);

	if (!ctx->uncertain && c->desc && (c->desc->flags & NVRM_F_FREE) &&
	    nvrm_free_reply(c->inl, min_t(u32, c->size, rsp->inline_len), ret,
			    &freed)) {
		struct nvrm_pin *p, *tmp;

		/* nvrm_unpin sleeps, so drop pin_lock around it. ctx->lock
		 * serializes all list mutations and keeps the cached next
		 * entry valid. */
		spin_lock(&ctx->pin_lock);
		list_for_each_entry_safe(p, tmp, &ctx->pins, node) {
			if (nvrm_object_freed(&p->object, &freed)) {
				list_del(&p->node);
				spin_unlock(&ctx->pin_lock);
				nvrm_unpin(p);
				spin_lock(&ctx->pin_lock);
			}
		}
		spin_unlock(&ctx->pin_lock);
	}

out:
	if (gated)
		up_read(&balloon_gate);
	if (c->pin) {
		if (submitted)
			nvrm_quarantine(dev, &c->pin->node,
					&dev->quarantined_pins, c->pin->npages);
		else
			nvrm_unpin(c->pin);
	}
	nvrm_xfer_free(x);
	kvfree(c->aux);
	kvfree(c->inl);
	mutex_unlock(&ctx->lock);
	return ret;
}

static long nvrm_node_ioctl(struct file *filp, unsigned int cmd,
			    unsigned long arg)
{
	struct nvrm_ctx *ctx = filp->private_data;
	struct nvrm_dev *dev;
	struct call c;
	long ret;

	if (!ctx || !ctx->dev || !ctx->dev->tbl.blob ||
	    READ_ONCE(ctx->dev->stopping))
		return -ENODEV;
	dev = ctx->dev;

	memset(&c, 0, sizeof(c));
	c.ctx = ctx;
	c.dev = dev;
	c.t = &dev->tbl;
	c.emb_off = NVRM_NONE_U32;
	c.fd_off = NVRM_NONE_U32;
	c.fd_token = NVRM_NONE_U64;
	/* Not stated by default; the host then falls back to the caller's own
	 * mirror, which is what every path except a cross-process import wants. */
	c.fd_proc = NVRM_NONE_U32;
	c.aux_fd_off = NVRM_NONE_U32;
	c.aux_fd_token = NVRM_NONE_U64;
	c.aux_fd_proc = NVRM_NONE_U32;
	c.aux_fd_len = 8;
	c.addr = (u64)arg;

	/* (0) Decode ioctl number/size. UVM uses raw command numbers and
	 * table-defined sizes; reject unknown commands. */
	if (ctx_is_uvm(ctx)) {
		c.nr = cmd;
		c.desc = find_ioctl(c.t, ctx->dev_tag, c.nr);
		if (!c.desc || c.desc->size == NVRM_SIZE_FROM_IOC) {
			pr_warn_ratelimited(
				"virtio_nvrm: UVM command %#x unknown -- no size available\n",
				cmd);
			return -EOPNOTSUPP;
		}
		c.size = c.desc->size;
	} else {
		c.nr = _IOC_NR(cmd);
		c.size = _IOC_SIZE(cmd);
		c.desc = find_ioctl(c.t, ctx->dev_tag, c.nr);
		if (c.desc && (c.desc->flags & NVRM_F_XFER)) {
			ret = resolve_xfer(&c, (void __user *)arg);
			if (ret)
				return ret;
		}
	}
	if (c.size > c.t->hdr.max_inline)
		return -EMSGSIZE;

	return nvrm_call_run(&c);
}

/* Host-visible window: guest-physical SHMEM slots reserved by win_alloc and
 * populated with RM mappings by the host. */

struct nvrm_winmap {
	struct kref ref;
	struct nvrm_dev *dev;
	u64 off;
	size_t len;
	/* Whose mapping this was. The release can outlive the FD (the vma
	 * holds a reference), which is why the id sits here and not on the
	 * context. */
	u32 guest_proc;
};

static void winmap_release(struct kref *ref)
{
	struct nvrm_winmap *m = container_of(ref, struct nvrm_winmap, ref);

	nvrm_simple(m->dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0, m->off, m->len,
		    NULL, false, m->guest_proc);
	win_free(m->dev, m->off, m->len);
	nvrm_dev_put(m->dev);
	kfree(m);
}

/* Track VMA open/close, including splits from partial munmap, so the final close releases the host window mapping. */
static void nvrm_win_vm_open(struct vm_area_struct *vma)
{
	kref_get(&((struct nvrm_winmap *)vma->vm_private_data)->ref);
}

static void nvrm_win_vm_close(struct vm_area_struct *vma)
{
	kref_put(&((struct nvrm_winmap *)vma->vm_private_data)->ref,
		 winmap_release);
}

static const struct vm_operations_struct nvrm_win_vm_ops = {
	.open = nvrm_win_vm_open,
	.close = nvrm_win_vm_close,
};

static int nvrm_mmap_window(struct nvrm_ctx *ctx, struct vm_area_struct *vma,
			    size_t len)
{
	struct nvrm_dev *dev = ctx->dev;
	struct nvrm_winmap *m;
	u64 cache = 0;
	long off;
	int ret;

	off = win_alloc(dev, len);
	if (off < 0)
		return (int)off;

	m = kzalloc(sizeof(*m), GFP_KERNEL);
	if (!m) {
		win_free(dev, off, len);
		return -ENOMEM;
	}
	kref_init(&m->ref);
	m->dev = dev;
	m->off = off;
	m->len = len;
	m->guest_proc = ctx->proc ? ctx->proc->id : 0;

	/* The host maps this window offset and returns the NVOS33 cache type. */
	ret = nvrm_simple(dev, NVRM_KIND_MAP_PREPARE, ctx->dev_tag, 0,
			  ctx->token, off, len, &cache, true,
			  ctx->proc ? ctx->proc->id : 0);
	if (ret < 0) {
		/* Log the caller and requested size with mapping failures. */
		if (ret != -ERESTARTSYS)
			pr_warn_ratelimited(
				"virtio_nvrm: MAP_PREPARE failed: %d (%s[%d], %s node, %zu KiB at window+%#lx)\n",
				ret, current->comm, task_pid_nr(current),
				ctx->dev_tag == NVRM_DEV_CTL ? "ctl" :
				ctx->dev_tag == NVRM_DEV_GPU ? "gpu" :
							       "other",
				(size_t)(len >> 10), (unsigned long)off);
		/* An interrupted/timed-out prepare may already have mapped
		 * the slot. Release it on the host before bitmap reuse, or
		 * later allocations repeatedly hit the stale reservation
		 * (measured 2026-08-17). Releasing a never-created mapping
		 * is harmless. */
		if (ret == -ERESTARTSYS || ret == -ETIMEDOUT)
			nvrm_simple(dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0, off,
				    len, NULL, false,
				    ctx->proc ? ctx->proc->id : 0);
		kfree(m);
		win_free(dev, off, len);
		return ret;
	}

	/* Cacheability encoding, same as virtio-gpu uses: 1 = cached (system
	 * memory on the ctl node), 2 = uncached (BAR on the GPU node). */
	if (cache == 2)
		vma->vm_page_prot = pgprot_noncached(vma->vm_page_prot);

	vm_flags_set(vma, VM_IO | VM_DONTEXPAND | VM_DONTDUMP);
	/* Do not inherit this session-bound mapping across fork. The child must initialize its own GPU context. */
	vm_flags_set(vma, VM_DONTCOPY);

	if (io_remap_pfn_range(vma, vma->vm_start,
			       (dev->win_base + off) >> PAGE_SHIFT, len,
			       vma->vm_page_prot)) {
		nvrm_simple(dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0, off, len, NULL,
			    false, ctx->proc ? ctx->proc->id : 0);
		kfree(m);
		win_free(dev, off, len);
		return -EAGAIN;
	}

	kref_get(&dev->ref);
	vma->vm_private_data = m;
	vma->vm_ops = &nvrm_win_vm_ops;
	return 0;
}

/* mmap, part 2: UVM pool backed by self-owned pages */

struct nvrm_pool {
	struct list_head quarantine_node;
	struct kref ref;
	struct page **pages;
	unsigned long npages;
};

static void pool_release(struct kref *ref)
{
	struct nvrm_pool *p = container_of(ref, struct nvrm_pool, ref);
	unsigned long i;

	for (i = 0; i < p->npages; i++)
		if (p->pages[i])
			put_page(p->pages[i]);
	nvrm_uncharge(p->npages);
	stat_pool_pages -= p->npages;
	kvfree(p->pages);
	kfree(p);
	module_put(THIS_MODULE);
}

static void nvrm_pool_vm_open(struct vm_area_struct *vma)
{
	kref_get(&((struct nvrm_pool *)vma->vm_private_data)->ref);
}

static void nvrm_pool_vm_close(struct vm_area_struct *vma)
{
	kref_put(&((struct nvrm_pool *)vma->vm_private_data)->ref,
		 pool_release);
}

static const struct vm_operations_struct nvrm_pool_vm_ops = {
	.open = nvrm_pool_vm_open,
	.close = nvrm_pool_vm_close,
};

/* UVM's mmap offset names the pool GPU VA. Allocate guest pages, insert
 * them into the VMA, and send GPA runs for host
 * OS-descriptor/EXTERNAL_RANGE backing at that VA.
 * The module owns these pages; no UVM-FD mmap, prefault, mlock or migration
 * control is needed. Pool references govern their lifetime. */
static int nvrm_mmap_pool(struct nvrm_ctx *ctx, struct vm_area_struct *vma,
			  u64 gpu_va, size_t len)
{
	struct nvrm_dev *dev = ctx->dev;
	struct nvrm_pool *p;
	struct nvrm_gpa_run *runs;
	struct nvrm_xfer *x;
	struct nvrm_req *r;
	struct nvrm_rsp *rsp;
	unsigned long i, npages = len >> PAGE_SHIFT;
	u32 nruns;
	int ret;
	bool submitted = false;

	if (!npages)
		return -EINVAL;
	if ((npages << PAGE_SHIFT) != len)
		return -EINVAL;

	/* Reserve quota before allocating pages. Oversized pool requests
	 * must fail with ENOMEM without exhausting guest RAM. */
	ret = nvrm_charge(npages);
	if (ret)
		return ret;

	p = kzalloc(sizeof(*p), GFP_KERNEL);
	if (!p) {
		nvrm_uncharge(npages);
		return -ENOMEM;
	}
	kref_init(&p->ref);
	p->pages = kvzalloc(npages * sizeof(*p->pages), GFP_KERNEL);
	if (!p->pages) {
		kfree(p);
		nvrm_uncharge(npages);
		return -ENOMEM;
	}
	if (!try_module_get(THIS_MODULE)) {
		kvfree(p->pages);
		kfree(p);
		nvrm_uncharge(npages);
		return -ENODEV;
	}
	p->npages = npages;
	stat_pool_pages += npages;

	vm_flags_set(vma,
		     VM_MIXEDMAP | VM_DONTEXPAND | VM_DONTDUMP | VM_DONTCOPY);

	for (i = 0; i < npages; i++) {
		/* Allow allocation failure without invoking the guest OOM
		 * killer or printing redundant warnings; return ENOMEM to
		 * the caller. */
		p->pages[i] = alloc_page(GFP_USER | __GFP_ZERO |
					 __GFP_RETRY_MAYFAIL | __GFP_NOWARN);
		if (!p->pages[i]) {
			ret = -ENOMEM;
			goto err;
		}
		ret = vm_insert_page(vma, vma->vm_start + (i << PAGE_SHIFT),
				     p->pages[i]);
		if (ret)
			goto err;
	}

	runs = kvzalloc(npages * sizeof(*runs), GFP_KERNEL);
	if (!runs) {
		ret = -ENOMEM;
		goto err;
	}
	nruns = nvrm_runs_from_pages(p->pages, npages, runs, npages);
	if (!nruns || (size_t)nruns * sizeof(*runs) > dev->tbl.hdr.max_aux) {
		kvfree(runs);
		ret = -EMSGSIZE;
		goto err;
	}

	x = nvrm_xfer_alloc(sizeof(*r) + (size_t)nruns * sizeof(*runs),
			    sizeof(*rsp));
	if (IS_ERR(x)) {
		kvfree(runs);
		ret = PTR_ERR(x);
		goto err;
	}
	r = nvrm_req_init(dev, x, NVRM_KIND_UVM_POOL_BACK,
			  ctx->proc ? ctx->proc->id : 0);
	r->dev_tag = ctx->dev_tag;
	r->target_token = ctx->token;
	r->addr = gpu_va;
	r->map_len = len;
	r->gpa_run_count = nruns;
	r->aux_len = nruns * sizeof(*runs);
	memcpy((u8 *)x->req + sizeof(*r), runs, (size_t)nruns * sizeof(*runs));
	x->req_len = sizeof(*r) + (size_t)nruns * sizeof(*runs);
	kvfree(runs);

	ret = nvrm_xfer_run(dev, &x, true, &submitted);
	if (ret)
		goto err_xfer;
	rsp = x->rsp;
	ret = rsp->ret;
	if (ret) {
		pr_warn("virtio_nvrm: UvmPoolBack @%#llx (%zu bytes, %u runs) rejected: %d\n",
			gpu_va, len, nruns, ret);
		goto err_xfer;
	}
	nvrm_xfer_free(x);

	vma->vm_private_data = p;
	vma->vm_ops = &nvrm_pool_vm_ops;
	return 0;

err_xfer:
	nvrm_xfer_free(x);
err:
	/* VMA teardown drops only its own page references. Native RM may
	 * still own submitted backing, including after a failed rollback. */
	if (submitted)
		nvrm_quarantine(dev, &p->quarantine_node,
				&dev->quarantined_pools, p->npages);
	else
		kref_put(&p->ref, pool_release);
	return ret;
}

static int nvrm_node_mmap(struct file *filp, struct vm_area_struct *vma)
{
	struct nvrm_ctx *ctx = filp->private_data;
	size_t len = vma->vm_end - vma->vm_start;
	u64 off = (u64)vma->vm_pgoff << PAGE_SHIFT;

	if (!ctx || !ctx->dev || !ctx->dev->tbl.blob ||
	    READ_ONCE(ctx->dev->stopping))
		return -ENODEV;
	if (!len)
		return -EINVAL;

	if (ctx_is_uvm(ctx)) {
		if (!off)
			return -EOPNOTSUPP;
		return nvrm_mmap_pool(ctx, vma, off, len);
	}
	/* Only offset 0 is valid; the preceding RM_MAP_MEMORY on this file
	 * identifies the mapping. */
	if (off)
		return -EINVAL;
	return nvrm_mmap_window(ctx, vma, len);
}

static const struct file_operations nvrm_node_fops = {
	.owner = THIS_MODULE,
	.open = nvrm_node_open,
	.release = nvrm_node_release,
	.unlocked_ioctl = nvrm_node_ioctl,
	.compat_ioctl = compat_ptr_ioctl,
	.mmap = nvrm_node_mmap,
	.poll = nvrm_node_poll,
};

/* Create device nodes last; remove them first during teardown. */

struct chrdev_range {
	unsigned int major, baseminor, count;
	const char *name;
	bool registered;
};

static struct chrdev_range ranges[] = {
	{ NV_FRONTEND_MAJOR, 0, NV_MAX_GPUS, "nvidia" },
	{ NV_FRONTEND_MAJOR, NV_MINOR_CTL, 1, "nvidiactl" },
	{ NV_UVM_MAJOR, 0, 2, "nvidia-uvm" },
};

struct node_spec {
	unsigned int major, minor;
	const char *name;
	struct device *dev;
	bool created;
};

static struct node_spec nodes[NV_MAX_GPUS + 3];
static unsigned int n_nodes;
static struct class *nvrm_class;

/* The real driver opens its nodes to everyone (on the host: crw-rw-rw-).
 * Without this the guest would need root or a udev rule after all. */
static char *nvrm_devnode(const struct device *dev, umode_t *mode)
{
	if (mode)
		*mode = 0666;
	return NULL;
}

static char gpu_names[NV_MAX_GPUS][16];

static void nvrm_nodes_teardown(void)
{
	unsigned int i;

	for (i = 0; i < n_nodes; i++) {
		if (nodes[i].created) {
			device_destroy(nvrm_class,
				       MKDEV(nodes[i].major, nodes[i].minor));
			nodes[i].created = false;
		}
	}
	n_nodes = 0;
	if (nvrm_class) {
		class_destroy(nvrm_class);
		nvrm_class = NULL;
	}
	for (i = 0; i < ARRAY_SIZE(ranges); i++) {
		if (ranges[i].registered) {
			__unregister_chrdev(ranges[i].major,
					    ranges[i].baseminor,
					    ranges[i].count, ranges[i].name);
			ranges[i].registered = false;
		}
	}
}

static int nvrm_nodes_setup(void)
{
	unsigned int i, g;
	int ret;

	if (gpu_count < 1 || gpu_count > NV_MAX_GPUS)
		return -EINVAL;

	for (i = 0; i < ARRAY_SIZE(ranges); i++) {
		ret = __register_chrdev(ranges[i].major, ranges[i].baseminor,
					ranges[i].count, ranges[i].name,
					&nvrm_node_fops);
		if (ret) {
			pr_err("virtio_nvrm: chrdev %u:%u '%s': %d -- does nvrm_nodes.ko hold the nodes? Then load nvrm_nodes.ko with create_nodes=0\n",
			       ranges[i].major, ranges[i].baseminor,
			       ranges[i].name, ret);
			goto err;
		}
		ranges[i].registered = true;
	}

	nvrm_class = class_create("nvrm");
	if (IS_ERR(nvrm_class)) {
		ret = PTR_ERR(nvrm_class);
		nvrm_class = NULL;
		goto err;
	}
	nvrm_class->devnode = nvrm_devnode;

	n_nodes = 0;
	for (g = 0; g < gpu_count; g++) {
		snprintf(gpu_names[g], sizeof(gpu_names[g]), "nvidia%u", g);
		nodes[n_nodes++] = (struct node_spec){ NV_FRONTEND_MAJOR, g,
						       gpu_names[g] };
	}
	nodes[n_nodes++] = (struct node_spec){ NV_FRONTEND_MAJOR, NV_MINOR_CTL,
					       "nvidiactl" };
	nodes[n_nodes++] = (struct node_spec){ NV_UVM_MAJOR, 0, "nvidia-uvm" };
	nodes[n_nodes++] =
		(struct node_spec){ NV_UVM_MAJOR, 1, "nvidia-uvm-tools" };

	for (i = 0; i < n_nodes; i++) {
		nodes[i].dev = device_create(
			nvrm_class, NULL, MKDEV(nodes[i].major, nodes[i].minor),
			NULL, "%s", nodes[i].name);
		if (IS_ERR(nodes[i].dev)) {
			ret = PTR_ERR(nodes[i].dev);
			pr_err("virtio_nvrm: device_create %s: %d\n",
			       nodes[i].name, ret);
			goto err;
		}
		nodes[i].created = true;
	}
	return 0;

err:
	nvrm_nodes_teardown();
	return ret;
}

/* probe / remove */

/* Defined further down, with the vblank engine it belongs to. */
static void vblank_engine_init(void);

/* virtio_find_vqs changed to virtqueue_info[] in Linux 6.11. Support both
 * the Ubuntu 6.8 guest and Nix 6.12 build. A failed discovery already
 * deletes its queues, so the one-queue fallback starts fresh. */
static int nvrm_find_vqs(struct nvrm_dev *dev)
{
	struct virtio_device *vdev = dev->vdev;
	struct virtqueue *vqs[2];
	int ret;

#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 11, 0)
	{
		vq_callback_t *cbs[2] = { nvrm_vq_cb, nvrm_evq_cb };
		const char *const names[2] = { "nvrm", "nvrm-events" };

		ret = virtio_find_vqs(vdev, 2, vqs, cbs, names, NULL);
	}
#else
	{
		struct virtqueue_info vqi[2] = {
			{ "nvrm", nvrm_vq_cb, false },
			{ "nvrm-events", nvrm_evq_cb, false },
		};

		ret = virtio_find_vqs(vdev, 2, vqs, vqi, NULL);
	}
#endif
	if (!ret) {
		dev->vq = vqs[0];
		dev->evq = vqs[1];
		return 0;
	}

	pr_warn("virtio_nvrm: two queues refused (%d) -- events disabled: device offers one queue\n",
		ret);
	dev->vq = virtio_find_single_vq(vdev, nvrm_vq_cb, "nvrm");
	if (IS_ERR(dev->vq)) {
		ret = PTR_ERR(dev->vq);
		dev->vq = NULL;
		return ret;
	}
	dev->evq = NULL;
	return 0;
}

/* Post NVRM_EVQ_BUFS request-sized inbufs after virtio_device_ready, then
 * kick once. Callbacks reuse these buffers until nvrm_evq_drain. */
static int nvrm_evq_fill(struct nvrm_dev *dev)
{
	unsigned int i;

	for (i = 0; i < NVRM_EVQ_BUFS; i++) {
		struct scatterlist sg;
		void *buf = kzalloc(sizeof(struct nvrm_req), GFP_KERNEL);
		int ret;

		if (!buf)
			return -ENOMEM;
		sg_init_one(&sg, buf, sizeof(struct nvrm_req));
		ret = virtqueue_add_inbuf(dev->evq, &sg, 1, buf, GFP_KERNEL);
		if (ret < 0) {
			kfree(buf);
			/* A queue shorter than NVRM_EVQ_BUFS is not an error:
			 * whatever fits is what the host may fill. */
			if (ret == -ENOSPC && i)
				return 0;
			return ret;
		}
	}
	return 0;
}

/* After virtio_reset_device: give back what the device still held. */
static void nvrm_evq_drain(struct nvrm_dev *dev)
{
	void *buf;

	if (!dev->evq)
		return;
	while ((buf = virtqueue_detach_unused_buf(dev->evq)) != NULL)
		kfree(buf);
}

static void nvrm_transport_stop(struct nvrm_dev *dev)
{
	struct nvrm_xfer *x;
	unsigned long flags;
	unsigned long index;
	struct nvrm_ctx *ctx;

	spin_lock_irqsave(&dev->vq_lock, flags);
	WRITE_ONCE(dev->stopping, true);
	spin_unlock_irqrestore(&dev->vq_lock, flags);
	wake_up_all(&dev->vq_space);
	virtio_reset_device(dev->vdev);
	virtio_synchronize_cbs(dev->vdev);
	cancel_work_sync(&dev->events_work);

	spin_lock_irqsave(&dev->vq_lock, flags);
	while ((x = virtqueue_detach_unused_buf(dev->vq)) != NULL) {
		x->transport_error = -ENODEV;
		WRITE_ONCE(x->done, true);
		if (x->abandoned)
			queue_work(dev->release_wq, &x->release_work);
		else
			wake_up_all(&x->wq);
		atomic_dec(&dev->inflight);
	}
	spin_unlock_irqrestore(&dev->vq_lock, flags);
	wake_up_all(&dev->drain);
	/* The XArray lock also excludes final file release. */
	xa_lock(&dev->ctx_xa);
	xa_for_each(&dev->ctx_xa, index, ctx) wake_up_all(&ctx->events_wq);
	xa_unlock(&dev->ctx_xa);
	nvrm_evq_drain(dev);
	dev->vdev->config->del_vqs(dev->vdev);
	dev->vq = NULL;
	dev->evq = NULL;
	destroy_workqueue(dev->release_wq);
	dev->release_wq = NULL;
}

static void balloon_start(void);

static int nvrm_probe(struct virtio_device *vdev)
{
	struct nvrm_dev *dev;
	struct virtio_shm_region shm;
	u64 token = 0;
	int ret;

	mutex_lock(&nvrm_device_lock);
	if (nvrm_bound) {
		mutex_unlock(&nvrm_device_lock);
		return -EBUSY;
	}
	nvrm_bound = true;
	mutex_unlock(&nvrm_device_lock);

	dev = kzalloc(sizeof(*dev), GFP_KERNEL);
	if (!dev) {
		ret = -ENOMEM;
		goto err_bound;
	}
	kref_init(&dev->ref);
	dev->vdev = vdev;
	get_device(&vdev->dev);
	/* Initialize the timer before any kernel RM call can arm it. */
	vblank_engine_init();
	spin_lock_init(&dev->vq_lock);
	spin_lock_init(&dev->evq_lock);
	xa_init(&dev->ctx_xa);
	INIT_WORK(&dev->events_work, nvrm_events_work);
	mutex_init(&dev->win_lock);
	mutex_init(&dev->proc_lock);
	mutex_init(&dev->quarantine_lock);
	INIT_LIST_HEAD(&dev->quarantined_pins);
	INIT_LIST_HEAD(&dev->quarantined_pools);
	INIT_LIST_HEAD(&dev->procs);
	bdf_init(dev);
	init_waitqueue_head(&dev->vq_space);
	init_waitqueue_head(&dev->drain);
	atomic_set(&dev->seq, 0);
	atomic_set(&dev->inflight, 0);
	vdev->priv = dev;
	dev->release_wq =
		alloc_workqueue("nvrm-release", WQ_UNBOUND | WQ_MEM_RECLAIM, 0);
	if (!dev->release_wq) {
		ret = -ENOMEM;
		goto err_free;
	}

	/* Try request and event queues. One-queue fallback supports older
	 * backends/VMM configurations without event delivery. */
	ret = nvrm_find_vqs(dev);
	if (ret)
		goto err_free;
	dev->vq_free = dev->vq->num_free;
	/* Post event inbufs after virtio_device_ready. Earlier posting made
	 * the VMM adopt the populated avail index as its starting point,
	 * skipping every initial buffer (measured 2026-08-15). */

	/* The host-visible window. A non-GPU device gets one for free: the
	 * generic vhost-user SHMEM patch for Cloud Hypervisor
	 * (patches/0001-generic-vhost-user-shmem.patch) is not tied to
	 * virtio-gpu. */
	if (virtio_get_shm_region(vdev, &shm, NVRM_SHM_ID_HOST_VISIBLE)) {
		dev->win_base = shm.addr;
		dev->win_len = shm.len;
		dev->win_bitmap =
			bitmap_zalloc(shm.len >> PAGE_SHIFT, GFP_KERNEL);
		if (!dev->win_bitmap) {
			ret = -ENOMEM;
			goto err_vq;
		}
		pr_info("virtio_nvrm: window %llu MiB @%#llx\n",
			dev->win_len >> 20, dev->win_base);
	} else {
		pr_warn("virtio_nvrm: no host-visible window -- mmap will fail\n");
	}

	/* From here on the device may be talked to. */
	virtio_device_ready(vdev);
	if (dev->evq) {
		ret = nvrm_evq_fill(dev);
		if (ret) {
			pr_warn("virtio_nvrm: event inbufs: %d -- events disabled\n",
				ret);
			/* Not fatal: the request path does not depend on it. */
		} else {
			virtqueue_kick(dev->evq);
		}
	}

	ret = nvrm_simple(dev, NVRM_KIND_HELLO, 0, NVRM_PROTO_VERSION, 0, 0, 0,
			  &token, false, 0);
	if (ret) {
		pr_err("virtio_nvrm: Hello rejected (%d) -- protocol v%u\n",
		       ret, NVRM_PROTO_VERSION);
		ret = -EPROTO;
		goto err_win;
	}
	pr_info("virtio_nvrm: Hello accepted, protocol v%u negotiated\n",
		NVRM_PROTO_VERSION);

	ret = nvrm_fetch_tables(dev);
	if (ret)
		goto err_win;

	mutex_lock(&nvrm_device_lock);
	nvrm = dev;
	mutex_unlock(&nvrm_device_lock);

	/* Externally visible registrations as the LAST step: any earlier and
	 * there would be a node that does not have its tables yet. */
	if (create_nodes) {
		ret = nvrm_nodes_setup();
		if (ret)
			goto err_tables;
	}

	pr_info("virtio_nvrm: ready (nodes %s, %u GPU%s, max_pin %u MiB)\n",
		create_nodes ? "on" : "off", gpu_count,
		gpu_count > 1 ? "s" : "", max_pin_mib);
	balloon_start();
	return 0;

err_tables:
	mutex_lock(&nvrm_device_lock);
	nvrm = NULL;
	mutex_unlock(&nvrm_device_lock);
err_win:
err_vq:
	nvrm_transport_stop(dev);
err_free:
	if (dev->release_wq)
		destroy_workqueue(dev->release_wq);
	vdev->priv = NULL;
	nvrm_dev_put(dev);
err_bound:
	mutex_lock(&nvrm_device_lock);
	nvrm_bound = false;
	mutex_unlock(&nvrm_device_lock);
	return ret;
}

/* Defined with the rest of the kernel RM API, below. */
static void kapi_session_close(void);
static void balloon_stop(void);

static void nvrm_remove(struct virtio_device *vdev)
{
	struct nvrm_dev *dev = vdev->priv;

	mutex_lock(&nvrm_device_lock);
	nvrm = NULL;
	mutex_unlock(&nvrm_device_lock);
	nvrm_nodes_teardown();
	/* Existing kernel sessions may send teardown requests until stop. */
	balloon_stop();
	kapi_session_close();
	nvrm_transport_stop(dev);
	vdev->priv = NULL;
	nvrm_dev_put(dev);
	mutex_lock(&nvrm_device_lock);
	nvrm_bound = false;
	mutex_unlock(&nvrm_device_lock);
	pr_info("virtio_nvrm: removed\n");
}

/* Kernel RM API for NVKMS: a function table serving the same NVOS requests
 * as the ioctl path, using kernel caller memory. */
/* nvidia_get_rm_ops is NVKMS's only imported NVIDIA symbol. Its op table
 * uses the existing interpreter, virtqueue and session machinery;
 * call_in/call_out handle kernel memory. */

/* Lazy NVKMS control-node session. nvidia_get_rm_ops may run before the
 * virtio device is ready. */
static struct nvrm_ctx *kapi_ctx;
static DEFINE_MUTEX(kapi_lock);
static const struct nvrm_modeset_callbacks *kapi_callbacks;
/* Private enumeration client, released by kapi_session_close. */
static u32 kapi_client;
/* Shared NVKMS identity for its control and GPU nodes. */
static struct nvrm_proc *kapi_proc;

/* open_gpu creates a GPU-node session. index follows GET_PROBED_IDS order.
 * The probe-order/node-index relationship is validated only for one GPU;
 * unknown gpuIds are rejected. */
struct kapi_gpu {
	u32 gpu_id;
	u32 index;
	struct nvrm_ctx *ctx;
	unsigned int refs;
};
static struct kapi_gpu kapi_gpus[NVRM_NV_MAX_GPUS];
static u32 kapi_gpu_count;

/* NVKMS has its own host session and handle namespace. pid stays NULL so
 * nvrm_proc_get cannot match this entry to a userspace process. */
static struct nvrm_proc *nvrm_proc_kernel(struct nvrm_dev *dev,
					  const char *comm)
{
	struct nvrm_proc *p;
	int ret;

	p = kzalloc(sizeof(*p), GFP_KERNEL);
	if (!p)
		return ERR_PTR(-ENOMEM);

	mutex_lock(&dev->proc_lock);
	ret = nvrm_proc_alloc_id(&p->id);
	if (ret) {
		mutex_unlock(&dev->proc_lock);
		kfree(p);
		return ERR_PTR(ret);
	}
	kref_get(&dev->ref);
	p->dev = dev;
	p->pid = NULL;
	p->vnr = 0;
	strscpy(p->comm, comm, sizeof(p->comm));
	refcount_set(&p->ref, 1);
	list_add(&p->node, &dev->procs);
	mutex_unlock(&dev->proc_lock);
	return p;
}

/* Open a kernel session on one node, as `proc`, or as NVKMS when `proc` is
 * NULL (the VRAM balloon is the one caller with an identity of its own).
 * Caller holds kapi_lock. */
static struct nvrm_ctx *kapi_ctx_open(struct nvrm_dev *dev, u32 dev_tag,
				      u32 index, struct nvrm_proc *proc)
{
	struct nvrm_proc_info info;
	struct nvrm_ctx *ctx;
	u64 token = 0;
	int ret;

	if (!dev || !dev->tbl.blob)
		return ERR_PTR(-ENODEV);

	ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);
	if (!ctx)
		return ERR_PTR(-ENOMEM);
	mutex_init(&ctx->lock);
	spin_lock_init(&ctx->pin_lock);
	INIT_LIST_HEAD(&ctx->pins);
	ctx->dev = dev;
	ctx->dev_tag = dev_tag;
	ctx->gpu_index = index;

	/* All NVKMS nodes share one guest_proc identity and RM handle
	 * namespace. */
	if (proc) {
		refcount_inc(&proc->ref);
		ctx->proc = proc;
	} else if (kapi_proc) {
		refcount_inc(&kapi_proc->ref);
		ctx->proc = kapi_proc;
	} else {
		ctx->proc = nvrm_proc_kernel(dev, "nvidia-modeset");
		if (IS_ERR(ctx->proc)) {
			ret = PTR_ERR(ctx->proc);
			ctx->proc = NULL;
			goto err;
		}
		kapi_proc = ctx->proc;
	}

	memset(&info, 0, sizeof(info));
	info.pid = ctx->proc->vnr;
	memcpy(info.comm, ctx->proc->comm,
	       min(sizeof(info.comm), sizeof(ctx->proc->comm)));
	info.comm[sizeof(info.comm) - 1] = '\0';

	ret = nvrm_simple_info(dev, NVRM_KIND_OPEN, ctx->dev_tag,
			       ctx->gpu_index, 0, 0, 0, &token, false,
			       ctx->proc->id, &info);
	if (ret < 0)
		goto err;
	/* Counted on the same books as the user nodes: NVKMS's sessions are
	 * mirrored by the host exactly like a user process's, and a counter
	 * that saw only one of the two doors would read LOWER than the truth
	 * and make the host look like it leaks. */
	stat_ctx_opened++;
	stat_ctx_open++;
	ctx->token = token;
	return ctx;

err:
	if (ctx->proc) {
		if (ctx->proc == kapi_proc &&
		    refcount_read(&kapi_proc->ref) == 1)
			kapi_proc = NULL;
		nvrm_proc_put(dev, ctx->proc);
	}
	mutex_destroy(&ctx->lock);
	kfree(ctx);
	return ERR_PTR(ret);
}

static void kapi_ctx_close(struct nvrm_ctx *ctx)
{
	int ret;

	if (!ctx)
		return;
	ret = nvrm_simple(ctx->dev, NVRM_KIND_CLOSE, ctx->dev_tag, 0,
			  ctx->token, 0, 0, NULL, false,
			  ctx->proc ? ctx->proc->id : 0);
	nvrm_ctx_release_pins(ctx, ret);
	stat_ctx_closed++;
	if (stat_ctx_open)
		stat_ctx_open--;
	if (ctx->proc && ctx->proc == kapi_proc &&
	    refcount_read(&kapi_proc->ref) == 1)
		kapi_proc = NULL;
	nvrm_proc_put(ctx->dev, ctx->proc);
	mutex_destroy(&ctx->lock);
	kfree(ctx);
}

static struct nvrm_ctx *kapi_ctx_open_current(u32 dev_tag, u32 index,
					      struct nvrm_proc *proc)
{
	struct nvrm_dev *dev = nvrm_dev_get();
	struct nvrm_ctx *ctx = kapi_ctx_open(dev, dev_tag, index, proc);

	nvrm_dev_put(dev);
	return ctx;
}

/* The control session. Caller holds kapi_lock. */
static struct nvrm_ctx *kapi_session(void)
{
	struct nvrm_ctx *ctx;

	if (kapi_ctx)
		return kapi_ctx;
	/* Kernel RM ops use the control node; the API is not bound to a GPU
	 * file. */
	ctx = kapi_ctx_open_current(NVRM_DEV_CTL, 0, NULL);
	if (IS_ERR(ctx))
		return ctx;
	kapi_ctx = ctx;
	pr_info("virtio_nvrm: NVKMS session open (guest_proc %u)\n",
		ctx->proc->id);
	return ctx;
}

/* Local NVA083_GRID_DISPLAYLESS controls. NVKMS derives the virtual
 * connector from these replies; no host display state is modified. */

/* One head. NVIDIA reports the same for the displayless path
 * (GRID_DISPLAYLESS_NUM_HEADS, objgriddisplayless.c:35) and three controls
 * have to agree about it, so it is named once. */
#define NVRM_VDISP_NUM_HEADS 1u

/* The EDID builder lives in its own translation unit so a userspace test
 * can run edid-decode over exactly these bytes. See nvrm_edid.c. */
#include "nvrm_edid.c"

/* NVKMS chooses the local displayless object's handle. Intercept every
 * later operation on it because host RM has no corresponding object. Only
 * one virtual display object is supported. */
static u32 vdisp_handle;
/* Qualify the local handle by hClient. NVKMS and nvidia-drm allocate
 * overlapping handle numbers in separate RM clients (unix_rm_handle.c:214).
 * Matching the handle alone swallowed another client's FREE and caused
 * duplicate-name failures (2026-08-17). */
static u32 vdisp_client;
/* NVKMS issues this display handle without allocating an object
 * (nvkms-evo.c:5223). Learn it from the missing-parent event and qualify it
 * by client. */
static u32 vdisp_phantom;
static u32 vdisp_phantom_client;
static DEFINE_MUTEX(vdisp_lock);

/* Answer the alloc of NVA083_GRID_DISPLAYLESS ourselves. Returns true when
 * this module has taken the call. */
static bool vdisp_alloc(u8 *params)
{
	u32 hclass = rd32(params, NVRM_NVOS64_HCLASS_OFF);
	u32 handle = rd32(params, NVRM_NVOS64_HOBJECTNEW_OFF);
	u32 client = rd32(params, NVRM_NVOS64_HROOT_OFF);

	if (!vdisplay || hclass != NVRM_CLASS_DISPLAYLESS)
		return false;

	mutex_lock(&vdisp_lock);
	vdisp_handle = handle;
	vdisp_client = client;
	mutex_unlock(&vdisp_lock);
	wr32(params, NVRM_NVOS64_STATUS_OFF, NVRM_NV_OK);
	pr_info("virtio_nvrm: virtual display: NVA083 object %#x of client %#x is ours (%ux%u, INVENTED)\n",
		handle, client, vdisplay_width, vdisplay_height);
	return true;
}

static bool vdisp_free(u8 *params)
{
	bool ours;
	/* NVOS00: hRoot@0, hObjectParent@4, hObjectOld@8, status@12. */
	u32 client = rd32(params, 0);
	u32 handle = rd32(params, 8);

	/* No `vdisplay` guard, for the reason vblank_free states: the recorded
	 * pair is the authority. It can only exist because vdisplay was on at
	 * alloc time, and a free that misses it because someone toggled the
	 * parameter would send RM a handle it has never heard of. */
	mutex_lock(&vdisp_lock);
	/* Match client and handle to avoid consuming another client's FREE. */
	ours = vdisp_handle && handle == vdisp_handle && client == vdisp_client;
	if (ours) {
		vdisp_handle = 0;
		vdisp_client = 0;
	}
	mutex_unlock(&vdisp_lock);
	if (!ours)
		return false;
	wr32(params, 12, NVRM_NV_OK);
	return true;
}

/* Implement all six NVA083 controls. Recorded NVKMS calls use
 * GET_NUM_HEADS, GET_MAX_RESOLUTION and GET_EDID. */
static bool vdisp_control(u8 *params)
{
	u32 client = rd32(params, NVRM_NVOS54_HCLIENT_OFF);
	u32 cmd = rd32(params, NVRM_NVOS54_CMD_OFF);
	u32 obj = rd32(params, NVRM_NVOS54_HOBJECT_OFF);
	u32 size = rd32(params, NVRM_NVOS54_PARAMSSIZE_OFF);
	void *p = (void *)(uintptr_t)rd64(params, NVRM_NVOS54_PARAMS_OFF);
	bool ours;
	/* Capture the expected pair under the same lock as the lookup for
	 * accurate diagnostics. */
	u32 have_handle, have_client;

	if (!vdisplay)
		return false;
	mutex_lock(&vdisp_lock);
	/* Match both client and handle; handles alone are not unique. */
	ours = vdisp_handle && obj == vdisp_handle && client == vdisp_client;
	have_handle = vdisp_handle;
	have_client = vdisp_client;
	mutex_unlock(&vdisp_lock);
	if (!ours) {
		/* Acknowledge controls on the unallocated displayHandle.
		 * Displayless NVKMS has no raster generator to notify. */
		mutex_lock(&vdisp_lock);
		if (vdisp_phantom && obj == vdisp_phantom &&
		    client == vdisp_phantom_client)
			ours = true;
		mutex_unlock(&vdisp_lock);
		if (ours) {
			wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_OK);
			if (display > 1)
				pr_info("virtio_nvrm: virtual display: %#x on the phantom display object answered OK\n",
					cmd);
			return true;
		}

		/* Displayless NVKMS requests GC6 blocking, a
		 * kernel-privileged control that CAP_SYS_ADMIN cannot grant
		 * (nvkms-evo.c:5199). Acknowledge locally: this guest
		 * programs no physical display, while the host owns
		 * display/power state. No host GC6 reference is acquired. */
		switch (cmd) {
		case NVRM_CTRL_GC6_BLOCKER:
		case NVRM_CTRL_VT_SWITCH:
		case NVRM_CTRL_VT_GET_FB_INFO:
			/* These controls require kernel privilege and
			 * concern host display/power state. Leave zeroed
			 * params to report no guest console. */
			wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_OK);
			if (display > 1)
				pr_info("virtio_nvrm: virtual display: %#x answered OK (nothing to do)\n",
					cmd);
			return true;
		default:
			/* A control outside the recorded (client, handle)
			 * pair reaches host RM and fails OBJECT_NOT_FOUND.
			 * This diagnostic checks the single-display-object
			 * assumption behind OPEN-QUESTIONS 16's
			 * disconnected-connector symptom; it has not fired
			 * in recorded runs. */
			if ((cmd & 0xffff0000u) == 0xa0830000u)
				pr_warn_ratelimited(
					"virtio_nvrm: virtual display: NVA083 control %#x on object %#x of client %#x, but our pair is %#x/%#x -- forwarding to a host that does not have this object (OPEN-QUESTIONS 16)\n",
					cmd, obj, client, have_client,
					have_handle);
			return false;
		}
	}

	/* Kernel params are directly addressable; reject a missing buffer. */
	if (!p || !size) {
		wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return true;
	}

	switch (cmd) {
	case NVRM_CTRL_VD_GET_NUM_HEADS:
		/* NVKMS reads maxNumHeads (nvkms-rm.c:946). Report one
		 * head, matching GRID_DISPLAYLESS_NUM_HEADS. */
		if (size < 8)
			goto too_small;
		wr32(p, 0, NVRM_VDISP_NUM_HEADS);
		wr32(p, 4, NVRM_VDISP_NUM_HEADS);
		break;
	case NVRM_CTRL_VD_GET_MAX_RES:
		/* GET_MAX_RESOLUTION returns the surface ceiling, not the
		 * EDID mode. Validate headIndex as NVIDIA does
		 * (griddisplaylessctrl.c); NVKMS uses head 0. */
		if (size < 12)
			goto too_small;
		if (rd32(p, 0) >= NVRM_VDISP_NUM_HEADS) {
			pr_warn("virtio_nvrm: virtual display: GET_MAX_RESOLUTION for head %u, there is %u\n",
				rd32(p, 0), (u32)NVRM_VDISP_NUM_HEADS);
			wr32(params, NVRM_NVOS54_STATUS_OFF,
			     NVRM_NV_ERR_INVALID_ARGUMENT);
			return true;
		}
		wr32(p, 4, vdisplay_max_width);
		wr32(p, 8, vdisplay_max_height);
		break;
	case NVRM_CTRL_VD_IS_ACTIVE:
		/* Report the virtual display active. Native displayActive
		 * is set by a host console connection; this backend has no
		 * such channel. No current NVKMS/nvidia-drm caller reads
		 * this control. */
		if (size < 1)
			goto too_small;
		((u8 *)p)[0] = 1;
		break;
	case NVRM_CTRL_VD_IS_CONNECTED:
		/* Report connected when numHeads > 0, as
		 * griddisplaylessctrl.c does. */
		if (size < 4)
			goto too_small;
		wr32(p, 0, NVRM_VDISP_NUM_HEADS > 0);
		break;
	case NVRM_CTRL_VD_GET_MAX_PIXELS:
		/* maxPixels is an independent surface limit. */
		if (size < 8)
			goto too_small;
		wr64(p, 0, vdisplay_max_pixels);
		break;
	case NVRM_CTRL_VD_GET_EDID: {
		/* Match griddisplaylessGetDefaultEDID_IMPL
		 * (objgriddisplayless.c:296-334): size 0 queries length;
		 * short buffers fail; a NULL buffer with nonzero sufficient
		 * size fails; otherwise copy. Report the required size on
		 * every path.
		 * One EDID serves both connector types, matching NVIDIA's
		 * digital/analog mode sets. */
		u64 buf;
		u32 want;

		if (size < 16)
			goto too_small;
		buf = rd64(p, 0);
		want = rd32(p, 8);
		wr32(p, 8, NVRM_EDID_LEN);
		if (want == 0)
			break; /* size query, nothing to write */
		if (want < NVRM_EDID_LEN) {
			pr_warn("virtio_nvrm: virtual display: EDID buffer of %u bytes, need %u\n",
				want, (u32)NVRM_EDID_LEN);
			wr32(params, NVRM_NVOS54_STATUS_OFF,
			     NVRM_NV_ERR_BUFFER_TOO_SMALL);
			return true;
		}
		if (!buf) {
			wr32(params, NVRM_NVOS54_STATUS_OFF,
			     NVRM_NV_ERR_INVALID_ARGUMENT);
			return true;
		}
		{
			u8 edid[NVRM_EDID_LEN];

			nvrm_build_edid(edid, vdisplay_width, vdisplay_height,
					vdisplay_vblank_hz);
			memcpy((void *)(uintptr_t)buf, edid, NVRM_EDID_LEN);
		}
		break;
	}
	default:
		/* A control on our object that we do not answer is a finding,
		 * not a nuisance: it means NVKMS wants something this display
		 * cannot claim to have. */
		pr_warn("virtio_nvrm: virtual display: unanswered control %#x -- refused\n",
			cmd);
		wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return true;
	}

	if (display > 1)
		pr_info("virtio_nvrm: virtual display: control %#x answered\n",
			cmd);
	wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_OK);
	return true;

too_small:
	pr_warn("virtio_nvrm: virtual display: control %#x with %u bytes of params\n",
		cmd, size);
	wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
	return true;
}

/* Replace NV04_DISPLAY_COMMON with the displayless class so
 * nvRmAllocDisplays selects it. Preserve numClasses; NULL-buffer count
 * queries need no rewrite. */
static void vdisp_rewrite_classlist(u8 *params)
{
	static const u32 cores[] = NVRM_DISP_CLASSES;
	u32 size = rd32(params, NVRM_NVOS54_PARAMSSIZE_OFF);
	void *p = (void *)(uintptr_t)rd64(params, NVRM_NVOS54_PARAMS_OFF);
	u32 n, i, j, out = 0, dropped = 0;
	bool have_displayless = false;
	u32 *list;
	u64 buf;

	if (!vdisplay || !p || size < 16)
		return;
	if (rd32(params, NVRM_NVOS54_STATUS_OFF) != NVRM_NV_OK)
		return;

	/* GET_CLASSLIST carries numClasses and an 8-aligned pointer. The
	 * initial NULL-buffer count query needs no rewrite. */
	n = rd32(p, 0);
	buf = rd64(p, 8);
	if (!buf || !n)
		return;

	list = (u32 *)(uintptr_t)buf;
	for (i = 0; i < n; i++)
		if (list[i] == NVRM_CLASS_DISPLAYLESS)
			have_displayless = true;
	if (have_displayless)
		return; /* already done, or a card that has it */

	/* Remove classes NVKMS prefers over displayless; otherwise it
	 * selects a physical-display HAL with displaylessHw set. */
	for (i = 0; i < n; i++) {
		bool drop = (list[i] == NVRM_CLASS_DISPLAY_COMMON);

		for (j = 0; !drop && j < ARRAY_SIZE(cores); j++)
			drop = (list[i] == cores[j]);
		if (drop) {
			dropped++;
			continue;
		}
		list[out++] = list[i];
	}
	if (!dropped) {
		pr_warn_ratelimited(
			"virtio_nvrm: virtual display: nothing to drop from the class list -- left alone\n");
		return;
	}
	list[out++] = NVRM_CLASS_DISPLAYLESS;
	wr32(p, 0, out);
	pr_info("virtio_nvrm: virtual display: class list %u -> %u (dropped %u display classes, added %#x)\n",
		n, out, dropped, NVRM_CLASS_DISPLAYLESS);
}

/* Displayless NVKMS registers an RG callback under an unallocated
 * displayHandle (nvkms-rm.c:5338, nvkms-evo.c:5223). Accept only the
 * substituted OS-event allocation that fails OBJECT_NOT_FOUND, with
 * vdisplay enabled.
 * No raster-generator callback is delivered. DisplaylessFlipWorker polls at
 * 100 us while flips remain queued (nvkms-displayless.c:304). It still
 * requires the kernel CPU mapping provided by kapi_map_memory for its
 * semaphore surface. */
static void vdisp_event_on_missing_parent(u8 *params)
{
	if (!vdisplay)
		return;
	if (rd32(params, NVRM_NVOS64_HCLASS_OFF) != NVRM_CLASS_EVENT_OS_EVENT)
		return;
	if (rd32(params, NVRM_NVOS64_STATUS_OFF) !=
	    NVRM_NV_ERR_OBJECT_NOT_FOUND)
		return;

	wr32(params, NVRM_NVOS64_STATUS_OFF, NVRM_NV_OK);
	mutex_lock(&vdisp_lock);
	vdisp_phantom = rd32(params, NVRM_NVOS64_HOBJECTPARENT_OFF);
	vdisp_phantom_client = rd32(params, NVRM_NVOS64_HROOT_OFF);
	mutex_unlock(&vdisp_lock);
	pr_info("virtio_nvrm: virtual display: event parent %#x of client %#x does not exist -- answered OK, and that pair is now known\n",
		rd32(params, NVRM_NVOS64_HOBJECTPARENT_OFF),
		rd32(params, NVRM_NVOS64_HROOT_OFF));
}

/* Serve NV9010 vblank callbacks only for kernel callers. pProc is a guest
 * kernel function pointer and must never be forwarded or accepted from
 * userspace; the ioctl table rejects this class.
 * The hrtimer invokes enabled callbacks at the effective EDID refresh rate,
 * matching RM's interrupt-context contract (_vblankCallback,
 * vblank_callback.c:36).
 * Invoke under vblank_lock so FREE fences every callback before NVKMS
 * releases its arguments. Callees must not sleep or reenter this module;
 * the checked NVKMS callback takes only nvkms_timers.lock. */

/* One head (NVRM_VDISP_NUM_HEADS); the slots are for CLIENTS of that head:
 * NVKMS registers one RG callback per head, vblank-sem-control and the
 * headsurface can add their own. Eight is headroom, not a measurement. */
#define NVRM_VBLANK_SLOTS 8u
struct vblank_slot {
	u32 handle; /* 0 = free */
	u32 client; /* hRoot, for the log */
	u64 proc; /* OSVBLANKCALLBACKPROC in guest kernel text */
	u64 parm1;
	u64 parm2;
	bool enabled; /* bIsVblankNotifyEnable, NV_TRUE at construct */
};
static struct vblank_slot vblank_slots[NVRM_VBLANK_SLOTS];
/* Invoke under vblank_lock so free/drop_all fence callbacks. Callees must
 * not sleep or reenter kapi; NVKMS queues work from its handler. */
static DEFINE_SPINLOCK(vblank_lock);
static struct hrtimer vblank_timer;
static bool vblank_armed; /* under vblank_lock */
/* Serialize timer arm/disarm, including device removal outside NVKMS's
 * lock. Never acquire this mutex from the hrtimer callback. */
static DEFINE_MUTEX(vblank_engine_lock);

/* Use the effective EDID rate, not the requested rate. Its 16-bit 10 kHz
 * pixel clock limits 4K120 to 75 Hz, 1080p240 to 226 Hz and 1440p240 to 167
 * Hz. nvrm_edid_effective applies the shared clamp, including the RB2
 * fallback. */
static u32 vblank_hz(void)
{
	u32 w, h, hz, hb;

	nvrm_edid_effective(READ_ONCE(vdisplay_width),
			    READ_ONCE(vdisplay_height),
			    READ_ONCE(vdisplay_vblank_hz), &w, &h, &hz, &hb);
	return hz;
}

static ktime_t vblank_period(void)
{
	return ktime_set(0, NSEC_PER_SEC / vblank_hz());
}

static enum hrtimer_restart vblank_tick(struct hrtimer *t)
{
	unsigned long flags;
	u32 i;

	spin_lock_irqsave(&vblank_lock, flags);
	if (!vblank_armed) {
		spin_unlock_irqrestore(&vblank_lock, flags);
		return HRTIMER_NORESTART;
	}
	for (i = 0; i < NVRM_VBLANK_SLOTS; i++) {
		struct vblank_slot *s = &vblank_slots[i];

		if (s->handle && s->enabled) {
			/* ABI-identical to OSVBLANKCALLBACKPROC (NvP64 is a
			 * u64). A kCFI kernel would trap on the type hash;
			 * this guest's kernel does not carry kCFI. */
			((void (*)(u64, u64))(uintptr_t)s->proc)(s->parm1,
								 s->parm2);
			stat_vblank_fired++;
		}
	}
	spin_unlock_irqrestore(&vblank_lock, flags);

	hrtimer_forward_now(t, vblank_period());
	return HRTIMER_RESTART;
}

/* Once, from nvrm_probe, before the device can carry a kapi call. A lazy
 * init from the alloc path was a data race on the ready flag. */
static void vblank_engine_init(void)
{
	hrtimer_init(&vblank_timer, CLOCK_MONOTONIC, HRTIMER_MODE_REL);
	vblank_timer.function = vblank_tick;
}

/* Arm when a slot is filled, disarm when the last empties. Process context
 * only. hrtimer_start on an already-queued timer requeues it, so calling
 * this for every transition is idempotent rather than clever. */
static void vblank_engine_update(void)
{
	unsigned long flags;
	bool want = false;
	u32 i;

	mutex_lock(&vblank_engine_lock);
	spin_lock_irqsave(&vblank_lock, flags);
	for (i = 0; i < NVRM_VBLANK_SLOTS; i++)
		if (vblank_slots[i].handle)
			want = true;
	vblank_armed = want;
	spin_unlock_irqrestore(&vblank_lock, flags);

	if (want)
		hrtimer_start(&vblank_timer, vblank_period(), HRTIMER_MODE_REL);
	else
		hrtimer_cancel(&vblank_timer);
	mutex_unlock(&vblank_engine_lock);
}

/* Answer the alloc of NV9010_VBLANK_CALLBACK ourselves. Returns true when
 * this module has taken the call. Kernel callers only: kapi_op is the one
 * door this is wired into. */
static bool vblank_alloc(u8 *params)
{
	u32 hclass = rd32(params, NVRM_NVOS64_HCLASS_OFF);
	u32 handle = rd32(params, NVRM_NVOS64_HOBJECTNEW_OFF);
	u32 client = rd32(params, NVRM_NVOS64_HROOT_OFF);
	const u8 *ap;
	u64 proc, parm1, parm2;
	u32 head;
	unsigned long flags;
	u32 i;

	if (!vdisplay || hclass != NVRM_CLASS_VBLANK_CALLBACK)
		return false;

	/* Kernel allocation params use the cl9010 layout: pProc@0,
	 * LogicalHead@8, pParm1@16, pParm2@24. */
	ap = (const u8 *)(uintptr_t)rd64(params, NVRM_NVOS64_PALLOCPARMS_OFF);
	if (!ap) {
		wr32(params, NVRM_NVOS64_STATUS_OFF,
		     NVRM_NV_ERR_INVALID_ARGUMENT);
		return true;
	}
	proc = rd64(ap, 0);
	head = rd32(ap, 8);
	parm1 = rd64(ap, 16);
	parm2 = rd64(ap, 24);

	if (!proc || head >= NVRM_VDISP_NUM_HEADS) {
		wr32(params, NVRM_NVOS64_STATUS_OFF,
		     NVRM_NV_ERR_INVALID_ARGUMENT);
		return true;
	}

	spin_lock_irqsave(&vblank_lock, flags);
	for (i = 0; i < NVRM_VBLANK_SLOTS; i++)
		if (!vblank_slots[i].handle)
			break;
	if (i == NVRM_VBLANK_SLOTS) {
		spin_unlock_irqrestore(&vblank_lock, flags);
		pr_warn("virtio_nvrm: vblank: all %u slots taken -- refused\n",
			NVRM_VBLANK_SLOTS);
		wr32(params, NVRM_NVOS64_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return true;
	}
	vblank_slots[i] = (struct vblank_slot){
		.handle = handle,
		.client = client,
		.proc = proc,
		.parm1 = parm1,
		.parm2 = parm2,
		/* NV_TRUE at construct, exactly like vblcbConstruct_IMPL. */
		.enabled = true,
	};
	spin_unlock_irqrestore(&vblank_lock, flags);
	vblank_engine_update();

	wr32(params, NVRM_NVOS64_STATUS_OFF, NVRM_NV_OK);
	/* Log the effective EDID rate used by the timer, which may differ
	 * from the requested rate. */
	pr_info("virtio_nvrm: vblank: callback %#x (client %#x, head %u) is ours -- serviced at %u Hz (INVENTED)\n",
		handle, client, head, vblank_hz());
	return true;
}

static bool vblank_free(u8 *params)
{
	/* NVOS00: hRoot@0, hObjectParent@4, hObjectOld@8, status@12. */
	u32 client = rd32(params, 0);
	u32 handle = rd32(params, 8);
	unsigned long flags;
	bool ours = false;
	u32 i;

	/* No `vdisplay` guard here or in the control below: the slot table is
	 * the authority. A slot can only exist because vdisplay was on at
	 * alloc time, and a FREE that misses its slot because someone toggled
	 * the parameter would leave a firing pProc behind a freed object. */
	if (!handle)
		return false;
	spin_lock_irqsave(&vblank_lock, flags);
	for (i = 0; i < NVRM_VBLANK_SLOTS; i++) {
		/* Match client and handle: NVKMS core and nvidia-drm
		 * allocate overlapping handle numbers in separate clients. */
		if (vblank_slots[i].handle == handle &&
		    vblank_slots[i].client == client) {
			vblank_slots[i] = (struct vblank_slot){ 0 };
			ours = true;
			break;
		}
	}
	spin_unlock_irqrestore(&vblank_lock, flags);
	if (!ours)
		return false;
	vblank_engine_update();
	wr32(params, 12, NVRM_NV_OK);
	return true;
}

static bool vblank_control(u8 *params)
{
	u32 client = rd32(params, NVRM_NVOS54_HCLIENT_OFF);
	u32 cmd = rd32(params, NVRM_NVOS54_CMD_OFF);
	u32 obj = rd32(params, NVRM_NVOS54_HOBJECT_OFF);
	u32 size = rd32(params, NVRM_NVOS54_PARAMSSIZE_OFF);
	const u8 *p =
		(const u8 *)(uintptr_t)rd64(params, NVRM_NVOS54_PARAMS_OFF);
	unsigned long flags;
	bool ours = false;
	u32 i;

	spin_lock_irqsave(&vblank_lock, flags);
	/* Match both client and handle. */
	for (i = 0; i < NVRM_VBLANK_SLOTS; i++)
		if (vblank_slots[i].handle && vblank_slots[i].handle == obj &&
		    vblank_slots[i].client == client)
			break;
	if (i < NVRM_VBLANK_SLOTS)
		ours = true;
	if (ours && cmd == NVRM_CTRL_SET_VBLANK_NOTIFY && p && size >= 1)
		/* Read the kernel caller's bSetVBlankNotifyEnable
		 * (ctrl9010.h). */
		vblank_slots[i].enabled = *p != 0;
	spin_unlock_irqrestore(&vblank_lock, flags);

	if (!ours)
		return false;
	if (cmd == NVRM_CTRL_SET_VBLANK_NOTIFY && p && size >= 1) {
		wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_OK);
	} else {
		/* A call RM would dispatch to the 9010 object and this module
		 * does not know. Forwarding is worse: the host has never
		 * heard of the handle. */
		pr_warn("virtio_nvrm: vblank: control %#x on %#x is not implemented -- refused\n",
			cmd, obj);
		wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
	}
	return true;
}

/* The NVKMS session is going away, and pProc points into nvidia-modeset.ko.
 * After this returns, no callback is in flight and none will fire. */
static void vblank_drop_all(void)
{
	unsigned long flags;
	u32 i;

	spin_lock_irqsave(&vblank_lock, flags);
	for (i = 0; i < NVRM_VBLANK_SLOTS; i++)
		vblank_slots[i] = (struct vblank_slot){ 0 };
	vblank_armed = false;
	spin_unlock_irqrestore(&vblank_lock, flags);
	/* Unconditional: the timer exists since probe. */
	mutex_lock(&vblank_engine_lock);
	hrtimer_cancel(&vblank_timer);
	mutex_unlock(&vblank_engine_lock);
}

/* Queue-1 KIND_EVENT_FIRED delivery. The host substitutes OS events for
 * guest kernel callbacks and returns the original class and identity (wire
 * layout: nvrm-wire).
 * 0x79 wakes the owning file; 0x7e invokes its registered guest callback.
 * FREE fences calls through event_cb_lock. Class 0x78 is dropped: its
 * native callBackToMiniport requires host nv_state and has no guest
 * equivalent. */

/* Name event fields by their role; KIND_EVENT_FIRED reuses the Req layout. */
#define ev_class(r) ((r)->ioctl_nr)
#define ev_hclient(r) ((r)->fd_field_off)
#define ev_hevent(r) ((r)->embedded_ptr_off)
#define ev_data(r) ((r)->inline_len)
#define ev_status(r) ((r)->aux_len)
#define ev_notify(r) ((r)->nested_count)
#define ev_kc(r) ((r)->addr)

/* Measured GNOME uses 26 callback registrations; reserve 64 slots.
 * Exhaustion leaves the host allocation live without a guest callback, and
 * is logged. */
#define NVRM_EVENT_CB_SLOTS 64u
struct event_cb_slot {
	u32 client; /* NVOS64.hRoot */
	u32 handle; /* NVOS64.hObjectNew; 0 = free */
	u32 cls; /* what the guest asked for (0x7e) */
	u32 notify_index; /* NV0005.notifyIndex, unstripped */
	u32 proc; /* the NVKMS session's process id */
	u64 kc; /* NVOS10_EVENT_KERNEL_CALLBACK_EX*, guest kernel VA */
};
static struct event_cb_slot event_cb_slots[NVRM_EVENT_CB_SLOTS];
/* Invoke callbacks under event_cb_lock; FREE/drop_all fence all calls.
 * Acquire with irqsave from workqueue/kapi context, never the virtqueue
 * callback.
 * Callbacks must neither sleep nor synchronously reenter kapi, which takes
 * this lock. Checked NVKMS/nvidia-drm handlers only queue work or take
 * their own spinlocks. */
static DEFINE_SPINLOCK(event_cb_lock);

/* Capture kernel NV0005 params before submission. The host rewrites hClass
 * and data; keep those reply changes for vdisp_event_on_missing_parent.
 * Read offsets from nvrm_wire.h (cl0005.h:40-46). */
struct event_cb_pending {
	bool armed;
	u32 client;
	u32 notify_index;
	u64 kc;
};

static void event_cb_before_alloc(const u8 *params,
				  struct event_cb_pending *pend)
{
	const u8 *ap;

	pend->armed = false;
	if (rd32(params, NVRM_NVOS64_HCLASS_OFF) !=
	    NVRM_CLASS_EVENT_KERNEL_CALLBACK_EX)
		return;
	ap = (const u8 *)(uintptr_t)rd64(params, NVRM_NVOS64_PALLOCPARMS_OFF);
	if (!ap)
		return;
	pend->client = rd32(params, NVRM_NVOS64_HROOT_OFF);
	pend->notify_index = rd32(ap, NVRM_NV0005_NOTIFYINDEX_OFF);
	pend->kc = rd64(ap, NVRM_NV0005_DATA_OFF);
	pend->armed = pend->kc != 0;
}

/* After successful kernel ALLOC, register by (client, handle). Handles can
 * overlap across NVKMS/nvidia-drm clients. */
static void event_cb_after_alloc(const u8 *params,
				 const struct event_cb_pending *pend, long ret)
{
	u32 handle = rd32(params, NVRM_NVOS64_HOBJECTNEW_OFF);
	unsigned long flags;
	u32 proc;
	u32 i;

	if (!pend->armed)
		return;
	if (ret || rd32(params, NVRM_NVOS64_STATUS_OFF) != NVRM_NV_OK ||
	    !handle)
		return;
	/* kapi_lock is held by kapi_forward's caller chain (kapi_op runs
	 * under nvidia-modeset's own serialisation, kapi_forward_on takes
	 * kapi_lock); kapi_proc is stable for the read. */
	proc = kapi_proc ? kapi_proc->id : 0;

	spin_lock_irqsave(&event_cb_lock, flags);
	for (i = 0; i < NVRM_EVENT_CB_SLOTS; i++)
		if (!event_cb_slots[i].handle)
			break;
	if (i == NVRM_EVENT_CB_SLOTS) {
		spin_unlock_irqrestore(&event_cb_lock, flags);
		pr_warn("virtio_nvrm: events: all %u callback slots taken -- event %#x/%#x is registered on the host but will never fire here (stat_events_dropped will count it)\n",
			NVRM_EVENT_CB_SLOTS, pend->client, handle);
		return;
	}
	event_cb_slots[i] = (struct event_cb_slot){
		.client = pend->client,
		.handle = handle,
		.cls = NVRM_CLASS_EVENT_KERNEL_CALLBACK_EX,
		.notify_index = pend->notify_index,
		.proc = proc,
		.kc = pend->kc,
	};
	stat_events_registered++;
	spin_unlock_irqrestore(&event_cb_lock, flags);
	if (display > 1)
		pr_info("virtio_nvrm: events: kernel callback %#x/%#x idx %#x kc %#llx -> slot %u\n",
			pend->client, handle, pend->notify_index,
			(unsigned long long)pend->kc, i);
}

/* Fence callbacks before forwarding FREE: NVKMS releases their blocks as
 * soon as it returns. Freeing a client removes all its slots. The host must
 * still receive the FREE. */
static void event_cb_free(const u8 *params)
{
	u32 client = rd32(params, 0);
	u32 handle = rd32(params, 8);
	unsigned long flags;
	u32 i;

	if (!handle)
		return;
	spin_lock_irqsave(&event_cb_lock, flags);
	for (i = 0; i < NVRM_EVENT_CB_SLOTS; i++) {
		struct event_cb_slot *sl = &event_cb_slots[i];

		if (!sl->handle || sl->client != client)
			continue;
		if (sl->handle == handle || handle == client) {
			*sl = (struct event_cb_slot){ 0 };
			if (stat_events_registered)
				stat_events_registered--;
		}
	}
	spin_unlock_irqrestore(&event_cb_lock, flags);
}

/* The NVKMS session is going away; every kc points into nvidia-modeset.ko. */
static void event_cb_drop_all(void)
{
	unsigned long flags;
	u32 i;

	spin_lock_irqsave(&event_cb_lock, flags);
	for (i = 0; i < NVRM_EVENT_CB_SLOTS; i++)
		event_cb_slots[i] = (struct event_cb_slot){ 0 };
	stat_events_registered = 0;
	spin_unlock_irqrestore(&event_cb_lock, flags);
}

/* Semaphore-surface waiters carry a guest NVOS10 callback pointer. The host
 * substitutes an OS event and returns KIND_EVENT_FIRED with hEvent=0 and
 * the original pointer in addr.
 * Register the (client, proc, kc) slot before submitting: queue-1 firing
 * may overtake the reply. Remove it on failed registration. Invocations are
 * one-shot. Native RM's user-handle interpretation cannot use a guest
 * pointer (os.c:1741-1767). */
#define NVRM_SEMSURF_SLOTS 64u
struct semsurf_slot {
	bool used;
	u32 client; /* NVOS54.hClient of the registration */
	u32 proc; /* the NVKMS session's process id */
	u64 kc; /* NVOS10_EVENT_KERNEL_CALLBACK_EX*, guest kernel VA */
	/* Cache func/arg at arm time; NVKMS may free the callback block
	 * before a delayed firing arrives. This avoids rereading freed
	 * memory, but arg itself can still be stale without teardown
	 * acknowledgement. The limiter remains opt-in because it widens
	 * this race (OPEN-QUESTIONS 38). */
	u64 func;
	u64 arg;
};
static struct semsurf_slot semsurf_slots[NVRM_SEMSURF_SLOTS];

/* Under event_cb_lock. */
static void semsurf_slot_del(u32 client, u64 kc)
{
	u32 i;

	for (i = 0; i < NVRM_SEMSURF_SLOTS; i++) {
		struct semsurf_slot *sl = &semsurf_slots[i];

		if (sl->used && sl->client == client && sl->kc == kc) {
			sl->used = false;
			if (stat_semsurf_waiters)
				stat_semsurf_waiters--;
			return;
		}
	}
}

struct semsurf_pending {
	bool armed; /* REGISTER_WAITER, slot pre-filled */
	bool unreg; /* UNREGISTER_WAITER */
	u32 client;
	u64 kc;
};

/* Kernel CONTROL before submission; both NVOS54 and its params pointer
 * refer to guest kernel memory. */
static void semsurf_before_control(const u8 *params,
				   struct semsurf_pending *pend)
{
	u32 cmd = rd32(params, NVRM_NVOS54_CMD_OFF);
	unsigned long flags;
	const u8 *pp;
	u64 kc;
	u32 i;

	pend->armed = false;
	pend->unreg = false;
	if (cmd != NVRM_CTRL_SEMSURF_REG_WAITER &&
	    cmd != NVRM_CTRL_SEMSURF_UNREG_WAITER)
		return;
	pp = (const u8 *)(uintptr_t)rd64(params, NVRM_NVOS54_PARAMS_OFF);
	if (!pp)
		return;
	kc = rd64(pp, cmd == NVRM_CTRL_SEMSURF_REG_WAITER ?
			      NVRM_SEMSURF_REG_HANDLE_OFF :
			      NVRM_SEMSURF_UNREG_HANDLE_OFF);
	/* Zero requests no notification; 32-bit values are native OS-event
	 * IDs. Only kernel pointers need translation. */
	if (kc <= 0xffffffffull)
		return;
	pend->client = rd32(params, NVRM_NVOS54_HCLIENT_OFF);
	pend->kc = kc;

	if (cmd == NVRM_CTRL_SEMSURF_UNREG_WAITER) {
		pend->unreg = true;
		return;
	}

	spin_lock_irqsave(&event_cb_lock, flags);
	for (i = 0; i < NVRM_SEMSURF_SLOTS; i++)
		if (!semsurf_slots[i].used)
			break;
	if (i == NVRM_SEMSURF_SLOTS) {
		spin_unlock_irqrestore(&event_cb_lock, flags);
		pr_warn("virtio_nvrm: events: all %u semsurf waiter slots taken -- waiter kc %#llx will fire on the host and be dropped here\n",
			NVRM_SEMSURF_SLOTS, (unsigned long long)kc);
		return;
	}
	semsurf_slots[i] = (struct semsurf_slot){
		.used = true,
		.client = pend->client,
		.proc = kapi_proc ? kapi_proc->id : 0,
		.kc = kc,
		/* While the block is provably alive: the caller is executing
		 * the registration control that names it. */
		.func = rd64((const u8 *)(uintptr_t)kc,
			     NVRM_NVOS10_CB_EX_FUNC_OFF),
		.arg = rd64((const u8 *)(uintptr_t)kc,
			    NVRM_NVOS10_CB_EX_ARG_OFF),
	};
	stat_semsurf_waiters++;
	pend->armed = true;
	spin_unlock_irqrestore(&event_cb_lock, flags);
	if (display > 1)
		pr_info("virtio_nvrm: events: semsurf waiter client %#x kc %#llx -> slot %u\n",
			pend->client, (unsigned long long)kc, i);
}

/* Kernel CONTROL after its reply. */
static void semsurf_after_control(const u8 *params,
				  const struct semsurf_pending *pend, long ret)
{
	unsigned long flags;
	bool ok;

	if (!pend->armed && !pend->unreg)
		return;
	ok = !ret && rd32(params, NVRM_NVOS54_STATUS_OFF) == NVRM_NV_OK;

	spin_lock_irqsave(&event_cb_lock, flags);
	if (pend->armed && !ok) {
		/* Keep slots only for NV_OK. ALREADY_SIGNALLED means the
		 * semaphore was reached without registering a callback
		 * (ctrl00da.h:189-196). */
		semsurf_slot_del(pend->client, pend->kc);
	} else if (pend->unreg) {
		/* Remove the slot on every cancellation result: NVKMS frees
		 * its callback block immediately
		 * (nvkms-kapi-sync.c:497-501), even if a firing is still in
		 * transit. Keeping it risks use-after-free (OPEN-QUESTIONS
		 * 38).
		 * A dropped late firing delays the fence; nvidia-drm's
		 * timeout path checks the live semaphore and completes it
		 * if reached (nvidia-drm-fence.c:820-827). */
		if (!ok)
			stat_semsurf_late_unreg++;
		semsurf_slot_del(pend->client, pend->kc);
	}
	spin_unlock_irqrestore(&event_cb_lock, flags);
}

/* One semsurf firing: hEvent = 0 is the marker (no event object exists for
 * a waiter). Process context (work item), like event_fire_callback. */
static void semsurf_fire(const struct nvrm_req *r)
{
	unsigned long flags;
	u64 func, arg;
	u32 i;

	spin_lock_irqsave(&event_cb_lock, flags);
	for (i = 0; i < NVRM_SEMSURF_SLOTS; i++) {
		struct semsurf_slot *sl = &semsurf_slots[i];

		if (!sl->used || sl->kc != ev_kc(r) ||
		    sl->client != ev_hclient(r) || sl->proc != r->guest_proc)
			continue;
		/* Remove the one-shot slot before calling:
		 * SemaphoreSurfaceKapiCallback frees its block
		 * (nvkms-kapi-sync.c:374-380). Invoke under event_cb_lock;
		 * callees may only queue work or take their own spinlocks. */
		sl->used = false;
		if (stat_semsurf_waiters)
			stat_semsurf_waiters--;
		/* Use the cached callback, never reread *kc. */
		func = sl->func;
		arg = sl->arg;
		if (func)
			((void (*)(void *, void *, u32, u32, u32))(
				uintptr_t)func)((void *)(uintptr_t)arg, NULL, 0,
						ev_data(r), ev_status(r));
		stat_semsurf_fired++;
		stat_events_delivered++;
		spin_unlock_irqrestore(&event_cb_lock, flags);
		return;
	}
	spin_unlock_irqrestore(&event_cb_lock, flags);
	stat_events_dropped++;
	stat_events_drop_noslot++;
	pr_warn_ratelimited(
		"virtio_nvrm: events: semsurf waiter kc %#llx (client %#x, proc %u) fired, no slot -- dropped\n",
		(unsigned long long)ev_kc(r), ev_hclient(r), r->guest_proc);
}

/* The NVKMS session is going away, semsurf half. */
static void semsurf_drop_all(void)
{
	unsigned long flags;
	u32 i;

	spin_lock_irqsave(&event_cb_lock, flags);
	for (i = 0; i < NVRM_SEMSURF_SLOTS; i++)
		semsurf_slots[i] = (struct semsurf_slot){ 0 };
	stat_semsurf_waiters = 0;
	spin_unlock_irqrestore(&event_cb_lock, flags);
}

/* One firing of a kernel-callback event. Process context (work item). */
static void event_fire_callback(const struct nvrm_req *r)
{
	unsigned long flags;
	u32 i;

	if (ev_class(r) != NVRM_CLASS_EVENT_KERNEL_CALLBACK_EX) {
		/* 0x78, or a class the host invented later: see the header. */
		stat_events_dropped++;
		stat_events_drop_class++;
		pr_warn_ratelimited(
			"virtio_nvrm: events: class %#x cannot be served in the guest -- dropped\n",
			ev_class(r));
		return;
	}

	/* hEvent=0 identifies a semaphore waiter keyed by callback pointer. */
	if (ev_hevent(r) == 0) {
		semsurf_fire(r);
		return;
	}

	spin_lock_irqsave(&event_cb_lock, flags);
	for (i = 0; i < NVRM_EVENT_CB_SLOTS; i++) {
		struct event_cb_slot *sl = &event_cb_slots[i];
		u64 func, arg;

		if (!sl->handle || sl->handle != ev_hevent(r) ||
		    sl->client != ev_hclient(r) || sl->proc != r->guest_proc)
			continue;
		/* Require the returned callback pointer to match the slot.
		 * Handle reuse may otherwise deliver a stale pointer to
		 * freed memory. */
		if (sl->kc != ev_kc(r)) {
			spin_unlock_irqrestore(&event_cb_lock, flags);
			stat_events_dropped++;
			stat_events_drop_noslot++;
			pr_warn_ratelimited(
				"virtio_nvrm: events: %#x/%#x fired with kc %#llx, slot holds %#llx -- dropped\n",
				ev_hclient(r), ev_hevent(r),
				(unsigned long long)ev_kc(r),
				(unsigned long long)sl->kc);
			return;
		}
		/* Reject notifiers whose handlers dereference event
		 * payload: substituted OS events carry no payload, so arg2
		 * is NULL (nvkms-rm.c:1696-1720,1774-1785;
		 * osapi.c:504-535). Host DP_IRQ events can occur. Mask
		 * notifyIndex to bits 15:0 as RM does. */
		switch (sl->notify_index & NVRM_NOTIFY_INDEX_MASK) {
		case NVRM_NOTIFIER_DP_IRQ:
		case NVRM_NOTIFIER_LPWR_DIFR_PREFETCH:
		case NVRM_NOTIFIER_HDMI_FRL_RETRAIN:
			spin_unlock_irqrestore(&event_cb_lock, flags);
			stat_events_dropped++;
			stat_events_drop_filtered++;
			return;
		default:
			break;
		}
		/* Read NVOS10_EVENT_KERNEL_CALLBACK_EX from guest kernel
		 * memory and call under event_cb_lock using
		 * Callback5ArgVoidReturn (nvos.h:398-416; os.c:1538).
		 * Checked NVKMS handlers queue timers with GFP_ATOMIC or
		 * take their own fence spinlock; they must not sleep or
		 * reenter kapi. */
		func = rd64((const u8 *)(uintptr_t)sl->kc,
			    NVRM_NVOS10_CB_EX_FUNC_OFF);
		arg = rd64((const u8 *)(uintptr_t)sl->kc,
			   NVRM_NVOS10_CB_EX_ARG_OFF);
		if (func)
			((void (*)(void *, void *, u32, u32, u32))(
				uintptr_t)func)((void *)(uintptr_t)arg, NULL,
						ev_hevent(r), ev_data(r),
						ev_status(r));
		stat_events_delivered++;
		spin_unlock_irqrestore(&event_cb_lock, flags);
		return;
	}
	spin_unlock_irqrestore(&event_cb_lock, flags);
	stat_events_dropped++;
	stat_events_drop_noslot++;
	pr_warn_ratelimited(
		"virtio_nvrm: events: %#x/%#x fired, no slot (proc %u) -- dropped\n",
		ev_hclient(r), ev_hevent(r), r->guest_proc);
}

/* Wake a userspace file under the XArray lock, which also protects context
 * removal. */
static void event_fire_wakeup(struct nvrm_dev *dev, const struct nvrm_req *r)
{
	struct nvrm_ctx *ctx = NULL;
	unsigned long key;

	if (nvrm_ctx_key(r->guest_proc, r->target_token, &key)) {
		xa_lock(&dev->ctx_xa);
		ctx = xa_load(&dev->ctx_xa, key);
		if (ctx) {
			atomic_set(&ctx->events_pending, 1);
			wake_up_interruptible(&ctx->events_wq);
			stat_events_delivered++;
		}
		xa_unlock(&dev->ctx_xa);
	}
	if (!ctx) {
		/* The file closed before this firing was delivered. */
		stat_events_dropped++;
		stat_events_drop_noslot++;
	}
}

/* The work item: drain the ring, hand every firing on. Process context, so
 * that the ring is emptied under evq_lock but nothing is CALLED under it. */
static void nvrm_events_work(struct work_struct *work)
{
	struct nvrm_dev *dev = container_of(work, struct nvrm_dev, events_work);
	struct nvrm_req r;
	unsigned long flags;

	for (;;) {
		spin_lock_irqsave(&dev->evq_lock, flags);
		if (dev->ev_head == dev->ev_tail) {
			spin_unlock_irqrestore(&dev->evq_lock, flags);
			return;
		}
		r = dev->ev_ring[dev->ev_head & (NVRM_EV_RING - 1)];
		dev->ev_head++;
		spin_unlock_irqrestore(&dev->evq_lock, flags);

		if (ev_class(&r) == NVRM_CLASS_EVENT_OS_EVENT)
			event_fire_wakeup(dev, &r);
		else
			event_fire_callback(&r);
	}
}

/* Defined further down, with the kernel-path mapping code it belongs to. */
static void kapi_maps_drop(void);

static void kapi_session_close(void)
{
	struct nvrm_ctx *ctx;
	u32 i;

	/* Before the session goes: NVKMS does not always unmap what it mapped,
	 * and a window slot nobody owns is a slot the next mapping cannot use. */
	kapi_maps_drop();
	/* And no vblank callback may outlive it: pProc points into
	 * nvidia-modeset.ko. */
	vblank_drop_all();
	/* Same for the event callbacks: every kc is NVKMS memory. After this
	 * a firing that is still in the ring finds no slot and is counted. */
	event_cb_drop_all();
	semsurf_drop_all();

	mutex_lock(&kapi_lock);
	ctx = kapi_ctx;
	kapi_ctx = NULL;
	if (!ctx) {
		mutex_unlock(&kapi_lock);
		return;
	}

	/* GPU sessions first: they sit on top of the control one and share its
	 * process entry. */
	for (i = 0; i < kapi_gpu_count; i++) {
		if (kapi_gpus[i].ctx) {
			kapi_ctx_close(kapi_gpus[i].ctx);
			kapi_gpus[i].ctx = NULL;
		}
		kapi_gpus[i].refs = 0;
	}
	kapi_gpu_count = 0;
	/* The host drops everything the session held when the token closes, so
	 * this handle is gone either way. Clearing it is about THIS module: a
	 * stale handle reused after a reopen would name somebody else's
	 * object. */
	kapi_client = 0;
	kapi_ctx_close(ctx);
	mutex_unlock(&kapi_lock);
}

/* Forward an op using generated nr/size; kernel calls have no _IOC size
 * encoding. fd_tok selects the mapping node, or NVRM_NONE_U64 for this
 * context's own token. */
static long kapi_forward_on(struct nvrm_ctx *target, u32 nr, void *params,
			    u32 size, u64 fd_tok)
{
	struct nvrm_ctx *ctx;
	struct call c;
	long ret;

	/* Held for the WHOLE call, not just while opening. Dropping it here
	 * would leave a window in which kapi_session_close() frees ctx between
	 * the lookup and the use. There is no contention to save: calls on one
	 * session serialise on ctx->lock inside nvrm_call_run() anyway, and
	 * there is exactly one NVKMS session. */
	mutex_lock(&kapi_lock);
	ctx = target ? target : kapi_session();
	if (IS_ERR(ctx)) {
		mutex_unlock(&kapi_lock);
		return PTR_ERR(ctx);
	}

	memset(&c, 0, sizeof(c));
	c.ctx = ctx;
	c.dev = ctx->dev;
	c.t = &ctx->dev->tbl;
	c.emb_off = NVRM_NONE_U32;
	c.fd_off = NVRM_NONE_U32;
	c.fd_token = NVRM_NONE_U64;
	/* Not stated by default; the host then falls back to the caller's own
	 * mirror, which is what every path except a cross-process import wants. */
	c.fd_proc = NVRM_NONE_U32;
	c.aux_fd_off = NVRM_NONE_U32;
	c.aux_fd_token = NVRM_NONE_U64;
	/* An auxiliary FD can belong to another userspace session.
	 * Initialize its owner sentinel explicitly; zero is a live session
	 * ID. */
	c.aux_fd_proc = NVRM_NONE_U32;
	c.aux_fd_len = 8;
	c.kern = true;
	c.addr = (u64)(uintptr_t)params;
	c.nr = nr;
	c.size = size;
	c.desc = find_ioctl(c.t, ctx->dev_tag, c.nr);

	if (c.size > c.t->hdr.max_inline) {
		ret = -EMSGSIZE;
		goto out;
	}
	/* Escape-level FD fields use this kernel context's token, or the
	 * explicit mapping-node token. Do not resolve an integer through
	 * current: a kthread has no relevant user FD table. Control-level
	 * imported FDs are handled separately in gather_embedded. */
	if (c.desc && c.desc->fd_off != NVRM_NONE_U32) {
		if (nvrm_range_valid(c.desc->fd_off, 4, c.size)) {
			c.fd_off = c.desc->fd_off;
			memcpy(&c.fd_orig, (u8 *)params + c.desc->fd_off, 4);
			c.fd_token = fd_tok != NVRM_NONE_U64 ? fd_tok :
							       ctx->token;
		} else {
			pr_warn("virtio_nvrm: escape %#x claims an fd at +%u, past its %u-byte block\n",
				nr, c.desc->fd_off, size);
			ret = -EINVAL;
			goto out;
		}
	}
	ret = nvrm_call_run(&c);
out:
	mutex_unlock(&kapi_lock);
	return ret;
}

static long kapi_forward_fd(u32 nr, void *params, u32 size, u64 fd_tok)
{
	return kapi_forward_on(NULL, nr, params, size, fd_tok);
}

static long kapi_forward(u32 nr, void *params, u32 size)
{
	return kapi_forward_on(NULL, nr, params, size, NVRM_NONE_U64);
}

/* nvidia_modeset_rm_ops_t contains seven functions, version_string and
 * system_info. */

/* No alternate RM stack is needed: execution runs on the host. NVIDIA's API
 * permits alloc_stack success with sp=NULL on architectures without
 * alternate stacks. */
static int kapi_alloc_stack(void **sp)
{
	*sp = NULL;
	return 0;
}

static void kapi_free_stack(void *sp)
{
}

/* Use an independent root client for GPU enumeration instead of borrowing
 * NVKMS's handle. hObjectNew=NV01_NULL_OBJECT requests an RM-assigned
 * handle (nvkms.c:6371); session teardown frees it. */
static int kapi_client_ensure(void)
{
	u8 p[NVRM_KSIZE_ALLOC];
	long ret;
	u32 status;

	if (kapi_client)
		return 0;

	memset(p, 0, sizeof(p));
	/* hRoot, hObjectParent, hObjectNew and pAllocParms all stay 0:
	 * NV01_ROOT is class 0 and takes no allocation parameters. */
	wr32(p, NVRM_NVOS64_HCLASS_OFF, 0 /* NV01_ROOT */);

	ret = kapi_forward(NVRM_KESC_ALLOC, p, NVRM_KSIZE_ALLOC);
	if (ret) {
		pr_warn("virtio_nvrm: could not allocate an RM client: %ld\n",
			ret);
		return ret;
	}
	status = rd32(p, NVRM_NVOS64_STATUS_OFF);
	if (status) {
		pr_warn("virtio_nvrm: RM refused a client: status %#x\n",
			status);
		return -EIO;
	}
	kapi_client = rd32(p, NVRM_NVOS64_HOBJECTNEW_OFF);
	if (!kapi_client) {
		pr_warn("virtio_nvrm: RM returned client handle 0\n");
		return -EIO;
	}
	return 0;
}

/* One control on our own client. `params` is kernel memory; the interpreter
 * knows that from c.kern and fetches it with memcpy instead of
 * copy_from_user. */
static int kapi_control(u32 cmd, void *params, u32 size)
{
	u8 p[NVRM_KSIZE_CONTROL];
	long ret;
	u32 status;

	memset(p, 0, sizeof(p));
	wr32(p, NVRM_NVOS54_HCLIENT_OFF, kapi_client);
	wr32(p, NVRM_NVOS54_HOBJECT_OFF, kapi_client);
	wr32(p, NVRM_NVOS54_CMD_OFF, cmd);
	wr64(p, NVRM_NVOS54_PARAMS_OFF, (u64)(uintptr_t)params);
	wr32(p, NVRM_NVOS54_PARAMSSIZE_OFF, size);

	ret = kapi_forward(NVRM_KESC_CONTROL, p, NVRM_KSIZE_CONTROL);
	if (ret)
		return ret;
	status = rd32(p, NVRM_NVOS54_STATUS_OFF);
	if (status) {
		pr_warn("virtio_nvrm: control %#x: status %#x\n", cmd, status);
		return -EIO;
	}
	return 0;
}

/* Enumerate GPUs from RM. os_device_ptr names the guest device that
 * mediates access; it is not supplied by host RM.
 * For display, use the virtio PCI parent so PRIME dma-buf imports have
 * dma_mask and DMA ops. struct virtio_device lacks them; using it caused
 * dma_map_sgtable warnings (2026-08-08). */
static u32 kapi_enumerate_gpus_on(struct nvrm_dev *dev,
				  struct nvrm_gpu_info *gpu_info)
{
	u8 *probed;
	u32 count = 0;
	u32 i;

	if (!dev || !dev->vdev)
		return 0;
	if (kapi_client_ensure())
		return 0;

	/* Keep RM's three parallel arrays in one 384-byte heap allocation. */
	probed = kvzalloc(NVRM_SIZE_PROBED_IDS, GFP_KERNEL);
	if (!probed)
		return 0;

	if (kapi_control(NVRM_CTRL_GPU_GET_PROBED_IDS, probed,
			 NVRM_SIZE_PROBED_IDS)) {
		kvfree(probed);
		return 0;
	}

	for (i = 0; i < NVRM_MAX_DEVICES && count < NVRM_NV_MAX_GPUS; i++) {
		u8 pci[NVRM_SIZE_PCI_INFO];
		u32 id = rd32(probed, i * 4);

		if (id == NVRM_GPU_INVALID_ID)
			continue;

		memset(pci, 0, sizeof(pci));
		wr32(pci, NVRM_PCI_INFO_GPUID_OFF, id);
		if (kapi_control(NVRM_CTRL_GPU_GET_PCI_INFO, pci,
				 NVRM_SIZE_PCI_INFO)) {
			pr_warn("virtio_nvrm: no PCI info for GPU %#x -- skipped\n",
				id);
			continue;
		}

		memset(&gpu_info[count], 0, sizeof(gpu_info[count]));
		/* kapi_forward already rewrote this reply. GET_PCI_INFO
		 * below converts the guest ID back on submission; do not
		 * mediate either ID twice. */
		gpu_info[count].gpu_id = id;
		gpu_info[count].pci_info.domain =
			rd32(pci, NVRM_PCI_INFO_DOMAIN_OFF);
		gpu_info[count].pci_info.bus =
			(u8)rd16(pci, NVRM_PCI_INFO_BUS_OFF);
		gpu_info[count].pci_info.slot =
			(u8)rd16(pci, NVRM_PCI_INFO_SLOT_OFF);
		/* GET_PCI_INFO returns domain, bus and slot and NO function.
		 * Unmediated that leaves a zero we know about rather than one
		 * we guessed; mediated, the guest's own function is known and
		 * gets written. */
		gpu_info[count].pci_info.function = 0;

		if (bdf_on(dev)) {
			gpu_info[count].pci_info.domain = dev->bdf_domain;
			gpu_info[count].pci_info.bus = dev->bdf_bus;
			gpu_info[count].pci_info.slot = dev->bdf_slot;
			gpu_info[count].pci_info.function = dev->bdf_func;
		}
		gpu_info[count].needs_numa_setup = 0;
		gpu_info[count].is_soc_disp = 0;
		/* See the comment above this function: the virtio device names
		 * the mediation, its PCI parent can do DMA. A dma-buf import
		 * needs the second. */
		if (display && dev->vdev->dev.parent &&
		    dev_is_pci(dev->vdev->dev.parent))
			gpu_info[count].os_device_ptr = dev->vdev->dev.parent;
		else
			gpu_info[count].os_device_ptr = &dev->vdev->dev;

		/* Remember it: open_gpu() is handed a gpu_id and needs a node.
		 * The MEDIATED id, because that is the one nvidia-drm was
		 * handed above and the one it will hand back. */
		kapi_gpus[count].gpu_id = gpu_info[count].gpu_id;
		kapi_gpus[count].index = count;
		count++;
	}

	kvfree(probed);
	kapi_gpu_count = count;
	pr_info("virtio_nvrm: enumerate_gpus: %u GPU(s) from RM\n", count);
	return count;
}

static u32 kapi_enumerate_gpus(struct nvrm_gpu_info *gpu_info)
{
	struct nvrm_dev *dev = nvrm_dev_get();
	u32 ret = kapi_enumerate_gpus_on(dev, gpu_info);

	nvrm_dev_put(dev);
	return ret;
}

/* open_gpu/close_gpu: raise and lower a reference on one GPU.
 *
 * `reset_aware` is ignored. On the host it tells RM the caller survives a
 * GPU reset; there is no reset path across this virtqueue, so honouring it
 * would be a promise this module cannot keep. */
static struct kapi_gpu *kapi_gpu_find(u32 gpu_id)
{
	u32 i;

	for (i = 0; i < kapi_gpu_count; i++)
		if (kapi_gpus[i].gpu_id == gpu_id)
			return &kapi_gpus[i];
	return NULL;
}

static int kapi_open_gpu(u32 gpu_id, void *sp, u8 reset_aware)
{
	struct kapi_gpu *g;
	struct nvrm_ctx *ctx;
	int ret = 0;

	mutex_lock(&kapi_lock);
	g = kapi_gpu_find(gpu_id);
	if (!g) {
		/* Never invent a node number for a GPU RM did not report. */
		pr_warn("virtio_nvrm: open_gpu(%#x): not in the enumerated list\n",
			gpu_id);
		ret = -ENODEV;
		goto out;
	}
	if (g->refs) {
		g->refs++;
		goto out;
	}
	ctx = kapi_ctx_open_current(NVRM_DEV_GPU, g->index, NULL);
	if (IS_ERR(ctx)) {
		ret = PTR_ERR(ctx);
		pr_warn("virtio_nvrm: open_gpu(%#x): node %u would not open: %d\n",
			gpu_id, g->index, ret);
		goto out;
	}
	g->ctx = ctx;
	g->refs = 1;
	pr_info("virtio_nvrm: open_gpu(%#x) -> /dev/nvidia%u\n", gpu_id,
		g->index);
out:
	mutex_unlock(&kapi_lock);
	return ret;
}

static void kapi_close_gpu(u32 gpu_id, void *sp, u8 reset_aware)
{
	struct kapi_gpu *g;

	mutex_lock(&kapi_lock);
	g = kapi_gpu_find(gpu_id);
	if (g && g->refs && --g->refs == 0) {
		kapi_ctx_close(g->ctx);
		g->ctx = NULL;
	}
	mutex_unlock(&kapi_lock);
}

/* Store callbacks with NVIDIA's single-owner rule
 * (nv-modeset-interface.c:45). They currently have no caller; see
 * nvrm_kapi.h. */
static int kapi_set_callbacks(const struct nvrm_modeset_callbacks *cb)
{
	if ((kapi_callbacks && cb) || (!kapi_callbacks && !cb))
		return -EINVAL;
	kapi_callbacks = cb;
	return 0;
}

/* Kernel MAP_MEMORY performs both RM_MAP_MEMORY and MAP_PREPARE because
 * NVKMS expects an address from one op.
 * NVOS33 MEM_SPACE selects the returned address: USER receives the SHMEM
 * guest-physical address for its own ioremap; CLIENT receives a kernel VA
 * from our ioremap (nvkms-kapi.c:2158-2173). CLIENT is used by semaphore,
 * LUT and displayless flip surfaces.
 * Every failure must set status: op returns void, and a zero status would
 * make callers use an invalid address. */
struct kapi_map {
	struct list_head list;
	/* What was handed out, and therefore what UNMAP_MEMORY names again:
	 * either the guest-physical `win_base + off` or, for a _CLIENT
	 * mapping, the kernel VA below. One key, so the lookup stays one
	 * comparison. */
	u64 linear;
	/* Non-NULL exactly when this module ioremapped the window pages. The
	 * mapping owns it and iounmaps it in kapi_unmap_memory. */
	void __iomem *kva;
	/* Save RM's original host address for UNMAP_MEMORY. The kernel
	 * caller receives a guest address instead, so it cannot supply the
	 * host mapping identity later. */
	u64 host_linear;
	u64 off;
	size_t len; /* window bytes reserved: page-rounded */
	/* Each mapping owns a fresh node: RM permits one mmap context per
	 * open file and removes it only on close (nv-usermap.c:104-120,
	 * nv.c:1079). Reusing a session's node fails the second mapping
	 * with NV_ERR_STATE_IN_USE. */
	struct nvrm_ctx *ctx;
};
static LIST_HEAD(kapi_map_list);
static DEFINE_MUTEX(kapi_map_lock);

/* The kapi session's identifiers, read under the lock kapi_forward also uses.
 * Taken separately so the lock is never held across a virtqueue round trip. */
static int kapi_session_ids(u32 *dev_tag, u64 *token, u32 *proc_id)
{
	int ret = 0;

	mutex_lock(&kapi_lock);
	if (kapi_ctx) {
		*dev_tag = kapi_ctx->dev_tag;
		*token = kapi_ctx->token;
		*proc_id = kapi_ctx->proc ? kapi_ctx->proc->id : 0;
	} else {
		ret = -ENODEV;
	}
	mutex_unlock(&kapi_lock);
	return ret;
}

/* Open a dedicated GPU node and register it against the session's control
 * FD. Subdevice mappings require a GPU node; the sysmem fallback in
 * kapi_map_memory handles control-node mappings. */
static void kapi_map_ctx_close(struct nvrm_ctx *ctx);

static struct nvrm_ctx *kapi_map_ctx_open(bool ctl)
{
	struct nvrm_ctx *ctx, *sess;
	u8 reg[NVRM_KSIZE_REGISTER_FD];
	u32 index = kapi_gpu_count ? kapi_gpus[0].index : 0;
	u64 sess_token;
	long ret;

	mutex_lock(&kapi_lock);
	sess = kapi_session();
	if (IS_ERR(sess)) {
		mutex_unlock(&kapi_lock);
		return sess;
	}
	sess_token = sess->token;
	ctx = ctl ? kapi_ctx_open_current(NVRM_DEV_CTL, 0, NULL) :
		    kapi_ctx_open_current(NVRM_DEV_GPU, index, NULL);
	mutex_unlock(&kapi_lock);
	if (IS_ERR(ctx))
		return ctx;

	/* Bind to the control node using its token. The numeric FD field is
	 * unused. */
	memset(reg, 0, sizeof(reg));
	ret = kapi_forward_on(ctx, NVRM_KESC_REGISTER_FD, reg,
			      NVRM_KSIZE_REGISTER_FD, sess_token);
	if (ret) {
		pr_warn("virtio_nvrm: kernel MAP_MEMORY: REGISTER_FD failed: %ld\n",
			ret);
		/* Under kapi_lock, like kapi_close_gpu and every unmap:
		 * kapi_ctx_close touches kapi_proc, and this path runs while
		 * NVKMS is live (kapi_session_close, the one caller without the
		 * lock, runs at device removal when nothing else does). */
		kapi_map_ctx_close(ctx);
		return ERR_PTR(ret);
	}
	return ctx;
}

static void kapi_map_ctx_close(struct nvrm_ctx *ctx)
{
	if (!ctx)
		return;
	mutex_lock(&kapi_lock);
	kapi_ctx_close(ctx);
	mutex_unlock(&kapi_lock);
}

static long kapi_map_memory_on(struct nvrm_dev *dev, u8 *params)
{
	struct nvrm_ctx *mctx;
	struct kapi_map *km;
	u32 proc_id, flags;
	u64 cache = 0, len, win_len;
	bool want_kva;
	long off, ret;

	/* Both operands are constants generated into nvrm_wire.h, so this is
	 * a statement about the header and not about this call: it holds or
	 * the module must not build. As a runtime `if` it cost a comparison
	 * per mapping and, worse, read as a refusal a guest could provoke. */
	BUILD_BUG_ON(NVRM_NVOS33_LENGTH_OFF + 8 > NVRM_KSIZE_MAP_MEMORY);

	if (!dev)
		return -ENODEV;

	/* Which of the two addresses this caller wants. See the block comment
	 * above `struct kapi_map` for why bit 14 answers this completely.
	 *
	 * Read HERE, before the round trip, and not from the block RM
	 * hands back: flags is an IN field, and nothing promises that what
	 * comes back still carries it. The caller's intent is only reliably
	 * readable while it is still the caller's block. */
	flags = rd32(params, NVRM_NVOS33_FLAGS_OFF);
	want_kva = !(flags & NVRM_NVOS33_FLAGS_MEM_SPACE_USER);

	/* Preserve MEM_SPACE_USER only for the guest address decision.
	 * Clear it before forwarding: rmapiValidateKernelMapping rejects it
	 * for user-privilege clients, including the backend
	 * (mapping_cpu.c:862). RM creates a userspace mapping for that
	 * client either way. */
	if (!want_kva)
		wr32(params, NVRM_NVOS33_FLAGS_OFF,
		     flags & ~NVRM_NVOS33_FLAGS_MEM_SPACE_USER);

	/* Choose the mapping FD by RM's response: BAR-backed memory needs a
	 * GPU node; system memory needs the control node
	 * (RmCreateMmapContextLocked). Allocation class alone cannot
	 * determine this.
	 * Retry on NV_ERR_INVALID_ARGUMENT. RM rolls back a failed
	 * mmap-context creation (escape.c:600-616), so no partial mapping
	 * remains.
	 * Widen bare NVOS33 to its escape form with an FD field. The
	 * numeric FD is unused; fd_tok identifies this mapping's node. */
	{
		u8 saved[NVRM_KSIZE_MAP_MEMORY];
		bool ctl = false;

		memcpy(saved, params, sizeof(saved));
		for (;;) {
			u8 wire[NVRM_KWIRE_MAP_MEMORY];
			u32 st;

			mctx = kapi_map_ctx_open(ctl);
			if (IS_ERR(mctx)) {
				pr_warn("virtio_nvrm: kernel MAP_MEMORY: no %s node for the mapping: %ld\n",
					ctl ? "control" : "GPU", PTR_ERR(mctx));
				wr32(params, NVRM_NVOS33_STATUS_OFF,
				     NVRM_NV_ERR_NOT_SUPPORTED);
				return 0;
			}
			proc_id = mctx->proc ? mctx->proc->id : 0;

			memset(wire, 0, sizeof(wire));
			memcpy(wire, params, NVRM_KSIZE_MAP_MEMORY);
			ret = kapi_forward_fd(NVRM_KESC_MAP_MEMORY, wire,
					      NVRM_KWIRE_MAP_MEMORY,
					      mctx->token);
			memcpy(params, wire, NVRM_KSIZE_MAP_MEMORY);
			if (ret) {
				kapi_map_ctx_close(mctx);
				return ret;
			}
			st = rd32(params, NVRM_NVOS33_STATUS_OFF);
			if (!st)
				break;

			kapi_map_ctx_close(mctx);
			if (st == NVRM_NV_ERR_INVALID_ARGUMENT && !ctl) {
				/* The one refusal that names its own remedy. */
				if (display > 1)
					pr_info("virtio_nvrm: kernel MAP_MEMORY: hMemory %#x is not on this GPU's BARs -- retrying on a control node\n",
						rd32(params, 8));
				memcpy(params, saved, sizeof(saved));
				ctl = true;
				continue;
			}
			pr_warn("virtio_nvrm: kernel MAP_MEMORY refused: status %#x, hClient %#x hDevice %#x hMemory %#x offset %#llx length %llu (%s node)\n",
				st, rd32(params, 0), rd32(params, 4),
				rd32(params, 8), rd64(params, 16),
				rd64(params, NVRM_NVOS33_LENGTH_OFF),
				ctl ? "control" : "GPU");
			return 0;
		}
	}

	len = rd64(params, NVRM_NVOS33_LENGTH_OFF);
	if (!len) {
		pr_warn("virtio_nvrm: kernel MAP_MEMORY with length 0\n");
		kapi_map_ctx_close(mctx);
		wr32(params, NVRM_NVOS33_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return 0;
	}
	/* Reserve whole SHMEM pages even for smaller RM mappings. The
	 * caller keeps its original length; address translation below still
	 * follows MEM_SPACE. */
	win_len = ALIGN(len, PAGE_SIZE);
	if (win_len < len) { /* only reachable on a bogus huge length */
		pr_warn("virtio_nvrm: kernel MAP_MEMORY with unusable length %llu\n",
			len);
		kapi_map_ctx_close(mctx);
		wr32(params, NVRM_NVOS33_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return 0;
	}

	/* 2. A slot in the host-visible window, and the host places it there.
	 *
	 * On THIS mapping's node, not the session's: the host mmaps the file
	 * the token names, and RM hung the mmap context off exactly that file
	 * a moment ago. Naming the session here and the mapping's node above
	 * would be two halves of two different mappings. */
	off = win_alloc(dev, (size_t)win_len);
	if (off < 0) {
		pr_warn("virtio_nvrm: kernel MAP_MEMORY: no window slot for %llu bytes\n",
			win_len);
		kapi_map_ctx_close(mctx);
		wr32(params, NVRM_NVOS33_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return 0;
	}
	/* interruptible = false, always: NVKMS is called from insmod, rmmod and
	 * kthreads, and nobody signals those. Measured once as an rmmod that
	 * never returned. */
	ret = nvrm_simple(dev, NVRM_KIND_MAP_PREPARE, mctx->dev_tag, 0,
			  mctx->token, (u64)off, win_len, &cache, false,
			  proc_id);
	if (ret < 0) {
		pr_warn("virtio_nvrm: kernel MAP_MEMORY: MAP_PREPARE failed: %ld\n",
			ret);
		win_free(dev, (u64)off, (size_t)win_len);
		kapi_map_ctx_close(mctx);
		wr32(params, NVRM_NVOS33_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return 0;
	}

	km = kzalloc(sizeof(*km), GFP_KERNEL);
	if (!km) {
		nvrm_simple(dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0, (u64)off,
			    win_len, NULL, false, proc_id);
		win_free(dev, (u64)off, (size_t)win_len);
		kapi_map_ctx_close(mctx);
		wr32(params, NVRM_NVOS33_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return 0;
	}
	km->off = (u64)off;
	km->len = (size_t)win_len;
	km->ctx = mctx;
	km->host_linear = rd64(params, NVRM_NVOS33_LINEAR_OFF);

	/* Use ioremap for the SHMEM BAR; it is not page-backed guest RAM
	 * for memremap. Follow the host's reported cache type, matching the
	 * userspace mapping. Different memory types for one physical page
	 * create x86 PAT conflicts. */
	if (want_kva) {
		km->kva = (cache == 2) ?
				  ioremap(dev->win_base + (u64)off, km->len) :
				  ioremap_cache(dev->win_base + (u64)off,
						km->len);
		if (!km->kva) {
			pr_warn("virtio_nvrm: kernel MAP_MEMORY: ioremap of %zu bytes at %#llx (cache %llu) failed\n",
				km->len, dev->win_base + (u64)off, cache);
			kfree(km);
			nvrm_simple(dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0,
				    (u64)off, win_len, NULL, false, proc_id);
			win_free(dev, (u64)off, (size_t)win_len);
			kapi_map_ctx_close(mctx);
			wr32(params, NVRM_NVOS33_STATUS_OFF,
			     NVRM_NV_ERR_NOT_SUPPORTED);
			return 0;
		}
		km->linear = (u64)(uintptr_t)km->kva;
	} else {
		km->linear = dev->win_base + (u64)off;
	}

	mutex_lock(&kapi_map_lock);
	list_add(&km->list, &kapi_map_list);
	mutex_unlock(&kapi_map_lock);

	wr64(params, NVRM_NVOS33_LINEAR_OFF, km->linear);
	pr_info("virtio_nvrm: kernel MAP_MEMORY: %llu bytes as %s %#llx (window +%#llx, %zu reserved, flags %#x, cache %llu)\n",
		len, want_kva ? "kernel VA" : "guest-physical", km->linear,
		(u64)off, km->len, flags, cache);
	return 0;
}

static long kapi_map_memory(u8 *params)
{
	struct nvrm_dev *dev = nvrm_dev_get();
	long ret = kapi_map_memory_on(dev, params);

	nvrm_dev_put(dev);
	return ret;
}

static long kapi_unmap_memory_on(struct nvrm_dev *dev, u8 *params)
{
	struct kapi_map *km, *tmp, *found = NULL;
	u32 dev_tag, proc_id;
	u64 token, linear;
	long ret;

	if (!dev)
		return -ENODEV;

	linear = rd64(params, NVRM_NVOS34_LINEAR_OFF);
	mutex_lock(&kapi_map_lock);
	list_for_each_entry_safe(km, tmp, &kapi_map_list, list) {
		if (km->linear == linear) {
			list_del(&km->list);
			found = km;
			break;
		}
	}
	mutex_unlock(&kapi_map_lock);

	/* Give RM back the address RM issued. See `host_linear`. */
	if (found)
		wr64(params, NVRM_NVOS34_LINEAR_OFF, found->host_linear);
	ret = kapi_forward(NVRM_KESC_UNMAP_MEMORY, params,
			   NVRM_KSIZE_UNMAP_MEMORY);
	if (found)
		wr64(params, NVRM_NVOS34_LINEAR_OFF, linear);

	if (found) {
		/* The kernel mapping goes first: after MAP_RELEASE there is no
		 * host mapping behind those window pages any more. */
		if (found->kva)
			iounmap(found->kva);
		if (!kapi_session_ids(&dev_tag, &token, &proc_id))
			nvrm_simple(dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0,
				    found->off, found->len, NULL, false,
				    proc_id);
		win_free(dev, found->off, found->len);
		/* Last: closing the node is what frees RM's mmap context, and
		 * nothing else does (nv.c:1079). A node kept beyond its
		 * mapping is a node no later mapping can use. */
		kapi_map_ctx_close(found->ctx);
		kfree(found);
	} else if (linear) {
		/* Not ours: say so rather than leaving the window slot behind
		 * on the assumption that it will turn up later. */
		pr_warn_ratelimited(
			"virtio_nvrm: kernel UNMAP_MEMORY for %#llx, which this module never handed out\n",
			linear);
	}
	return ret;
}

static long kapi_unmap_memory(u8 *params)
{
	struct nvrm_dev *dev = nvrm_dev_get();
	long ret = kapi_unmap_memory_on(dev, params);

	nvrm_dev_put(dev);
	return ret;
}

/* Every window slot the kernel path still holds, given back. Called from the
 * session teardown, because NVKMS does not always unmap what it mapped. */
static void kapi_maps_drop(void)
{
	struct kapi_map *km, *tmp;
	u32 dev_tag, proc_id;
	u64 token;
	bool have_ids;
	LIST_HEAD(doomed);

	have_ids = kapi_session_ids(&dev_tag, &token, &proc_id) == 0;

	/* Remove under kapi_map_lock, then close outside it. Closing needs
	 * kapi_lock; avoid nesting those locks. */
	mutex_lock(&kapi_map_lock);
	list_splice_init(&kapi_map_list, &doomed);
	mutex_unlock(&kapi_map_lock);

	list_for_each_entry_safe(km, tmp, &doomed, list) {
		list_del(&km->list);
		/* Before MAP_RELEASE, same order as kapi_unmap_memory: after
		 * it there is no host mapping behind those window pages. */
		if (km->kva)
			iounmap(km->kva);
		if (have_ids)
			nvrm_simple(km->ctx->dev, NVRM_KIND_MAP_RELEASE, 0, 0,
				    0, km->off, km->len, NULL, false, proc_id);
		win_free(km->ctx->dev, km->off, km->len);
		kapi_map_ctx_close(km->ctx);
		kfree(km);
	}
}

/* The VRAM balloon owns separate sessions and chunks. balloon_gate prevents
 * process allocations from consuming quota between freeing a chunk and
 * retrying NVKMS.
 * The backend ledger releases bytes before replying to FREE. SCANOUT chunks
 * also leave contiguous VRAM holes, but physical-card pressure or
 * allocations by other VMs/host may still make the retry fail. No cross-VM
 * reservation is implied.
 * Never use FIXED_ADDRESS for the retry: that would let the guest select
 * host physical VRAM addresses. */

/* Plain arithmetic, shared with test/vramcheck.c. */
#include "nvrm_vram.c"

/* A refill that the ledger refuses waits 1 s, then twice as long each time,
 * up to 30 s: the balloon takes room the ledger gives and never asks in a
 * loop. */
#define NVRM_BALLOON_BACKOFF_MIN HZ
#define NVRM_BALLOON_BACKOFF_MAX (30 * HZ)

static void balloon_worker(struct work_struct *work);
static DECLARE_DELAYED_WORK(balloon_work, balloon_worker);

/* Protect balloon state. Acquire before kapi_lock; never acquire from
 * inside kapi_lock. */
static DEFINE_MUTEX(balloon_lock);

static struct {
	struct nvrm_proc *proc; /* the identity both sessions share */
	struct nvrm_ctx *ctl, *gpu;
	u32 client, device; /* RM handles, 0 = not allocated */
	bool cut; /* `shape` is cut for `target` */
	u64 target; /* bytes */
	struct nvrm_balloon_shape shape;
	/* Bit i: chunk i is held. Bit shape.full is the rest. Chunk i is
	 * handle device + 1 + i, so a handle needs no table. */
	DECLARE_BITMAP(held, NVRM_BALLOON_MAX_CHUNKS + 1);
	u64 held_bytes;
	unsigned long backoff;
	bool filled_once; /* the first fill was announced */
} balloon;

/* Queueing the work is allowed only while the device lives: remove sets
 * `balloon_gone` under this lock before it cancels, so nothing re-arms
 * behind it. The same lock keeps the served list below. */
static DEFINE_SPINLOCK(balloon_kick_lock);
static bool balloon_gone = true;

/* Display buffers that got their room from the balloon, by (client, handle).
 * When NVKMS frees one, that room is the balloon's to take back. More than
 * fit are not remembered; the backoff refills those. */
static struct {
	u32 client, handle;
} balloon_served[16];

static void balloon_kick(unsigned long delay, bool sooner)
{
	unsigned long flags;

	spin_lock_irqsave(&balloon_kick_lock, flags);
	if (!balloon_gone) {
		if (sooner)
			mod_delayed_work(system_wq, &balloon_work, delay);
		else
			queue_delayed_work(system_wq, &balloon_work, delay);
	}
	spin_unlock_irqrestore(&balloon_kick_lock, flags);
}

static int display_reserve_set(const char *val, const struct kernel_param *kp)
{
	int v, ret;

	ret = kstrtoint(val, 0, &v);
	if (ret)
		return ret;
	if (v < -1)
		return -EINVAL;
	WRITE_ONCE(display_reserve_mib, v);
	/* At load the device does not exist yet and the kick does nothing:
	 * probe starts the balloon with whatever the parameters say then. */
	balloon_kick(0, true);
	return 0;
}

/* What the balloon should hold, in bytes, and the scanout buffer it is cut
 * into. */
static u64 balloon_target(u64 *scanout)
{
	int v = READ_ONCE(display_reserve_mib);
	u32 w = READ_ONCE(vdisplay_width), h = READ_ONCE(vdisplay_height);

	*scanout = w && h && w <= NVRM_RESERVE_MAX_DIM &&
				   h <= NVRM_RESERVE_MAX_DIM ?
			   nvrm_scanout_bytes(w, h) :
			   0;
	if (v >= 0)
		return (u64)v << 20;
	return READ_ONCE(display) ?
		       (u64)nvrm_display_reserve_auto_mib(w, h) << 20 :
		       0;
}

static u64 balloon_chunk_bytes(u32 i)
{
	return i < balloon.shape.full ? balloon.shape.chunk :
					balloon.shape.rest;
}

/* One NV04_ALLOC on the balloon's control session: 0, RM's status, or the
 * transport's negative errno. */
static long balloon_alloc(u8 *p)
{
	long ret = kapi_forward_on(balloon.ctl, NVRM_KESC_ALLOC, p,
				   NVRM_KSIZE_ALLOC, NVRM_NONE_U64);

	return ret ? ret : rd32(p, NVRM_NVOS64_STATUS_OFF);
}

/* Initialize balloon sessions/client/device under balloon_lock. Its
 * separate guest_proc avoids sharing NVKMS's handles and accounting.
 * Keep a GPU node open: RM requires it when a userspace client allocates a
 * device (device.c:141-149). Memory is allocated under that device; no
 * subdevice is needed. */
static long balloon_open(void)
{
	u8 p[NVRM_KSIZE_ALLOC], dp[NVRM_DEVICE_ALLOC_SIZE];
	struct nvrm_ctx *ctx;
	long ret;

	if (balloon.device)
		return 0;
	if (!balloon.proc) {
		struct nvrm_dev *dev = nvrm_dev_get();

		if (!dev)
			return -ENODEV;
		balloon.proc = nvrm_proc_kernel(dev, "nvrm-balloon");
		nvrm_dev_put(dev);
		if (IS_ERR(balloon.proc)) {
			ret = PTR_ERR(balloon.proc);
			balloon.proc = NULL;
			return ret;
		}
	}
	if (!balloon.ctl || !balloon.gpu) {
		mutex_lock(&kapi_lock);
		ctx = balloon.ctl ? balloon.ctl :
				    kapi_ctx_open_current(NVRM_DEV_CTL, 0,
							  balloon.proc);
		if (!IS_ERR(ctx)) {
			balloon.ctl = ctx;
			ctx = kapi_ctx_open_current(NVRM_DEV_GPU, 0,
						    balloon.proc);
			if (!IS_ERR(ctx))
				balloon.gpu = ctx;
		}
		mutex_unlock(&kapi_lock);
		if (IS_ERR(ctx))
			return PTR_ERR(ctx);
	}
	if (!balloon.client) {
		/* hObjectNew 0: RM picks the client handle, as for NVKMS's. */
		memset(p, 0, sizeof(p));
		wr32(p, NVRM_NVOS64_HCLASS_OFF, NVRM_CLASS_ROOT);
		ret = balloon_alloc(p);
		if (ret)
			return ret;
		balloon.client = rd32(p, NVRM_NVOS64_HOBJECTNEW_OFF);
		if (!balloon.client)
			return -EIO;
	}
	/* Device instance 0, this guest's one GPU. */
	memset(dp, 0, sizeof(dp));
	wr32(dp, NVRM_DEVICE_ALLOC_ID_OFF, 0);
	memset(p, 0, sizeof(p));
	wr32(p, NVRM_NVOS64_HROOT_OFF, balloon.client);
	wr32(p, NVRM_NVOS64_HOBJECTPARENT_OFF, balloon.client);
	wr32(p, NVRM_NVOS64_HOBJECTNEW_OFF, balloon.client + 1);
	wr32(p, NVRM_NVOS64_HCLASS_OFF, NVRM_CLASS_DEVICE);
	wr64(p, NVRM_NVOS64_PALLOCPARMS_OFF, (u64)(uintptr_t)dp);
	ret = balloon_alloc(p);
	if (!ret)
		balloon.device = balloon.client + 1;
	return ret;
}

/* Under balloon_lock, free the client and its charged children, then close
 * sessions and release the process identity. */
static void balloon_close(void)
{
	u8 p[NVRM_KSIZE_FREE];

	if (balloon.client && balloon.ctl) {
		memset(p, 0, sizeof(p));
		wr32(p, NVRM_NVOS00_HROOT_OFF, balloon.client);
		wr32(p, NVRM_NVOS00_HOBJECTOLD_OFF, balloon.client);
		kapi_forward_on(balloon.ctl, NVRM_KESC_FREE, p, sizeof(p),
				NVRM_NONE_U64);
	}
	balloon.client = 0;
	balloon.device = 0;
	bitmap_zero(balloon.held, NVRM_BALLOON_MAX_CHUNKS + 1);
	balloon.held_bytes = 0;
	mutex_lock(&kapi_lock);
	kapi_ctx_close(balloon.gpu);
	kapi_ctx_close(balloon.ctl);
	mutex_unlock(&kapi_lock);
	balloon.gpu = NULL;
	balloon.ctl = NULL;
	if (balloon.proc)
		nvrm_proc_put(balloon.proc->dev, balloon.proc);
	balloon.proc = NULL;
}

/* Chunk i, with NVKMS's own SCANOUT attributes (nvrm_balloon_chunk_params).
 * 0, RM's status, or a negative errno. Caller holds balloon_lock. */
static long balloon_alloc_chunk(u32 i)
{
	u8 p[NVRM_KSIZE_ALLOC], a[NVRM_MEMALLOC_SIZE];
	long ret;

	nvrm_balloon_chunk_params(a, balloon_chunk_bytes(i));
	memset(p, 0, sizeof(p));
	wr32(p, NVRM_NVOS64_HROOT_OFF, balloon.client);
	wr32(p, NVRM_NVOS64_HOBJECTPARENT_OFF, balloon.device);
	wr32(p, NVRM_NVOS64_HOBJECTNEW_OFF, balloon.device + 1 + i);
	wr32(p, NVRM_NVOS64_HCLASS_OFF, NVRM_CLASS_MEMORY_LOCAL_USER);
	wr64(p, NVRM_NVOS64_PALLOCPARMS_OFF, (u64)(uintptr_t)a);
	ret = balloon_alloc(p);
	if (!ret) {
		set_bit(i, balloon.held);
		balloon.held_bytes += balloon_chunk_bytes(i);
	}
	return ret;
}

/* Chunk i, given back. The bit goes whatever RM's status: the backend
 * releases the charge regardless of it (session.rs, the NVOS00 branch), and
 * that charge is the room. Only a failed transport keeps the bit. Caller
 * holds balloon_lock. */
static long balloon_free_chunk(u32 i)
{
	u8 p[NVRM_KSIZE_FREE];
	long ret;

	memset(p, 0, sizeof(p));
	wr32(p, NVRM_NVOS00_HROOT_OFF, balloon.client);
	wr32(p, NVRM_NVOS00_HOBJECTPARENT_OFF, balloon.device);
	wr32(p, NVRM_NVOS00_HOBJECTOLD_OFF, balloon.device + 1 + i);
	ret = kapi_forward_on(balloon.ctl, NVRM_KESC_FREE, p, sizeof(p),
			      NVRM_NONE_U64);
	if (ret) {
		pr_warn_ratelimited(
			"virtio_nvrm: balloon: freeing chunk %u failed: %ld\n",
			i, ret);
		return ret;
	}
	if (rd32(p, NVRM_NVOS00_STATUS_OFF))
		pr_warn_ratelimited(
			"virtio_nvrm: balloon: RM answered the free of chunk %u with status %#x\n",
			i, rd32(p, NVRM_NVOS00_STATUS_OFF));
	clear_bit(i, balloon.held);
	balloon.held_bytes -= balloon_chunk_bytes(i);
	return 0;
}

/* Free chunks until `want` bytes are free or nothing is held (see
 * nvrm_balloon_pick for which); returns the bytes freed. Caller holds
 * balloon_lock. */
static u64 balloon_deflate(u64 want)
{
	u64 freed = 0;

	while (freed < want) {
		u32 full = balloon.shape.full;
		int k = nvrm_balloon_pick(bitmap_weight(balloon.held, full),
					  balloon.shape.rest &&
						  test_bit(full, balloon.held),
					  balloon.shape.rest, want - freed);
		u32 i;

		if (k < 0)
			break;
		i = k ? (u32)find_last_bit(balloon.held, full) : full;
		if (balloon_free_chunk(i))
			break;
		freed += balloon_chunk_bytes(i);
	}
	return freed;
}

/* Fill to the target, from probe, from a write to display_reserve_mib, when
 * a buffer the balloon made room for is freed, and on the backoff while it
 * stays below. One chunk at a time and never around a refusal: what the
 * ledger will not give now, it may give on the next round. */
static void balloon_worker(struct work_struct *work)
{
	struct nvrm_balloon_shape shape;
	u64 scanout, target, before;
	long ret;
	u32 i, n;

	mutex_lock(&balloon_lock);
	if (READ_ONCE(balloon_gone))
		goto out;
	target = balloon_target(&scanout);
	nvrm_balloon_shape(target, scanout, &shape);
	if (!balloon.cut || target != balloon.target ||
	    shape.chunk != balloon.shape.chunk) {
		/* A new size: let go of everything and cut anew. */
		if (balloon.held_bytes || !target)
			balloon_close();
		balloon.target = target;
		balloon.shape = shape;
		balloon.cut = true;
		balloon.backoff = NVRM_BALLOON_BACKOFF_MIN;
		balloon.filled_once = false;
		if (!target)
			pr_info("virtio_nvrm: balloon off (display_reserve_mib=%d, display=%u)\n",
				READ_ONCE(display_reserve_mib),
				READ_ONCE(display));
	}
	if (!balloon.target)
		goto out;

	before = balloon.held_bytes;
	n = balloon.shape.full + (balloon.shape.rest ? 1 : 0);
	ret = balloon_open();
	while (!ret && (i = (u32)find_first_zero_bit(balloon.held, n)) < n)
		ret = balloon_alloc_chunk(i);

	if (balloon.held_bytes == balloon.target) {
		balloon.backoff = NVRM_BALLOON_BACKOFF_MIN;
		if (!balloon.filled_once)
			pr_info("virtio_nvrm: balloon holds %llu MiB (%u x %llu KiB + %llu KiB, %s %ux%u) -- guest processes reach the VRAM cap that much earlier, NVKMS gets it back when it is refused a display buffer\n",
				balloon.target >> 20, balloon.shape.full,
				balloon.shape.chunk >> 10,
				balloon.shape.rest >> 10,
				READ_ONCE(display_reserve_mib) < 0 ?
					"auto for" :
					"fixed, cut for",
				READ_ONCE(vdisplay_width),
				READ_ONCE(vdisplay_height));
		else if (balloon.held_bytes != before)
			pr_info_ratelimited(
				"virtio_nvrm: balloon refilled to %llu MiB (+%llu KiB)\n",
				balloon.target >> 20,
				(balloon.held_bytes - before) >> 10);
		balloon.filled_once = true;
	} else {
		pr_info_ratelimited(
			"virtio_nvrm: balloon holds %llu of %llu KiB, the next step failed (%s %#lx) -- again in %lu s\n",
			balloon.held_bytes >> 10, balloon.target >> 10,
			ret < 0 ? "errno" : "status", ret < 0 ? -ret : ret,
			balloon.backoff / HZ);
		balloon_kick(balloon.backoff, false);
		balloon.backoff =
			min(balloon.backoff * 2, NVRM_BALLOON_BACKOFF_MAX);
	}
out:
	mutex_unlock(&balloon_lock);
}

/* An NV04_ALLOC from NVKMS as it was asked, kept for a second try. */
struct balloon_ask {
	u8 *alloc; /* NVKMS's own params; NULL = not a display buffer */
	u8 saved[NVRM_MEMALLOC_SIZE];
};

static void balloon_before_alloc(u8 *params, struct balloon_ask *ask)
{
	u32 hclass = rd32(params, NVRM_NVOS64_HCLASS_OFF);
	u8 *alloc = (u8 *)(uintptr_t)rd64(params, NVRM_NVOS64_PALLOCPARMS_OFF);

	ask->alloc = NULL;
	if (!alloc || !nvrm_balloon_eligible(hclass, alloc))
		return;
	memcpy(ask->saved, alloc, sizeof(ask->saved));
	ask->alloc = alloc;
}

/* NVKMS was answered. If a display buffer was refused for want of room,
 * give room back and ask again, in the same call, until it is granted or the
 * balloon is empty: Xwayland does not recover from a first failure, so this
 * one has to succeed. The retry is asked with the params exactly as NVKMS
 * wrote them, in case the refusal wrote over any. */
static long balloon_after_alloc(u8 *params, struct balloon_ask *ask, long ret)
{
	u64 want, gave = 0, got, held, target;
	unsigned int tries = 0;
	unsigned long flags;
	u32 st, i;

	if (!ask->alloc || ret ||
	    rd32(params, NVRM_NVOS64_STATUS_OFF) != NVRM_NV_ERR_NO_MEMORY)
		return ret;
	want = rd64(ask->saved, NVRM_MEMALLOC_SIZE_OFF);

	down_write(&balloon_gate);
	mutex_lock(&balloon_lock);
	for (;;) {
		got = balloon_deflate(want);
		if (!got)
			break;
		gave += got;
		memcpy(ask->alloc, ask->saved, sizeof(ask->saved));
		ret = kapi_forward(NVRM_KESC_ALLOC, params, NVRM_KSIZE_ALLOC);
		tries++;
		if (ret || rd32(params, NVRM_NVOS64_STATUS_OFF) !=
				   NVRM_NV_ERR_NO_MEMORY)
			break;
		/* Another NVKMS allocation took the room first: the gate holds
		 * processes back, not NVKMS. Give the next chunk. */
	}
	st = ret ? 0 : rd32(params, NVRM_NVOS64_STATUS_OFF);
	held = balloon.held_bytes;
	target = balloon.target;
	mutex_unlock(&balloon_lock);
	up_write(&balloon_gate);

	if (tries && !ret && st == NVRM_NV_OK) {
		spin_lock_irqsave(&balloon_kick_lock, flags);
		for (i = 0; i < ARRAY_SIZE(balloon_served); i++)
			if (!balloon_served[i].handle) {
				balloon_served[i].client =
					rd32(params, NVRM_NVOS64_HROOT_OFF);
				balloon_served[i].handle = rd32(
					params, NVRM_NVOS64_HOBJECTNEW_OFF);
				break;
			}
		spin_unlock_irqrestore(&balloon_kick_lock, flags);
	}
	if (gave)
		balloon_kick(NVRM_BALLOON_BACKOFF_MIN, false);
	pr_info_ratelimited(
		"virtio_nvrm: balloon: %s[%d] was refused a display buffer of %llu KiB (flags %#x attr %#x attr2 %#x); gave back %llu KiB in %u tr%s, now %s -- balloon holds %llu of %llu KiB\n",
		current->comm, task_tgid_nr(current), want >> 10,
		rd32(ask->saved, NVRM_MEMALLOC_FLAGS_OFF),
		rd32(ask->saved, NVRM_MEMALLOC_ATTR_OFF),
		rd32(ask->saved, NVRM_MEMALLOC_ATTR2_OFF), gave >> 10, tries,
		tries == 1 ? "y" : "ies",
		ret			    ? "the transport failed" :
		st == NVRM_NV_OK	    ? "granted" :
		st == NVRM_NV_ERR_NO_MEMORY ? "still refused" :
					      "refused otherwise",
		held >> 10, target >> 10);
	return ret;
}

/* NVKMS freed an object. If it was a buffer the balloon made room for, the
 * balloon may take that room back now. */
static void balloon_note_free(const u8 *params)
{
	u32 client = rd32(params, NVRM_NVOS00_HROOT_OFF);
	u32 handle = rd32(params, NVRM_NVOS00_HOBJECTOLD_OFF);
	unsigned long flags;
	bool hit = false;
	u32 i;

	spin_lock_irqsave(&balloon_kick_lock, flags);
	for (i = 0; i < ARRAY_SIZE(balloon_served); i++)
		if (balloon_served[i].handle == handle &&
		    balloon_served[i].client == client) {
			balloon_served[i].handle = 0;
			hit = true;
		}
	spin_unlock_irqrestore(&balloon_kick_lock, flags);
	if (hit)
		balloon_kick(0, true);
}

/* From probe, once the device carries calls. */
static void balloon_start(void)
{
	unsigned long flags;

	spin_lock_irqsave(&balloon_kick_lock, flags);
	balloon_gone = false;
	spin_unlock_irqrestore(&balloon_kick_lock, flags);
	balloon_kick(0, true);
}

/* From remove, while the device still answers: no more work, then
 * everything back. No suspend path exists in this driver to hook. */
static void balloon_stop(void)
{
	unsigned long flags;

	spin_lock_irqsave(&balloon_kick_lock, flags);
	balloon_gone = true;
	memset(balloon_served, 0, sizeof(balloon_served));
	spin_unlock_irqrestore(&balloon_kick_lock, flags);
	cancel_delayed_work_sync(&balloon_work);
	mutex_lock(&balloon_lock);
	balloon_close();
	balloon.cut = false;
	mutex_unlock(&balloon_lock);
}

/* Generated status offset per kernel op, or NVRM_KSTAT_NONE. Refusal
 * handling and tracing share this lookup so they agree on each NVOS layout. */
#define NVRM_KSTAT_NONE ((u32)~0u)

static u32 kapi_status_off(u32 op)
{
	static const struct {
		u32 op;
		u32 status_off;
	} tbl[] = {
		{ NVRM_KSTAT_FREE },
		{ NVRM_KSTAT_ALLOC_MEMORY },
		{ NVRM_KSTAT_ALLOC },
		{ NVRM_KSTAT_MAP_MEMORY },
		{ NVRM_KSTAT_UNMAP_MEMORY },
		{ NVRM_KSTAT_ALLOC_CONTEXT_DMA },
		{ NVRM_KSTAT_MAP_MEMORY_DMA },
		{ NVRM_KSTAT_UNMAP_MEMORY_DMA },
		{ NVRM_KSTAT_BIND_CONTEXT_DMA },
		{ NVRM_KSTAT_CONTROL },
		{ NVRM_KSTAT_DUP_OBJECT },
		{ NVRM_KSTAT_SHARE },
		{ NVRM_KSTAT_ADD_VBLANK_CALLBACK },
	};
	u32 i;

	for (i = 0; i < ARRAY_SIZE(tbl); i++)
		if (tbl[i].op == op)
			return tbl[i].status_off;
	return NVRM_KSTAT_NONE;
}

/* Trace kernel ALLOC/FREE by (client, handle), class and responder. Locally
 * answered calls never reach host RM, so this identifies divergent object
 * accounting. Enabled only with display > 1. */
static void kapi_ledger(const char *verb, const u8 *params, u32 handle_off,
			u32 hclass, u32 status_off, const char *who)
{
	if (display <= 1)
		return;
	pr_info("virtio_nvrm: ledger: %s client %#x parent %#x handle %#x class %#x -> status %#x (%s)\n",
		verb, rd32(params, 0), rd32(params, 4),
		rd32(params, handle_off), hclass, rd32(params, status_off),
		who);
}

/* Write NV_ERR_NOT_SUPPORTED into the parameter block of an op this module
 * does not implement, so that the caller sees a refusal rather than the
 * zeroes it arrived with. */
static void kapi_op_refuse(u8 *ops, u32 op)
{
	u32 off = kapi_status_off(op);

	if (off != NVRM_KSTAT_NONE) {
		wr32(ops + NVRM_KAPI_PARAMS_OFF, off,
		     NVRM_NV_ERR_NOT_SUPPORTED);
		pr_warn("virtio_nvrm: kernel RM op %#x is not implemented -- refused\n",
			op);
		return;
	}

	/* VID_HEAP_CONTROL carries a params pointer, so status has no fixed
	 * union offset. Unknown ops and that form can only be logged here. */
	pr_warn("virtio_nvrm: kernel RM op %#x is not implemented AND cannot be refused in place\n",
		op);
}

static void kapi_op(void *sp, void *ops_cmd)
{
	u8 *ops = ops_cmd;
	void *params = ops + NVRM_KAPI_PARAMS_OFF;
	u32 op = rd32(ops, 0);
	long ret;

	switch (op) {
	case NVRM_KOP_FREE:
		/* Handle the local displayless object; it has no host RM
		 * object. */
		if (vdisp_free(params)) {
			kapi_ledger("FREE ", params, 8, 0, 12,
				    "vdisp, NOT sent");
			return;
		}
		if (vblank_free(params)) {
			kapi_ledger("FREE ", params, 8, 0, 12,
				    "vblank, NOT sent");
			return;
		}
		/* Fence any callback slot before forwarding FREE. */
		event_cb_free(params);
		ret = kapi_forward(NVRM_KESC_FREE, params, NVRM_KSIZE_FREE);
		if (!ret)
			balloon_note_free(params);
		kapi_ledger("FREE ", params, 8, 0, 12,
			    ret ? "host, TRANSPORT FAILED" : "host");
		break;
	case NVRM_KOP_CONTROL:
		/* The command, not just the verdict. A kernel-path control that
		 * answers 0 and still leaves the caller giving up is only
		 * readable if the log says WHICH control it was. */
		if (display > 1)
			pr_info("virtio_nvrm: kernel CONTROL %#x on %#x\n",
				rd32(params, NVRM_NVOS54_CMD_OFF),
				rd32(params, NVRM_NVOS54_HOBJECT_OFF));
		if (vdisp_control(params))
			return;
		if (vblank_control(params))
			return;
		/* Arm semaphore callback slots before submission; event
		 * delivery may precede the reply. */
		{
			struct semsurf_pending spend;

			semsurf_before_control(params, &spend);
			ret = kapi_forward(NVRM_KESC_CONTROL, params,
					   NVRM_KSIZE_CONTROL);
			semsurf_after_control(params, &spend, ret);
		}
		/* The class list is the one real answer this module edits, and
		 * only with `vdisplay` on. */
		if (!ret && rd32(params, NVRM_NVOS54_CMD_OFF) ==
				    NVRM_CTRL_GET_CLASSLIST)
			vdisp_rewrite_classlist(params);
		break;
	case NVRM_KOP_ALLOC:
		if (display > 1)
			pr_info("virtio_nvrm: kernel ALLOC, class %#x\n",
				rd32(params, NVRM_NVOS64_HCLASS_OFF));
		/* NVA083 is a class this card does not have and this module
		 * answers. Nothing about it reaches the host. */
		if (vdisp_alloc(params)) {
			kapi_ledger("ALLOC", params, NVRM_NVOS64_HOBJECTNEW_OFF,
				    rd32(params, NVRM_NVOS64_HCLASS_OFF),
				    NVRM_NVOS64_STATUS_OFF, "vdisp, NOT sent");
			return;
		}
		/* Serve NV9010 guest kernel callbacks locally from the
		 * vblank timer. */
		if (vblank_alloc(params)) {
			kapi_ledger("ALLOC", params, NVRM_NVOS64_HOBJECTNEW_OFF,
				    rd32(params, NVRM_NVOS64_HCLASS_OFF),
				    NVRM_NVOS64_STATUS_OFF, "vblank, NOT sent");
			return;
		}
		/* Capture callback identity before the host substitutes an
		 * OS event; register the slot after successful allocation.
		 * Display-buffer OOM may release balloon chunks and retry. */
		{
			struct event_cb_pending pend;
			struct balloon_ask bask;

			event_cb_before_alloc(params, &pend);
			balloon_before_alloc(params, &bask);
			ret = kapi_forward(NVRM_KESC_ALLOC, params,
					   NVRM_KSIZE_ALLOC);
			ret = balloon_after_alloc(params, &bask, ret);
			vdisp_event_on_missing_parent(params);
			event_cb_after_alloc(params, &pend, ret);
		}
		kapi_ledger("ALLOC", params, NVRM_NVOS64_HOBJECTNEW_OFF,
			    rd32(params, NVRM_NVOS64_HCLASS_OFF),
			    NVRM_NVOS64_STATUS_OFF,
			    ret ? "host, TRANSPORT FAILED" : "host");
		break;
	case NVRM_KOP_MAP_MEMORY:
		if (!display)
			goto unimplemented;
		ret = kapi_map_memory(params);
		break;
	case NVRM_KOP_UNMAP_MEMORY:
		if (!display)
			goto unimplemented;
		ret = kapi_unmap_memory(params);
		break;
	case NVRM_KOP_MAP_MEMORY_DMA:
	case NVRM_KOP_UNMAP_MEMORY_DMA:
		/* GPU DMA mappings need no CPU/window translation:
		 * dmaOffset is a GPU VA in the hDma page tables. Forward
		 * unchanged for NVKMS push buffers and head surfaces.
		 * Enabled only with display. */
		if (!display)
			goto unimplemented;
		if (op == NVRM_KOP_MAP_MEMORY_DMA)
			ret = kapi_forward(NVRM_KESC_MAP_MEMORY_DMA, params,
					   NVRM_KSIZE_MAP_MEMORY_DMA);
		else
			ret = kapi_forward(NVRM_KESC_UNMAP_MEMORY_DMA, params,
					   NVRM_KSIZE_UNMAP_MEMORY_DMA);
		break;
	case NVRM_KOP_DUP_OBJECT:
		/* Duplicate the semaphore surface into nvidia-drm's client.
		 * Required for display fence setup; enabled only with
		 * display. */
		if (!display)
			goto unimplemented;
		ret = kapi_forward(NVRM_KESC_DUP_OBJECT, params,
				   NVRM_KSIZE_DUP_OBJECT);
		break;
	default:
unimplemented:
		/* Unimplemented ops must write an error status: op returns
		 * void and untouched zeroed fields look successful. Use
		 * generated per-op status offsets; callers must never map
		 * an uninitialized address. */
		kapi_op_refuse(ops, op);
		return;
	}
	/* Trace RM status separately from the transport result: ret=0 only
	 * confirms ioctl transport, not RM success. */
	if (display > 1) {
		u32 off = kapi_status_off(op);

		if (off != NVRM_KSTAT_NONE)
			pr_info("virtio_nvrm: kernel op %#x -> status %#x (ret %ld)\n",
				op, rd32(params, off), ret);
		else
			pr_info("virtio_nvrm: kernel op %#x -> ret %ld (status offset unknown)\n",
				op, ret);
	}
	if (ret)
		pr_warn_ratelimited(
			"virtio_nvrm: kernel RM op %#x failed: %ld\n", op, ret);
}

/* Export NVKMS's RM entry point. Preserve NVIDIA's version-check contract:
 * on mismatch, return our generated DRIVER_VERSION string and an NV_STATUS
 * error. */
u32 nvidia_get_rm_ops(struct nvrm_modeset_rm_ops *rm_ops)
{
	const struct nvrm_modeset_rm_ops local = {
		.version_string = NVRM_DRIVER_VERSION,
		.system_info = { .allow_write_combining = 0 },
		.alloc_stack = kapi_alloc_stack,
		.free_stack = kapi_free_stack,
		.enumerate_gpus = kapi_enumerate_gpus,
		.open_gpu = kapi_open_gpu,
		.close_gpu = kapi_close_gpu,
		.op = kapi_op,
		.set_callbacks = kapi_set_callbacks,
	};

	if (strcmp(rm_ops->version_string, NVRM_DRIVER_VERSION) != 0) {
		pr_err("virtio_nvrm: version mismatch -- nvidia-modeset.ko says %s, this module carries %s\n",
		       rm_ops->version_string, NVRM_DRIVER_VERSION);
		rm_ops->version_string = NVRM_DRIVER_VERSION;
		return NVRM_NV_ERR_GENERIC;
	}

	*rm_ops = local;
	pr_info("virtio_nvrm: nvidia_get_rm_ops -- NVKMS attached (%s)\n",
		NVRM_DRIVER_VERSION);
	return NVRM_NV_OK;
}
EXPORT_SYMBOL(nvidia_get_rm_ops);

static const struct virtio_device_id nvrm_ids[] = {
	{ VIRTIO_ID_NVRM, VIRTIO_DEV_ANY_ID },
	{ 0 },
};

static unsigned int nvrm_features[] = {
	VIRTIO_RING_F_INDIRECT_DESC,
};

static struct virtio_driver nvrm_driver = {
	.driver.name = KBUILD_MODNAME,
	.driver.owner = THIS_MODULE,
	.id_table = nvrm_ids,
	.feature_table = nvrm_features,
	.feature_table_size = ARRAY_SIZE(nvrm_features),
	.probe = nvrm_probe,
	.remove = nvrm_remove,
};

module_virtio_driver(nvrm_driver);

MODULE_DEVICE_TABLE(virtio, nvrm_ids);
MODULE_LICENSE("GPL");
/* Import the DMA_BUF symbol namespace for dma_buf_attach and related APIs.
 * MODULE_IMPORT_NS requires a string literal from Linux 6.13 onward; older
 * guest kernels require the bare token. */
#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 13, 0)
MODULE_IMPORT_NS(DMA_BUF);
#else
MODULE_IMPORT_NS("DMA_BUF");
#endif
MODULE_DESCRIPTION("virtio-nvrm: NVIDIA RM escapes across the VM boundary");
MODULE_VERSION("1");
