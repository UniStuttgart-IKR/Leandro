// SPDX-License-Identifier: GPL-2.0-only
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * virtio_nvrm - the guest driver that actually serves the NVIDIA nodes.
 *
 * The device is called virtio-nvrm, the host end vhost-user-nvrm. What is
 * transported here is the NVIDIA RM ESCAPE SURFACE (RM = the Resource
 * Manager, the kernel driver behind /dev/nvidiactl and /dev/nvidiaN, whose
 * ioctls NVIDIA calls escapes) -- not CUDA, which sits one
 * layer above and is why the API completeness comes for free. Display is
 * not a second transport either: with `vdisplay=1` this module presents
 * NVIDIA's own displayless class to NVKMS (NVIDIA's modesetting kernel
 * module, nvidia-modeset.ko, which reaches RM through nvidia_get_rm_ops()
 * rather than through a device node) and services the vblank (per-frame
 * vertical-blank) and event
 * callbacks itself (the sections further down). "nvrm" is also the honest
 * statement of scope: a version-locked proprietary kernel ABI, lockstep as
 * a design assumption. How the pieces fit together, at length:
 * docs/ARCHITECTURE.md.
 *
 * The point of the whole exercise: an UNMODIFIED application (nvidia-smi,
 * python train.py, a third-party CUDA binary) runs in the guest without
 * LD_PRELOAD, because /dev/nvidia* are real nodes and this module carries
 * open/ioctl/mmap across the VM boundary.
 *
 * THIS MODULE IS DUMB, ON PURPOSE.
 * Not a single NVIDIA constant lives here: no escape number, no struct
 * size, no field offset. The host sends a descriptor table at startup
 * (kind GET_TABLES), and this code is merely its interpreter. The source
 * of truth stays crates/nvrm-abi/src/xlate.rs -- one number, one place, one
 * version bump. The table keys on (device type, nr), because 0x27 on the
 * ctl node is RM_ALLOC_MEMORY and on the uvm node PAGEABLE_MEM_ACCESS --
 * uvm being /dev/nvidia-uvm, NVIDIA's unified-memory driver, whose commands
 * travel as raw numbers with no _IOC encoding to read a size out of.
 *
 * Coexistence with nvrm_nodes.ko (docs/OPEN-QUESTIONS.md item 2):
 * /proc/driver/nvidia belongs to nvrm_nodes.ko and is NOT touched here.
 * Both modules run side by side: nvrm_nodes.ko with create_nodes=0
 * (supplies params), virtio_nvrm.ko owns the nodes and the forwarding.
 * Without nvrm_nodes.ko, params is missing -- this module deliberately
 * does not stand alone.
 *
 * Kernel pin: 6.8.0-136-generic (Ubuntu 24.04, GUEST_IMAGE).
 */

#include <linux/build_bug.h>
#include <linux/cdev.h>
#include <linux/device.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/highmem.h>
#include <linux/hrtimer.h>
#include <linux/idr.h>
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

/*
 * Virtio-PCI *modern* maps PCI device 0x1040+type and accepts only
 * 0x1040..0x107f -- i.e. types 0..63. The obvious candidate 0x4E56 ("NV")
 * lies outside that window; measured: the driver silently never binds. 60
 * lies above the ids assigned by virtio 1.3 (~42) and inside the window.
 *
 * THE ONE PLACE. Should virtio-nvrm ever get an official spec id, the
 * number changes here and in crates/vhost-user-nvrm/src/nvrm.rs -- nowhere
 * else.
 */
#define VIRTIO_ID_NVRM 60

/* The shmid under which the host-visible window (the shared-memory
 * region the host places this guest's mappings into) lives. The Cloud
 * Hypervisor patch (patches/0001-generic-vhost-user-shmem.patch) assigns
 * shmids by region-list index; 1 matches the id virtio-gpu uses for its
 * HOST_VISIBLE region. */
#define NVRM_SHM_ID_HOST_VISIBLE 1

/* Majors/minors of the real driver -- NO RM semantics, just the numbers
 * libcuda looks its nodes up under. Cross-checked against the host's
 * /proc/devices: "195 nvidia", "195 nvidiactl", "235 nvidia-uvm". */
#define NV_FRONTEND_MAJOR 195
#define NV_UVM_MAJOR	  235
#define NV_MINOR_CTL	  255
#define NV_MAX_GPUS	  8

/* Pages per pin_user_pages_fast round: bounds the latency and allocation
 * size of a single call without capping the total length. */
#define PIN_CHUNK_PAGES 4096

/* How long a teardown call (MAP_RELEASE from vm_ops->close) waits for the
 * host. It must not wait for a signal there -- the caller cannot handle
 * -ERESTARTSYS -- but it must not hang forever either. After the timeout
 * the buffer passes to the callback. */
#define NVRM_TEARDOWN_TIMEOUT (10 * HZ)

static bool create_nodes = true;
module_param(create_nodes, bool, 0444);
MODULE_PARM_DESC(create_nodes, "Create the NVIDIA nodes (default: yes). Off when nvrm_nodes.ko holds them");

static unsigned int gpu_count = 1;
module_param(gpu_count, uint, 0444);
MODULE_PARM_DESC(gpu_count, "Number of /dev/nvidiaN nodes (default 1)");

/*
 * Full BDF mediation: which PCI address the guest is told the card sits at.
 *
 * With the switch off, enumerate_gpus() and every other answer forward the
 * HOST's, because that is what RM answers: this rig's card sits at
 * 0000:2d:00.0 on the host, and nothing in the guest's own PCI bus is at
 * that address. Everything that then looks the address up -- NVIDIA's X
 * driver reads /sys/bus/pci/devices/<BDF>/config, and libnvidia-glcore
 * carries the same pattern -- looks somewhere empty.
 *
 * A real vGPU guest never sees this: there the mediated function, RM's
 * answer and sysfs all name the GUEST's address, and the guest's view is
 * internally consistent. Setting this to 1 makes ours consistent the same
 * way, by reporting the address of the virtio device that actually mediates
 * the card -- in every answer that carries an address, and in every question
 * that names one.
 *
 * It is one switch and not two because gpuId IS the address
 * (gpuGenerate32BitId(), gpu.c:292):
 *
 *     ((domain & 0xffff) << 16) | (bus << 8) | device
 *
 * Host 0000:2d:00.0 -> 0x2d00, measured. Rewriting the address but not the
 * id (or the other way round) would produce a contradiction that any caller
 * asking both questions can see.
 *
 * WARNING: the guest address is read from the PARENT PCI FUNCTION of this
 * virtio device, never computed from the virtio device type. OASIS derives
 * the PCI DEVICE ID from the type (0x1040 + type, so type 60 -> 0x107c);
 * the ADDRESS is whatever slot the VMM handed out, and cloud-hypervisor
 * hands out the next free one in creation order. Measured on this rig, same
 * device, same type, two runs: 00:07.0 with a virtio-gpu beside it, 00:06.0
 * without. The device id was 1af4:107c both times.
 *
 * OFF by default, and the reason is a measurement, not caution.
 *
 * The host id is LEARNED from the first enumeration answer, and the very
 * first NVML call after the module loads is that answer -- so it is served
 * half mediated and fails. Measured 2026-08-08 on a fresh module:
 *
 *     1. nvidia-smi -L   ->  "No devices found."
 *     2. nvidia-smi -L   ->  GPU 0: Leandro RTX 2070 (UUID: ...)
 *
 * That is what turned the `smi` stage of the GPU gate red. Until the id is
 * learned EAGERLY -- at probe, not from the first reply that needs it --
 * this switch stays off, and the display rig (lea_display_modules,
 * scripts/lib/provision.sh) turns it on where a consistent address actually
 * matters.
 */
static unsigned int bdf_mediation;
module_param(bdf_mediation, uint, 0644);
MODULE_PARM_DESC(bdf_mediation, "report the guest's own PCI address for the card in every RM answer (default 0 = off, see the comment)");

/*
 * Finds what the table misses. With this on, every control reply is scanned
 * for the host's gpuId and the command that carries it is named -- which
 * beats guessing which of ~900 controls quotes an address. Off by default:
 * it reads the whole params buffer of every control.
 */
static unsigned int bdf_debug;
module_param(bdf_debug, uint, 0644);
MODULE_PARM_DESC(bdf_debug, "log every control reply that still carries the host's gpuId (default 0)");

/*
 * The display path, off by default.
 *
 * Everything this gates is work that only a DISPLAY needs -- kernel-path RM
 * operations that NVKMS and nvidia-drm ask for while building a screen. The
 * compute path (CUDA, PyTorch, nvidia-smi) does not reach any of it, and the
 * gate measures the compute path. So the switch exists for one reason: with
 * `display=0` this module behaves EXACTLY as it did before the display work
 * started, which is what makes a regression provable rather than argued.
 *
 * `display=1` is set by the display rig (scripts/lib/provision.sh) and by the display gate.
 */
static unsigned int display;
module_param(display, uint, 0644);
MODULE_PARM_DESC(display, "serve the kernel-path RM operations a display needs (0 = off, 1 = on, 2 = on and verbose)");

/*
 * The virtual display, off by default.
 *
 * NVKMS has a path for a GPU with no connectors of its own -- NVIDIA built it
 * for GRID -- and it asks for exactly three things: how many heads, how large
 * they may be, and an EDID. `DisplaylessProbeValidDisplays` (nvkms-rm.c:402)
 * derives the connector list from the head count alone, with no RM call at
 * all. Measured 2026-08-08.
 *
 * What this switch turns on is an INVENTION: there is no monitor. The
 * module answers `NVA083_GRID_DISPLAYLESS` itself -- nothing about it reaches
 * the host, because there is no host state behind it -- and hands NVKMS an
 * EDID this file writes. That is a different kind of mediation from the BDF
 * or the card name, where a real answer is rewritten; here the answer is
 * made up, and the log says so at load time.
 *
 * The entry point is the class list: `nvRmAllocDisplays` checks
 * NV04_DISPLAY_COMMON FIRST (nvkms-rm.c:1819), and this card has it, so the
 * displayless branch is unreachable until 0x0073 is swapped for 0xa083 in
 * the answer to NV0080_CTRL_CMD_GPU_GET_CLASSLIST. One out, one in --
 * numClasses does not change, so the counting call needs no handling.
 *
 * Needs `display` on as well: the kernel-path ops still have to be served.
 */
static unsigned int vdisplay;
module_param(vdisplay, uint, 0644);
MODULE_PARM_DESC(vdisplay, "present a virtual display to NVKMS via NVA083_GRID_DISPLAYLESS (0 = off, 1 = on). Needs display=1: the kernel-path RM operations a display uses are refused without it");

static unsigned int vdisplay_width = 1920;
module_param(vdisplay_width, uint, 0644);
MODULE_PARM_DESC(vdisplay_width, "width of the virtual display (default 1920)");

static unsigned int vdisplay_height = 1080;
module_param(vdisplay_height, uint, 0644);
MODULE_PARM_DESC(vdisplay_height, "height of the virtual display (default 1080)");

/*
 * The CEILING, which is a different thing from the mode above.
 *
 * NVKMS copies GET_MAX_RESOLUTION straight into pDevEvo->caps
 * (nvkms-rm.c:1409) -- maxWidthInPixels, maxHeight and maxWidthInBytes, which
 * bound every surface it will accept. Answering that call with
 * vdisplay_width/height, as this did, makes the one offered mode also the
 * hard limit: nothing larger can ever be allocated, not even a client's
 * offscreen buffer.
 *
 * The defaults are NVIDIA's own for unlicensed passthrough on Linux
 * (GRID_DISPLAYLESS_LINUX_MAX_HRES/VRES/PIXELS,
 * objgriddisplayless.c:38-39,54). maxPixels is a SEPARATE bound next to the
 * resolution, not derived from it: 4096000 is exactly 2560x1600, so 1080p
 * fits and 4K does not. Whoever raises one raises both.
 */
static unsigned int vdisplay_max_width = 2560;
module_param(vdisplay_max_width, uint, 0644);
MODULE_PARM_DESC(vdisplay_max_width, "maximum width NVKMS may use (default 2560, NVIDIA's Linux displayless limit)");

static unsigned int vdisplay_max_height = 1600;
module_param(vdisplay_max_height, uint, 0644);
MODULE_PARM_DESC(vdisplay_max_height, "maximum height NVKMS may use (default 1600, NVIDIA's Linux displayless limit)");

static unsigned int vdisplay_max_pixels = 4096000;
module_param(vdisplay_max_pixels, uint, 0644);
MODULE_PARM_DESC(vdisplay_max_pixels, "maximum pixel count, a bound of its own next to the resolution (default 4096000 = 2560x1600)");

/* The raster rate of a raster generator that does not exist. The EDID this
 * module invents advertises 60 Hz, and the callbacks served from it (see the
 * vblank section) fire at this rate. Writable for experiments. */
static unsigned int vdisplay_vblank_hz = 60;
module_param(vdisplay_vblank_hz, uint, 0644);
MODULE_PARM_DESC(vdisplay_vblank_hz, "rate of the virtual display's vblank callbacks (default 60)");

static unsigned long stat_vblank_fired;
module_param(stat_vblank_fired, ulong, 0444);
MODULE_PARM_DESC(stat_vblank_fired, "vblank callback invocations served from the virtual display");

/* The event return channel (queue 1, KIND_EVENT_FIRED). Three counters, so
 * that "26 registered, 0 delivered" -- the measured GNOME state before this
 * channel existed -- has a reader on the guest side too:
 *   registered  0x7e slots filled (kernel callbacks NVKMS asked for)
 *   delivered   0x79 wake-ups + 0x7e callback invocations
 *   dropped     ring full, unknown fd, no slot, kc mismatch, denylisted
 *               notifier, class 0x78/unknown
 * Under GNOME with `vkprobe --present` running, `delivered` must tick. */
static unsigned long stat_events_delivered;
module_param(stat_events_delivered, ulong, 0444);
MODULE_PARM_DESC(stat_events_delivered, "host events handed on: fd wake-ups plus kernel-callback invocations");

static unsigned long stat_events_dropped;
module_param(stat_events_dropped, ulong, 0444);
MODULE_PARM_DESC(stat_events_dropped, "host events with nowhere to go, all reasons (see the four below)");
/* The four reasons apart -- 144k "dropped" in half an hour of CS2 read
 * like a leak until the split showed them to be the host monitor's DP_IRQ
 * at 60 Hz, filtered on purpose (2026-08-15). */
static unsigned long stat_events_drop_ringfull;
module_param(stat_events_drop_ringfull, ulong, 0444);
MODULE_PARM_DESC(stat_events_drop_ringfull, "dropped: guest ring full before the workqueue drained it -- the one that costs frames");
static unsigned long stat_events_drop_filtered;
module_param(stat_events_drop_filtered, ulong, 0444);
MODULE_PARM_DESC(stat_events_drop_filtered, "dropped: DP_IRQ/HDMI/LPWR notifiers of the HOST display, filtered on purpose");
static unsigned long stat_events_drop_noslot;
module_param(stat_events_drop_noslot, ulong, 0444);
MODULE_PARM_DESC(stat_events_drop_noslot, "dropped: no callback slot for (client, hEvent) -- registration missed or freed");
static unsigned long stat_events_drop_class;
module_param(stat_events_drop_class, ulong, 0444);
MODULE_PARM_DESC(stat_events_drop_class, "dropped: class 0x78 or unknown -- not callable in the guest");

static unsigned long stat_events_registered;
module_param(stat_events_registered, ulong, 0444);
MODULE_PARM_DESC(stat_events_registered, "kernel-callback events (0x7e) this module holds a slot for");

/*
 * How many device nodes this guest currently holds open, as the module sees
 * them -- one per struct file, which is what the host mirrors one for one.
 *
 * The number the host could not check itself. The session behind
 * OPEN-QUESTIONS 31 measured 2003 open nvidiactl FDs in the backend while
 * the guest showed 48 in its per-process fd directories (the question
 * records the 2003; the 48 was read alongside), and the two are not
 * comparable: a struct file
 * outlives its FD for as long as a mapping references it, and the mirror is
 * keyed on the FILE. Without this counter "the host leaks" and "the guest
 * still holds them" look exactly alike from the host, and two nights of
 * guessing went into that gap.
 */
static unsigned long stat_ctx_open;
module_param(stat_ctx_open, ulong, 0444);
MODULE_PARM_DESC(stat_ctx_open, "device-node contexts (struct file) currently open");
static unsigned long stat_ctx_opened;
module_param(stat_ctx_opened, ulong, 0444);
MODULE_PARM_DESC(stat_ctx_opened, "device-node contexts ever opened");
static unsigned long stat_ctx_closed;
module_param(stat_ctx_closed, ulong, 0444);
MODULE_PARM_DESC(stat_ctx_closed, "device-node contexts ever released (KIND_CLOSE sent)");

/* A semaphore surface (NV_SEMAPHORE_SURFACE) is the object nvidia-drm hangs
 * its fences off; its WAITERS (control 0xda0003) are the SECOND way NVKMS
 * hands out a kernel callback pointer, and the first user is a Wayland
 * compositor: nvidia-drm's semsurf fences never signalled, weston hung in
 * gbm_surface_lock_front_buffer polling a sync_file forever (2026-08-16).
 * `waiters` must go back to its idle count after a compositor exits;
 * `fired` must tick once per frame while one runs. */
static unsigned long stat_semsurf_waiters;
module_param(stat_semsurf_waiters, ulong, 0444);
MODULE_PARM_DESC(stat_semsurf_waiters, "semaphore-surface waiter slots currently armed (control 0xda0003)");
/* Unregisters RM refused because the waiter had already fired, i.e. the
 * races that semsurf_after_control() now retires instead of leaving armed.
 * Each one is a use-after-free that did NOT happen. */
static unsigned long stat_semsurf_late_unreg;
module_param(stat_semsurf_late_unreg, ulong, 0444);
MODULE_PARM_DESC(stat_semsurf_late_unreg, "unregisters that lost the race with the firing (slot retired anyway)");
static unsigned long stat_semsurf_fired;
module_param(stat_semsurf_fired, ulong, 0444);
MODULE_PARM_DESC(stat_semsurf_fired, "semaphore-surface waiter callbacks invoked");

static unsigned int max_pin_mib = 1024;
module_param(max_pin_mib, uint, 0644);
MODULE_PARM_DESC(max_pin_mib, "Upper bound on concurrently pinned guest memory in MiB (default 1024)");

/* Read-only observability -- /sys/module/virtio_nvrm/parameters/. Without
 * them, "the pin path fired" would be a guess instead of a measurement,
 * and a leak would show only as missing memory. */
static unsigned long stat_pinned_kib;
module_param(stat_pinned_kib, ulong, 0444);
MODULE_PARM_DESC(stat_pinned_kib, "Guest memory currently pinned, in KiB");

static unsigned long stat_osdesc_pins;
module_param(stat_osdesc_pins, ulong, 0444);
MODULE_PARM_DESC(stat_osdesc_pins, "OS descriptors whose pages were resolved here");

static unsigned long stat_pool_pages;
module_param(stat_pool_pages, ulong, 0444);
MODULE_PARM_DESC(stat_pool_pages, "Pages owned by this module for UVM pools");

#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 11, 0)
#define nvrm_fd_file(f) ((f).file)
#else
#define nvrm_fd_file(f) fd_file(f)
#endif

/* ------------------------------------------------------------------ *
 * Tables: parsed, not trusted
 *
 * Parser and lookup functions live in nvrm_tables.c -- included as one
 * translation unit so that test/tabcheck.c can run the SAME code in
 * userspace against the stream from `nvrm-genhdr --dump-tables`. After
 * changing anything here, run test.sh check (step c-interpreter) -- the diff
 * test compares the C reader's view field by field with the Rust
 * writer's view.
 * ------------------------------------------------------------------ */

#include "nvrm_tables.c"

/* ------------------------------------------------------------------ *
 * Device
 * ------------------------------------------------------------------ */

/* Receive buffers pre-posted on the event queue. The host writes ONE
 * `struct nvrm_req` per firing (KIND_EVENT_FIRED) and never waits: no free
 * buffer on its side means the event is dropped and counted there. */
#define NVRM_EVQ_BUFS 256u
/* Firings taken out of the queue but not yet handed on. Power of two.
 * Every drop is a wait that falls back to its 10 ms poll.
 *
 * 128 overflowed under CS2: 106k ring-full drops in one deathmatch
 * (2026-08-15) while the workqueue drained.
 * 1024 holds the RUNNING desktop -- including a live Moonlight stream
 * at 34,500 events/s, 3.87 M events in a burst test and 1.55 M during a
 * deathmatch, all with drop_ringfull at 0. What it does NOT hold is the
 * SESSION START: sampling the counter every 2 s across a fresh
 * desktop `up` put every drop in one 26-second window ~2 minutes after
 * boot (gdm handing the session over, and the X restart), ~10 000 of them while
 * ~19 000 events/s arrived and the workqueue competed with X and GNOME
 * coming up. ~2 % overflowed; the ring was close, not hopeless.
 * 8192 x 160 B = 1.25 MiB, once per guest, to absorb 8x that burst. */
#define NVRM_EV_RING 8192u

struct nvrm_dev {
	struct virtio_device *vdev;
	struct virtqueue *vq;
	/* Protects the virtqueue AND every request's done/abandoned fields.
	 * Spinlock, because the callback arrives from interrupt context. */
	spinlock_t vq_lock;

	/* Queue 1, host -> guest: RM events (see the event section). NULL when
	 * the device offers only one queue -- then poll() never wakes and the
	 * kernel callbacks never fire, exactly the state before this channel. */
	struct virtqueue *evq;
	/* Protects `evq` AND the ring below. NOT vq_lock: that one belongs to
	 * queue 0 and to requests in flight; the two queues have nothing to
	 * say to each other. Taken from the vq callback (IRQ) with irqsave and
	 * from the work item (process) the same way. */
	spinlock_t evq_lock;
	struct nvrm_req ev_ring[NVRM_EV_RING];
	unsigned int ev_head, ev_tail;	/* under evq_lock; tail - head = filled */
	/* Hands the ring on in process context. Nothing is CALLED from the
	 * IRQ callback: neither NVKMS' callbacks nor a wait-queue lookup. */
	struct work_struct events_work;
	/* (proc id << 32 | token) -> nvrm_ctx, USER nodes only. Answers "which
	 * fd does this EVENT_FIRED mean" for the 0x79 wake-ups. Tokens are
	 * per session (the host's Mirror starts at 1 for every session), so
	 * the token alone would collide between processes -- hence the
	 * unsigned-long key with the process id in the upper half, and an
	 * XArray rather than the int-keyed IDR. */
	struct xarray ctx_xa;
	/* Waiters for a free descriptor slot. */
	wait_queue_head_t vq_space;
	atomic_t seq;
	/* Requests in flight -- remove() waits for them. */
	atomic_t inflight;
	wait_queue_head_t drain;

	/* Host-visible window: guest-physical memory, filled by the host. */
	u64 win_base;
	u64 win_len;
	unsigned long *win_bitmap;	/* one page per bit */
	struct mutex win_lock;

	struct nvrm_tables tbl;

	/* Guest processes that have this device open. The dense id from the
	 * IDR travels in every req; the host keeps one session per id. */
	struct idr proc_idr;
	struct list_head procs;
	struct mutex proc_lock;

	/* BDF mediation -- see the bdf_mediation parameter.
	 *
	 * `host_id` is LEARNED, not configured: the first gpuId RM names in an
	 * answer is the host's, and it encodes the host address. A second,
	 * different one means more than one GPU, and one virtio device cannot
	 * stand for two addresses without inventing one -- so mediation turns
	 * itself OFF there rather than lie. That boundary is deliberate.
	 */
	u32 bdf_guest_id;	/* 0 = no PCI parent, mediation impossible */
	u32 bdf_host_id;	/* 0 = not learned yet */
	bool bdf_disabled;	/* more than one GPU seen */
	u16 bdf_domain;
	u8 bdf_bus, bdf_slot, bdf_func;
};

/*
 * A guest process the way the host gets to see it.
 *
 * Why an OWN, dense id instead of the tgid: Linux hands out PIDs again after
 * a process ends. A recycled number would attribute a dead process's
 * allocations to a new one -- and since RM anchors its USERD (a
 * channel's user-mode doorbell page) separation on
 * that (kernel_fifo.c:508-511), the result would be worse than one wrong row
 * in a table. The IDR id becomes free once the host has torn the session down
 * (KIND_PROC_GONE), and never before.
 *
 * The key is the `struct pid *` of the THREAD GROUP, not the number: the same
 * process gets ONE entry for all of its nodes (ctl, gpu, uvm) -- it has to
 * share them, its RM handles live in ONE session. `vnr` is the number as seen
 * from inside the guest (pid_vnr), so the display stays correct when
 * containers run in the guest.
 */
struct nvrm_proc {
	struct list_head node;
	struct pid *pid;
	u32 id;
	u32 vnr;
	char comm[TASK_COMM_LEN];
	refcount_t ref;
};

/* Exactly one device. Several would mean several sets of /dev/nvidia* --
 * that is not an extension path, it is a mix-up. */
static struct nvrm_dev *nvrm;

/*
 * Guest memory this module holds down -- pinned application pages AND
 * self-owned pool pages, across all contexts. ONE account, ONE knob: that
 * way the error message can name both the limit and the parameter that
 * changes it, and nobody has to add two numbers in their head.
 */
static atomic_long_t held_pages = ATOMIC_LONG_INIT(0);

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

/* ------------------------------------------------------------------ *
 * One round trip over the virtqueue
 * ------------------------------------------------------------------ */

struct nvrm_xfer {
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
	kfree(x);
}

/* How many scatterlist entries a buffer of this size needs at most. kvmalloc
 * may fall back to vmalloc -- then the buffer is split page by page. */
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
	/* n == 0 means the vmalloc branch ran with len == 0: the loop body
	 * never executed and sg_mark_end(&sg[n - 1]) would set the end bit on
	 * the entry BEFORE the table.
	 *
	 * Unreachable by construction today, and this line says so rather than
	 * pretending otherwise: nvrm_xfer_alloc refuses a cap below
	 * sizeof(struct nvrm_req)/sizeof(struct nvrm_rsp), and nvrm_xfer_run
	 * refuses a req_len below the header, so both callers pass len > 0. It
	 * becomes effective the moment a third caller builds a list over a
	 * buffer whose length it has not bounded -- which is the change a
	 * refactor makes. sg_init_table has already terminated the table, so
	 * returning 0 leaves a well-formed empty list and the WARN says who
	 * did it.
	 */
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
	if (req_cap < sizeof(struct nvrm_req) || rsp_cap < sizeof(struct nvrm_rsp))
		return ERR_PTR(-EINVAL);

	x = kzalloc(sizeof(*x), GFP_KERNEL);
	if (!x)
		return ERR_PTR(-ENOMEM);
	init_waitqueue_head(&x->wq);
	x->req_cap = req_cap;
	x->rsp_cap = rsp_cap;
	x->req = kvzalloc(req_cap, GFP_KERNEL);
	x->rsp = kvzalloc(rsp_cap, GFP_KERNEL);
	x->sg_req = kmalloc_array(nvrm_sg_max(req_cap), sizeof(*x->sg_req), GFP_KERNEL);
	x->sg_rsp = kmalloc_array(nvrm_sg_max(rsp_cap), sizeof(*x->sg_rsp), GFP_KERNEL);
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
		x->done = true;
		if (x->abandoned) {
			/* The waiter left on a signal and handed the buffer
			 * over -- only NOW may it be freed, the device has
			 * just stopped writing into it. */
			spin_unlock_irqrestore(&dev->vq_lock, flags);
			nvrm_xfer_free(x);
			atomic_dec(&dev->inflight);
			spin_lock_irqsave(&dev->vq_lock, flags);
		} else {
			wake_up(&x->wq);
			atomic_dec(&dev->inflight);
		}
		woke = true;
	}
	spin_unlock_irqrestore(&dev->vq_lock, flags);
	if (woke) {
		wake_up(&dev->vq_space);
		wake_up(&dev->drain);
	}
}

/* Defined in the event section, below the vblank engine it is modelled on. */
static void nvrm_events_work(struct work_struct *work);

/*
 * Queue 1 callback: the host has written KIND_EVENT_FIRED requests into the
 * inbufs this module posted. IRQ context, under evq_lock.
 *
 * Only two things happen here: the firing is copied into the ring, and the
 * SAME buffer goes straight back onto the queue (GFP_ATOMIC -- nothing is
 * allocated, the sg entry describes memory the driver already owns). Every
 * consequence -- waking an fd, calling into nvidia-modeset.ko -- is left to
 * events_work: a wait-queue lookup wants the XArray lock, and NVKMS' callback
 * is a foreign function that this module must not run with interrupts off
 * and a virtqueue lock held. The ring, not the queue, is what absorbs a
 * burst; when it is full the firing is dropped and counted, never waited
 * for -- the host is not waiting either.
 */
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
				dev->ev_ring[dev->ev_tail & (NVRM_EV_RING - 1)] = *r;
				dev->ev_tail++;
				queued = true;
			} else {
				stat_events_dropped++;
				stat_events_drop_ringfull++;
			}
		}
		/* Re-post the very buffer, whatever it carried. A buffer that
		 * cannot be re-posted (queue torn down underneath us) is
		 * freed here rather than leaked -- the detach path in remove
		 * only sees buffers the queue still holds. */
		sg_init_one(&sg, buf, sizeof(struct nvrm_req));
		if (virtqueue_add_inbuf(vq, &sg, 1, buf, GFP_ATOMIC) < 0)
			kfree(buf);
	}
	virtqueue_kick(vq);
	spin_unlock_irqrestore(&dev->evq_lock, flags);

	if (queued)
		queue_work(system_highpri_wq, &dev->events_work);
}

/*
 * One round trip: queue the request, kick, wait for the reply.
 *
 * `interruptible`: in the ioctl path YES -- a process must stay killable even
 * when the host stays silent (this is the spot where a guest would otherwise
 * hang unkillably). In the teardown path (vm_ops->close) NO, there is nobody
 * there who could act on -ERESTARTSYS; a timeout takes its place.
 *
 * WARNING, ownership: if this function returns -ERESTARTSYS or -ETIMEDOUT,
 * `x` belongs to the callback from that moment on. The caller must neither
 * touch nor free it -- the device may still be writing into it.
 */
static int nvrm_xfer_run(struct nvrm_dev *dev, struct nvrm_xfer *x, bool interruptible)
{
	struct scatterlist *sgs[2];
	unsigned long flags;
	int err;

	if (x->req_len < sizeof(struct nvrm_req) || x->req_len > x->req_cap)
		return -EINVAL;

	x->n_req = nvrm_sg_fill(x->sg_req, nvrm_sg_max(x->req_cap), x->req, x->req_len);
	x->n_rsp = nvrm_sg_fill(x->sg_rsp, nvrm_sg_max(x->rsp_cap), x->rsp, x->rsp_cap);
	sgs[0] = x->sg_req;
	sgs[1] = x->sg_rsp;

	for (;;) {
		spin_lock_irqsave(&dev->vq_lock, flags);
		err = virtqueue_add_sgs(dev->vq, sgs, 1, 1, x, GFP_ATOMIC);
		if (!err) {
			atomic_inc(&dev->inflight);
			virtqueue_kick(dev->vq);
		}
		spin_unlock_irqrestore(&dev->vq_lock, flags);
		if (err != -ENOSPC)
			break;
		/* Queue full: wait until a slot frees up. */
		if (interruptible) {
			if (wait_event_interruptible(dev->vq_space,
						     dev->vq->num_free > 0))
				return -ERESTARTSYS;
		} else if (!wait_event_timeout(dev->vq_space, dev->vq->num_free > 0,
					       NVRM_TEARDOWN_TIMEOUT)) {
			return -ETIMEDOUT;
		}
	}
	if (err)
		return err;

	if (interruptible) {
		/*
		 * KILLABLE, not interruptible, and the difference is a bug.
		 *
		 * By this line the request is ALREADY on the virtqueue and
		 * kicked, so the host may have carried it out. Waiting
		 * interruptibly and answering -ERESTARTSYS hands the decision
		 * to the kernel, which re-runs the WHOLE ioctl -- issuing a
		 * second time an operation that has already taken effect on
		 * the host. For a read that is merely wasteful. For a
		 * one-shot escape it is wrong: NV_ESC_ATTACH_GPUS_TO_FD is
		 * refused with EINVAL once the FD carries GPUs (nv.c,
		 * `nvlfp->num_attached_gpus != 0`), so the restart is told
		 * "invalid" for an attach that SUCCEEDED, and the caller
		 * builds its GL state believing the FD has no GPU.
		 *
		 * Measured 2026-08-20 with `strace -f` on the compositor's
		 * Xwayland under 30 concurrent GL clients:
		 *
		 *   ioctl(142, ...0x46,0xd4...) = ? ERESTARTSYS
		 *   ioctl(142, ...0x46,0xd4...) = -1 EINVAL
		 *
		 * -- the same FD, twice, the second one the kernel's restart.
		 * The backend saw the pair as one token attached twice: 118
		 * of 118 failures in that session, none of them a fresh
		 * token, so nothing was RM refusing an id. Under serial load
		 * the signal pressure is absent and it never fires, which is
		 * why this hid behind "2 in 1_392_006 calls" for so long.
		 * See OPEN-QUESTIONS 45.
		 *
		 * Killable keeps the escape hatch that matters: a wedged host
		 * must not leave an unkillable task. A fatal signal still
		 * returns here, and there is no restart to fear then because
		 * the task is dying. Ordinary signals no longer re-issue.
		 *
		 * The queue-full wait above stays interruptible on purpose:
		 * nothing has been submitted at that point, so a restart
		 * re-issues nothing.
		 */
		if (wait_event_killable(x->wq, READ_ONCE(x->done))) {
			/* Signal. The buffer stays with the device -- it must
			 * not be freed here, so ownership is handed over. */
			spin_lock_irqsave(&dev->vq_lock, flags);
			if (!x->done) {
				x->abandoned = true;
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
			spin_unlock_irqrestore(&dev->vq_lock, flags);
			pr_warn("virtio_nvrm: host does not answer -- teardown request abandoned\n");
			return -ETIMEDOUT;
		}
		spin_unlock_irqrestore(&dev->vq_lock, flags);
	}

	if (x->rsp_len < sizeof(struct nvrm_rsp)) {
		pr_warn_ratelimited("virtio_nvrm: reply of %u bytes is too short\n", x->rsp_len);
		return -EIO;
	}
	return 0;
}

/* Prepare a request header. `guest_proc` is the dense id of the guest process
 * doing the talking (0 = device-wide request without an owner, e.g. HELLO or
 * GET_TABLES). */
static struct nvrm_req *nvrm_req_init(struct nvrm_dev *dev, struct nvrm_xfer *x, u32 kind,
				      u32 guest_proc)
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
static int nvrm_simple_info(struct nvrm_dev *dev, u32 kind, u32 dev_tag, u32 ioctl_nr,
			    u64 target_token, u64 addr, u64 map_len, u64 *token_out,
			    bool interruptible, u32 guest_proc,
			    const struct nvrm_proc_info *info)
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

	ret = nvrm_xfer_run(dev, x, interruptible);
	if (ret)
		return ret;	/* on -ERESTARTSYS/-ETIMEDOUT x belongs to the callback */

	rsp = x->rsp;
	ret = rsp->ret;
	if (token_out)
		*token_out = rsp->token;
	nvrm_xfer_free(x);
	return ret;
}

/* A small request without payload; the reply is the header alone. */
static int nvrm_simple(struct nvrm_dev *dev, u32 kind, u32 dev_tag, u32 ioctl_nr,
		       u64 target_token, u64 addr, u64 map_len, u64 *token_out,
		       bool interruptible, u32 guest_proc)
{
	return nvrm_simple_info(dev, kind, dev_tag, ioctl_nr, target_token, addr,
				map_len, token_out, interruptible, guest_proc, NULL);
}

/* ------------------------------------------------------------------ *
 * Fetch and parse the tables
 * ------------------------------------------------------------------ */

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

		ret = nvrm_xfer_run(dev, x, false);
		if (ret)
			goto fail;	/* x may now belong to the callback */
		rsp = x->rsp;
		if (rsp->ret != 0) {
			pr_err("virtio_nvrm: GET_TABLES rejected: %d\n", rsp->ret);
			ret = -EIO;
			nvrm_xfer_free(x);
			goto fail;
		}
		if (!total) {
			total = rsp->token;
			if (total < sizeof(struct nvrm_table_hdr) || total > SZ_1M) {
				pr_err("virtio_nvrm: implausible table length %llu\n", total);
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

	/* From here on the stream is checked, not believed -- in nvrm_tables.c,
	 * so the test binary can run the same checks. */
	{
		const char *why;

		ret = nvrm_tables_parse(t, &why);
		if (ret) {
			pr_err("virtio_nvrm: tables rejected: %s\n", why);
			goto fail;
		}
	}

	pr_info("virtio_nvrm: tables v%u accepted -- %zu bytes, checksum %#010x (%u ioctls, %u classes, %u controls, %u nested)\n",
		t->hdr.table_version, t->len, t->hdr.checksum,
		t->hdr.n_ioctl, t->hdr.n_class, t->hdr.n_ctrl, t->hdr.n_nested);
	return 0;

fail:
	kvfree(t->blob);
	memset(t, 0, sizeof(*t));
	return ret;
}

/* ------------------------------------------------------------------ *
 * The window: the guest manages the space, the host fills it
 * ------------------------------------------------------------------ */

static long win_alloc(struct nvrm_dev *dev, size_t len)
{
	unsigned long npages = len >> PAGE_SHIFT;
	unsigned long total = dev->win_len >> PAGE_SHIFT;
	unsigned long start;

	if (!dev->win_bitmap || !npages)
		return -ENOMEM;
	mutex_lock(&dev->win_lock);
	start = bitmap_find_next_zero_area(dev->win_bitmap, total, 0, npages, 0);
	if (start >= total) {
		unsigned long used = bitmap_weight(dev->win_bitmap, total);

		mutex_unlock(&dev->win_lock);
		/* Refusal ledger: this one was SILENT and cost CS2 its life
		 * (2026-08-15: 938 MiB in a 1 GiB window, then a 32 MiB ask). */
		pr_warn_ratelimited("virtio_nvrm: window full: %s[%d] asked %zu KiB, %lu of %lu MiB in use -- ENOSPC\n",
				    current->comm, task_pid_nr(current), len >> 10,
				    (used << PAGE_SHIFT) >> 20, (total << PAGE_SHIFT) >> 20);
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

/* ------------------------------------------------------------------ *
 * Context: one per struct file
 *
 * The host mirrors every open device fd and answers with a TOKEN -- its id
 * for that file, minted per session, and the only name this module ever uses
 * for it afterwards. Which session is the guest_proc id (struct nvrm_proc
 * above): this module's own dense per-process number, not a pid.
 * ------------------------------------------------------------------ */

/* One pinned memory range behind an OS descriptor
 * (NV01_MEMORY_SYSTEM_OS_DESCRIPTOR: memory RM pins rather than copies).
 *
 * Keyed on the hMemory handle, NOT "everything lives until close()": RM holds
 * the pages only until the guest frees the object, and a training run creates
 * and drops thousands of them. Whatever is left falls in release() at the
 * latest.
 */
struct nvrm_pin {
	struct list_head node;
	u32 handle;
	struct page **pages;
	unsigned long npages;
	/* A PRIME import -- a dma-buf handed over from another DRM device --
	 * BORROWS its pages from the exporter: they were never
	 * pinned by us, and unpinning them would decrement somebody else's
	 * reference. The attachment is what has to be given back instead. */
	struct dma_buf *dmabuf;
	struct dma_buf_attachment *attach;
	struct sg_table *sgt;
};

struct nvrm_ctx {
	struct nvrm_dev *dev;
	/* Who owns this FD -- the process that OPENED it. An inherited FD, or
	 * one passed along via SCM_RIGHTS, keeps that owner: the RM handles
	 * behind it live in that session, not in the user's. Same rule as for
	 * the open file description. */
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

	/* The event return channel, per fd. nvrm_node_poll sleeps on the
	 * wait queue; the flag is what the host's "readable" (an EVENT_FIRED
	 * of class NV01_EVENT_OS_EVENT for this token) sets, and what poll
	 * clears -- see nvrm_node_poll for why poll clears it. */
	wait_queue_head_t events_wq;
	atomic_t events_pending;
	/* In dev->ctx_xa. User nodes only; the NVKMS session (kapi_ctx_open)
	 * is never indexed -- its events go through the callback slots. */
	bool indexed;
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

/*
 * FD translation via the IDENTITY of the open file, not via the number:
 * fdget() yields the struct file, and f_op decides whether it is one of this
 * module's. That makes dup(), fork() and O_CLOEXEC irrelevant -- the number
 * may change freely, the token hangs off the open file description.
 *
 * And a token is only half an answer.
 *
 * The host mints tokens PER SESSION and every guest process has its own, so
 * the same number means different files in different sessions. Whoever sends
 * a token onwards has to send whose it is -- and this is the only place that
 * can say: the owner is readable from the ctx behind the file, and only while
 * fdget() still holds it. There is no token -> proc map to ask afterwards.
 *
 * `proc_out` may be NULL for the escape-level fd field, which names the
 * caller's own session by construction and needs no owner.
 */
static int nvrm_token_of_fd(int n, u64 *tok, u32 *proc_out)
{
	struct fd f = fdget(n);
	struct file *file = nvrm_fd_file(f);
	int ret = -EBADF;

	if (file && file->f_op == &nvrm_node_fops) {
		struct nvrm_ctx *c = file->private_data;

		if (c) {
			*tok = c->token;
			/* ctx->proc is assigned once at open and released only
			 * at release, and the fdget reference keeps the file --
			 * hence the ctx, hence the proc -- alive here. No
			 * proc_lock needed. */
			if (proc_out)
				*proc_out = c->proc ? c->proc->id : 0;
			ret = 0;
		}
	}
	fdput(f);
	return ret;
}

/* ------------------------------------------------------------------ *
 * Pinning with accounting
 * ------------------------------------------------------------------ */

static void nvrm_unpin(struct nvrm_pin *p)
{
	if (p->attach) {
		/* BORROWED pages (a PRIME import). They were never pinned by
		 * us and are not ours to charge, dirty or unpin -- giving the
		 * attachment back is the whole release. */
		dma_buf_unmap_attachment_unlocked(p->attach, p->sgt,
						  DMA_BIDIRECTIONAL);
		dma_buf_detach(p->dmabuf, p->attach);
		kvfree(p->pages);
		kfree(p);
		return;
	}
	/* dirty_lock(..., true): the pin used FOLL_WRITE, the GPU may have
	 * written into these pages. */
	unpin_user_pages_dirty_lock(p->pages, p->npages, true);
	nvrm_uncharge(p->npages);
	stat_pinned_kib -= p->npages << (PAGE_SHIFT - 10);
	kvfree(p->pages);
	kfree(p);
}

/*
 * The same GpaRun wire, filled from an sg_table instead of from a user VA.
 *
 * This is the PRIME import: the DISPLAY device allocated the buffer and the
 * RENDER device imports it, which is how every Optimus laptop works and what
 * NVKMS asks for with NVOS32_DESCRIPTOR_TYPE_OS_DMA_BUF_PTR. The pages are
 * guest RAM the host already has mapped, so nothing new is invented -- only
 * the source of the page list differs.
 *
 * `dev` must be a device that can do DMA. A struct virtio_device cannot;
 * its PCI parent can. See os_device_ptr in kapi_enumerate_gpus().
 */
static struct nvrm_pin *nvrm_pin_dmabuf(struct dma_buf *dmabuf, struct device *dev)
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
	if (va + len < va)	/* overflow */
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

	/* FOLL_WRITE breaks COW (otherwise a fresh anonymous page still points
	 * at the shared zero page), FOLL_LONGTERM prevents later migration --
	 * which is exactly what mlock does not do. */
	while (done < npages) {
		unsigned long want = min_t(unsigned long, PIN_CHUNK_PAGES, npages - done);
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

/* ------------------------------------------------------------------ *
 * open / release
 * ------------------------------------------------------------------ */

/* Which node is this? From (major, minor) -- the only place where the real
 * driver's numbers matter. */
static int node_dev_tag(unsigned int major, unsigned int minor, u32 *tag, u32 *idx)
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

/* Get this guest process's entry -- share an existing one or create a new
 * one. Returns with a reference held. */
static struct nvrm_proc *nvrm_proc_get(struct nvrm_dev *dev)
{
	struct pid *pid = get_task_pid(current, PIDTYPE_TGID);
	struct nvrm_proc *p, *fresh = NULL;
	int id;

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
	/* Starts at 1: id 0 stays reserved for "not specified" (device-wide
	 * requests that have no owning process). */
	id = idr_alloc(&dev->proc_idr, fresh, 1, 0, GFP_KERNEL);
	if (id < 0) {
		mutex_unlock(&dev->proc_lock);
		kfree(fresh);
		put_pid(pid);
		return ERR_PTR(id);
	}
	fresh->pid = pid;		/* the reference passes to the entry */
	fresh->id = (u32)id;
	fresh->vnr = (u32)pid_vnr(pid);
	get_task_comm(fresh->comm, current);
	refcount_set(&fresh->ref, 1);
	list_add(&fresh->node, &dev->procs);
	mutex_unlock(&dev->proc_lock);
	return fresh;
}

/* Drop a reference. When the last one falls the guest process is done with
 * the GPU: the host may tear its session down, and only THEN does the id
 * become free again.
 *
 * The ORDER is the point. Until 2026-08-18 the id left the IDR before
 * PROC_GONE went out, and idr_alloc hands out the lowest free id: a process
 * opening a node in that window got the very id whose session the host was
 * about to tear down, and its OPEN could land in that session -- every later
 * ioctl on the new fd then answered EBADF. So: off the list under the lock
 * (a new open by the same pid gets a fresh entry), the round trip to the
 * host with the id still allocated, and only then the id back to the IDR. */
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

	/* Outside the lock: this message waits for the host. Not interruptible
	 * -- this path is also taken on SIGKILL. */
	nvrm_simple(dev, NVRM_KIND_PROC_GONE, 0, 0, 0, 0, 0, NULL, false, id);

	mutex_lock(&dev->proc_lock);
	idr_remove(&dev->proc_idr, id);
	mutex_unlock(&dev->proc_lock);
	put_pid(p->pid);
	kfree(p);
}

static int nvrm_node_open(struct inode *inode, struct file *filp)
{
	struct nvrm_dev *dev = nvrm;
	struct nvrm_ctx *ctx;
	struct nvrm_proc_info info;
	u64 token = 0;
	int ret;

	if (!dev || !dev->tbl.blob)
		return -ENODEV;

	ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);
	if (!ctx)
		return -ENOMEM;
	mutex_init(&ctx->lock);
	spin_lock_init(&ctx->pin_lock);
	INIT_LIST_HEAD(&ctx->pins);
	init_waitqueue_head(&ctx->events_wq);
	atomic_set(&ctx->events_pending, 0);
	ctx->dev = dev;

	ret = node_dev_tag(imajor(inode), iminor(inode), &ctx->dev_tag, &ctx->gpu_index);
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
	memcpy(info.comm, ctx->proc->comm, min(sizeof(info.comm), sizeof(ctx->proc->comm)));
	info.comm[sizeof(info.comm) - 1] = '\0';

	ret = nvrm_simple_info(dev, NVRM_KIND_OPEN, ctx->dev_tag, ctx->gpu_index,
			       0, 0, 0, &token, true, ctx->proc->id, &info);
	if (ret < 0)
		goto err;
	ctx->token = token;

	/* Into the token index, so that a host wake-up can find this fd. A
	 * failure here is not a failed open: the fd works, it just never
	 * wakes -- the pre-channel behaviour, said out loud. */
	{
		unsigned long key;

		if (!nvrm_ctx_key(ctx->proc->id, token, &key)) {
			pr_warn_once("virtio_nvrm: token %llu does not fit the event index -- fd will not wake\n",
				     (unsigned long long)token);
		} else if (xa_err(xa_store(&dev->ctx_xa, key, ctx, GFP_KERNEL))) {
			pr_warn_ratelimited("virtio_nvrm: event index full for proc %u token %llu -- fd will not wake\n",
					    ctx->proc->id, (unsigned long long)token);
		} else {
			ctx->indexed = true;
		}
	}
	filp->private_data = ctx;
	stat_ctx_opened++;
	stat_ctx_open++;
	return 0;

err:
	if (ctx->proc)
		nvrm_proc_put(dev, ctx->proc);
	mutex_destroy(&ctx->lock);
	kfree(ctx);
	return ret;
}

static int nvrm_node_release(struct inode *inode, struct file *filp)
{
	struct nvrm_ctx *ctx = filp->private_data;
	struct nvrm_pin *p, *tmp;

	if (!ctx)
		return 0;

	/* Out of the event index FIRST, under the XArray's own lock -- the
	 * same lock events_work holds while it dereferences what it found.
	 * After xa_erase returns, no wake-up can reach this ctx any more, and
	 * the kfree at the bottom is safe against a firing that is still in
	 * the ring. */
	if (ctx->indexed) {
		unsigned long key;

		if (nvrm_ctx_key(ctx->proc->id, ctx->token, &key))
			xa_erase(&ctx->dev->ctx_xa, key);
		ctx->indexed = false;
	}

	/* Own pages first, then the host. This path is taken on SIGKILL just
	 * the same -- which is why EVERYTHING the context owns hangs off here,
	 * and nothing off a cleanup ioctl that nobody calls. */
	list_for_each_entry_safe(p, tmp, &ctx->pins, node) {
		list_del(&p->node);
		nvrm_unpin(p);
	}
	/* Not interruptible: the caller may already be dying, an -ERESTARTSYS
	 * would have no recipient here. */
	nvrm_simple(ctx->dev, NVRM_KIND_CLOSE, ctx->dev_tag, 0, ctx->token, 0, 0,
		    NULL, false, ctx->proc ? ctx->proc->id : 0);
	stat_ctx_closed++;
	if (stat_ctx_open)
		stat_ctx_open--;
	/* Node first, process second: if the last reference falls here, a
	 * PROC_GONE follows -- and the host tears down a session whose tokens
	 * are already closed. */
	nvrm_proc_put(ctx->dev, ctx->proc);
	mutex_destroy(&ctx->lock);
	kfree(ctx);
	return 0;
}

/*
 * poll: readable when the host said so.
 *
 * Without .poll the VFS would return DEFAULT_POLLMASK -- "always readable" --
 * and libcuda would spin in its event loop (it polls eight event FDs, 193
 * calls in a measured CUDA start-up). Modelled on nvidia_poll (nv.c:2276-
 * 2324): sleep on the fd's wait queue, answer POLLIN|POLLPRI when an event
 * is pending. The host tells us with a KIND_EVENT_FIRED of class
 * NV01_EVENT_OS_EVENT for this token; events_work sets the flag and wakes
 * the queue.
 *
 * WHY poll CLEARS the flag (like the native `dataless_event_pending`,
 * nv.c:2320): NVIDIA's Vulkan never calls NV_ESC_RM_GET_EVENT_DATA
 * after `poll -> 3` (vkcube trace: 0 x 0x52, 8 x poll) -- a flag that
 * stayed up would make that client spin. Nothing is lost by it: every host
 * post produces its own EVENT_FIRED (flag back to 1), and a forwarded 0x52
 * that comes back with status NV_OK and MoreEvents != 0 re-arms the flag
 * (nvrm_call_run, step 8b) because the host queue is not empty yet. One
 * spurious wake-up (flag from a FIRED, then a 0x52 that drains everything)
 * is harmless: poll may be spurious, and the native 0x52 then answers
 * NV_ERR_OPERATING_SYSTEM (osapi.c:519-524), which is what the userspace
 * loop expects to see at the end of a drain.
 */
static __poll_t nvrm_node_poll(struct file *filp, struct poll_table_struct *pt)
{
	struct nvrm_ctx *ctx = filp->private_data;

	if (!ctx)
		return EPOLLERR;
	poll_wait(filp, &ctx->events_wq, pt);
	if (atomic_xchg(&ctx->events_pending, 0))
		return EPOLLIN | EPOLLPRI;
	return 0;
}

/* ------------------------------------------------------------------ *
 * ioctl -- the interpreter
 *
 * Every escape carries one of RM's fixed parameter blocks from nvos.h, named
 * after the struct: NVOS64 for RM_ALLOC, NVOS54 for RM_CONTROL, NVOS00 for
 * RM_FREE, NVOS02 for RM_ALLOC_MEMORY, NVOS33 for RM_MAP_MEMORY (later in
 * this file also NVOS41 for RM_GET_EVENT_DATA, NVOS10 for RM_ALLOC_EVENT,
 * NVOS34 for RM_CONFIG_GET_EX). Their field
 * offsets are never typed in here -- they arrive in the table or come out of
 * nvrm_wire.h. hClass, where one appears, is RM's class number for the kind
 * of object an alloc creates.
 * ------------------------------------------------------------------ */

/* Everything one call needs as intermediate state. */
struct call {
	struct nvrm_ctx *ctx;
	struct nvrm_dev *dev;
	const struct nvrm_tables *t;
	const struct nvrm_ioctl_desc *desc;

	u32 nr;
	u32 size;
	u64 addr;		/* where the inline block is written back */
	/* Kernel caller (NVKMS through nvidia_get_rm_ops) instead of a guest
	 * process through ioctl(2). Decides ONLY how memory is fetched and
	 * written back -- see call_in()/call_out(). */
	bool kern;

	u8 *inl;
	/* the out-of-line block beside inl: embedded-pointer payloads */
	u8 *aux;
	size_t aux_len;
	size_t params_len;	/* the params buffer only, without nested */

	u32 emb_off;
	u64 saved_ptr;		/* guest address of the params buffer */

	u32 fd_off;
	u64 fd_token;
	/* Which guest process owns fd_token. Same rule as aux_fd_proc below:
	 * NVRM_NONE_U32 = not stated, never 0. A token is minted per session,
	 * so the host cannot resolve it without being told whose it is. */
	u32 fd_proc;
	u32 fd_orig;		/* the application's own fd number */

	u32 aux_fd_off;
	u64 aux_fd_token;
	/* Which guest process owns aux_fd_token. NVRM_NONE_U32 = not stated;
	 * never 0, which is a live session id (the "not stated" caller). */
	u32 aux_fd_proc;
	u8 aux_fd_orig[8];
	u32 aux_fd_len;		/* 8 for an NvP64 (alloc), 4 for an NvS32 (control) */

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

/*
 * ---- BDF mediation -------------------------------------------------------
 *
 * Two directions, and both are needed. The guest asks questions that NAME a
 * gpuId (GET_ID_INFO, GET_PCI_INFO) and gets answers that CONTAIN gpuIds and
 * addresses (GET_PROBED_IDS, GET_ATTACHED_IDS, and the two above echoing
 * their argument). Rewriting only the answers would give the guest an id it
 * cannot then ask about.
 */

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

/*
 * Learn the host's id from an answer. Everything downstream keys off this,
 * and it is the one place that decides mediation is not representable.
 */
static void bdf_learn(struct nvrm_dev *dev, u32 host_id)
{
	if (host_id == NVRM_GPU_INVALID_ID || host_id == 0)
		return;
	/* Our OWN id coming back is not a second GPU. It arrives constantly:
	 * every mediated answer is read again by the next caller, and NVKMS
	 * hands the id it was given straight back to us. Measured as a false
	 * "second GPU" that switched mediation off two milliseconds after it
	 * had been switched on. */
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

/*
 * host -> guest, for ids travelling towards the guest.
 *
 * `learn` is false everywhere except the two ENUMERATION answers
 * (GET_PROBED_IDS, GET_ATTACHED_IDS). Those two are RM's statement of which
 * GPUs exist; every other field merely quotes one. Learning from all of them
 * was measured as a false "second GPU (0xffff)" -- a derived field whose
 * value is not an id at all -- which switched mediation off in the middle of
 * an X server start and left nvidia-drm with a half-mediated view.
 */
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

/*
 * The table, generated. NVRM_BDF_SCALARS and NVRM_BDF_ARRAYS come from the
 * SDK structs -- see nvrm-genhdr.rs for why this is not a hand-written
 * switch.
 */
struct bdf_scalar { u32 cmd; u32 off; };
struct bdf_array  { u32 cmd; u32 off; u32 count; u32 stride; };

static const struct bdf_scalar bdf_scalars[] = { NVRM_BDF_SCALARS };
static const struct bdf_array  bdf_arrays[]  = { NVRM_BDF_ARRAYS };

static const struct bdf_array *bdf_find_array(u32 cmd)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(bdf_arrays); i++)
		if (bdf_arrays[i].cmd == cmd)
			return &bdf_arrays[i];
	return NULL;
}

/*
 * NV_ESC_CARD_INFO -- the address that is not a control.
 *
 * A plain ioctl whose reply is an ARRAY of nv_ioctl_card_info_t in the
 * INLINE block, each entry carrying both the BDF and the gpuId. NVML reads
 * it, and this was the last place the host address still came through after
 * every gpuId-bearing control had been mediated: measured with bdf_debug,
 * which found nothing, precisely because it only ever looked at controls.
 */
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
		wr32(c->inl, base + NVRM_CARD_INFO_GPUID_OFF, dev->bdf_guest_id);
		wr32(c->inl, pci + NVRM_PCI_DOMAIN_OFF, dev->bdf_domain);
		c->inl[pci + NVRM_PCI_BUS_OFF] = dev->bdf_bus;
		c->inl[pci + NVRM_PCI_SLOT_OFF] = dev->bdf_slot;
		c->inl[pci + NVRM_PCI_FUNC_OFF] = dev->bdf_func;
	}
}

/*
 * NV_ESC_ATTACH_GPUS_TO_FD -- the address that is not a control EITHER.
 *
 * A plain escape whose entire inline block is an array of gpuIds. It reached
 * neither mediation path: bdf_rewrite_request() hangs on d->cmd_off and this
 * escape has no table entry at all, so it does not even enter
 * gather_embedded() ("an escape with no entry is simply forwarded",
 * nvrm-abi/src/table.rs). The guest therefore sent its own mediated id to a
 * host that has never heard of it, and nvidia_dev_get() answered EINVAL
 * (nv.c:2645). Measured 2026-08-15: bdf_mediation=1 -> -1, =0 -> 0.
 *
 * The COUNT comes from the _IOC size, never from a constant. The host
 * derives its own the same way (arg_size / sizeof(NvU32), nv.c:2605); the
 * 32 entries this rig was observed to send are an observation, not a
 * promise.
 *
 * BOTH directions, because the inline block is copied back to the caller
 * (write_back() -> call_out()). Rewriting only the question would hand the
 * application the HOST's id in its own buffer -- exactly what bdf_debug
 * exists to find.
 *
 * A zero slot means "no GPU here"; the host skips it (nv.c:2639) and both
 * translations pass it through untouched, so nothing special is needed.
 */
static void bdf_rewrite_attach_gpus(struct call *c, bool to_host)
{
	struct nvrm_dev *dev = c->dev;
	u32 off;

	/* UVM carries its command number RAW, without _IOC encoding, so its
	 * numbers live in the same range as the escapes. Only the control
	 * node can see this one at all -- NV_CTL_DEVICE_ONLY, nv.c:2608. */
	if (c->ctx->dev_tag != NVRM_DEV_CTL ||
	    c->nr != NVRM_ESC_ATTACH_GPUS_TO_FD || !bdf_on(dev))
		return;

	for (off = 0; off + 4 <= c->size; off += 4) {
		u32 id = rd32(c->inl, off);

		wr32(c->inl, off, to_host ? bdf_to_host(dev, id)
					  : bdf_to_guest(dev, id, false));
	}
}

/*
 * The question side: the guest names the card by the address IT can see,
 * and RM knows only its own. Called with the params buffer fetched, before
 * the request goes out.
 */
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
		wr32(c->aux, sc->off, bdf_to_host(c->dev, rd32(c->aux, sc->off)));
	}
	if (sc)
		return;
	/* ATTACH_IDS, DETACH_IDS and P2P_CAPS_MATRIX are questions that carry
	 * an ARRAY -- the last one TWO, so every row that names the cmd is
	 * walked, not just the first. */
	for (ar = bdf_arrays; ar < bdf_arrays + ARRAY_SIZE(bdf_arrays); ar++) {
		if (ar->cmd != cmd)
			continue;
		for (i = 0; i < ar->count; i++) {
			u32 off = ar->off + i * ar->stride;

			if (c->params_len < 4 || off > c->params_len - 4)
				break;
			wr32(c->aux, off, bdf_to_host(c->dev, rd32(c->aux, off)));
		}
	}
}

/*
 * The answer side. Called after the reply has landed in c->aux and before
 * it is copied out to the caller.
 *
 * WARNING: this runs on EVERY control, so the fast path is two short linear
 * scans -- 11 scalar rows and 7 array rows -- and nothing else: no
 * allocation, no copy, no
 * lock. Anything heavier belongs behind the cmd match, not in front of it.
 */
static void bdf_rewrite_reply(struct call *c, u32 cmd)
{
	struct nvrm_dev *dev = c->dev;
	const struct bdf_scalar *sc;
	const struct bdf_array *ar;
	u32 i;

	/* Out before anything else when the switch is off. This is the hot
	 * path -- every control reply passes here -- and "off behaves exactly
	 * as before" has to be true of the instruction count too, not just of
	 * the answer. Without this the two table scans ran regardless. */
	if (!bdf_mediation && !bdf_debug)
		return;

	/* Diagnosis runs BEFORE the table, on every reply. Running it only
	 * for commands the table misses was a blind spot that cost a round:
	 * a mediated control can still carry the host address in a SECOND
	 * field, and that is exactly the case worth finding. */
	if (bdf_debug && dev->bdf_host_id && c->params_len >= 4) {
		u32 hbus = (dev->bdf_host_id >> 8) & 0xff;
		u32 off;

		for (off = 0; off + 4 <= c->params_len; off += 4) {
		u32 v = rd32(c->aux, off);

		/* The id as a whole, and -- at bdf_debug 2 --
		 * the bare bus number, because an address does
		 * not have to travel as an id. */
		if (v == dev->bdf_host_id)
			pr_info_ratelimited("virtio_nvrm: bdf_debug: control %#x carries host id %#x at +%u (params %zu)\n",
				    cmd, dev->bdf_host_id,
				    off, c->params_len);
		else if (bdf_debug > 1 && v == hbus)
			pr_info_ratelimited("virtio_nvrm: bdf_debug: control %#x carries host bus %#x at +%u (params %zu)\n",
				    cmd, hbus, off, c->params_len);
		}
		/* An address does not have to be a number at all --
		 * RM hands NVML a printed busId in places. Level 3
		 * looks for the text. */
		if (bdf_debug > 2 && c->params_len >= 5) {
		char want[8];
		size_t k;

		scnprintf(want, sizeof(want), "%02x:%02x",
			  hbus, (dev->bdf_host_id) & 0xff);
		for (k = 0; k + 5 <= c->params_len; k++)
			if (!strncasecmp((char *)c->aux + k, want, 5)) {
			pr_info_ratelimited("virtio_nvrm: bdf_debug: control %#x carries the printed host address at +%zu (params %zu)\n",
				    cmd, k, c->params_len);
			break;
			}
		}
	}


	/* The address as {index, data} pairs, which is how NVML reads it --
	 * and the reason nvidia-smi kept printing the host's bus long after
	 * every gpuId had been mediated. */
	if (cmd == NVRM_CTRL_BUS_GET_INFO_V2 || cmd == NVRM_CTRL_BUS_GET_INFO) {
		u32 n;

		if (!bdf_on(dev) || c->params_len < NVRM_BUS_INFO_LIST_OFF + 4)
			return;
		n = rd32(c->aux, 0);			/* busInfoListSize */
		if (n > NVRM_BUS_INFO_MAX_LIST)
			n = NVRM_BUS_INFO_MAX_LIST;
		for (i = 0; i < n; i++) {
			u32 e = NVRM_BUS_INFO_LIST_OFF + i * NVRM_BUS_INFO_ENTRY_SIZE;
			u32 d = e + NVRM_BUS_INFO_DATA_OFF;
			u32 val;

			if (d + 4 > c->params_len)
				return;
			switch (rd32(c->aux, e)) {
			case NVRM_BUS_INFO_INDEX_BUS:    val = dev->bdf_bus; break;
			case NVRM_BUS_INFO_INDEX_DEVICE: val = dev->bdf_slot; break;
			case NVRM_BUS_INFO_INDEX_DOMAIN: val = dev->bdf_domain; break;
			default: continue;
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

				if (c->params_len < 4 || off > c->params_len - 4)
					break;
				wr32(c->aux, off,
				     bdf_to_guest(dev, rd32(c->aux, off),
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
		wr32(c->aux, sc->off, bdf_to_guest(dev, rd32(c->aux, sc->off), false));
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

/*
 * Fetch and write-back -- the ONLY two places that know whether the caller
 * is a guest PROCESS or the guest KERNEL.
 *
 * The same interpreter serves both: a process through ioctl(2), and NVKMS
 * through nvidia_get_rm_ops(). Tables, virtqueue, request layout and every
 * pointer rule are identical; only the fetch differs -- copy_from_user for
 * the one, memcpy for the other.
 *
 * A branch, deliberately, and not a second interpreter: a second copy of
 * this marshalling would be a second set of bugs, and the first copy is the
 * one CUDA depends on.
 */
static int call_in(const struct call *c, void *dst, u64 src, size_t n)
{
	if (!n)
		return 0;
	if (c->kern) {
		memcpy(dst, (void *)(uintptr_t)src, n);
		return 0;
	}
	return copy_from_user(dst, (void __user *)(uintptr_t)src, n) ? -EFAULT : 0;
}

static int call_out(const struct call *c, u64 dst, const void *src, size_t n)
{
	if (!n)
		return 0;
	if (c->kern) {
		memcpy((void *)(uintptr_t)dst, src, n);
		return 0;
	}
	return copy_to_user((void __user *)(uintptr_t)dst, src, n) ? -EFAULT : 0;
}

/* Resolve XFER: the real number, size and pointer sit in the payload. Must
 * happen BEFORE the size is determined, otherwise the bytes of the wrapper
 * struct travel instead of the payload (that mistake only shows up later, as
 * garbage data).
 *
 * User-only, and the __user pointer says so: the kernel path carries its
 * number and size in the call itself and never reaches an XFER escape. */
static int resolve_xfer(struct call *c, void __user *arg)
{
	const struct nvrm_table_hdr *h = &c->t->hdr;
	u8 hdrbuf[64];
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

/* The embedded pointer and the pointers inside it -- step (3) of
 * nvrm_call_run, hoisted out. */
static int gather_embedded(struct call *c)
{
	const struct nvrm_ioctl_desc *d = c->desc;
	const struct nvrm_class_desc *cls = NULL;
	u32 plen = 0;
	u32 i;

	if (!d || d->emb_ptr_off == NVRM_NONE_U32)
		return 0;
	if (d->emb_ptr_off + 8 > c->size)
		return -EINVAL;

	/* Blocked controls are not sent at all. Which ones those are stands in
	 * the table (CF_BLOCK) -- the module reads the number, it does not know
	 * it. The effective block sits in the HOST; here it only saves the trip
	 * and makes the failure early and unambiguous. */
	if (d->cmd_off != NVRM_NONE_U32 && d->cmd_off + 4 <= c->size) {
		const struct nvrm_ctrl_desc *blk = find_ctrl(c->t, rd32(c->inl, d->cmd_off));

		if (blk && (blk->flags & NVRM_CF_BLOCK)) {
			pr_warn_ratelimited("virtio_nvrm: control %#x is blocked -- not forwarded\n",
					    rd32(c->inl, d->cmd_off));
			return -EPERM;
		}
	}

	/* The POINTER first, the LENGTH second -- a NULL pointer means
	 * "parameterless call" and ends the matter before any table is
	 * consulted. Classes such as NV01_ROOT_CLIENT have no alloc params at
	 * all and therefore appear in no table; looking up first rejects them
	 * wrongly with EOPNOTSUPP (measured: hClass 0x0, cuInit did not get
	 * past its first alloc). */
	c->saved_ptr = rd64(c->inl, d->emb_ptr_off);
	if (!c->saved_ptr)
		return 0;

	switch (d->emb_len_kind) {
	case NVRM_EMB_LEN_FIELD:
		if (d->emb_len_off + 4 > c->size)
			return -EINVAL;
		plen = rd32(c->inl, d->emb_len_off);
		break;
	case NVRM_EMB_LEN_CLASS:
		if (d->emb_len_off + 4 > c->size)
			return -EINVAL;
		cls = find_class(c->t, rd32(c->inl, d->emb_len_off));
		if (!cls) {
			/* Unknown hClass: do NOT guess. A wrong length would be
			 * an out-of-bounds read in the driver's copy_from_user
			 * on the host side. */
			pr_warn_ratelimited("virtio_nvrm: hClass %#x unknown -- EOPNOTSUPP instead of a guess (%s[%d])\n",
					    rd32(c->inl, d->emb_len_off),
					    current->comm, task_pid_nr(current));
			return -EOPNOTSUPP;
		}
		plen = cls->param_size;
		/* pRightsRequested is not supported -- it appears in no
		 * measured run. Non-null here: fail loudly. */
		if (d->rights_off != NVRM_NONE_U32 && c->size == d->rights_if_size) {
			if (d->rights_off + 8 > c->size)
				return -EINVAL;
			if (rd64(c->inl, d->rights_off))
				return -EOPNOTSUPP;
		}
		break;
	case NVRM_EMB_LEN_FIXED:
		/* The length is a constant that sits IN emb_len_off itself
		 * (NV_ESC_RM_GET_EVENT_DATA: NVOS41.pEvent -> one NvUnixEvent,
		 * out-only). Before this arm existed the escape fell into the
		 * `default` below and the RAW guest pointer travelled to the
		 * host, where RM's os_memcpy_to_user (osapi.c:531) would have
		 * written into the daemon's address space -- unreachable only
		 * because poll never woke anybody. It does now. */
		plen = d->emb_len_off;
		break;
	default:
		return 0;
	}

	if (!plen)
		return 0;	/* length 0: nothing to take along */
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

	/* SECOND-level pointers: which field is a pointer, and where its length
	 * sits, comes from the control table. Without an entry RM would receive
	 * a guest VA and answer with 0x1e/0x3a. */
	if (d->cmd_off != NVRM_NONE_U32 && d->cmd_off + 4 <= c->size) {
		const struct nvrm_ctrl_desc *ct = find_ctrl(c->t, rd32(c->inl, d->cmd_off));

		if (ct && ct->count) {
			struct { u32 ptr_off; u64 gva; u32 len; } plan[NVRM_MAX_NESTED];
			u32 n = 0;
			size_t total = c->params_len;
			u8 *bigger;

			if (ct->count > NVRM_MAX_NESTED ||
			    ct->first + ct->count > c->t->hdr.n_nested)
				return -EPROTO;

			/* Read everything out of the params buffer FIRST, then
			 * copy it over -- otherwise a remembered pointer refers
			 * to the old buffer. */
			for (i = 0; i < ct->count; i++) {
				const struct nvrm_nested_row *row = &c->t->nested[ct->first + i];
				u64 gva;
				u32 nlen;

				if (row->ptr_off + 8 > c->params_len)
					continue;
				gva = rd64(c->aux, row->ptr_off);
				if (row->len_kind == NVRM_NLEN_FIXED) {
					nlen = row->len_off;
				} else {
					if (row->len_off + 4 > c->params_len)
						return -EINVAL;
					if (check_mul_overflow(rd32(c->aux, row->len_off),
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
		/* The GUARD, and it is not optional for the event classes:
		 * NV0005_ALLOC_PARAMETERS reuses `data` (@16) as an fd for
		 * NV01_EVENT_OS_EVENT and as a callback POINTER for the
		 * kernel-callback classes, and says which by hClass (@8) in
		 * the same buffer. The outer class does not decide it --
		 * measured, NVIDIA's Vulkan allocates under 0x0005 with the
		 * inner class 0x79 where CUDA uses 0x0079 directly. */
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
			pr_warn_ratelimited("virtio_nvrm: kernel path names fd %lld in alloc params\n",
					    (long long)val);
			return -EBADF;
		} else {
			u64 tok;

			if (nvrm_token_of_fd((int)val, &tok, &c->aux_fd_proc)) {
				pr_warn_ratelimited("virtio_nvrm: alloc params name foreign fd %lld\n",
						    (long long)val);
				return -EBADF;
			}
			c->aux_fd_token = tok;
		}
	}
no_alloc_fd:

	/* The same problem one level along: an fd inside a CONTROL's params.
	 *
	 * NVIDIA's EGL is the first consumer here to use one --
	 * NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD (0x3d05) hands RM an
	 * already-open /dev/nvidiactl fd, and the guest's number means nothing
	 * on the host (measured: NV_ERR_INVALID_PARAMETER, 0x3b). Which control
	 * and which offset comes from the TABLE (NVRM_CF_FD), not from a number
	 * written here.
	 *
	 * WARNING: FOUR bytes. These fds are NvS32, while cls->fd_off above
	 * names an NvP64. Copying eight would take the neighbouring field with
	 * it -- for 0x3d05 that neighbour is `flags`.
	 */
	if (d->cmd_off != NVRM_NONE_U32 && d->cmd_off + 4 <= c->size) {
		const struct nvrm_ctrl_desc *ct =
			find_ctrl(c->t, rd32(c->inl, d->cmd_off));

		if (ct && (ct->flags & NVRM_CF_FD) && ct->fd_off != NVRM_NONE_U32) {
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
				/* WHOSE table the number belongs to is the
				 * whole question, and `c->kern` alone does
				 * not answer it.
				 *
				 * The fd at ESCAPE level names which session
				 * a mapping belongs to, and for a kernel
				 * caller that is its own -- kapi_forward_on()
				 * answers "it is us" and never reads the
				 * number. This one is different:
				 * IMPORT_OBJECT_FROM_FD names the session
				 * that EXPORTED the object, which is a
				 * userspace one. "It is us" would be wrong,
				 * so the number has to be resolved.
				 *
				 * And it can be: NVKMS runs the import
				 * inside the caller's own ioctl, so `current`
				 * is that process. Measured 2026-08-15, the
				 * refusal naming its context: "kernel path
				 * names fd 38 in control 0x3d06 (comm Xorg,
				 * pid 3049, process)".
				 *
				 * A kthread is the case the refusal was
				 * really built against -- there `current` is
				 * somebody else entirely -- so that one still
				 * refuses. nvrm_token_of_fd() is the second
				 * line: it accepts only our own nodes, so a
				 * number that means nothing here fails rather
				 * than resolving to something wrong.
				 */
				pr_warn_ratelimited("virtio_nvrm: kthread names fd %d in control %#x (comm %s, pid %d) -- no table to read it against\n",
						    val, rd32(c->inl, d->cmd_off),
						    current->comm, current->pid);
				return -EBADF;
			} else {
				u64 tok;

				if (nvrm_token_of_fd((int)val, &tok,
						     &c->aux_fd_proc)) {
					pr_warn_ratelimited("virtio_nvrm: control %#x names foreign fd %d\n",
							    rd32(c->inl, d->cmd_off), val);
					return -EBADF;
				}
				c->aux_fd_token = tok;
			}
		}
	}

	/* The guest names the card by the address IT can see; RM knows only
	 * its own. See bdf_mediation. */
	if (d->cmd_off != NVRM_NONE_U32 && d->cmd_off + 4 <= c->size)
		bdf_rewrite_request(c, rd32(c->inl, d->cmd_off));

	return 0;
}

/*
 * NV01_MEMORY_SYSTEM_OS_DESCRIPTOR from the kernel, with a dma-buf or an
 * sg_table as the descriptor.
 *
 * The wire keeps the shape the host already knows -- GPA runs -- but this
 * call DOES have a params buffer, unlike the process form where aux carries
 * runs and nothing else. So aux becomes params ++ runs: the host reads the
 * params it needs (limit, attr, type) at the front and the runs behind them.
 * `params_len` says where the boundary is, and the host branch keys off
 * ioctl_nr == NV_ESC_RM_ALLOC, a combination that used to be refused
 * outright -- so an older host answers with a clean error instead of
 * misreading anything.
 */
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
	desc  = rd64(c->aux, NVRM_OSDESC_DESCRIPTOR_OFF);
	limit = rd64(c->aux, NVRM_OSDESC_LIMIT_OFF);

	if (display > 1)
		pr_info("virtio_nvrm: osdesc(kernel): descriptorType %u, descriptor %#llx, limit %#llx\n",
			dtype, desc, limit);

	switch (dtype) {
	case NVRM_OSDESC_OS_DMA_BUF_PTR:
		dmabuf = (struct dma_buf *)(uintptr_t)desc;
		break;
	case NVRM_OSDESC_OS_SGT_PTR:
		/* {sgt, gem}. We take the dma_buf route instead of walking a
		 * foreign sg_table whose lifetime nobody promised us -- the
		 * exporter is the same object either way.
		 *
		 * And this is NOT the path a PRIME import takes.
		 * nv_drm_gem_prime_import_sg_table receives an sg_table from
		 * the DRM core and then calls
		 * getSystemMemoryHandleFromDmaBuf with the DMA_BUF
		 * (nvidia-drm-gem-dma-buf.c:155) -- type 5, which is built
		 * above. getSystemMemoryHandleFromSgt (type 6) appears in
		 * exactly one place, nv_drm_gem_export_dmabuf_memory_ioctl,
		 * and only on the branch where pMemory is already NULL, i.e.
		 * where that dma-buf import has ALREADY failed.
		 *
		 * So this warning in the log is a SYMPTOM, not a cause. If
		 * it appears, the question is why the type-5 import above did
		 * not produce a handle -- most likely nvrm_pin_dmabuf refusing
		 * pages without a struct page. Reading it as "the import path
		 * needs sg_table support" sends the next person to build the
		 * wrong thing. */
		pr_warn_ratelimited("virtio_nvrm: OS descriptor type %u (sg_table) is not built; the dma-buf import above failed first -- look there\n",
				    dtype);
		return -EOPNOTSUPP;
	default:
		pr_warn_ratelimited("virtio_nvrm: OS descriptor from the kernel path with descriptorType %u -- not built\n",
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
		pr_warn_ratelimited("virtio_nvrm: no DMA-capable device for a PRIME import\n");
		return -EOPNOTSUPP;
	}

	pin = nvrm_pin_dmabuf(dmabuf, dmadev);
	if (IS_ERR(pin)) {
		pr_warn_ratelimited("virtio_nvrm: PRIME import: page walk failed: %ld\n",
				    PTR_ERR(pin));
		return PTR_ERR(pin);
	}
	c->pin = pin;
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

/*
 * NV01_MEMORY_SYSTEM_OS_DESCRIPTOR (hClass 0x71).
 *
 * This call describes memory that RM PINS instead of copying -- a guest VA is
 * meaningless on the host side (measured: 0x1e INVALID_ADDRESS). So the module
 * resolves the pages here and sends GPA runs; the host reassembles them into a
 * host VA. In the kernel that is a one-liner (page_to_pfn) instead of the
 * userspace detour through pagemap -- and it needs no CAP_SYS_ADMIN.
 */
static int gather_osdesc(struct call *c)
{
	const struct nvrm_table_hdr *h = &c->t->hdr;
	const struct nvrm_ioctl_desc *d = c->desc;
	struct nvrm_gpa_run *runs;
	unsigned long pmem;
	u64 limit, dlen;
	u32 nruns, max_runs;

	/*
	 * The KERNEL form, and it is a different call than the process one.
	 *
	 * A process allocates OS-described memory with NV_ESC_RM_ALLOC_MEMORY
	 * (NVOS02: the address and the limit sit in the INLINE block, and the
	 * table marks that escape with NVRM_F_OSDESC). NVKMS instead uses
	 * NV04_ALLOC (NVOS64) with hClass 0x71 and puts
	 * NV_OS_DESC_MEMORY_ALLOCATION_PARAMS in the PARAMS buffer -- so the
	 * flag never matches and the alloc used to travel to the host with a
	 * guest kernel pointer in it. Measured 2026-08-08 as
	 *   kernel ALLOC, class 0x71 / osdesc: nr 0x2b has no OSDESC flag
	 * on an import that then reported success with nothing behind it.
	 */
	if (c->kern && d && c->nr == NVRM_KESC_ALLOC &&
	    NVRM_NVOS64_HCLASS_OFF + 4 <= c->size &&
	    rd32(c->inl, NVRM_NVOS64_HCLASS_OFF) == h->osdesc_class)
		return gather_osdesc_kern(c);

	if (!d || !(d->flags & NVRM_F_OSDESC)) {
		/* Only for an ALLOC. This used to print for every kernel call
		 * that passed through here -- FREE, CONTROL, MAP_MEMORY -- and
		 * an "osdesc:" line about a FREE reads like a finding when it
		 * is a tautology. */
		if (display > 1 && c->kern && c->nr == NVRM_KESC_ALLOC)
			pr_info("virtio_nvrm: osdesc: alloc nr %#x has no OSDESC flag\n", c->nr);
		return 0;
	}
	if (d->emb_len_off + 4 > c->size)
		return -EINVAL;
	if (display > 1 && c->kern)
		pr_info("virtio_nvrm: osdesc: class at +%u is %#x, looking for %#x\n",
			d->emb_len_off, rd32(c->inl, d->emb_len_off), h->osdesc_class);
	if (rd32(c->inl, d->emb_len_off) != h->osdesc_class)
		return 0;	/* other memory class: forward normally */
	/* A kernel caller describes KERNEL memory here, and nvrm_pin_range()
	 * below resolves a USER address with get_user_pages(). Pinning the
	 * wrong address space would not fail, it would send the host somebody
	 * else's pages -- so refuse instead, loudly. Not needed to load NVKMS;
	 * if a later stage wants it, it needs its own page walk (vmalloc_to_page
	 * / virt_to_page), not this one. */
	if (c->kern) {
		u32 dtype = NVRM_NONE_U32;

		if (NVRM_OSDESC_TYPE_OFF + 4 <= c->size)
			dtype = rd32(c->inl, NVRM_OSDESC_TYPE_OFF);
		/* Name the KIND, because the answer decides the next build:
		 * type 0 would be a user VA (impossible from here), while PRIME
		 * import arrives as a dma_buf pointer (5) or an sg_table (6).
		 * Those two are guest RAM the host already has mapped -- the
		 * same GpaRun wire the user path uses, only walked out of an
		 * sg_table instead of pinned out of a user VA. */
		pr_warn_ratelimited("virtio_nvrm: OS descriptor from the kernel path is not supported (descriptorType %u)\n",
				    dtype);
		return -EOPNOTSUPP;
	}
	/* Every field the pin path reads out of the inline block, and every
	 * field it later writes back or reads after the call (step 9 of
	 * nvrm_call_run: status and the created handle) has to lie inside
	 * the block the caller sent -- the _IOC size of a 0x27 is the
	 * caller's word. */
	if (h->osdesc_pmem_off + 8 > c->size || h->osdesc_limit_off + 8 > c->size ||
	    h->osdesc_status_off + 4 > c->size || h->osdesc_handle_off + 4 > c->size)
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
	c->limit_orig = limit;

	/* For this call the aux buffer carries the runs and NOTHING else; it
	 * has no params buffer. */
	max_runs = (u32)(c->pin->npages);
	if ((size_t)max_runs * sizeof(*runs) > h->max_aux)
		max_runs = h->max_aux / sizeof(*runs);
	runs = kvzalloc((size_t)max_runs * sizeof(*runs), GFP_KERNEL);
	if (!runs)
		return -ENOMEM;
	nruns = nvrm_runs_from_pages(c->pin->pages, c->pin->npages, runs, max_runs);
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
static int write_back(struct call *c, const struct nvrm_rsp *rsp, const u8 *body)
{
	size_t il = min_t(size_t, rsp->inline_len, c->size);
	size_t al = min_t(size_t, rsp->aux_len, c->aux_len);
	u32 i;

	if (il)
		memcpy(c->inl, body, il);
	if (al)
		memcpy(c->aux, body + rsp->inline_len, al);

	if (c->emb_off != NVRM_NONE_U32 && c->saved_ptr) {
		/* 1. nested buffers back to their guest addresses */
		for (i = 0; i < c->n_nested; i++) {
			if (call_out(c, c->nested_gva[i],
				     c->aux + c->nested[i].aux_off,
				     c->nested[i].len))
				return -EFAULT;
			/* 2. pointer INSIDE the params buffer back to the
			 *    guest original */
			wr64(c->aux, c->nested[i].ptr_off, c->nested_gva[i]);
		}
		/* 3. restore the application's own fd number */
		if (c->aux_fd_off != NVRM_NONE_U32)
			memcpy(c->aux + c->aux_fd_off, c->aux_fd_orig,
			       c->aux_fd_len);
		/* 3b. the card's address, in the guest's own bus. Before the
		 *     copy-out, so the caller never sees the host's. */
		if (c->desc->cmd_off != NVRM_NONE_U32 &&
		    c->desc->cmd_off + 4 <= c->size)
			bdf_rewrite_reply(c, rd32(c->inl, c->desc->cmd_off));
		/* 4. params buffer back -- ONLY params_len bytes, the nested
		 *    buffers sit behind it */
		if (call_out(c, c->saved_ptr, c->aux, c->params_len))
			return -EFAULT;
		/* 5. the pointer in the inline block belongs to the
		 *    application, not to the host */
		wr64(c->inl, c->emb_off, c->saved_ptr);
	}

	if (c->nr == NVRM_ESC_CARD_INFO)
		bdf_rewrite_card_info(c);
	bdf_rewrite_attach_gpus(c, false);

	/* Last check, AFTER every rewrite: does the inline block still name
	 * the host? Whatever answers here is a place the mediation misses. */
	if (bdf_debug && c->dev->bdf_host_id) {
		u32 off;

		for (off = 0; off + 4 <= c->size; off += 4)
			if (rd32(c->inl, off) == c->dev->bdf_host_id) {
				pr_info_ratelimited("virtio_nvrm: bdf_debug: ioctl %u inline still names host %#x at +%u (size %u)\n",
						    c->nr, c->dev->bdf_host_id,
						    off, c->size);
				break;
			}
	}

	/* Likewise for the fd field and the OS descriptor address: the
	 * application must see its own values. */
	if (c->fd_off != NVRM_NONE_U32 && c->fd_off + 4 <= c->size)
		memcpy(c->inl + c->fd_off, &c->fd_orig, 4);
	/* Only on the PROCESS path. There the inline block is NVOS02, the host
	 * rewrote pMemory (osdesc_pmem_off @24) and limit (@32) to its own host
	 * VA, and the application must read its own back. The KERNEL path
	 * (gather_osdesc_kern) carries an NVOS64 inline instead -- @24 is
	 * pRightsRequested and @32 is paramsSize/flags there -- and its
	 * descriptor lives in the params buffer, which the host already handed
	 * back intact (it restores the NVOS64 tail before replying). Restoring
	 * on that path would write the dma_buf pointer over NVOS64 fields the
	 * guest never set here: in bounds, and status@40/hObjectNew@8 stay
	 * correct, but a silent scribble on NVKMS's own IN fields. */
	if (c->pin && !c->kern) {
		wr64(c->inl, c->t->hdr.osdesc_pmem_off, c->pmem_orig);
		wr64(c->inl, c->t->hdr.osdesc_limit_off, c->limit_orig);
	}

	if (call_out(c, c->addr, c->inl, c->size))
		return -EFAULT;
	return 0;
}

/*
 * Steps (1) to (9), plus (8b) -- shared by both callers.
 *
 * Everything above this point differs between them: a process reads its
 * command number out of the _IOC encoding, NVKMS passes number and size
 * straight in. From here on nothing does: same tables, same request, same
 * virtqueue. `c->kern` decides only how memory is fetched (call_in/call_out).
 */
static long nvrm_call_run(struct call *c)
{
	struct nvrm_ctx *ctx = c->ctx;
	struct nvrm_dev *dev = c->dev;
	struct nvrm_xfer *x = NULL;
	struct nvrm_req *r;
	struct nvrm_rsp *rsp;
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

	/* (2) fd field: number -> token, via the identity of the file.
	 *
	 * ONLY for a process. A kernel caller has no fd table, and its token
	 * was already named by kapi_forward() -- resolving the number here as
	 * well would overwrite that with a lookup against `current`, which for
	 * a kthread is somebody else's table. Measured as "fd field points at
	 * foreign fd 0" on a block whose fd byte was never meant to be read. */
	if (!c->kern && c->desc && c->desc->fd_off != NVRM_NONE_U32) {
		s32 n;

		if (c->desc->fd_off + 4 > c->size) {
			ret = -EINVAL;
			goto out;
		}
		memcpy(&n, c->inl + c->desc->fd_off, 4);
		c->fd_off = c->desc->fd_off;
		memcpy(&c->fd_orig, &n, 4);
		if (n < 0) {
			c->fd_token = NVRM_NONE_U64;	/* -1: pass through unchanged */
		} else if (nvrm_token_of_fd(n, &c->fd_token, &c->fd_proc)) {
			pr_warn_ratelimited("virtio_nvrm: fd field points at foreign fd %d\n", n);
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

	/* (5) The gpuIds that no control carries. Last, so that it sees the
	 *     inline block exactly as it will travel. */
	bdf_rewrite_attach_gpus(c, true);

	/* (6) Build the request. */
	x = nvrm_xfer_alloc(sizeof(*r) + c->size + c->aux_len,
			    sizeof(*rsp) + c->size + c->aux_len);
	if (IS_ERR(x)) {
		ret = PTR_ERR(x);
		x = NULL;
		goto out;
	}
	r = nvrm_req_init(dev, x, NVRM_KIND_IOCTL, ctx->proc ? ctx->proc->id : 0);
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

	/* (7) There and back. Interruptible for a PROCESS -- a hanging host
	 *     must not produce an unkillable one, and a SIGKILL ends the wait.
	 *
	 *     NOT for the kernel path: NVKMS is called from insmod, rmmod
	 *     and kernel threads, and NOBODY SIGNALS THOSE. The same wait that
	 *     protects a process leaves `rmmod nvidia_drm` blocked forever once
	 *     the host stops answering -- measured, with a dead
	 *     vhost-user-nvrm: the DRM nodes were already gone and the module
	 *     sat at refcount 1. So a kernel caller takes the bounded wait
	 *     instead, the same one the teardown path uses; it gives up after
	 *     NVRM_TEARDOWN_TIMEOUT and says so. */
	ret = nvrm_xfer_run(dev, x, !c->kern);
	if (ret) {
		if (ret == -ERESTARTSYS || ret == -ETIMEDOUT)
			x = NULL;	/* now belongs to the callback */
		goto out;
	}
	rsp = x->rsp;
	if (sizeof(*rsp) + (size_t)rsp->inline_len + rsp->aux_len > x->rsp_len) {
		pr_warn_ratelimited("virtio_nvrm: reply claims %u+%u bytes, but only %u arrived\n",
				    rsp->inline_len, rsp->aux_len, x->rsp_len);
		ret = -EIO;
		goto out;
	}

	/* (8) Write back to the application. */
	ret = write_back(c, rsp, (u8 *)x->rsp + sizeof(*rsp));
	if (ret)
		goto out;
	ret = rsp->ret;

	/* (8b) A forwarded NV_ESC_RM_GET_EVENT_DATA that says "more where that
	 *      came from" re-arms the poll flag: poll cleared it on the way
	 *      out (see nvrm_node_poll), and the host queue is not empty yet.
	 *      NVOS41: pEvent@0, MoreEvents@8, status@12 (nvos.h:1941-1946).
	 *      c->inl holds the ANSWER here -- write_back copied it in. */
	if (!c->kern && c->nr == NVRM_ESC_RM_GET_EVENT_DATA && ret == 0 &&
	    c->size >= NVRM_NVOS41_SIZE &&
	    rd32(c->inl, NVRM_NVOS41_STATUS_OFF) == NVRM_NV_OK &&
	    rd32(c->inl, NVRM_NVOS41_MOREEVENTS_OFF))
		atomic_set(&ctx->events_pending, 1);

	/* (9) Accounting: attach the pin to the handle that was created -- and
	 *     detach it again when that handle is freed. */
	if (c->pin && ret == 0) {
		u32 status = rd32(c->inl, c->t->hdr.osdesc_status_off);

		if (status == 0) {
			c->pin->handle = rd32(c->inl, c->t->hdr.osdesc_handle_off);
			spin_lock(&ctx->pin_lock);
			list_add_tail(&c->pin->node, &ctx->pins);
			spin_unlock(&ctx->pin_lock);
			c->pin = NULL;	/* now belongs to the context */
		}
	}
	if (ret == 0 && c->desc && (c->desc->flags & NVRM_F_FREE) &&
	    c->desc->handle_off != NVRM_NONE_U32 && c->desc->handle_off + 4 <= c->size) {
		u32 h = rd32(c->inl, c->desc->handle_off);
		struct nvrm_pin *p, *tmp;

		/* The lock is dropped around nvrm_unpin() (it sleeps). The
		 * cached `tmp` stays valid across that gap only because
		 * ctx->lock -- held for the whole of nvrm_call_run -- is
		 * what serialises every other mutator of ctx->pins;
		 * pin_lock alone would not make this loop safe. */
		spin_lock(&ctx->pin_lock);
		list_for_each_entry_safe(p, tmp, &ctx->pins, node) {
			if (p->handle == h) {
				list_del(&p->node);
				spin_unlock(&ctx->pin_lock);
				nvrm_unpin(p);
				spin_lock(&ctx->pin_lock);
			}
		}
		spin_unlock(&ctx->pin_lock);
	}

out:
	if (c->pin)
		nvrm_unpin(c->pin);
	nvrm_xfer_free(x);
	kvfree(c->aux);
	kvfree(c->inl);
	mutex_unlock(&ctx->lock);
	return ret;
}

static long nvrm_node_ioctl(struct file *filp, unsigned int cmd, unsigned long arg)
{
	struct nvrm_ctx *ctx = filp->private_data;
	struct nvrm_dev *dev;
	struct call c;
	long ret;

	if (!ctx || !ctx->dev || !ctx->dev->tbl.blob)
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

	/* (0) Determine number and size. UVM carries its number raw, without
	 *     _IOC encoding -- so the size does not live in the command, it
	 *     lives in the table. Unknown means EOPNOTSUPP (== ENOTSUP in
	 *     userspace), not a guess. */
	if (ctx_is_uvm(ctx)) {
		c.nr = cmd;
		c.desc = find_ioctl(c.t, ctx->dev_tag, c.nr);
		if (!c.desc || c.desc->size == NVRM_SIZE_FROM_IOC) {
			pr_warn_ratelimited("virtio_nvrm: UVM command %#x unknown -- no size available\n",
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

/* ------------------------------------------------------------------ *
 * mmap, part 1: through the host-visible window
 *
 * The window is the virtio SHMEM region this device exposes: a range of
 * guest-physical memory the host can place its own RM mappings into. Every
 * mapping a guest process or NVKMS gets hold of reaches the card through it,
 * so the guest hands out the space (win_alloc) and the host fills it.
 * ------------------------------------------------------------------ */

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

	nvrm_simple(m->dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0, m->off, m->len, NULL, false,
		    m->guest_proc);
	win_free(m->dev, m->off, m->len);
	kfree(m);
}

/* open/close are mandatory here: without them the host mapping would stay
 * behind when the client munmaps or dies -- and the window would fill up.
 * open() also covers the VMA split caused by a partial munmap. */
static void nvrm_win_vm_open(struct vm_area_struct *vma)
{
	kref_get(&((struct nvrm_winmap *)vma->vm_private_data)->ref);
}

static void nvrm_win_vm_close(struct vm_area_struct *vma)
{
	kref_put(&((struct nvrm_winmap *)vma->vm_private_data)->ref, winmap_release);
}

static const struct vm_operations_struct nvrm_win_vm_ops = {
	.open = nvrm_win_vm_open,
	.close = nvrm_win_vm_close,
};

static int nvrm_mmap_window(struct nvrm_ctx *ctx, struct vm_area_struct *vma, size_t len)
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

	/* The host places the mapping at the offset in the window named here
	 * and reports back the CACHEABILITY -- the host knows the NVOS33
	 * flags, the guest does not guess. */
	ret = nvrm_simple(dev, NVRM_KIND_MAP_PREPARE, ctx->dev_tag, 0, ctx->token,
			  off, len, &cache, true, ctx->proc ? ctx->proc->id : 0);
	if (ret < 0) {
		/* WHO asked for WHAT. A bare errno cost an hour on 2026-08-16
		 * (which process, which size?) -- the rule since: every
		 * refusal names its caller, or it is not a measurement. */
		if (ret != -ERESTARTSYS)
			pr_warn_ratelimited("virtio_nvrm: MAP_PREPARE failed: %d (%s[%d], %s node, %zu KiB at window+%#lx)\n",
					    ret, current->comm, task_pid_nr(current),
					    ctx->dev_tag == NVRM_DEV_CTL ? "ctl" :
					    ctx->dev_tag == NVRM_DEV_GPU ? "gpu" : "other",
					    (size_t)(len >> 10), (unsigned long)off);
		/*
		 * A refusal is not the only way out of that call. On
		 * -ERESTARTSYS and -ETIMEDOUT the request BELONGS TO THE
		 * CALLBACK (see nvrm_xfer_run) -- the host may already have
		 * prepared this window, and we simply stopped listening.
		 * Dropping the bitmap bit without telling the host then
		 * leaves a reservation nobody owns: the guest believes the
		 * offset is free, the host knows it is taken, and because
		 * win_alloc always hands out the LOWEST free area, every
		 * later mapping asks for that same offset and is refused.
		 *
		 * Measured 2026-08-17 on the desktop guest: one
		 * interrupted prepare was enough to make every Vulkan client
		 * fail forever -- vkcube, vkcubepp, vkgears and vkprobe all
		 * died in device creation, with
		 *   MAP_PREPARE failed: -16 (... 1024 KiB at window+0xce41000)
		 * repeating at a FIXED offset while not a single Vulkan
		 * process was running. It survives every process, so the
		 * session is done for. Killing a game at the wrong moment is
		 * enough to trigger it, and it is not Wayland-specific.
		 *
		 * So say it out loud. A MAP_RELEASE for a window the host
		 * never prepared is harmless; a leaked window is not.
		 */
		if (ret == -ERESTARTSYS || ret == -ETIMEDOUT)
			nvrm_simple(dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0, off, len,
				    NULL, false, ctx->proc ? ctx->proc->id : 0);
		kfree(m);
		win_free(dev, off, len);
		return ret;
	}

	/* Cacheability encoding, same as virtio-gpu uses: 1 = cached (system
	 * memory on the ctl node), 2 = uncached (BAR on the GPU node). */
	if (cache == 2)
		vma->vm_page_prot = pgprot_noncached(vma->vm_page_prot);

	vm_flags_set(vma, VM_IO | VM_DONTEXPAND | VM_DONTDUMP);
	/* fork does NOT inherit this mapping: behind it sits a host object tied
	 * to this one session, and a child that kept using the same pages would
	 * see another session's state. Whoever wants GPU work after a fork
	 * re-initializes CUDA -- which libcuda does anyway. */
	vm_flags_set(vma, VM_DONTCOPY);

	if (io_remap_pfn_range(vma, vma->vm_start,
			       (dev->win_base + off) >> PAGE_SHIFT,
			       len, vma->vm_page_prot)) {
		nvrm_simple(dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0, off, len, NULL, false,
			    ctx->proc ? ctx->proc->id : 0);
		kfree(m);
		win_free(dev, off, len);
		return -EAGAIN;
	}

	vma->vm_private_data = m;
	vma->vm_ops = &nvrm_win_vm_ops;
	return 0;
}

/* ------------------------------------------------------------------ *
 * mmap, part 2: UVM pool backed by self-owned pages
 * ------------------------------------------------------------------ */

struct nvrm_pool {
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
}

static void nvrm_pool_vm_open(struct vm_area_struct *vma)
{
	kref_get(&((struct nvrm_pool *)vma->vm_private_data)->ref);
}

static void nvrm_pool_vm_close(struct vm_area_struct *vma)
{
	kref_put(&((struct nvrm_pool *)vma->vm_private_data)->ref, pool_release);
}

static const struct vm_operations_struct nvrm_pool_vm_ops = {
	.open = nvrm_pool_vm_open,
	.close = nvrm_pool_vm_close,
};

/*
 * UVM uses the mmap offset as an ADDRESS: mmap(0x204a00000, .., uvm_fd,
 * 0x204a00000) is the semaphore pool at GPU VA 0x204a00000.
 *
 * This module puts its OWN pages there (alloc_page + vm_insert_page) and
 * reports their GPAs to the host, which attaches them to the same GPU VA via
 * OS descriptor + EXTERNAL_RANGE. No mmap on the uvm FD -> no address
 * coupling.
 *
 * Owning instead of pinning: the pages belong to this module, they cannot
 * migrate away, and their lifetime visibly hangs off the VMA. That removes
 * any need for a read-modify-write prefault, mlock or NOHUGEPAGE.
 */
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

	if (!npages)
		return -EINVAL;
	if ((npages << PAGE_SHIFT) != len)
		return -EINVAL;

	/* Charge the quota BEFORE the first page. Without it a guest process
	 * that asks for more managed memory than the guest has RAM drains the
	 * whole machine -- measured: requesting 9216 MiB in a 4 GiB VM summoned
	 * the OOM killer instead of returning an honest ENOMEM. Whoever asks
	 * for too much should fail, not the neighbour. */
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
	p->npages = npages;
	stat_pool_pages += npages;

	vm_flags_set(vma, VM_MIXEDMAP | VM_DONTEXPAND | VM_DONTDUMP | VM_DONTCOPY);

	for (i = 0; i < npages; i++) {
		/* __GFP_RETRY_MAYFAIL: this may fail, but it must NOT summon the
		 * OOM killer -- that would hit some arbitrary process in the
		 * guest, not the one that asked for too much. __GFP_NOWARN,
		 * because the failure is passed up cleanly here and the
		 * application sees it as ENOMEM. */
		p->pages[i] = alloc_page(GFP_USER | __GFP_ZERO |
					 __GFP_RETRY_MAYFAIL | __GFP_NOWARN);
		if (!p->pages[i]) {
			ret = -ENOMEM;
			goto err;
		}
		ret = vm_insert_page(vma, vma->vm_start + (i << PAGE_SHIFT), p->pages[i]);
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

	x = nvrm_xfer_alloc(sizeof(*r) + (size_t)nruns * sizeof(*runs), sizeof(*rsp));
	if (IS_ERR(x)) {
		kvfree(runs);
		ret = PTR_ERR(x);
		goto err;
	}
	r = nvrm_req_init(dev, x, NVRM_KIND_UVM_POOL_BACK, ctx->proc ? ctx->proc->id : 0);
	r->dev_tag = ctx->dev_tag;
	r->target_token = ctx->token;
	r->addr = gpu_va;
	r->map_len = len;
	r->gpa_run_count = nruns;
	r->aux_len = nruns * sizeof(*runs);
	memcpy((u8 *)x->req + sizeof(*r), runs, (size_t)nruns * sizeof(*runs));
	x->req_len = sizeof(*r) + (size_t)nruns * sizeof(*runs);
	kvfree(runs);

	ret = nvrm_xfer_run(dev, x, true);
	if (ret) {
		if (ret == -ERESTARTSYS || ret == -ETIMEDOUT)
			x = NULL;
		goto err_xfer;
	}
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
	/* Full teardown BEFORE returning: the kernel discards the half-built
	 * VMA (returning the references inserted into it); the references held
	 * by this module are dropped here. */
	kref_put(&p->ref, pool_release);
	return ret;
}

static int nvrm_node_mmap(struct file *filp, struct vm_area_struct *vma)
{
	struct nvrm_ctx *ctx = filp->private_data;
	size_t len = vma->vm_end - vma->vm_start;
	u64 off = (u64)vma->vm_pgoff << PAGE_SHIFT;

	if (!ctx || !ctx->dev || !ctx->dev->tbl.blob)
		return -ENODEV;
	if (!len)
		return -EINVAL;

	if (ctx_is_uvm(ctx)) {
		if (!off)
			return -EOPNOTSUPP;
		return nvrm_mmap_pool(ctx, vma, off, len);
	}
	/* Frontend: the offset is always 0 -- WHICH mapping is meant comes from
	 * the preceding RM_MAP_MEMORY on the same FD. Anything else is already
	 * rejected by the real driver. */
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

/* ------------------------------------------------------------------ *
 * Create the nodes (the LAST step) and tear them down (mirrored)
 * ------------------------------------------------------------------ */

struct chrdev_range {
	unsigned int major, baseminor, count;
	const char *name;
	bool registered;
};

static struct chrdev_range ranges[] = {
	{ NV_FRONTEND_MAJOR, 0,		   NV_MAX_GPUS, "nvidia" },
	{ NV_FRONTEND_MAJOR, NV_MINOR_CTL, 1,		"nvidiactl" },
	{ NV_UVM_MAJOR,	     0,		   2,		"nvidia-uvm" },
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
			device_destroy(nvrm_class, MKDEV(nodes[i].major, nodes[i].minor));
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
			__unregister_chrdev(ranges[i].major, ranges[i].baseminor,
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
			       ranges[i].major, ranges[i].baseminor, ranges[i].name, ret);
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
		nodes[n_nodes++] = (struct node_spec){ NV_FRONTEND_MAJOR, g, gpu_names[g] };
	}
	nodes[n_nodes++] = (struct node_spec){ NV_FRONTEND_MAJOR, NV_MINOR_CTL, "nvidiactl" };
	nodes[n_nodes++] = (struct node_spec){ NV_UVM_MAJOR, 0, "nvidia-uvm" };
	nodes[n_nodes++] = (struct node_spec){ NV_UVM_MAJOR, 1, "nvidia-uvm-tools" };

	for (i = 0; i < n_nodes; i++) {
		nodes[i].dev = device_create(nvrm_class, NULL,
					     MKDEV(nodes[i].major, nodes[i].minor),
					     NULL, "%s", nodes[i].name);
		if (IS_ERR(nodes[i].dev)) {
			ret = PTR_ERR(nodes[i].dev);
			pr_err("virtio_nvrm: device_create %s: %d\n", nodes[i].name, ret);
			goto err;
		}
		nodes[i].created = true;
	}
	return 0;

err:
	nvrm_nodes_teardown();
	return ret;
}

/* ------------------------------------------------------------------ *
 * probe / remove
 * ------------------------------------------------------------------ */

/* Defined further down, with the vblank engine it belongs to. */
static void vblank_engine_init(void);

/*
 * Queue discovery for one or two queues. The transport API changed shape in
 * 6.11 (virtqueue_info[] instead of parallel callback/name arrays); the
 * measured guest kernel is 6.8 and the nix package builds against 6.12
 * (nix/packages/guest-modules.nix says which newer kernels do NOT build
 * yet), so both forms are needed. On failure the transport has already
 * deleted whatever it set up, and the one-queue request is a fresh attempt.
 */
static int nvrm_find_vqs(struct nvrm_dev *dev)
{
	struct virtio_device *vdev = dev->vdev;
	struct virtqueue *vqs[2];
	int ret;

#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 11, 0)
	{
		vq_callback_t *cbs[2] = { nvrm_vq_cb, nvrm_evq_cb };
		const char * const names[2] = { "nvrm", "nvrm-events" };

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

/* Post the receive buffers on the event queue. AFTER virtio_device_ready
 * (see the probe for why), then one kick. Each buffer is one nvrm_req; the callback
 * re-posts the same buffer, so these NVRM_EVQ_BUFS blocks live until
 * nvrm_evq_drain detaches them. */
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

static int nvrm_probe(struct virtio_device *vdev)
{
	struct nvrm_dev *dev;
	struct virtio_shm_region shm;
	u64 token = 0;
	int ret;

	if (nvrm)
		return -EBUSY;

	dev = kzalloc(sizeof(*dev), GFP_KERNEL);
	if (!dev)
		return -ENOMEM;
	dev->vdev = vdev;
	/* Before the device can carry a single kapi call: the vblank engine's
	 * timer exists from here on. A lazy init raced -- see the vblank
	 * section. */
	vblank_engine_init();
	spin_lock_init(&dev->vq_lock);
	spin_lock_init(&dev->evq_lock);
	xa_init(&dev->ctx_xa);
	INIT_WORK(&dev->events_work, nvrm_events_work);
	mutex_init(&dev->win_lock);
	mutex_init(&dev->proc_lock);
	INIT_LIST_HEAD(&dev->procs);
	idr_init(&dev->proc_idr);
	bdf_init(dev);
	init_waitqueue_head(&dev->vq_space);
	init_waitqueue_head(&dev->drain);
	atomic_set(&dev->seq, 0);
	atomic_set(&dev->inflight, 0);
	vdev->priv = dev;

	/* Two queues: 0 carries requests and their answers, 1 carries the
	 * host's KIND_EVENT_FIRED (event section). A device that offers only
	 * one queue -- an older backend, or a VMM started with
	 * queue_sizes=[256] -- still works, minus the events: that is the
	 * state before the channel existed, and it is said out loud. */
	ret = nvrm_find_vqs(dev);
	if (ret)
		goto err_free;
	/* The event inbufs are posted AFTER virtio_device_ready, below -- not
	 * here. Measured 2026-08-15 (the queue held 64 buffers then; it is
	 * NVRM_EVQ_BUFS now): posted before it, the buffers were
	 * in the avail ring at the moment the VMM enabled the vring, and the
	 * host's queue state took that avail index as its STARTING point
	 * (next_avail=64 on the very first kick, next_used=0, "guest posted no
	 * inbuf" for every firing). virtio-net fills its receive rings the
	 * same way for the same reason. */

	/* The host-visible window. A non-GPU device gets one for free: the
	 * generic vhost-user SHMEM patch for Cloud Hypervisor
	 * (patches/0001-generic-vhost-user-shmem.patch) is not tied to
	 * virtio-gpu. */
	if (virtio_get_shm_region(vdev, &shm, NVRM_SHM_ID_HOST_VISIBLE)) {
		dev->win_base = shm.addr;
		dev->win_len = shm.len;
		dev->win_bitmap = bitmap_zalloc(shm.len >> PAGE_SHIFT, GFP_KERNEL);
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
	nvrm = dev;
	if (dev->evq) {
		ret = nvrm_evq_fill(dev);
		if (ret) {
			pr_warn("virtio_nvrm: event inbufs: %d -- events disabled\n", ret);
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

	/* Externally visible registrations as the LAST step: any earlier and
	 * there would be a node that does not have its tables yet. */
	if (create_nodes) {
		ret = nvrm_nodes_setup();
		if (ret)
			goto err_tables;
	}

	pr_info("virtio_nvrm: ready (nodes %s, %u GPU%s, max_pin %u MiB)\n",
		create_nodes ? "on" : "off", gpu_count, gpu_count > 1 ? "s" : "",
		max_pin_mib);
	return 0;

err_tables:
	kvfree(dev->tbl.blob);
	memset(&dev->tbl, 0, sizeof(dev->tbl));
err_win:
	nvrm = NULL;
	bitmap_free(dev->win_bitmap);
err_vq:
	virtio_reset_device(vdev);
	nvrm_evq_drain(dev);
	vdev->config->del_vqs(vdev);
err_free:
	xa_destroy(&dev->ctx_xa);
	mutex_destroy(&dev->win_lock);
	kfree(dev);
	vdev->priv = NULL;
	return ret;
}

/* Defined with the rest of the kernel RM API, below. */
static void kapi_session_close(void);

static void nvrm_remove(struct virtio_device *vdev)
{
	struct nvrm_dev *dev = vdev->priv;

	/* Mirror image of setup: visibility goes first, transport second. Open
	 * FDs hold the module refcount via .owner, so nobody can be in the
	 * middle of a call here -- except abandoned requests, which are waited
	 * for below. */
	nvrm_nodes_teardown();
	/* The NVKMS session hangs off no file, so no fd holds it: it has to be
	 * given back by hand, and while the device still answers. Without this
	 * its process entry would still be in dev->procs at the WARN_ON below
	 * -- which is exactly what that check is for. */
	kapi_session_close();
	nvrm = NULL;

	wait_event_timeout(dev->drain, atomic_read(&dev->inflight) == 0, 5 * HZ);
	if (atomic_read(&dev->inflight))
		pr_warn("virtio_nvrm: %d requests still outstanding\n", atomic_read(&dev->inflight));

	virtio_reset_device(vdev);
	/* After the reset no vq callback runs any more, so nothing queues the
	 * work again; what is queued finishes here. Whatever is still in the
	 * ring finds no slot (kapi_session_close dropped them) and no fd
	 * (none open, see below) -- counted, not called. */
	cancel_work_sync(&dev->events_work);
	nvrm_evq_drain(dev);
	vdev->config->del_vqs(vdev);
	kvfree(dev->tbl.blob);
	bitmap_free(dev->win_bitmap);
	/* Open FDs hold the module refcount, so no process entry can be left --
	 * the IDR is torn down explicitly anyway, so that a leak would show up
	 * instead of staying silent. */
	WARN_ON(!list_empty(&dev->procs));
	/* Same argument for the token index: every entry is an open fd. */
	WARN_ON(!xa_empty(&dev->ctx_xa));
	xa_destroy(&dev->ctx_xa);
	idr_destroy(&dev->proc_idr);
	mutex_destroy(&dev->proc_lock);
	mutex_destroy(&dev->win_lock);
	kfree(dev);
	vdev->priv = NULL;
	pr_info("virtio_nvrm: removed\n");
}

/* ------------------------------------------------------------------ *
 * kernel RM API -- the second door, for NVKMS
 *
 * The "kapi door" everything below is named after: not a device node but a
 * function table, the one nvidia-modeset.ko (NVKMS) fetches from nvidia.ko
 * at load time. Same escapes, same NVOS blocks, kernel memory instead of a
 * process's.
 * ------------------------------------------------------------------ */
/*
 * nvidia-modeset.ko links against exactly ONE symbol of nvidia.ko:
 * nvidia_get_rm_ops (measured -- `nm -u` lists 104 undefined symbols, and
 * the other 103 are the kernel's own). Everything NVKMS asks of RM travels
 * through the op() member of the table that call fills in, as an
 * nvidia_kernel_rmapi_ops_t -- the same NVOS parameter blocks that the
 * /dev/nvidiactl escapes carry.
 *
 * So this is not a second protocol. It is a second CALLER on the one that
 * already runs: same tables, same virtqueue, same host session machinery.
 * The interpreter above serves it unchanged; only call_in()/call_out() know
 * that the memory is the kernel's rather than a process's.
 *
 * WHY IT LIVES HERE and not in a module of its own: a separate module would
 * need its own handle on the virtqueue, which means either exporting the
 * transport (a second public API to keep in step) or opening a second
 * queue (a second truth about who is talking to the host). One module, one
 * queue, one session table -- and the marshalling that CUDA depends on is
 * shared rather than copied.
 */

/* The NVKMS session: one host-side /dev/nvidiactl, held for as long as
 * nvidia-modeset.ko is loaded. Opened lazily -- nvidia_get_rm_ops() may be
 * called before anything has proved the device is alive. */
static struct nvrm_ctx *kapi_ctx;
static DEFINE_MUTEX(kapi_lock);
static const struct nvrm_modeset_callbacks *kapi_callbacks;
/* Our own RM root client -- see kapi_client_ensure() below. Declared here
 * because kapi_session_close() gives it back. */
static u32 kapi_client;
/* The process identity every NVKMS session shares -- control node and each
 * GPU node alike. */
static struct nvrm_proc *kapi_proc;

/*
 * Per-GPU sessions, opened by open_gpu().
 *
 * NVIDIA's header calls open_gpu "equivalent to opening and closing a
 * /dev/nvidiaN device file from user-space", and that is taken literally
 * here: a session on the GPU node.
 *
 * `index` is the position the GPU had in enumerate_gpus(), which is the
 * order RM reports in GET_PROBED_IDS. For the single GPU this rig has, that
 * is 0 and it is /dev/nvidia0. For several GPUs the mapping from RM's
 * probe order to the guest's node numbering is UNPROVEN -- so an unknown
 * gpu_id fails loudly rather than guessing an index.
 */
struct kapi_gpu {
	u32 gpu_id;
	u32 index;
	struct nvrm_ctx *ctx;
	unsigned int refs;
};
static struct kapi_gpu kapi_gpus[NVRM_NV_MAX_GPUS];
static u32 kapi_gpu_count;

/*
 * A process entry for a caller that is not a process.
 *
 * The host keeps one session per guest_proc id, and NVKMS deserves its own:
 * its RM handles are not the handles of whichever process happened to run
 * insmod. `pid` stays NULL, which is also what keeps nvrm_proc_get() from
 * ever handing this entry to a real process -- that lookup compares against
 * a `struct pid *` it just took, and that is never NULL.
 */
static struct nvrm_proc *nvrm_proc_kernel(struct nvrm_dev *dev)
{
	struct nvrm_proc *p;
	int id;

	p = kzalloc(sizeof(*p), GFP_KERNEL);
	if (!p)
		return ERR_PTR(-ENOMEM);

	mutex_lock(&dev->proc_lock);
	id = idr_alloc(&dev->proc_idr, p, 1, 0, GFP_KERNEL);
	if (id < 0) {
		mutex_unlock(&dev->proc_lock);
		kfree(p);
		return ERR_PTR(id);
	}
	p->pid = NULL;
	p->id = (u32)id;
	p->vnr = 0;
	strscpy(p->comm, "nvidia-modeset", sizeof(p->comm));
	refcount_set(&p->ref, 1);
	list_add(&p->node, &dev->procs);
	mutex_unlock(&dev->proc_lock);
	return p;
}

/* Open the NVKMS session, or return the one already open. Caller holds
 * kapi_lock. */
static struct nvrm_ctx *kapi_ctx_open(struct nvrm_dev *dev, u32 dev_tag, u32 index)
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

	/*
	 * ONE process identity for every session NVKMS holds -- the control
	 * node and each GPU node. Not a shortcut: it is the rule this module
	 * already states for guest processes, that the same process gets one
	 * entry for all of its nodes because its RM handles live in one
	 * session.
	 */
	if (kapi_proc) {
		refcount_inc(&kapi_proc->ref);
		ctx->proc = kapi_proc;
	} else {
		ctx->proc = nvrm_proc_kernel(dev);
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

	ret = nvrm_simple_info(dev, NVRM_KIND_OPEN, ctx->dev_tag, ctx->gpu_index,
			       0, 0, 0, &token, false, ctx->proc->id, &info);
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
		if (ctx->proc == kapi_proc && refcount_read(&kapi_proc->ref) == 1)
			kapi_proc = NULL;
		nvrm_proc_put(dev, ctx->proc);
	}
	mutex_destroy(&ctx->lock);
	kfree(ctx);
	return ERR_PTR(ret);
}

static void kapi_ctx_close(struct nvrm_ctx *ctx)
{
	struct nvrm_pin *p, *tmp;

	if (!ctx)
		return;
	/* What nvrm_node_release does for a process context, for the same
	 * reason: a PRIME import (gather_osdesc_kern) hangs its dma-buf
	 * attachment off the context, and RM's teardown on the host does not
	 * give a guest-side attachment back. Nothing else runs on this
	 * context here (kapi_lock is held or the device is being removed). */
	list_for_each_entry_safe(p, tmp, &ctx->pins, node) {
		list_del(&p->node);
		nvrm_unpin(p);
	}
	nvrm_simple(ctx->dev, NVRM_KIND_CLOSE, ctx->dev_tag, 0, ctx->token, 0, 0,
		    NULL, false, ctx->proc ? ctx->proc->id : 0);
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

/* The control session. Caller holds kapi_lock. */
static struct nvrm_ctx *kapi_session(void)
{
	struct nvrm_ctx *ctx;

	if (kapi_ctx)
		return kapi_ctx;
	/* The control node: RM's "any client" door, and the one the in-kernel
	 * API corresponds to -- rm_kernel_rmapi_op() is bound to no device
	 * file at all. */
	ctx = kapi_ctx_open(nvrm, NVRM_DEV_CTL, 0);
	if (IS_ERR(ctx))
		return ctx;
	kapi_ctx = ctx;
	pr_info("virtio_nvrm: NVKMS session open (guest_proc %u)\n", ctx->proc->id);
	return ctx;
}

/* ===========================================================================
 * The virtual display: NVA083_GRID_DISPLAYLESS, answered here
 * ===========================================================================
 *
 * Six controls on the object, and NVKMS derives the rest -- it was measured
 * asking only three of them (see the `vdisplay` comment). Nothing here talks
 * to the host: there is no card state behind a display that does not exist.
 */

/* One head. NVIDIA reports the same for the displayless path
 * (GRID_DISPLAYLESS_NUM_HEADS, objgriddisplayless.c:35) and three controls
 * have to agree about it, so it is named once. */
#define NVRM_VDISP_NUM_HEADS 1u

/* The EDID builder lives in its own translation unit so a userspace test
 * can run edid-decode over exactly these bytes. See nvrm_edid.c. */
#include "nvrm_edid.c"

/*
 * The one object this module OWNS rather than forwards.
 *
 * A handle of our own invention lives in the same number space as RM's.
 * NVKMS picks the handle (hObjectNew is an IN field for NV04_ALLOC), so there
 * is no collision to arrange -- but every later call naming it must be caught
 * here, because RM has never heard of it. That is the whole reason this is a
 * single handle and not a table: one virtual display, one object, and a call
 * that names it either is ours or is a bug.
 */
static u32 vdisp_handle;
/*
 * And the CLIENT it belongs to, because a handle alone does not name an
 * object. RM books objects under (hClient, hObject), and the kernel clients
 * behind this door hand their handles out from the SAME sequence: NVKMS core
 * and nvidia-drm's KAPI client both start at 0x10001 (unix_rm_handle.c:214
 * with clientData 1 on either side), so the number NVKMS picked for its
 * NVA083 is a number nvidia-drm will pick too, for something else entirely.
 *
 * Measured 2026-08-17, and it is the whole Vulkan-under-Wayland failure:
 *   ALLOC client 0xc1d0007f handle 0x1000d class 0x40 -> status 0x0 (host)
 *   FREE  client 0xc1d0007f handle 0x1000d          -> (vdisp, NOT sent)
 * nvidia-drm allocated vidmem at 0x1000d in ITS client, and the free was
 * swallowed here because NVKMS's NVA083 in a DIFFERENT client carried the
 * same number. The host kept the object, the guest's handle generator
 * recycled the number, and the next allocation of it came back
 * NV_ERR_INSERT_DUPLICATE_NAME (0x19) -- which is where every Vulkan client
 * under Wayland died. `vblank_free` had already spelled this out for the
 * NV9010 slots; this object never learned it.
 */
static u32 vdisp_client;
/*
 * The display object the displayless path issues a handle for and never
 * allocates (nvkms-evo.c:5223). Learned rather than guessed: it is the
 * parent of the one event RM answers with OBJECT_NOT_FOUND, and everything
 * NVKMS addresses to it afterwards meets the same emptiness.
 *
 * Client-qualified for the same reason as vdisp_handle above.
 */
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
	/* BOTH fields -- see vdisp_client. Matching the handle alone swallows
	 * another client's free, and the object it names leaks on the host. */
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

/*
 * The six controls NVA083 defines, answered here. Measured 2026-08-08: the
 * displayless path calls exactly three of them -- GET_NUM_HEADS,
 * GET_MAX_RESOLUTION, GET_EDID; IS_ACTIVE, IS_CONNECTED and GET_MAX_PIXELS
 * exist in the header and nowhere else. They are answered anyway, because a
 * refusal from an object we claim to own is a worse answer than the truth.
 */
static bool vdisp_control(u8 *params)
{
	u32 client = rd32(params, NVRM_NVOS54_HCLIENT_OFF);
	u32 cmd = rd32(params, NVRM_NVOS54_CMD_OFF);
	u32 obj = rd32(params, NVRM_NVOS54_HOBJECT_OFF);
	u32 size = rd32(params, NVRM_NVOS54_PARAMSSIZE_OFF);
	void *p = (void *)(uintptr_t)rd64(params, NVRM_NVOS54_PARAMS_OFF);
	bool ours;

	if (!vdisplay)
		return false;
	mutex_lock(&vdisp_lock);
	/* Handle AND client -- same collision as in vdisp_free. Answering a
	 * foreign client's control on a same-numbered object of its own would
	 * report a success RM never gave. */
	ours = vdisp_handle && obj == vdisp_handle && client == vdisp_client;
	mutex_unlock(&vdisp_lock);
	if (!ours) {
		/* A call addressed to the display object that was never
		 * allocated. NVKMS keeps talking to it -- SET_NOTIFICATION for
		 * the vblank callback is the first -- and RM keeps answering
		 * OBJECT_NOT_FOUND. Answering here says the same thing the
		 * displayless path already assumes: there is no raster
		 * generator, so there is nothing to notify about. */
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

		/*
		 * Three more calls that are not on our object and still ours
		 * to answer.
		 *
		 * nvAllocCoreChannelEvo blocks GC6 before it touches the
		 * display (`nvRmSetGc6Allowed`, nvkms-evo.c:5199) and takes
		 * `goto failed` when that is refused. RM refuses it here:
		 * measured 2026-08-08 as status 0x1b
		 * NV_ERR_INSUFFICIENT_PERMISSIONS, and CAP_SYS_ADMIN does NOT
		 * lift it -- the control is kernel-privileged.
		 *
		 * What is being asked for is a refcount that keeps the card out
		 * of a power state while a display is programmed. This guest
		 * programs no display and scans nothing out, and the card is
		 * driving the host's own monitors meanwhile, so GC6 is not a
		 * state it can reach. Answering NV_OK claims a block we did not
		 * take; what it costs is bounded by that.
		 */
		switch (cmd) {
		case NVRM_CTRL_GC6_BLOCKER:
		case NVRM_CTRL_VT_SWITCH:
		case NVRM_CTRL_VT_GET_FB_INFO:
			/* All three are kernel-privileged and all three are
			 * about state this guest does not have: a power block
			 * for a display it never programs, and the console
			 * framebuffer of a card it holds no console on. The
			 * params stay as NVKMS zeroed them, which reads back
			 * as "no console" -- the truth here. */
			wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_OK);
			if (display > 1)
				pr_info("virtio_nvrm: virtual display: %#x answered OK (nothing to do)\n",
					cmd);
			return true;
		default:
			return false;
		}
	}

	/* The params block of a KERNEL caller is a kernel pointer we may
	 * dereference directly -- unlike the ioctl path, where it belongs to
	 * a process. A NULL one is a caller bug, not something to guess at. */
	if (!p || !size) {
		wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return true;
	}

	switch (cmd) {
	case NVRM_CTRL_VD_GET_NUM_HEADS:
		/* { NvU32 numHeads; NvU32 maxNumHeads; } -- NVKMS reads the
		 * SECOND field (nvkms-rm.c:946). One head, which is also what
		 * NVIDIA reports for the displayless path
		 * (GRID_DISPLAYLESS_NUM_HEADS, objgriddisplayless.c:35). */
		if (size < 8)
			goto too_small;
		wr32(p, 0, NVRM_VDISP_NUM_HEADS);
		wr32(p, 4, NVRM_VDISP_NUM_HEADS);
		break;
	case NVRM_CTRL_VD_GET_MAX_RES:
		/* { headIndex; maxHResolution; maxVResolution; }
		 *
		 * The MAXIMUM, not the mode. See the vdisplay_max_* comment:
		 * NVKMS turns this into pDevEvo->caps and bounds every surface
		 * by it, so answering with the offered mode would make that
		 * mode the ceiling too.
		 *
		 * headIndex is an IN field and is validated the way NVIDIA
		 * validates it (griddisplaylessctrl.c: headIndex >= numHeads
		 * is NV_ERR_INVALID_ARGUMENT). NVKMS passes 0. */
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
		/* { NvBool isDisplayActive; }
		 *
		 * A DECISION, not a reading. At NVIDIA this is a back
		 * channel: displayActive[] is only ever set by
		 * griddisplaylessUpdateDisplayActive, from the host, when a
		 * console attaches -- and it defaults to NV_FALSE. There is no
		 * such host here, so a faithful "false" would mean "nothing is
		 * driving this screen", which is the opposite of true for a
		 * display whose whole purpose is to be driven.
		 *
		 * Nothing in NVKMS reads it (grep: no caller in
		 * nvidia-modeset or nvidia-drm), so this costs nothing today
		 * and is a claim only if some other client believes it. */
		if (size < 1)
			goto too_small;
		((u8 *)p)[0] = 1;
		break;
	case NVRM_CTRL_VD_IS_CONNECTED:
		/* { NvU32 isDisplayConnected; } -- numHeads > 0 at NVIDIA
		 * (griddisplaylessctrl.c), and there is one head. */
		if (size < 4)
			goto too_small;
		wr32(p, 0, NVRM_VDISP_NUM_HEADS > 0);
		break;
	case NVRM_CTRL_VD_GET_MAX_PIXELS:
		/* { NvU64 maxPixels; } -- a bound of its own beside the
		 * resolution, see the vdisplay_max_pixels comment. */
		if (size < 8)
			goto too_small;
		wr64(p, 0, vdisplay_max_pixels);
		break;
	case NVRM_CTRL_VD_GET_EDID: {
		/* { NvP64 pEdidBuffer; NvU32 edidSize; NvU8 connectorType; }
		 *
		 * `edidSize` is in/out and it, not the buffer pointer, is what
		 * decides. NVIDIA's own handler
		 * (griddisplaylessGetDefaultEDID_IMPL,
		 * objgriddisplayless.c:296-334) reads:
		 *
		 *   size == 0             report the size, write nothing
		 *   size <  actual        NV_ERR_BUFFER_TOO_SMALL
		 *   buffer == NULL        NV_ERR_INVALID_ARGUMENT
		 *   otherwise             copy
		 *
		 * and sets the size on EVERY path, including the failing ones.
		 * Mirrored exactly. Deciding on the pointer instead -- what
		 * this did before -- means a caller that passes a real buffer
		 * with a size smaller than the EDID gets the full EDID written
		 * into it, which is a write past the end of somebody else's
		 * allocation. NVKMS itself always allocates what we reported
		 * (nvkms-dpy.c:1245), so it could not trigger that; the next
		 * caller is not promised to be NVKMS.
		 *
		 * connectorType is ignored. NVIDIA keeps a digital and an
		 * analog blob with the same set of modes
		 * (objgriddisplayless.c:87), so one EDID answers both. */
		u64 buf;
		u32 want;

		if (size < 16)
			goto too_small;
		buf = rd64(p, 0);
		want = rd32(p, 8);
		wr32(p, 8, NVRM_EDID_LEN);
		if (want == 0)
			break;			/* size query, nothing to write */
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
		pr_info("virtio_nvrm: virtual display: control %#x answered\n", cmd);
	wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_OK);
	return true;

too_small:
	pr_warn("virtio_nvrm: virtual display: control %#x with %u bytes of params\n",
		cmd, size);
	wr32(params, NVRM_NVOS54_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
	return true;
}

/*
 * The entry point, and the only place a REAL answer is edited.
 *
 * `nvRmAllocDisplays` checks NV04_DISPLAY_COMMON first and this card has it,
 * so the displayless branch is unreachable until 0x0073 leaves the list.
 * One out, one in -- numClasses does not change, so the counting call (the
 * one with a NULL buffer) needs no handling at all.
 */
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

	/* NV0080_CTRL_GPU_GET_CLASSLIST_PARAMS: { NvU32 numClasses;
	 * NvP64 classList; } -- the pointer is 8-aligned, so it sits at 8.
	 * The FIRST of the two calls carries no buffer and only asks for the
	 * count; it needs nothing from us, because the count it reports is
	 * an upper bound and the second call reports the real one. */
	n = rd32(p, 0);
	buf = rd64(p, 8);
	if (!buf || !n)
		return;

	list = (u32 *)(uintptr_t)buf;
	for (i = 0; i < n; i++)
		if (list[i] == NVRM_CLASS_DISPLAYLESS)
			have_displayless = true;
	if (have_displayless)
		return;			/* already done, or a card that has it */

	/* Compact in place: every class NVKMS would choose AHEAD of the
	 * displayless one has to go, or it picks that HAL and then runs with
	 * displaylessHw set and a real dispClass -- measured as a device
	 * allocation that fails without printing anything. */
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
		pr_warn_ratelimited("virtio_nvrm: virtual display: nothing to drop from the class list -- left alone\n");
		return;
	}
	list[out++] = NVRM_CLASS_DISPLAYLESS;
	wr32(p, 0, out);
	pr_info("virtio_nvrm: virtual display: class list %u -> %u (dropped %u display classes, added %#x)\n",
		n, out, dropped, NVRM_CLASS_DISPLAYLESS);
}

/*
 * The one event on this path whose PARENT does not exist.
 *
 * NVKMS registers an RG vblank callback against `pDevEvo->displayHandle`
 * (nvkms-rm.c:5338) -- and the displayless path never allocates that object:
 * nvkms-evo.c:5223 skips the alloc under `if (!displaylessHw)` while the
 * handle number is issued regardless. RM therefore answers
 * NV_ERR_OBJECT_NOT_FOUND, and NVKMS turns that into "Failed to register RM
 * callback" and gives up on the device.
 *
 * Measured 2026-08-08: of the five event allocations on this path, four
 * carry a real parent and answer 0x0; exactly this one answers 0x57.
 *
 * What answering NV_OK claims: that a callback is registered which will
 * never be called. On this path that is already true of the thing it would
 * report -- there is no raster generator and no vblank, because nothing is
 * scanned out.
 *
 * And the flip path does not want one. The displayless HAL drives its
 * flips from a POLLING worker, not from an interrupt: DisplaylessFlipWorker
 * (nvkms-displayless.c:304) re-arms an nvkms_alloc_timer every
 * DISPLAYLESS_POLL_INTERVAL_USEC, which is 100 us, for as long as
 * ProcessPendingFlips still has anything queued, and stops arming it when
 * the queue drains. Nothing in that loop waits. So this registration is
 * bookkeeping for an interrupt that cannot arrive AND is not wanted, and
 * the missing host-to-guest event path (nvrm_node_poll, which returns 0 and
 * nothing ever wakes) does not block the displayless path.
 *
 * What that worker DOES need is a CPU mapping: it reads the flip
 * semaphore through pSurfaceEvo->cpuAddress[0]
 * (nvkms-displayless.c:265), and DisplaylessEnqueueFlip refuses the flip
 * outright with "Semaphore surface without CPU mapping!" if it is NULL.
 * That mapping is a flags-0 nvEvoCpuMapSurface, i.e. the kernel-VA branch
 * in kapi_map_memory.
 *
 * Narrow on purpose: only with `vdisplay`, only for a class the HOST wrote
 * (NV01_EVENT_OS_EVENT -- NVKMS itself never asks for that one, it asks for
 * the kernel-callback classes), and only for OBJECT_NOT_FOUND.
 */
static void vdisp_event_on_missing_parent(u8 *params)
{
	if (!vdisplay)
		return;
	if (rd32(params, NVRM_NVOS64_HCLASS_OFF) != NVRM_CLASS_EVENT_OS_EVENT)
		return;
	if (rd32(params, NVRM_NVOS64_STATUS_OFF) != NVRM_NV_ERR_OBJECT_NOT_FOUND)
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

/* ===========================================================================
 * Vblank for the virtual display: NV9010_VBLANK_CALLBACK, answered here
 * ===========================================================================
 *
 * `NV_VBLANK_CALLBACK_ALLOCATION_PARAMETERS` carries `NvP64 pProc` -- "Routine
 * to call at vblank time" (cl9010.h), a FUNCTION POINTER. Forwarding it would
 * hand the host a guest kernel address, which means nothing there; that is why
 * the class is deliberately absent from the tables (OPEN-QUESTIONS nr 7).
 * But on the KERNEL path the caller is NVKMS
 * (nvRmAddVBlankCallback, nvkms-rm.c:5141) and pProc is a live address in
 * nvidia-modeset.ko -- callable from HERE, in the guest. So this module is
 * the raster generator: an hrtimer at the virtual display's refresh rate does
 * what the real RM does from its vblank ISR (_vblankCallback,
 * vblank_callback.c:36):
 *
 *     if (bIsVblankNotifyEnable) pProc(pParm1, pParm2);
 *
 * The calling context matches by construction. RM fires this from interrupt
 * level; NVKMS's pProc (VBlankCallback, nvkms-modeset.c:1922) therefore only
 * re-arms a timer whose allocation is "called from an interrupt bottom half"
 * (kmalloc(GFP_ATOMIC), nvidia-modeset-linux.c:1149). An hrtimer callback is
 * exactly that level.
 *
 * Only the kernel path. A userspace 0x9010 alloc (the ioctl door) still
 * meets the table's EOPNOTSUPP: a process-supplied function pointer is not
 * something a kernel may ever call, and RM itself refuses the class below
 * RS_PRIV_LEVEL_KERNEL for the same reason.
 *
 * pProc is called UNDER vblank_lock. That is what makes FREE a fence, the
 * same guarantee RM gives: once the free returns, the callback cannot be in
 * flight and will not fire again -- NVKMS may release what pParm1 points to.
 * The callee takes only nvkms' own timer spinlock underneath, never this
 * module's locks, so the order vblank_lock -> nvkms_timers.lock is the only
 * one that exists.
 */

/* One head (NVRM_VDISP_NUM_HEADS); the slots are for CLIENTS of that head:
 * NVKMS registers one RG callback per head, vblank-sem-control and the
 * headsurface can add their own. Eight is headroom, not a measurement. */
#define NVRM_VBLANK_SLOTS 8u
struct vblank_slot {
	u32 handle;		/* 0 = free */
	u32 client;		/* hRoot, for the log */
	u64 proc;		/* OSVBLANKCALLBACKPROC in guest kernel text */
	u64 parm1;
	u64 parm2;
	bool enabled;		/* bIsVblankNotifyEnable, NV_TRUE at construct */
};
static struct vblank_slot vblank_slots[NVRM_VBLANK_SLOTS];
/* The vblank callback (pProc) is CALLED under this lock, from the hrtimer.
 * Same contract as event_cb_lock (defined below, at the event engine): the callee must neither sleep nor come
 * back through the kapi door into vblank_alloc/vblank_free (both take this
 * lock); NVKMS's vblank handler queues work. Holding the lock across the
 * call is what makes vblank_free/vblank_drop_all a fence -- after they
 * return no callback is in flight and none will fire into freed
 * nvidia-modeset text. */
static DEFINE_SPINLOCK(vblank_lock);
static struct hrtimer vblank_timer;
static bool vblank_armed;	/* under vblank_lock */
/* Serialises arm/disarm transitions against each other. The 9010 traffic
 * itself is serialised by NVKMS (nvkms_lock), but the device-remove path
 * (kapi_session_close -> vblank_drop_all) is not -- and an unserialised
 * cancel racing an arm leaves a live slot with a dead timer. Process
 * context only; never taken in the tick. */
static DEFINE_MUTEX(vblank_engine_lock);

/* The period of ONE vblank, at the rate the display actually advertises.
 *
 * Not at `vdisplay_vblank_hz`. That is the REQUEST, and the EDID may not
 * be able to express it: the DTD carries the pixel clock in two bytes of
 * 10 kHz. Measured 2026-08-19 against nvrm_edid_effective itself, since
 * the numbers here are the whole point -- 3840x2160 at 120 Hz comes back
 * as 75, 1920x1080 at 240 Hz as 226, 2560x1440 at 240 Hz as 167. Pacing
 * the callbacks off the request while the mode says something else is the
 * exact bug this file used to have in the other direction -- 120 Hz
 * callbacks under a 60 Hz mode, because the EDID builder had 60 hard-coded
 * and never saw this parameter (2026-08-15). `nvrm_edid_effective` is the
 * one place that decides, and both readers ask it.
 *
 * 2560x1440 at 165 Hz used to be the example here, and it is not one any
 * more: the wide-blanking ceiling is 162 Hz, but the narrow-blanking (RB2)
 * fallback in nvrm_edid_effective lifts it to 167 and the request goes
 * through untouched. An example that has stopped being an example is how a
 * reader learns to distrust the rest of the comment.
 *
 * The old guard here was its own third opinion: reject outside 1..240 and
 * silently fall back to 60. A request of 300 then gave a 60 Hz timer and a
 * 254 Hz mode. Clamping in one place cannot produce that. */
static u32 vblank_hz(void)
{
	u32 w, h, hz, hb;

	nvrm_edid_effective(READ_ONCE(vdisplay_width), READ_ONCE(vdisplay_height),
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
		hrtimer_start(&vblank_timer, vblank_period(),
			      HRTIMER_MODE_REL);
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

	/* The alloc params of a KERNEL caller are a kernel pointer (NVKMS
	 * passes its own stack variable, nvkms-rm.c:5152). Layout per
	 * cl9010.h: pProc@0, LogicalHead@8, pParm1@16, pParm2@24. */
	ap = (const u8 *)(uintptr_t)rd64(params, NVRM_NVOS64_PALLOCPARMS_OFF);
	if (!ap) {
		wr32(params, NVRM_NVOS64_STATUS_OFF, NVRM_NV_ERR_INVALID_ARGUMENT);
		return true;
	}
	proc = rd64(ap, 0);
	head = rd32(ap, 8);
	parm1 = rd64(ap, 16);
	parm2 = rd64(ap, 24);

	if (!proc || head >= NVRM_VDISP_NUM_HEADS) {
		wr32(params, NVRM_NVOS64_STATUS_OFF, NVRM_NV_ERR_INVALID_ARGUMENT);
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
		.handle = handle, .client = client,
		.proc = proc, .parm1 = parm1, .parm2 = parm2,
		/* NV_TRUE at construct, exactly like vblcbConstruct_IMPL. */
		.enabled = true,
	};
	spin_unlock_irqrestore(&vblank_lock, flags);
	vblank_engine_update();

	wr32(params, NVRM_NVOS64_STATUS_OFF, NVRM_NV_OK);
	/* The EFFECTIVE rate, not the requested one: vdisplay_vblank_hz is
	 * what was asked for, and nvrm_edid_effective may have clamped it to
	 * what the EDID's fixed-width fields can express (3840x2160 at 120 Hz
	 * comes back as 75). The timer is paced off the clamped rate, so
	 * logging the request would name a rate nothing runs at -- in the one
	 * line an operator reads to find out what the callbacks do. */
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
		/* BOTH fields. Handles are per-client numbers, and the two
		 * kernel clients behind this door hand them out from the SAME
		 * sequence: NVKMS core and nvidia-drm's KAPI client both
		 * start at 0x10001 (unix_rm_handle.c:214 with clientData 1 on
		 * either side). A free matched on the handle alone would
		 * swallow nvidia-drm freeing its notifier -- the host object
		 * leaks and NVKMS's callback dies without a word. */
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
	const u8 *p = (const u8 *)(uintptr_t)rd64(params, NVRM_NVOS54_PARAMS_OFF);
	unsigned long flags;
	bool ours = false;
	u32 i;

	spin_lock_irqsave(&vblank_lock, flags);
	/* Handle AND client -- same collision as in vblank_free above. */
	for (i = 0; i < NVRM_VBLANK_SLOTS; i++)
		if (vblank_slots[i].handle && vblank_slots[i].handle == obj &&
		    vblank_slots[i].client == client)
			break;
	if (i < NVRM_VBLANK_SLOTS)
		ours = true;
	if (ours && cmd == NVRM_CTRL_SET_VBLANK_NOTIFY && p && size >= 1)
		/* { NvBool bSetVBlankNotifyEnable; } -- ctrl9010.h. The
		 * kernel caller's pointer, dereferenced directly like the
		 * vdisp controls above. */
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

/* ===========================================================================
 * The event return channel: KIND_EVENT_FIRED on queue 1
 * ===========================================================================
 *
 * How RM fires natively (os.c:1493-1553 osNotifyEvent): an NV01_EVENT_OS_EVENT
 * (0x79) becomes nv_post_event + wake_up_interruptible on the fd's wait queue
 * (nv.c:4036-4086), read back with NV_ESC_RM_GET_EVENT_DATA; a kernel
 * callback NV01_EVENT_KERNEL_CALLBACK_EX (0x7e) becomes a direct call
 * `kc->func(kc->arg, NULL, hEvent, Data, Status)` (os.c:1533-1539). The host
 * substitutes BOTH kernel-callback classes with 0x79 on its side (session.rs
 * (1a')), because a guest kernel address means nothing over there -- and
 * before this section, that was the end of it: 26 callbacks registered
 * in a GNOME session, none delivered, vkprobe DEVICE_LOST at frame 5.
 *
 * Now the host drains its side and sends one KIND_EVENT_FIRED per firing on
 * queue 1. The Req is the carrier; the fields are reused as documented in
 * nvrm-wire (KIND_EVENT_FIRED):
 *   ioctl_nr          the class the GUEST asked for (0x79 / 0x7e / 0x78)
 *   target_token      the fd to wake (0x79) or the fd the alloc rode on
 *   guest_proc        owner session of target_token
 *   inline_len        Data,   aux_len Status  (both 0/NV_OK on this path)
 *   fd_field_off      hClient (NVOS64.hRoot of the alloc)
 *   embedded_ptr_off  hEvent  (NVOS64.hObjectNew of the alloc)
 *   nested_count      notifyIndex as the guest sent it (NV0005 @12)
 *   addr              the 8 bytes the guest put in NV0005.data @16 BEFORE
 *                     the host overwrote them: for 0x7e the pointer to the
 *                     NVOS10_EVENT_KERNEL_CALLBACK_EX in guest kernel memory
 *
 * The 0x7e side is the vblank engine again (above): a slot table filled on
 * the kernel ALLOC path, a FREE that is a fence, and the guest pointer called
 * from a context RM would also call it from -- only that the trigger is the
 * host, not a timer.
 *
 * 0x78 (NV01_EVENT_KERNEL_CALLBACK) is NOT served: natively it calls
 * `callBackToMiniport(NV_GET_NV_STATE(pGpu))` with a HOST-side nv_state
 * (os.c:1517-1524) -- nothing in this guest can stand in for that. Dropped
 * and counted; NVKMS asks for the _EX form.
 */

/* Which Req field means what -- kept as macros so that a reader of the work
 * item sees the roles, not the carrier's field names. */
#define ev_class(r)	((r)->ioctl_nr)
#define ev_hclient(r)	((r)->fd_field_off)
#define ev_hevent(r)	((r)->embedded_ptr_off)
#define ev_data(r)	((r)->inline_len)
#define ev_status(r)	((r)->aux_len)
#define ev_notify(r)	((r)->nested_count)
#define ev_kc(r)	((r)->addr)

/* backend log: 26 kernel-callback events registered in one GNOME session
 * (nvidia-drm fences, NVKMS hotplug/completion per head). 64 is headroom, not
 * a measurement -- when it runs out the alloc still succeeds on the host,
 * and the log says which registration will never fire. */
#define NVRM_EVENT_CB_SLOTS 64u
struct event_cb_slot {
	u32 client;		/* NVOS64.hRoot */
	u32 handle;		/* NVOS64.hObjectNew; 0 = free */
	u32 cls;		/* what the guest asked for (0x7e) */
	u32 notify_index;	/* NV0005.notifyIndex, unstripped */
	u32 proc;		/* the NVKMS session's process id */
	u64 kc;			/* NVOS10_EVENT_KERNEL_CALLBACK_EX*, guest kernel VA */
};
static struct event_cb_slot event_cb_slots[NVRM_EVENT_CB_SLOTS];
/* Callback is invoked UNDER this lock (fence semantics, see event_cb_free).
 * Taken with irqsave from process context only -- the work item and the
 * kapi door -- never from the vq callback.
 *
 * The contract that comes with calling under a spinlock: a callback invoked
 * from event_fire_callback/semsurf_fire must not call back into this
 * module's kapi door synchronously (kapi_op -> semsurf_before_control or
 * event_cb_* would take this very lock and deadlock), and must not sleep.
 * NVIDIA's own RM invokes these callbacks inside its locks too, and every
 * NVKMS/nvidia-drm handler checked only queues work or takes its own
 * spinlock (see the comment at the call in event_fire_callback). The lock
 * is kept because it IS the fence: after event_cb_free/event_cb_drop_all
 * return, no callback is in flight and none will fire. */
static DEFINE_SPINLOCK(event_cb_lock);

/* Kernel ALLOC, BEFORE it is sent: what the host is about to overwrite.
 * NV0005 params of a KERNEL caller are a kernel pointer (nvRmRegisterCallback
 * passes its own struct, nvkms-rm.c:1721-1746). Layout: hParentClient@0,
 * hSrcResource@4, hClass@8, notifyIndex@12, data@16 (cl0005.h:40-46) --
 * offsets from nvrm_wire.h. READ only. The host rewrites hClass@8 and
 * data@16 and both come back rewritten (session.rs (1a'), write-back);
 * vdisp_event_on_missing_parent keys on exactly that and must keep seeing
 * the host's version. */
struct event_cb_pending {
	bool armed;
	u32 client;
	u32 notify_index;
	u64 kc;
};

static void event_cb_before_alloc(const u8 *params, struct event_cb_pending *pend)
{
	const u8 *ap;

	pend->armed = false;
	if (rd32(params, NVRM_NVOS64_HCLASS_OFF) != NVRM_CLASS_EVENT_KERNEL_CALLBACK_EX)
		return;
	ap = (const u8 *)(uintptr_t)rd64(params, NVRM_NVOS64_PALLOCPARMS_OFF);
	if (!ap)
		return;
	pend->client = rd32(params, NVRM_NVOS64_HROOT_OFF);
	pend->notify_index = rd32(ap, NVRM_NV0005_NOTIFYINDEX_OFF);
	pend->kc = rd64(ap, NVRM_NV0005_DATA_OFF);
	pend->armed = pend->kc != 0;
}

/* Kernel ALLOC, AFTER the answer: the host said yes, so the object exists
 * over there and will fire. Take a slot, keyed (client, handle) -- both, for
 * the reason vblank_free spells out: NVKMS core and nvidia-drm's client hand
 * out handles from the same sequence. */
static void event_cb_after_alloc(const u8 *params, const struct event_cb_pending *pend, long ret)
{
	u32 handle = rd32(params, NVRM_NVOS64_HOBJECTNEW_OFF);
	unsigned long flags;
	u32 proc;
	u32 i;

	if (!pend->armed)
		return;
	if (ret || rd32(params, NVRM_NVOS64_STATUS_OFF) != NVRM_NV_OK || !handle)
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
		.client = pend->client, .handle = handle,
		.cls = NVRM_CLASS_EVENT_KERNEL_CALLBACK_EX,
		.notify_index = pend->notify_index,
		.proc = proc, .kc = pend->kc,
	};
	stat_events_registered++;
	spin_unlock_irqrestore(&event_cb_lock, flags);
	if (display > 1)
		pr_info("virtio_nvrm: events: kernel callback %#x/%#x idx %#x kc %#llx -> slot %u\n",
			pend->client, handle, pend->notify_index,
			(unsigned long long)pend->kc, i);
}

/* Kernel FREE, BEFORE it is sent (NVOS00: hRoot@0, hObjectOld@8). The fence:
 * NVKMS frees the NVOS10 block right after the free returns
 * (nvKmsKapiFreeChannelEvent), and RM's own guarantee is that no callback
 * is in flight once the free is done -- the slot goes under the same lock
 * the callback runs under. Freeing the CLIENT itself takes every slot of
 * that client with it (RM does the same, rm_client_free_os_events). Not
 * "ours" in the vblank sense: the free still goes to the host. */
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

/* ---- semaphore-surface waiters ------------------------------------------
 *
 * The second door for kernel callbacks, and it is not an event object.
 * nvidia-drm's fences (DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CREATE, the sync_files
 * every GBM compositor waits on) are signalled by NVKMS registering a waiter
 * on the semaphore surface: control 0xda0003 with a POINTER to an
 * NVOS10_EVENT_KERNEL_CALLBACK_EX in `notificationHandle`
 * (nvkms-kapi-sync.c:432). Natively RM calls that pointer when the
 * semaphore reaches the value.
 *
 * Through this module the control reaches host RM from a USERSPACE client,
 * and for those RM reads the handle as an OS-event id
 * (osUserHandleToKernelPtr, os.c:1741-1767) -- a guest kernel VA is no such
 * id, the registration fails, and nvidia-drm's comment says what happens
 * next: "the fence timeout will be relied upon" -- except the timeout timer
 * is only armed AFTER a successful registration (nvidia-drm-fence.c:1059-
 * 1076). Measured 2026-08-16: weston frozen in eglSwapBuffers, seven
 * sync_files pending, poll(timeout=-1) forever.
 *
 * So the host backend substitutes an OS event of its own (session.rs, the
 * semsurf section) and forwards the firing as KIND_EVENT_FIRED with
 * hEvent = 0 -- no event object exists, which is exactly the marker -- and
 * the callback pointer in `addr`. This side keeps the (client, proc, kc)
 * slot and makes the call, one-shot, like RM would have.
 *
 * The slot is filled BEFORE the control goes out, not after the reply:
 * the semaphore can reach the value the moment RM registers the waiter, and
 * the firing then overtakes the reply on queue 1. A slot filled too late is
 * a dropped fire and a fence that never signals -- the bug this section
 * exists to fix, rebuilt one layer down. The unfired slot is taken back in
 * semsurf_after_control when RM said no.
 */
#define NVRM_SEMSURF_SLOTS 64u
struct semsurf_slot {
	bool used;
	u32 client;		/* NVOS54.hClient of the registration */
	u32 proc;		/* the NVKMS session's process id */
	u64 kc;			/* NVOS10_EVENT_KERNEL_CALLBACK_EX*, guest kernel VA */
	/*
	 * `func`/`arg` are read ONCE here, at arm time, and never again.
	 *
	 * They used to be read at FIRE time, out of the block `kc` points
	 * at -- and that block belongs to NVKMS, which frees it as soon as it
	 * believes the callback is done with. In a split stack it can believe
	 * that while our firing is still in flight (host fired list ->
	 * virtqueue 1 -> ev_ring -> workqueue), and then the read is a read of
	 * freed memory. Measured 2026-08-18 with the frame limiter on, which
	 * widens that window from microseconds to milliseconds: a slab BUG()
	 * in __slab_free, reached through nvrm_events_work ->
	 * SemaphoreSurfaceKapiCallback -> __nv_drm_semsurf_ctx_callback ->
	 * nv_drm_free, and the guest hung behind it (OPEN-QUESTIONS 38).
	 *
	 * Caching removes the READ from the danger list. It does NOT
	 * remove the CALL: `arg` may still name an object NVKMS has freed,
	 * and only a teardown signal could close that half -- a signal this
	 * path does not yet carry, which is why the limiter that widens the
	 * window stays opt-in (OPEN-QUESTIONS 38).
	 */
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
	bool armed;		/* REGISTER_WAITER, slot pre-filled */
	bool unreg;		/* UNREGISTER_WAITER */
	u32 client;
	u64 kc;
};

/* Kernel CONTROL, BEFORE it is sent. `params` is the NVOS54 block in guest
 * kernel memory; its `params` pointer likewise (the kapi path). */
static void semsurf_before_control(const u8 *params, struct semsurf_pending *pend)
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
		  NVRM_SEMSURF_REG_HANDLE_OFF : NVRM_SEMSURF_UNREG_HANDLE_OFF);
	/* 0 = no notification asked; a value that fits in 32 bits is a user
	 * client's OS-event id, which passes through and works natively --
	 * only a kernel VA is ours to serve. */
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
		.used = true, .client = pend->client,
		.proc = kapi_proc ? kapi_proc->id : 0, .kc = kc,
		/* While the block is provably alive: the caller is executing
		 * the registration control that names it. */
		.func = rd64((const u8 *)(uintptr_t)kc, NVRM_NVOS10_CB_EX_FUNC_OFF),
		.arg  = rd64((const u8 *)(uintptr_t)kc, NVRM_NVOS10_CB_EX_ARG_OFF),
	};
	stat_semsurf_waiters++;
	pend->armed = true;
	spin_unlock_irqrestore(&event_cb_lock, flags);
	if (display > 1)
		pr_info("virtio_nvrm: events: semsurf waiter client %#x kc %#llx -> slot %u\n",
			pend->client, (unsigned long long)kc, i);
}

/* Kernel CONTROL, AFTER the answer. */
static void semsurf_after_control(const u8 *params, const struct semsurf_pending *pend,
				  long ret)
{
	unsigned long flags;
	bool ok;

	if (!pend->armed && !pend->unreg)
		return;
	ok = !ret && rd32(params, NVRM_NVOS54_STATUS_OFF) == NVRM_NV_OK;

	spin_lock_irqsave(&event_cb_lock, flags);
	if (pend->armed && !ok) {
		/* NV_OK is the only "a fire is coming (or came)". Everything
		 * else -- ALREADY_SIGNALLED included, which RM answers when
		 * the value was reached WITHOUT registering a notification
		 * (ctrl00da.h:189-196) -- means the slot would wait forever. */
		semsurf_slot_del(pend->client, pend->kc);
	} else if (pend->unreg) {
		/* Cancelled: NVKMS frees the NVOS10 block right after
		 * (nvkms-kapi-sync.c:497-501 via nvidia-drm), so the slot must
		 * not outlive this reply.
		 *
		 * On EVERY verdict, not only NV_OK -- and that is a
		 * correction. The old rule kept the slot when RM answered
		 * anything else, on the reasoning that the waiter must then
		 * have fired already and the fire path would take the slot.
		 * Native that holds, because RM runs the callback inside its
		 * own locks before it answers. Here it does not: our firing
		 * may still be in flight (host fired list -> virtqueue 1 ->
		 * ev_ring -> workqueue) while nvidia-drm, told "too late to
		 * cancel", walks on and frees the object `arg` names. The late
		 * fire then calls into freed memory -- measured as a slab
		 * BUG() in __slab_free (OPEN-QUESTIONS 38).
		 *
		 * Dropping the fire instead costs a fence signal, and that is
		 * the cheaper failure by a wide margin: nvidia-drm's own
		 * timeout path checks the LIVE semaphore value first
		 * (nvidia-drm-fence.c:820-827) and signals a fence whose value
		 * has landed as COMPLETED, not as timed out. So the worst case
		 * is a late fence, not a lost one -- against a guest-kernel
		 * use-after-free on the other side.
		 */
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
		/* One-shot, and the slot goes BEFORE the call: the callback
		 * frees the NVOS10 block (SemaphoreSurfaceKapiCallback,
		 * nvkms-kapi-sync.c:374-380), so a second matching fire must
		 * find nothing. Called UNDER the lock like event_fire_callback
		 * -- RM invokes these within its own locks too, and the
		 * semsurf chain (nvidia-drm's ctx callback) only queues work. */
		sl->used = false;
		if (stat_semsurf_waiters)
			stat_semsurf_waiters--;
		/* From the slot, NOT from *kc: see the struct comment. */
		func = sl->func;
		arg = sl->arg;
		if (func)
			((void (*)(void *, void *, u32, u32, u32))(uintptr_t)func)(
				(void *)(uintptr_t)arg, NULL, 0,
				ev_data(r), ev_status(r));
		stat_semsurf_fired++;
		stat_events_delivered++;
		spin_unlock_irqrestore(&event_cb_lock, flags);
		return;
	}
	spin_unlock_irqrestore(&event_cb_lock, flags);
	stat_events_dropped++;
	stat_events_drop_noslot++;
	pr_warn_ratelimited("virtio_nvrm: events: semsurf waiter kc %#llx (client %#x, proc %u) fired, no slot -- dropped\n",
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
		pr_warn_ratelimited("virtio_nvrm: events: class %#x cannot be served in the guest -- dropped\n",
				    ev_class(r));
		return;
	}

	/* hEvent 0: a semsurf waiter, which has no event object -- its slot
	 * is keyed by the callback pointer, not by a handle. */
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
		/* Cross-check: the 8 bytes the host saved BEFORE overwriting
		 * NV0005.data must be the pointer this slot was filled from. A
		 * mismatch means the handle was reused between two
		 * registrations this module did not see in order -- calling
		 * the old pointer would be a jump into freed memory. */
		if (sl->kc != ev_kc(r)) {
			spin_unlock_irqrestore(&event_cb_lock, flags);
			stat_events_dropped++;
			stat_events_drop_noslot++;
			pr_warn_ratelimited("virtio_nvrm: events: %#x/%#x fired with kc %#llx, slot holds %#llx -- dropped\n",
					    ev_hclient(r), ev_hevent(r),
					    (unsigned long long)ev_kc(r),
					    (unsigned long long)sl->kc);
			return;
		}
		/* Denylist. These NVKMS handlers dereference their second
		 * argument (pEventDataVoid: nvkms-rm.c:1696-1720, 1774-1785),
		 * which RM fills natively through osEventNotificationWithInfo
		 * (os.c:1634-1637). On the substituted path there IS no data
		 * -- the OS-event post carries info32=0 and NV_ESC_RM_GET_
		 * EVENT_DATA hands back no payload (osapi.c:504-535) -- so
		 * arg2 would be NULL and the handler would fault. The
		 * host RM does fire DP_IRQ: the RTX 2070's display belongs
		 * to the host desktop. NV0005_NOTIFY_INDEX_INDEX is 15:0
		 * (cl0005.h:58); RM strips the flags the same way
		 * (event_notification.c:849). */
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
		/* NVOS10_EVENT_KERNEL_CALLBACK_EX { func@0; void *arg@8 }
		 * (nvos.h:409-416), guest kernel memory owned by NVKMS -- read
		 * like call_in does on the kern path. Called UNDER the lock,
		 * exactly like vblank_tick: RM itself invokes these "within
		 * resman's locks" (nvkms-rm.c:4134-4137), and no NVKMS
		 * callback calls back into the kapi synchronously (that would
		 * be sleeping under a spinlock): ChannelEventHandler goes to
		 * cb->proc (nvidia-drm fence, its own spinlock); NonStall,
		 * Completion and Hotplug allocate a timer
		 * (nvkms_alloc_timer_with_ref_ptr, GFP_ATOMIC). Signature =
		 * Callback5ArgVoidReturn (nvos.h:398), call form os.c:1538. */
		func = rd64((const u8 *)(uintptr_t)sl->kc, NVRM_NVOS10_CB_EX_FUNC_OFF);
		arg = rd64((const u8 *)(uintptr_t)sl->kc, NVRM_NVOS10_CB_EX_ARG_OFF);
		if (func)
			((void (*)(void *, void *, u32, u32, u32))(uintptr_t)func)(
				(void *)(uintptr_t)arg, NULL, ev_hevent(r),
				ev_data(r), ev_status(r));
		stat_events_delivered++;
		spin_unlock_irqrestore(&event_cb_lock, flags);
		return;
	}
	spin_unlock_irqrestore(&event_cb_lock, flags);
	stat_events_dropped++;
	stat_events_drop_noslot++;
	pr_warn_ratelimited("virtio_nvrm: events: %#x/%#x fired, no slot (proc %u) -- dropped\n",
			    ev_hclient(r), ev_hevent(r), r->guest_proc);
}

/* One firing of an OS event: make the fd readable. Process context. The
 * XArray lock is what makes the ctx pointer safe to touch: nvrm_node_release
 * erases under the same lock BEFORE it frees. */
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
		/* The fd behind the token is gone (closed between firing and
		 * delivery) -- a wake with nobody to wake. */
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
	mutex_unlock(&kapi_lock);
	if (!ctx)
		return;

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
}

/*
 * One op, forwarded.
 *
 * `nr` and `size` come from nvrm_wire.h, which gets them from the vendor
 * headers through bindgen -- there is no _IOC encoding on this path to read
 * a size out of, and a size typed in by hand is the one that goes stale.
 *
 * `fd_tok` names WHICH session the fd field of this escape refers to.
 * NVRM_NONE_U64 means "this session", which is right for every escape but
 * one -- see kapi_map_memory, where each mapping needs a node of its own.
 */
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
	/* The kernel path is the REASON this field exists -- NVKMS imports an
	 * object from an fd that belongs to the calling userspace process, not
	 * to this session. Forgetting it here would leave the memset's 0, which
	 * is a live session id, and the host would answer the same EBADF as
	 * before the change. */
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
	/* An fd field names WHICH SESSION a mapping belongs to -- the ioctl path
	 * turns the number into a token via the identity of the open file
	 * (nvrm_token_of_fd). A kernel caller has no fd table, but it does have
	 * exactly one session, so the answer is not "resolve the number" but
	 * "it is us": the token is taken from this context directly and the
	 * number is never read.
	 *
	 * That distinction is the whole safety argument. Resolving a number
	 * against `current` from a kthread would be the silent mix-up the
	 * refusal below was built against; naming our own session cannot be
	 * wrong, because there is no other one.
	 *
	 * Measured: NV_ESC_RM_MAP_MEMORY (0x4e) is the first escape on this path
	 * that carries one, and without this it fails with -EOPNOTSUPP while
	 * nvidia-drm reports "Failed to import semaphore surface".
	 */
	if (c.desc && c.desc->fd_off != NVRM_NONE_U32) {
		if (c.desc->fd_off + 4 <= c.size) {
			c.fd_off = c.desc->fd_off;
			memcpy(&c.fd_orig, (u8 *)params + c.desc->fd_off, 4);
			c.fd_token = fd_tok != NVRM_NONE_U64 ? fd_tok : ctx->token;
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

/* ---- the members of nvidia_modeset_rm_ops_t: seven function pointers,
 *      plus the version string and the system_info word ---- */

/*
 * alloc_stack/free_stack: on the host these hand out an alternate stack for
 * RM's deep call chains. There is no RM in this kernel to give a stack to --
 * the work happens on the other side of the virtqueue. NVIDIA's own header
 * settles what to do here: "on architectures where an alternate stack is not
 * used, alloc_stack() will set sp=NULL even when it returns 0 (success).
 * I.e., check the return value, not the sp value."
 */
static int kapi_alloc_stack(void **sp)
{
	*sp = NULL;
	return 0;
}

static void kapi_free_stack(void *sp)
{
}

/*
 * A root client of our own.
 *
 * enumerate_gpus has to ASK RM, and asking needs a client. NVKMS has one --
 * it allocated it through us at load -- but reading its handle out of
 * traffic we are only forwarding would make this module's own state depend
 * on somebody else's. So: our own NV01_ROOT, allocated once, freed with the
 * session.
 *
 * hObjectNew goes in as NV01_NULL_OBJECT and RM assigns it, exactly as
 * nvKmsModuleLoad() does it (nvkms.c:6371).
 */
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
		pr_warn("virtio_nvrm: could not allocate an RM client: %ld\n", ret);
		return ret;
	}
	status = rd32(p, NVRM_NVOS64_STATUS_OFF);
	if (status) {
		pr_warn("virtio_nvrm: RM refused a client: status %#x\n", status);
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

/*
 * enumerate_gpus -- the list nvidia-drm builds its DRM devices from.
 *
 * Every value here comes from RM. A fabricated gpu_id would be worse than
 * reporting none: NVKMS compares it against ids RM hands it later, and the
 * disagreement would surface far from the lie.
 *
 * `os_device_ptr` is the one field RM cannot answer, because it is not a
 * property of the GPU but of how THIS kernel reaches it. nvidia-drm passes
 * it to drm_dev_alloc() as the parent device (nvidia-drm-drv.c:2025). On the
 * host that is the PCI device; in the guest there is no PCI device for the
 * card, and inventing one would be the same mistake in a different field.
 * What genuinely mediates the GPU here is the virtio device -- so that is
 * what the DRM node hangs off. bus_is_pci then stays false, which only
 * gates drm_device.pdev (nvidia-drm-drv.c:2066), a field 6.8 no longer has.
 *
 * With `display` on it is the PCI PARENT instead, and the reason is DMA,
 * not naming. A PRIME import maps the exporter's scatter list FOR THE
 * IMPORTING DEVICE (drm_gem_map_dma_buf -> dma_map_sgtable), and a
 * struct virtio_device has no dma_mask and no dma ops -- only its PCI parent
 * does. Measured 2026-08-08 as
 *   WARNING at kernel/dma/mapping.c:194 __dma_map_sg_attrs
 *   virtgpu_gem_map_dma_buf <- nv_drm_gem_prime_import
 * on an import that then reported success with nothing behind it.
 */
static u32 kapi_enumerate_gpus(struct nvrm_gpu_info *gpu_info)
{
	struct nvrm_dev *dev = nvrm;
	u8 *probed;
	u32 count = 0;
	u32 i;

	if (!dev || !dev->vdev)
		return 0;
	if (kapi_client_ensure())
		return 0;

	/* 384 bytes of three parallel arrays -- too big for the stack, and it
	 * has to be a single allocation because RM writes all of it. */
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
			pr_warn("virtio_nvrm: no PCI info for GPU %#x -- skipped\n", id);
			continue;
		}

		memset(&gpu_info[count], 0, sizeof(gpu_info[count]));
		/* NOT mediated again here: kapi_forward() builds a struct call
		 * and goes through nvrm_call_run(), so this id has ALREADY been
		 * through bdf_rewrite_reply() -- and the GET_PCI_INFO request
		 * below has its guest id turned back into the host's by
		 * bdf_rewrite_request(). Mediating twice was measured as the
		 * false "second GPU" above. */
		gpu_info[count].gpu_id = id;
		gpu_info[count].pci_info.domain = rd32(pci, NVRM_PCI_INFO_DOMAIN_OFF);
		gpu_info[count].pci_info.bus = (u8)rd16(pci, NVRM_PCI_INFO_BUS_OFF);
		gpu_info[count].pci_info.slot = (u8)rd16(pci, NVRM_PCI_INFO_SLOT_OFF);
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

/*
 * open_gpu/close_gpu: raise and lower a reference on one GPU.
 *
 * `reset_aware` is ignored. On the host it tells RM the caller survives a
 * GPU reset; there is no reset path across this virtqueue, so honouring it
 * would be a promise this module cannot keep.
 */
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
	ctx = kapi_ctx_open(nvrm, NVRM_DEV_GPU, g->index);
	if (IS_ERR(ctx)) {
		ret = PTR_ERR(ctx);
		pr_warn("virtio_nvrm: open_gpu(%#x): node %u would not open: %d\n",
			gpu_id, g->index, ret);
		goto out;
	}
	g->ctx = ctx;
	g->refs = 1;
	pr_info("virtio_nvrm: open_gpu(%#x) -> /dev/nvidia%u\n", gpu_id, g->index);
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

/* set_callbacks: store them, with NVIDIA's own one-in-one-out rule
 * (nv-modeset-interface.c:45). Nothing calls them -- see nvrm_kapi.h. */
static int kapi_set_callbacks(const struct nvrm_modeset_callbacks *cb)
{
	if ((kapi_callbacks && cb) || (!kapi_callbacks && !cb))
		return -EINVAL;
	kapi_callbacks = cb;
	return 0;
}

/*
 * NV04_MAP_MEMORY / NV04_UNMAP_MEMORY on the KERNEL path.
 *
 * A process maps in two steps: ioctl(RM_MAP_MEMORY) registers the mapping
 * with the host session, then mmap() on the same fd asks MAP_PREPARE to place
 * it at a chosen offset in the host-visible window. NVKMS has neither an fd
 * nor an mmap -- it calls op() once and reads an address out of the block.
 *
 * So this does both halves in one go. WHICH address it writes back depends
 * on NVOS33_FLAGS_MEM_SPACE, bit 14 of the flags word, and that bit is a
 * complete answer -- not a heuristic:
 *
 *   _USER   (1): the caller will hand the address to ioremap_wc, so it wants
 *                a GUEST-PHYSICAL one. `dev->win_base + off` is exactly
 *                right, because ioremap_wc of a window page reaches the host
 *                mapping behind it.
 *   _CLIENT (0): the caller DEREFERENCES the address itself. It needs a
 *                kernel VA, so this module ioremaps the window pages and
 *                hands out the result.
 *
 * The whole open-gpu-kernel-modules tree sets bit 14 in exactly one place,
 * and it is a switch on the caller's intent (nvkms-kapi.c:2158-2173):
 * NVKMS_KAPI_MAPPING_TYPE_USER sets it, _KERNEL leaves flags at 0. Who takes
 * which branch is unambiguous:
 *
 *   _USER    nvidia-drm-gem-nvkms-memory.c:212 -> ioremap_wc at :227
 *   _KERNEL  nvidia-drm-fence.c:270  (the flip semaphore surface),
 *            read back as *(pLinearAddress + n) at :185
 *   _KERNEL  nvidia-drm-crtc.c:456
 *   flags 0  nvkms-lut.c:171 -> nvkms-rm.c:3674, the 16896-byte colour
 *            lookup table, written as dst[dword] = ...
 *   flags 0  nvkms-surface.c:83 nvEvoCpuMapSurface, which is where a NISO
 *            surface gets the cpuAddress the displayless flip worker polls
 *            (nvkms-displayless.c:265). Without it NVKMS refuses the flip
 *            outright: "Semaphore surface without CPU mapping!"
 *
 * An earlier version of this comment claimed the opposite -- that NVKMS
 * asks with _MEM_SPACE_USER even from the kernel -- and generalised it from
 * nvkms-kapi.c to every caller. It generalised from the one branch of that
 * switch which is NOT the common case. The cost was the refusal below, and
 * with it the whole displayless path.
 *
 * This is the op whose earlier STUB caused a kernel WARNING: it left the
 * block untouched, nvidia-drm read the zeroed status as NV_OK and called
 * ioremap_wc on an uninitialised address. Every failure path below therefore
 * writes a status, and the caller never sees an address it did not get.
 */
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
	/* What RM answered on the HOST, before this module replaced it.
	 *
	 * RM identifies a mapping by the address it handed out, so
	 * UNMAP_MEMORY has to name that one and not ours. The process path
	 * gets this for free -- it forwards RM's answer to the process
	 * untouched, and libcuda gives the same number back. The kernel path
	 * overwrites the field, so it has to remember what it overwrote or
	 * every kernel mapping leaks one on the host. */
	u64 host_linear;
	u64 off;
	size_t len;		/* window bytes reserved: page-rounded */
	/* The node this mapping lives on, and it is ITS OWN.
	 *
	 * RM keeps at most ONE mmap context per open file, for the whole
	 * life of that file: nv_add_mapping_context_to_file answers a second
	 * one with NV_ERR_STATE_IN_USE (nv-usermap.c:104-120), and nothing
	 * ever clears the entry -- nvidia_mmap only reads it, and the list is
	 * emptied at close (nv.c:1079). A process does not notice because
	 * libcuda opens a fresh /dev/nvidia* for every mapping; the kernel
	 * path has no fd of its own and used to hand RM the session's, so the
	 * FIRST kernel mapping worked and the SECOND was refused. Measured
	 * 2026-08-08: NVKMS mapped at X start, then nvidia-drm's fence
	 * context got status 0x63 and reported "Failed to import semaphore
	 * surface".
	 */
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

/* A node of this mapping's own. See the `ctx` field of struct kapi_map for
 * why one per mapping and not one per session.
 *
 * A GPU node, not the control node, and registered against the session's
 * control fd -- the first of the three traps map_doorbell already records
 * for the process path (crates/nvrm-client/src/mem.rs): what gets mapped
 * hangs off the SUBDEVICE, and RM answers a control fd with
 * NV_ERR_INVALID_ARGUMENT. Measured 2026-08-08 as
 *   kernel op 0x21 -> status 0x1f
 * on NVKMS's usermode page, followed by
 *   nvidia-modeset: ERROR: GPU:0: Unable to allocate push buffer controls.
 */
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
	ctx = ctl ? kapi_ctx_open(nvrm, NVRM_DEV_CTL, 0)
		  : kapi_ctx_open(nvrm, NVRM_DEV_GPU, index);
	mutex_unlock(&kapi_lock);
	if (IS_ERR(ctx))
		return ctx;

	/* Bind it to the client's control node. The fd NUMBER in the block is
	 * never read -- the token beside it is what names the control session,
	 * exactly as for the fd field of a mapping. */
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

static long kapi_map_memory(u8 *params)
{
	struct nvrm_dev *dev = nvrm;
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

	/*
	 * And the bit does not travel. RM lets only KERNEL-privilege
	 * clients ask for _MEM_SPACE_USER at all:
	 *
	 *   if (privLevel < RS_PRIV_LEVEL_KERNEL) {
	 *       if (MEM_SPACE == USER) status = NV_ERR_INVALID_FLAGS;
	 *       bKernel = NV_FALSE;
	 *   }                       -- rmapiValidateKernelMapping, mapping_cpu.c:862
	 *
	 * In a native driver NVKMS IS the kernel and passes that test. Here
	 * the escape is issued by vhost-user-nvrm, a userspace process, whose
	 * RM client has user privilege -- so the same call comes back 0x29
	 * NV_ERR_INVALID_FLAGS. Measured 2026-08-13 on the 1920x1080x4 fbdev
	 * framebuffer: "Failed to map NvKmsKapiMemory", then "fbdev: Failed to
	 * setup generic emulation (ret=-12)".
	 *
	 * Nothing is lost by clearing it. Read that branch again: for a
	 * user-privilege client bKernel is NV_FALSE either way, so the host
	 * makes a USER mapping whichever value it sends -- which is exactly
	 * what the bit was asking for. The distinction the bit carries is
	 * about what the GUEST hands back to its caller, and that decision has
	 * already been taken, one line up, from the caller's own block.
	 */
	if (!want_kva)
		wr32(params, NVRM_NVOS33_FLAGS_OFF,
		     flags & ~NVRM_NVOS33_FLAGS_MEM_SPACE_USER);

	/*
	 * 0. + 1. The node this mapping lives on, and the mapping itself.
	 *
	 * WHICH kind of node depends on where the memory is, and only RM
	 * knows. RmCreateMmapContextLocked (osapi.c) decides: if the address
	 * is not in the device's BARs it treats the mapping as SYSTEM memory
	 * and associates it with the CONTROL device
	 * (`pNv = nv_get_ctl_state()`), otherwise with the GPU. That choice
	 * then has to match the fd handed alongside, because
	 * nv_add_mapping_context_to_file opens it as
	 * `nv_get_file_private(fd, NV_IS_CTL_DEVICE(nv), ...)` and answers a
	 * mismatch with NULL -> NV_ERR_INVALID_ARGUMENT (nv-usermap.c:47-49).
	 *
	 * The guest cannot tell in advance: the class of the allocation is not
	 * enough (an OS descriptor is sysmem too) and handles are reused. So
	 * ASK, and let the refusal say which one it wanted. Measured
	 * 2026-08-13: the NVKMS KAPI notifier surface
	 * (nvkms-kapi-notifiers.c:104, NV01_MEMORY_SYSTEM mapped on the
	 * subdevice) is the first mapping on this path that is sysmem, and it
	 * failed with exactly 0x1f while every vidmem mapping before it
	 * succeeded on a GPU node.
	 *
	 * Retrying is safe, and the escape says so itself: when
	 * rm_create_mmap_context fails, escape.c:600-616 calls
	 * Nv04UnmapMemoryWithSecInfo on the mapping it had just made. There is
	 * no half-mapping left behind to trip over.
	 *
	 * The escape takes nv_ioctl_nvos33_parameters_with_fd -- the NVOS33
	 * block plus the fd of the node the mapping belongs to -- while op()
	 * hands over the bare NVOS33. So the block is widened here and narrowed
	 * again below. The number in the fd field is never read; what counts is
	 * the TOKEN named beside it, and that is this mapping's own node.
	 */
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
					      NVRM_KWIRE_MAP_MEMORY, mctx->token);
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
				st, rd32(params, 0), rd32(params, 4), rd32(params, 8),
				rd64(params, 16), rd64(params, NVRM_NVOS33_LENGTH_OFF),
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
	/*
	 * The window works in whole pages; a mapping need not. Rounding the
	 * RESERVATION up is safe either way -- the extra bytes are ours and
	 * nobody is told about them.
	 *
	 * Rounding was tried on 2026-08-08 and the guest oopsed:
	 *
	 *   BUG: unable to handle page fault for address: 00003fff80024000
	 *   RIP: nvHsAllocDevice+0x1b6 [nvidia_modeset]
	 *
	 * That was not the rounding. It was handing a _CLIENT caller the
	 * guest-physical address and watching it dereference it. The 16896-byte
	 * colour lookup table is exactly such a caller, and it now takes the
	 * ioremap branch below.
	 */
	win_len = ALIGN(len, PAGE_SIZE);
	if (win_len < len) {		/* only reachable on a bogus huge length */
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
		pr_warn("virtio_nvrm: kernel MAP_MEMORY: no window slot for %llu bytes\n", win_len);
		kapi_map_ctx_close(mctx);
		wr32(params, NVRM_NVOS33_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return 0;
	}
	/* interruptible = false, always: NVKMS is called from insmod, rmmod and
	 * kthreads, and nobody signals those. Measured once as an rmmod that
	 * never returned. */
	ret = nvrm_simple(dev, NVRM_KIND_MAP_PREPARE, mctx->dev_tag, 0,
			  mctx->token, (u64)off, win_len, &cache, false, proc_id);
	if (ret < 0) {
		pr_warn("virtio_nvrm: kernel MAP_MEMORY: MAP_PREPARE failed: %ld\n", ret);
		win_free(dev, (u64)off, (size_t)win_len);
		kapi_map_ctx_close(mctx);
		wr32(params, NVRM_NVOS33_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return 0;
	}

	km = kzalloc(sizeof(*km), GFP_KERNEL);
	if (!km) {
		nvrm_simple(dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0, (u64)off, win_len,
			    NULL, false, proc_id);
		win_free(dev, (u64)off, (size_t)win_len);
		kapi_map_ctx_close(mctx);
		wr32(params, NVRM_NVOS33_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
		return 0;
	}
	km->off = (u64)off;
	km->len = (size_t)win_len;
	km->ctx = mctx;
	km->host_linear = rd64(params, NVRM_NVOS33_LINEAR_OFF);

	/* 3. The address the caller reads.
	 *
	 * ioremap and NOT memremap: the window is a virtio shared memory
	 * region -- host memory behind a BAR, not guest RAM -- and
	 * memremap(MEMREMAP_WB) wants a page-backed range.
	 *
	 * And the cacheability is the one the HOST just reported in
	 * `cache`, not a guess. The process path decides the same way
	 * (nvrm_node_mmap: 2 -> pgprot_noncached, otherwise cached), and the
	 * two halves must not disagree: x86 keeps one memory type per
	 * physical page, so a window page mapped uncached for a process and
	 * write-combining for the kernel is a PAT conflict, not a preference.
	 * The kapi path always opens a GPU node, so this is uncached today --
	 * writing it out anyway means the day cache_for() learns to tell a
	 * register from a framebuffer, this end already follows. */
	if (want_kva) {
		km->kva = (cache == 2) ? ioremap(dev->win_base + (u64)off, km->len)
				       : ioremap_cache(dev->win_base + (u64)off, km->len);
		if (!km->kva) {
			pr_warn("virtio_nvrm: kernel MAP_MEMORY: ioremap of %zu bytes at %#llx (cache %llu) failed\n",
				km->len, dev->win_base + (u64)off, cache);
			kfree(km);
			nvrm_simple(dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0, (u64)off,
				    win_len, NULL, false, proc_id);
			win_free(dev, (u64)off, (size_t)win_len);
			kapi_map_ctx_close(mctx);
			wr32(params, NVRM_NVOS33_STATUS_OFF, NVRM_NV_ERR_NOT_SUPPORTED);
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

static long kapi_unmap_memory(u8 *params)
{
	struct nvrm_dev *dev = nvrm;
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
	ret = kapi_forward(NVRM_KESC_UNMAP_MEMORY, params, NVRM_KSIZE_UNMAP_MEMORY);
	if (found)
		wr64(params, NVRM_NVOS34_LINEAR_OFF, linear);

	if (found) {
		/* The kernel mapping goes first: after MAP_RELEASE there is no
		 * host mapping behind those window pages any more. */
		if (found->kva)
			iounmap(found->kva);
		if (!kapi_session_ids(&dev_tag, &token, &proc_id))
			nvrm_simple(dev, NVRM_KIND_MAP_RELEASE, 0, 0, 0, found->off,
				    found->len, NULL, false, proc_id);
		win_free(dev, found->off, found->len);
		/* Last: closing the node is what frees RM's mmap context, and
		 * nothing else does (nv.c:1079). A node kept beyond its
		 * mapping is a node no later mapping can use. */
		kapi_map_ctx_close(found->ctx);
		kfree(found);
	} else if (linear) {
		/* Not ours: say so rather than leaving the window slot behind
		 * on the assumption that it will turn up later. */
		pr_warn_ratelimited("virtio_nvrm: kernel UNMAP_MEMORY for %#llx, which this module never handed out\n",
				    linear);
	}
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

	/* Taken off the list under its lock, given up outside it: closing a
	 * mapping's node needs kapi_lock, and taking that one INSIDE
	 * kapi_map_lock would be the only place in this module where the two
	 * nest -- an order nothing else establishes and every future caller
	 * would have to know about. */
	mutex_lock(&kapi_map_lock);
	list_splice_init(&kapi_map_list, &doomed);
	mutex_unlock(&kapi_map_lock);

	list_for_each_entry_safe(km, tmp, &doomed, list) {
		list_del(&km->list);
		/* Before MAP_RELEASE, same order as kapi_unmap_memory: after
		 * it there is no host mapping behind those window pages. */
		if (km->kva)
			iounmap(km->kva);
		if (nvrm && have_ids)
			nvrm_simple(nvrm, NVRM_KIND_MAP_RELEASE, 0, 0, 0, km->off,
				    km->len, NULL, false, proc_id);
		if (nvrm)
			win_free(nvrm, km->off, km->len);
		kapi_map_ctx_close(km->ctx);
		kfree(km);
	}
}

/*
 * Where `status` sits in the parameter block of a kernel-path op, or
 * NVRM_KSTAT_NONE when this module does not know the block.
 *
 * Two callers want the same answer for opposite reasons: the refusal has to
 * WRITE a status there, and the trace wants to READ the one RM wrote. Having
 * the table twice is how the two would drift apart.
 */
#define NVRM_KSTAT_NONE ((u32)~0u)

static u32 kapi_status_off(u32 op)
{
	static const struct { u32 op; u32 status_off; } tbl[] = {
		{ NVRM_KSTAT_FREE }, { NVRM_KSTAT_ALLOC_MEMORY },
		{ NVRM_KSTAT_ALLOC }, { NVRM_KSTAT_MAP_MEMORY },
		{ NVRM_KSTAT_UNMAP_MEMORY }, { NVRM_KSTAT_ALLOC_CONTEXT_DMA },
		{ NVRM_KSTAT_MAP_MEMORY_DMA }, { NVRM_KSTAT_UNMAP_MEMORY_DMA },
		{ NVRM_KSTAT_BIND_CONTEXT_DMA }, { NVRM_KSTAT_CONTROL },
		{ NVRM_KSTAT_DUP_OBJECT }, { NVRM_KSTAT_SHARE },
		{ NVRM_KSTAT_ADD_VBLANK_CALLBACK },
	};
	u32 i;

	for (i = 0; i < ARRAY_SIZE(tbl); i++)
		if (tbl[i].op == op)
			return tbl[i].status_off;
	return NVRM_KSTAT_NONE;
}

/*
 * The object ledger: one line per kernel-path ALLOC and FREE, naming the
 * pair the host books an object under -- (client, handle) -- plus the class
 * and WHO answered the call.
 *
 * Why it exists: an ALLOC that comes back NV_ERR_INSERT_DUPLICATE_NAME
 * (0x19) says the host already has an object at that (client, handle), while
 * the guest's own handle generator considered the number free. Only a log
 * that shows BOTH sides of every book entry can say which of the two lost
 * the entry -- and the "who" field is the whole point of it: a FREE this
 * module answers itself never reaches the host, and that is precisely how
 * the two books would drift apart.
 *
 * Behind `display > 1` like every other kernel-path trace, so a normal run
 * pays nothing for it.
 */
static void kapi_ledger(const char *verb, const u8 *params, u32 handle_off,
			u32 hclass, u32 status_off, const char *who)
{
	if (display <= 1)
		return;
	pr_info("virtio_nvrm: ledger: %s client %#x parent %#x handle %#x class %#x -> status %#x (%s)\n",
		verb, rd32(params, 0), rd32(params, 4),
		rd32(params, handle_off), hclass,
		rd32(params, status_off), who);
}

/*
 * Write NV_ERR_NOT_SUPPORTED into the parameter block of an op this module
 * does not implement, so that the caller sees a refusal rather than the
 * zeroes it arrived with.
 */
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

	/*
	 * NV04_VID_HEAP_CONTROL carries a POINTER to its parameter block, not
	 * the block, so its status is not at a fixed offset from the op --
	 * and any op outside the union is one this module does not know at
	 * all. Both can only be logged, loudly.
	 */
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
		/* Ours, if it is the virtual display's object -- RM has never
		 * heard of that handle. */
		if (vdisp_free(params)) {
			kapi_ledger("FREE ", params, 8, 0, 12, "vdisp, NOT sent");
			return;
		}
		if (vblank_free(params)) {
			kapi_ledger("FREE ", params, 8, 0, 12, "vblank, NOT sent");
			return;
		}
		/* Not ours -- but if it is an event slot, the slot goes BEFORE
		 * the host hears of the free (event section: the fence). */
		event_cb_free(params);
		ret = kapi_forward(NVRM_KESC_FREE, params, NVRM_KSIZE_FREE);
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
		/* Semsurf waiters carry a kernel callback pointer that the
		 * host substitutes -- the slot goes in BEFORE the call, or a
		 * fire that overtakes the reply is lost (semsurf section). */
		{
			struct semsurf_pending spend;

			semsurf_before_control(params, &spend);
			ret = kapi_forward(NVRM_KESC_CONTROL, params,
					   NVRM_KSIZE_CONTROL);
			semsurf_after_control(params, &spend, ret);
		}
		/* The class list is the one real answer this module edits, and
		 * only with `vdisplay` on. */
		if (!ret && rd32(params, NVRM_NVOS54_CMD_OFF) == NVRM_CTRL_GET_CLASSLIST)
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
		/* NV9010: a guest kernel function pointer. Serviced from the
		 * virtual display's own raster clock -- see the vblank
		 * section. Nothing about it reaches the host either. */
		if (vblank_alloc(params)) {
			kapi_ledger("ALLOC", params, NVRM_NVOS64_HOBJECTNEW_OFF,
				    rd32(params, NVRM_NVOS64_HCLASS_OFF),
				    NVRM_NVOS64_STATUS_OFF, "vblank, NOT sent");
			return;
		}
		/* NV01_EVENT_KERNEL_CALLBACK_EX: the host will substitute
		 * NV01_EVENT_OS_EVENT and overwrite the callback pointer, so
		 * what THIS side needs (client, notifyIndex, the pointer) is
		 * read BEFORE the call and the slot is filled AFTER the host
		 * said yes -- event section. */
		{
			struct event_cb_pending pend;

			event_cb_before_alloc(params, &pend);
			ret = kapi_forward(NVRM_KESC_ALLOC, params, NVRM_KSIZE_ALLOC);
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
		/* The GPU-side mapping, and the only one on this path that
		 * needs no translation at all.
		 *
		 * What comes back in dmaOffset is a GPU VIRTUAL address in
		 * the page tables of the hDma object. Those page tables belong
		 * to the one real card, which host and guest share, and the
		 * address is consumed by the GPU rather than by either CPU --
		 * so unlike NV04_MAP_MEMORY there is no window, no ioremap and
		 * nothing to remember. Straight through.
		 *
		 * NVKMS asks for it for the push buffer (nvkms-push.c:84) and
		 * the head surface (nvkms-headsurface.c:175). Measured
		 * 2026-08-13: without it, "Failed to allocate NvKmsKapiDevice"
		 * and no /dev/dri/card1 -- the wall directly behind the colour
		 * lookup table.
		 *
		 * Behind `display` like DUP_OBJECT: the compute path never asks
		 * for it, so with the switch off this module answers exactly as
		 * it did before the display work. */
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
		/* nvidia-drm duplicates the semaphore surface into its own
		 * client here. Refusing it leaves the NVIDIA GPU screen
		 * half-built: measured as
		 *   nv_drm_semsurf_fence_ctx_create_ioctl: Failed to import
		 *   semaphore surface
		 * right after the X server had already logged NVIDIA(G0).
		 *
		 * Behind `display`: nothing on the compute path asks for it,
		 * and with the switch off this module answers exactly as it did
		 * before the display work. */
		if (!display)
			goto unimplemented;
		ret = kapi_forward(NVRM_KESC_DUP_OBJECT, params,
				   NVRM_KSIZE_DUP_OBJECT);
		break;
	default:
unimplemented:
		/*
		 * Context DMA, vid heap, share and ADD_VBLANK_CALLBACK are
		 * unbuilt; map/unmap land here only with display=0, which
		 * refuses them. Saying so in the log is not enough.
		 *
		 * op() returns VOID. Leaving the parameter block untouched
		 * means the caller reads back whatever IT put there, and a
		 * zeroed status field reads as NV_OK. Measured on 2026-08-07:
		 * nvidia-drm asked for NV04_MAP_MEMORY, took the untouched
		 * block for a success, and handed the uninitialised address to
		 * ioremap_wc -- a kernel WARNING out of
		 * nv_drm_dumb_create/__nv_drm_gem_nvkms_map, from a call this
		 * module had merely declined to make.
		 *
		 * So an unimplemented op STATES its refusal. The offsets come
		 * from nvrm_wire.h, generated per op, because `status` sits at
		 * a different place in every NVOS block.
		 */
		kapi_op_refuse(ops, op);
		return;
	}
	/*
	 * RM's own verdict, for EVERY op this module forwards.
	 *
	 * The transport answer (`ret`) and RM's answer are two different
	 * things, and only the second one says whether the call did anything.
	 * A chain that ends in a driver error with `ret 0` all the way down
	 * used to be invisible here: measured 2026-08-08, where an import
	 * reported success with nothing behind it, and again where
	 * nv_drm_semsurf_fence_ctx_create_ioctl failed after four ops of
	 * which not one had said a word.
	 */
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
		pr_warn_ratelimited("virtio_nvrm: kernel RM op %#x failed: %ld\n",
				    op, ret);
}

/*
 * The one symbol nvidia-modeset.ko needs.
 *
 * The version check is NVIDIA's, kept as NVIDIA wrote it: NVKMS puts its own
 * NV_VERSION_STRING in before the call and expects a mismatch to come back
 * as the OTHER side's string plus an error. Ours comes from DRIVER_VERSION
 * by way of nvrm_wire.h -- the same number the host driver, the vendor tree
 * and every gate in this repo are pinned to.
 *
 * The u32 return is NV_STATUS (an NvU32, nvstatus.h:33).
 */
u32 nvidia_get_rm_ops(struct nvrm_modeset_rm_ops *rm_ops)
{
	const struct nvrm_modeset_rm_ops local = {
		.version_string	= NVRM_DRIVER_VERSION,
		.system_info	= { .allow_write_combining = 0 },
		.alloc_stack	= kapi_alloc_stack,
		.free_stack	= kapi_free_stack,
		.enumerate_gpus	= kapi_enumerate_gpus,
		.open_gpu	= kapi_open_gpu,
		.close_gpu	= kapi_close_gpu,
		.op		= kapi_op,
		.set_callbacks	= kapi_set_callbacks,
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
/* dma_buf_attach and its siblings live in the DMA_BUF symbol namespace. A
 * module that uses them without saying so does not fail at compile time --
 * modpost refuses at link time, and the message reads like a missing
 * dependency rather than a missing declaration.
 *
 * The macro started demanding a STRING in 6.13 ("module: Convert
 * symbol namespace to string literal"); passing the bare token there
 * fails with "expected ',' or ';' before 'DMA_BUF'", pointed at
 * moduleparam.h rather than at this line. Measured 2026-08-18 against
 * 6.18.44; the Ubuntu guests run 6.8 and take the first branch. */
#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 13, 0)
MODULE_IMPORT_NS(DMA_BUF);
#else
MODULE_IMPORT_NS("DMA_BUF");
#endif
MODULE_DESCRIPTION("virtio-nvrm: NVIDIA RM escapes across the VM boundary");
MODULE_VERSION("1");
