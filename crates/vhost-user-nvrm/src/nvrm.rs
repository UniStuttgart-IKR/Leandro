// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! The virtio-nvrm device side: the counterpart to `virtio_nvrm.ko`.
//!
//! Terms this file leans on, once (docs/ARCHITECTURE.md has the longer
//! story): RM is NVIDIA's Resource Manager, the kernel driver behind
//! /dev/nvidiactl and /dev/nvidiaN whose ioctls are called escapes; a
//! token is the host-issued id for one device fd the guest opened;
//! `guest_proc` is the dense per-process id the guest module assigns; the
//! host-visible window is the SHMEM region the guest maps RM mappings
//! through.
//!
//! Two virtqueues. Queue 0 is the request queue -- request buffer in,
//! response buffer out -- and was the only one until the event return
//! channel: the guest driver is purpose-built for this protocol. (The
//! retired virtio-gpu carrier had to reuse an EXISTING guest driver and
//! therefore join in the whole blob/capset/EXECBUFFER dance.)
//!
//! Queue 1 is the EVENT queue: the guest pre-posts `Req`-sized inbufs, the
//! device writes one `KIND_EVENT_FIRED` per RM event that fired on the
//! host (`on_poll`). WHY a second queue and not an answer piggy-back: a
//! device-initiated message either rides on a request the guest keeps
//! parked (a permanent request, or guest polling) or on a queue of its
//! own -- and the latter is the pattern of every virtio device that has
//! something to say unasked (virtio-input eventq, virtio-gpu cursorq).
//! Latency is one interrupt, and the crate gives exactly this
//! (`add_used` + `signal_used_queue` on `vrings[1]`).
//!
//! Three things this device does itself, before a message reaches a
//! session at all:
//!
//!  - `KIND_GET_TABLES`: deliver the descriptor tables, paginated.
//!  - `MapPrepare`: after registering the mapping with the session, place
//!    it via SHMEM_MAP at the window offset the guest named. The GUEST
//!    picks the offset here, because it manages the window -- the host
//!    checks it and answers with the cacheability instead of a
//!    virtio-gpu-style blob_id (the name survives in session.rs for the
//!    pending-mapping key).
//!  - `KIND_MAP_RELEASE`: take that same mapping back out.
//!
//! One session per guest process, keyed on `Req.guest_proc`. The guest
//! module keeps guest processes apart from each other (one context per
//! `struct file`, tokens reachable only through the owning context); across
//! the VM boundary, the VM is the unit the host can actually isolate.

use std::collections::{BTreeMap, HashMap};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, RwLock};

use vhost::vhost_user::message::{
    VhostUserMMap, VhostUserMMapFlags, VhostUserProtocolFeatures, VhostUserShMemConfig,
};
use vhost::vhost_user::VhostUserFrontendReqHandler;
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT};
use virtio_queue::QueueT;
use vm_memory::{GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};

use nvrm_abi::sys;
use nvrm_wire::{self as proto, Kind, Req, Rsp};

use crate::session::{Pollable, Session};

type Mem = GuestMemoryAtomic<GuestMemoryMmap<()>>;
type NvVring = VringRwLock<Mem>;

const VIRTIO_F_VERSION_1: u64 = 32;
const VHOST_USER_F_PROTOCOL_FEATURES: u64 = 30;

/// virtio_gpu.h:442-446 -- the cacheability encoding the guest is told.
/// Taken from the virtio-gpu protocol; `virtio_nvrm.ko` uses the same
/// values so the answer to MapPrepare does not have to be reinvented.
const MAP_CACHE_CACHED: u32 = 0x01;
const MAP_CACHE_UNCACHED: u32 = 0x02;

/// virtio_gpu.h:127 -- the shmid under which the guest driver looks for the
/// window (cloud-hypervisor derives it from the list index, which is why
/// index 0 stays empty).
const SHM_ID_HOST_VISIBLE: u8 = 1;

/// Size of the host-visible window (address space, PROT_NONE).
///
/// 8 GiB, and every step up from the 256 MiB this started at came from a
/// measurement rather than a guess. NVENC raised it first: a single
/// 1080p `h264_nvenc` session holds **250.6 MiB simultaneously** --
/// 30 surfaces of 6,328,320 bytes (181 MiB), one block of 58,720,256
/// bytes (56 MiB), and change -- and nothing is released until the process
/// exits. At 256 MiB the next allocation is the one that fails, which the
/// encoder reports as
///
/// ```text
/// CreateBitstreamBuffer failed: out of memory (10)
/// ```
///
/// The tell that this is capacity and not corruption: the failure follows
/// the PIXEL COUNT, not the width or the height. Measured, 3 frames each:
/// 1856x1044 and 1920x1008 pass, 1856x1080 and 1888x1062 fail -- the edge
/// sits just under 2.0 MPix, exactly where 30 surfaces stop fitting.
///
/// 1 GiB carries 4K by the same arithmetic (30 x 24.9 MiB + 56 MiB is
/// ~800 MiB). It costs nothing but address space: the window is an
/// anonymous PROT_NONE MAP_NORESERVE region until the backend maps
/// something into it, and the guest's page bitmap grows to 32 KiB.
///
/// And then a GAME. Measured 2026-08-15 with the tracer on CS2 in the
/// guest: 128 mappings, 938 MiB in the window, and the 129th -- 32 MiB --
/// finds no hole, mmap returns MAP_FAILED, and CS2 memcpys into NULL+16
/// from five GlobPool threads at once (SIGSEGV in libc, minidumps by the
/// handful). The user's native dust2 run maps 3.6 GB in its first 3000
/// trace lines. Same arithmetic, same answer: 8 GiB. Still address space
/// only; the bitmap is 256 KiB. The guest's window comes from the
/// virtio shmem region, so both sides move together through this one
/// constant.
const HOST_VISIBLE_SIZE: u64 = 8 << 30;

/// Which cacheability a window mapping gets. The **GPU node** never gets
/// write-back -- registers are `NV_MEMORY_UNCACHED`, framebuffer is
/// `UNCACHED` or write-combining. The **ctl node** maps system memory,
/// which is cached.
///
/// Registers cannot be told apart from framebuffer at this point, so the
/// GPU node gets a blanket `UNCACHED`: stricter than WC, hence never wrong,
/// at worst slower. The doorbell (`TURING_USERMODE_A`) is a register and
/// needs exactly that.
fn cache_for(dev: nvrm_abi::xlate::Dev) -> u32 {
    match dev {
        nvrm_abi::xlate::Dev::Gpu => MAP_CACHE_UNCACHED,
        _ => MAP_CACHE_CACHED,
    }
}

/// Virtio-PCI *modern* maps PCI device `0x1040 + type` and accepts only
/// `0x1040..0x107f` -- that is, types 0..63. The spec assigns up to ~42; 60
/// is free and works (measured: `1af4:107c`, `virtio5: 0x003c`). Must match
/// `--generic-vhost-user device_type=60` and `VIRTIO_ID_NVRM` in the driver
/// header. Should virtio-nvrm ever get an official ID, the number changes
/// at exactly those two places.
pub const VIRTIO_ID_NVRM: u32 = 60;

/// Indirect descriptors: a maximum-size message (1 MiB aux) is more than
/// 256 pages and would otherwise not fit into a 256-entry queue. The guest
/// module attaches its buffers as a page list; without this feature it
/// would have to cap the message size.
const VIRTIO_RING_F_INDIRECT_DESC: u64 = 28;

/// `LEA_DEBUG` set to a non-empty value. NOT `var_os(..).is_some()`: an EMPTY
/// value is still `Some("")`, so a launcher that passes
/// `LEA_DEBUG="${LEA_DEBUG:-}"` through -- the normal, careful-looking shell
/// idiom -- would turn the firehose on for every run. It did: 86,645 debug
/// lines on a per-frame path before anyone noticed the variable was set at
/// all. Read once, through the session's cached level.
fn debug() -> bool {
    crate::session::debug_level() >= 1
}

macro_rules! dlog {
    ($($a:tt)*) => { if debug() { eprintln!("vhost-user-nvrm/nvrm: {}", format!($($a)*)); } };
}

/// A mapping that sits in the host-visible window. The `File` keeps the RM
/// mapping open for as long as the VMM has it blended in.
struct WindowMap {
    len: u64,
    /// Which guest process placed it. `KIND_MAP_RELEASE` names ONE offset,
    /// and a process that dies -- crash, SIGKILL, or an exit that never
    /// tears down -- names none, so without an owner nothing could ever
    /// give these back. See [`Backend::release_window_of`].
    guest_proc: u32,
    _fd: std::fs::File,
}

/// Which window offsets a guest process placed.
///
/// A free function for the same reason `window_overlaps` is one: this is the
/// decision PROC_GONE acts on, it decides whether a dead process's slice of
/// the 8 GiB window comes back, and it must be checkable without a VMM.
fn window_of_proc(
    window: &std::collections::BTreeMap<u64, WindowMap>, guest_proc: u32,
) -> Vec<u64> {
    window
        .iter()
        .filter(|(_, m)| m.guest_proc == guest_proc)
        .map(|(&off, _)| off)
        .collect()
}

/// Does `[off, off+len)` touch a mapping that is already in the window?
///
/// A free function rather than a method so it can be tested without a
/// device: this is the check that stops one guest process from placing its
/// mapping on top of another's, and it is three lines of range arithmetic
/// that would be easy to get subtly wrong.
///
/// Looking at only the LAST mapping that starts before `off+len` is
/// sufficient, and the reason is the invariant this very function
/// maintains -- the mappings never overlap EACH OTHER. Let `o` be that
/// last key.
///   - If `o > off` it starts inside the query and `o + len > off` holds
///     trivially: overlap.
///   - If `o <= off` and it ends at or before `off`, then every earlier
///     mapping ends at or before `o`, hence at or before `off`: no overlap.
///
/// Callers check `off.checked_add(len)` against the window size first, so
/// the sum here cannot wrap.
fn window_overlaps(window: &std::collections::BTreeMap<u64, WindowMap>, off: u64, len: u64) -> bool {
    window
        .range(..off + len)
        .next_back()
        .is_some_and(|(&o, m)| o + m.len > off)
}

/// The `data` under which the device's OWN epoll fd sits in the worker's
/// epoll set. `register_listener` reserves `0..=num_queues()` for the two
/// queues and the exit event (event_loop.rs:119-121), so with two queues
/// the first free value is 3; it arrives in `handle_event` as
/// `device_event` (a u16, event_loop.rs:186).
const EVENT_LISTENER: u64 = 3;
const EVENT_LISTENER_U16: u16 = EVENT_LISTENER as u16;

/// One fd in the device's epoll set, resolved from the `data` word.
#[derive(Clone, Copy, Debug)]
enum PollSrc {
    /// A session's per-client event ctl: readable = drain with
    /// `Session::drain_os_events`, one `KIND_EVENT_FIRED` per firing.
    EventCtl { guest_proc: u32, h_client: u32, fd: RawFd },
    /// A guest fd that allocated an OS event itself: readable = tell the
    /// guest to wake the fd behind `token`.
    Client { guest_proc: u32, token: u64, fd: RawFd },
    /// The waiter poller's notify eventfd: readable = retired semsurf
    /// (semaphore-surface: the fence object nvidia-drm signals through)
    /// waiters to collect (`WaiterPoller::take_fired`). The waiter fds
    /// themselves are NOT in this set -- waiters.rs explains why epoll
    /// would eat their wakes.
    WaiterNotify,
}

/// The dedupe key of a `PollSrc`: `(guest_proc, token, h_client)` with the
/// unused half at its sentinel (`NONE_U64` for an event ctl, 0 for a
/// client fd -- 0 is not a client handle RM hands out).
type PollKey = (u32, u64, u32);

pub struct NvrmDevice {
    mem: Option<Mem>,
    event_idx: bool,
    /// Guest process ID -> its session. The key is `Req.guest_proc`, the
    /// dense ID the guest module assigns per `open`; 0 means "not stated"
    /// and shares a single session with every other caller that states
    /// nothing.
    ///
    /// Why per process and not per VM: `PoolState.pools` is keyed on the
    /// GPU VA, and libcuda places the semaphore pool of EVERY process at
    /// the same one (`0x204a00000`). With one session per VM, two
    /// concurrent managed-memory processes collided there structurally --
    /// the stopgap in `back_pool` (drop the older entry) was correct
    /// sequentially and a race under concurrency. Separate sessions solve
    /// it at the root: separate pools, separate tokens, separate mirrors.
    sessions: BTreeMap<u32, Session>,
    /// How often a `fd_field_token` missed the caller's own mirror. Only for
    /// the diagnostic below; see there for what it is proving.
    fd_field_misses: u64,
    /// The table stream. Built once, then only served out.
    tables: nvrm_abi::table::Tables,
    backend: Option<vhost::vhost_user::Backend>,
    /// Window offset -> what lies there. The guest picks the offset, the
    /// host checks it: nothing may overlap, nothing may cross the window
    /// boundary.
    window: BTreeMap<u64, WindowMap>,
    /// The VRAM cap. One ledger per device, i.e. per VM -- every session
    /// charges the same counter, because the VM is the only boundary the
    /// host can enforce (docs/FUTURE.md).
    vram: std::sync::Arc<crate::vram::Ledger>,

    /// The device's OWN epoll set for host fds that announce RM events.
    ///
    /// WHY a nested epoll and not `register_listener` per fd: the
    /// worker's `handle_event` runs under the backend's `RwLock::write`
    /// (backend.rs:594-604), and `VringEpollHandler::register_listener`
    /// reads `self.backend.num_queues()` under `RwLock::read`
    /// (event_loop.rs:120) -- std's RwLock is not reentrant, so registering
    /// an fd from inside `handle()`/`handle_event()` would deadlock the
    /// worker. This set is registered ONCE with the worker (`serve`, before
    /// `daemon.serve`), and every later `epoll_ctl` goes here, where no
    /// backend lock is involved.
    poll: Epoll,
    /// epoll `data` -> what it names. Ids count up from 1 and are never
    /// reused: a stale event for a deleted id resolves to nothing instead
    /// of to a newer fd.
    poll_srcs: HashMap<u64, PollSrc>,
    /// Dedupe: the same fd reported twice by a session (a client that
    /// allocates two OS events on one fd) is registered once.
    poll_ids: HashMap<PollKey, u64>,
    next_poll_id: u64,
    /// Running counter in `Req.seq` of every KIND_EVENT_FIRED (log only).
    ev_seq: u32,
    /// Counters for the summary line at PROC_GONE.
    ev_delivered: u64,
    ev_wakes: u64,
    ev_dropped_noslot: u64,
    ev_dropped_noqueue: u64,
    /// The one-time "event channel live" line: a run without LEA_DEBUG
    /// still bears witness that the channel carried something.
    ev_announced: bool,
    /// The semaphore-surface waiter poller (waiters.rs): its own thread,
    /// `poll(2)`, reporting through an eventfd in `poll`.
    waiters: crate::waiters::WaiterPoller,
}

impl NvrmDevice {
    fn new() -> anyhow::Result<Self> {
        let tables = nvrm_abi::table::build();
        eprintln!(
            "vhost-user-nvrm: tables v{} ready -- {} bytes, checksum {:#010x} \
             ({} ioctls, {} classes, {} controls, {} nested)",
            tables.version,
            tables.bytes.len(),
            tables.checksum,
            tables.n_ioctl,
            tables.n_class,
            tables.n_ctrl,
            tables.n_nested
        );
        Ok(Self {
            mem: None,
            event_idx: false,
            sessions: BTreeMap::new(),
            fd_field_misses: 0,
            tables,
            backend: None,
            window: BTreeMap::new(),
            vram: crate::vram::Ledger::new(),
            poll: Epoll::new().map_err(|e| anyhow::anyhow!("event epoll: {e}"))?,
            poll_srcs: HashMap::new(),
            poll_ids: HashMap::new(),
            next_poll_id: 1,
            ev_seq: 0,
            ev_delivered: 0,
            ev_wakes: 0,
            ev_dropped_noslot: 0,
            ev_dropped_noqueue: 0,
            ev_announced: false,
            waiters: crate::waiters::WaiterPoller::new()?,
        })
    }

    /// The poller's notify eventfd joins the device's epoll set once, at
    /// construction time of the set's bookkeeping. An eventfd is safe
    /// there: its counter stays until read, unlike the nvidia fds' flag.
    fn register_waiter_notify(&mut self) {
        let fd = self.waiters.notify_fd();
        let id = self.next_poll_id;
        self.next_poll_id += 1;
        match self.poll.ctl(ControlOperation::Add, fd, EpollEvent::new(EventSet::IN, id)) {
            Ok(()) => {
                self.poll_srcs.insert(id, PollSrc::WaiterNotify);
                dlog!("poll +{id}: WaiterNotify fd {fd}");
            }
            Err(e) => eprintln!(
                "vhost-user-nvrm: cannot watch waiter notify fd {fd}: {e} -- \
                 semsurf waiter fences will hang"
            ),
        }
    }

    // ---- the event return channel: registering ----------------------------

    fn poll_key(guest_proc: u32, p: &Pollable) -> PollKey {
        match *p {
            Pollable::EventCtl { h_client, .. } => (guest_proc, proto::NONE_U64, h_client),
            Pollable::Client { token, owner, .. } => (owner.unwrap_or(guest_proc), token, 0),
            // Never in the epoll set; register_pollables diverts these
            // before the key is asked for.
            Pollable::Waiter { .. } => unreachable!("waiter fds bypass the epoll set"),
        }
    }

    /// Put what a session reported into the epoll set.
    ///
    /// Event ctls are LEVEL-triggered: the device drains them itself, and
    /// whatever it leaves is meant to fire again. Client fds are
    /// EDGE-triggered: the GUEST drains those, through its own
    /// passed-through NV_ESC_RM_GET_EVENT_DATA -- level would be a busy
    /// loop until it does. Every `nv_post_event` is a
    /// `wake_up_interruptible` and thus a fresh edge (nv.c:4085), and
    /// coalescing is wanted: the guest only sets a flag.
    fn register_pollables(&mut self, guest_proc: u32, new: Vec<Pollable>) {
        for p in new {
            // Waiter fds bypass the epoll set entirely (waiters.rs says
            // why) -- they go to the poller thread, keyed by fd, replacing
            // whatever stale entry a recycled slot left behind.
            if let Pollable::Waiter { id, fd } = p {
                self.waiters.watch(crate::waiters::Watch { guest_proc, id, fd });
                continue;
            }
            let key = Self::poll_key(guest_proc, &p);
            if self.poll_ids.contains_key(&key) {
                continue;
            }
            let (fd, src, set) = match p {
                Pollable::EventCtl { h_client, fd } => {
                    (fd, PollSrc::EventCtl { guest_proc, h_client, fd }, EventSet::IN)
                }
                Pollable::Client { token, fd, owner } => (
                    fd,
                    // The firing names the token's OWNER, or the guest
                    // looks the token up in the wrong process.
                    PollSrc::Client { guest_proc: owner.unwrap_or(guest_proc), token, fd },
                    EventSet::IN | EventSet::EDGE_TRIGGERED,
                ),
                Pollable::Waiter { .. } => unreachable!("diverted above"),
            };
            let id = self.next_poll_id;
            self.next_poll_id += 1;
            match self.poll.ctl(ControlOperation::Add, fd, EpollEvent::new(set, id)) {
                Ok(()) => {
                    self.poll_srcs.insert(id, src);
                    self.poll_ids.insert(key, id);
                    dlog!("poll +{id}: {src:?} fd {fd}");
                }
                Err(e) => eprintln!("vhost-user-nvrm: event poll: cannot watch fd {fd} ({src:?}): {e}"),
            }
        }
    }

    /// Take one fd out of the set -- out of the KERNEL's set, not just our
    /// map. A level-triggered fd that stays registered fires forever:
    /// an event ctl on a GPU_IS_LOST answers POLLHUP (nv.c:2306) as long as
    /// the session lives, and a worker that only forgot its id spins on it
    /// while also serving queue 0. The fd is what the kernel needs for the
    /// DELETE, so the source carries it. `fd_hint` overrides it for the
    /// token path, where the caller has the more current number.
    fn unregister_id(&mut self, id: u64, fd_hint: Option<RawFd>) {
        let Some(src) = self.poll_srcs.remove(&id) else { return };
        let (key, fd) = match src {
            PollSrc::EventCtl { guest_proc, h_client, fd } => {
                ((guest_proc, proto::NONE_U64, h_client), fd)
            }
            PollSrc::Client { guest_proc, token, fd } => ((guest_proc, token, 0), fd),
            PollSrc::WaiterNotify => {
                // The poller's own eventfd never leaves the set while the
                // device lives; a HANG_UP here means the poller died.
                eprintln!("vhost-user-nvrm: waiter notify fd left the poll set -- semsurf waiter fences will hang");
                return;
            }
        };
        self.poll_ids.remove(&key);
        let fd = fd_hint.unwrap_or(fd);
        // EBADF/ENOENT when the file is already gone: the kernel dropped it
        // for us, and that is fine.
        let _ = self.poll.ctl(ControlOperation::Delete, fd, EpollEvent::new(EventSet::IN, id));
        dlog!("poll -{id}: {src:?}");
    }

    /// The guest closes a token: its host fd leaves the set BEFORE the
    /// session closes it. Order matters: `take_pending_map` duplicated
    /// mirror fds for the VMM, so the FILE may outlive the mirror's fd and
    /// keep the epoll registration alive -- and the fd NUMBER gets reused by
    /// the next open, which would then be watched under a stale id.
    fn unregister_token(&mut self, guest_proc: u32, token: u64) {
        let Some(&id) = self.poll_ids.get(&(guest_proc, token, 0)) else { return };
        let fd = self.sessions.get(&guest_proc).and_then(|s| s.mirror_raw(token));
        self.unregister_id(id, fd);
    }

    /// One event ctl leaves the set: its RM client was freed, and the
    /// session is handing the fd over to be closed. Same order as
    /// `unregister_token` and for the same reason -- out of the KERNEL's
    /// epoll set first, close second. The caller drops the fd only after
    /// this returns.
    fn unregister_event_ctl(&mut self, guest_proc: u32, h_client: u32) {
        let key = (guest_proc, proto::NONE_U64, h_client);
        let Some(&id) = self.poll_ids.get(&key) else { return };
        self.unregister_id(id, None);
    }

    /// Everything a session ever reported, on its way out.
    fn unregister_proc(&mut self, guest_proc: u32) {
        // The waiter poller's entries first: the session's slot fds close
        // with the session, and a stale entry would poll a reused number.
        self.waiters.unwatch_proc(guest_proc);
        let ids: Vec<u64> = self
            .poll_srcs
            .iter()
            .filter(|(_, s)| match **s {
                PollSrc::EventCtl { guest_proc: g, .. } | PollSrc::Client { guest_proc: g, .. } => {
                    g == guest_proc
                }
                PollSrc::WaiterNotify => false,
            })
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            let fd = match self.poll_srcs.get(&id) {
                Some(PollSrc::Client { token, .. }) => {
                    self.sessions.get(&guest_proc).and_then(|s| s.mirror_raw(*token))
                }
                // The event ctl closes with the session and takes its
                // registration along; the fd is not duplicated anywhere.
                _ => None,
            };
            self.unregister_id(id, fd);
        }
    }

    // ---- the event return channel: firing ---------------------------------

    fn next_seq(&mut self) -> u32 {
        self.ev_seq = self.ev_seq.wrapping_add(1);
        self.ev_seq
    }

    /// Write one KIND_EVENT_FIRED into the next inbuf the guest posted on
    /// the event queue. `false` = dropped and counted; NEVER waits -- the
    /// worker thread that runs this also serves queue 0.
    fn push_event(&mut self, evq: &NvVring, req: &Req) -> bool {
        if !evq.get_ref().is_enabled() {
            self.ev_dropped_noqueue += 1;
            return false;
        }
        let Some(mem) = self.mem.as_ref().map(|m| m.memory()) else {
            self.ev_dropped_noqueue += 1;
            return false;
        };
        let chain = evq.get_mut().get_queue_mut().pop_descriptor_chain(mem.clone());
        let Some(chain) = chain else {
            self.ev_dropped_noslot += 1;
            if self.ev_dropped_noslot <= 3 {
                let q = evq.get_ref();
                let vq = q.get_queue();
                dlog!(
                    "EVENT_FIRED dropped: guest posted no inbuf (ready={} size={} next_avail={} next_used={})",
                    vq.ready(), vq.size(), vq.next_avail(), vq.next_used()
                );
            }
            return false;
        };
        let head = chain.head_index();
        let mut w = match chain.writer(&*mem) {
            Ok(w) => w,
            Err(e) => {
                dlog!("event inbuf unusable: {e}");
                let _ = evq.add_used(head, 0);
                self.ev_dropped_noslot += 1;
                return false;
            }
        };
        if w.available_bytes() < Req::WIRE_LEN {
            dlog!("event inbuf of {} bytes < Req", w.available_bytes());
            let _ = evq.add_used(head, 0);
            self.ev_dropped_noslot += 1;
            return false;
        }
        if let Err(e) = std::io::Write::write_all(&mut w, req.as_bytes()) {
            dlog!("event inbuf write: {e}");
            let _ = evq.add_used(head, 0);
            self.ev_dropped_noslot += 1;
            return false;
        }
        if let Err(e) = evq.add_used(head, Req::WIRE_LEN as u32) {
            dlog!("event add_used: {e}");
            self.ev_dropped_noslot += 1;
            return false;
        }
        self.ev_delivered += 1;
        if !self.ev_announced {
            self.ev_announced = true;
            eprintln!("vhost-user-nvrm: event channel live (first KIND_EVENT_FIRED on queue 1)");
        }
        true
    }

    /// The device's epoll fd turned readable: some host fd has an RM event.
    ///
    /// Bounded rounds, so that queue 0 does not starve behind a chatty
    /// event source; the outer epoll is level-triggered on our fd, so
    /// whatever is left triggers the next `handle_event`. ONE
    /// `signal_used_queue` per pass, like `process_queue`.
    fn on_poll(&mut self, evq: &NvVring) -> std::io::Result<()> {
        let mut evs = [EpollEvent::default(); 64];
        let mut wrote_any = false;
        for _round in 0..8 {
            let n = match self.poll.wait(0, &mut evs) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if n == 0 {
                break;
            }
            for e in &evs[..n] {
                let id = e.data();
                let set = EventSet::from_bits(e.events()).unwrap_or(EventSet::empty());
                if set.intersects(EventSet::HANG_UP | EventSet::ERROR) {
                    // The fd died under us; the kernel drops it from the set
                    // when the last reference goes, but forget it now.
                    dlog!("poll id {id}: fd hung up / errored ({set:?})");
                    self.unregister_id(id, None);
                    continue;
                }
                match self.poll_srcs.get(&id).copied() {
                    Some(PollSrc::Client { guest_proc, token, .. }) => {
                        let seq = self.next_seq();
                        let req = Req {
                            seq,
                            kind: proto::KIND_EVENT_FIRED,
                            ioctl_nr: sys::NV01_EVENT_OS_EVENT,
                            target_token: token,
                            guest_proc,
                            ..Req::default()
                        };
                        dlog!("EVENT_FIRED class {:#x} proc {guest_proc} tok {token}",
                              sys::NV01_EVENT_OS_EVENT);
                        wrote_any |= self.push_event(evq, &req);
                        self.ev_wakes += 1;
                    }
                    Some(PollSrc::EventCtl { guest_proc, h_client, .. }) => {
                        let (fired, ctl_alive) = match self.sessions.get_mut(&guest_proc) {
                            Some(s) => {
                                let f = s.drain_os_events(h_client);
                                (f, s.has_event_ctl(h_client))
                            }
                            None => {
                                self.unregister_id(id, None);
                                continue;
                            }
                        };
                        // The drain may have CLOSED the ctl (a refusal it
                        // cannot recover from). Forget the registration
                        // with it: a later alloc opens a fresh ctl under
                        // the same key, and a stale entry would dedupe it
                        // away -- that client's events would never arrive
                        // again, silently.
                        if !ctl_alive {
                            self.unregister_id(id, None);
                        }
                        for f in fired {
                            let seq = self.next_seq();
                            let req = Req {
                                seq,
                                kind: proto::KIND_EVENT_FIRED,
                                ioctl_nr: f.reg.class,
                                target_token: f.reg.token,
                                inline_len: f.info32,
                                aux_len: sys::NV_OK,
                                fd_field_off: f.reg.h_client,
                                fd_field_token: f.reg.id as u64,
                                embedded_ptr_off: f.reg.h_event,
                                nested_count: f.reg.notify_index,
                                addr: f.reg.guest_data,
                                guest_proc,
                                ..Req::default()
                            };
                            dlog!(
                                "EVENT_FIRED class {:#x} proc {guest_proc} tok {} hClient {:#x} \
                                 hEvent {:#x} idx {:#x} id {}",
                                f.reg.class, f.reg.token, f.reg.h_client, f.reg.h_event,
                                f.reg.notify_index, f.reg.id
                            );
                            wrote_any |= self.push_event(evq, &req);
                        }
                    }
                    Some(PollSrc::WaiterNotify) => {
                        // Retired semsurf waiters. The poller already took
                        // them out of its watch list; the session retires
                        // the books and hands back what the guest needs to
                        // call its NVOS10 block: hEvent = 0 is the marker
                        // (a waiter has no event object), `addr` the
                        // callback pointer.
                        for w in self.waiters.take_fired() {
                            let woken = self
                                .sessions
                                .get_mut(&w.guest_proc)
                                .and_then(|s| s.semsurf_wake(w.id));
                            let Some((h_client, kc, token)) = woken else {
                                dlog!("waiter id {} of proc {}: no books -- dropped", w.id, w.guest_proc);
                                continue;
                            };
                            let seq = self.next_seq();
                            let req = Req {
                                seq,
                                kind: proto::KIND_EVENT_FIRED,
                                ioctl_nr: sys::NV01_EVENT_KERNEL_CALLBACK_EX,
                                target_token: token,
                                aux_len: sys::NV_OK,
                                fd_field_off: h_client,
                                fd_field_token: w.id as u64,
                                embedded_ptr_off: 0,
                                addr: kc,
                                guest_proc: w.guest_proc,
                                ..Req::default()
                            };
                            dlog!(
                                "EVENT_FIRED semsurf proc {} hClient {h_client:#x} kc {kc:#x} id {}",
                                w.guest_proc, w.id
                            );
                            wrote_any |= self.push_event(evq, &req);
                            self.ev_wakes += 1;
                        }
                    }
                    None => {
                        // Never registered by us, or already forgotten: a
                        // stale readiness for an id we dropped. Nothing to
                        // delete by fd -- we do not know it any more.
                        dlog!("poll: event for unknown id {id}");
                    }
                }
            }
        }
        if wrote_any {
            evq.signal_used_queue()
                .map_err(|e| std::io::Error::other(format!("event signal: {e}")))?;
        }
        Ok(())
    }

    /// Every FD this backend holds, split by who holds it, against the FD
    /// count the kernel actually reports for this process.
    ///
    /// The one line OPEN-QUESTIONS 31 was missing. It could say that
    /// 2003 `nvidiactl` FDs were open and that only one guest process was
    /// alive, but not WHICH structure held them -- and a leak whose owner is
    /// unknown cannot be fixed, only guessed at. Anything the sum does not
    /// account for is held outside every session.
    ///
    /// Behind its OWN switch, `LEA_FD_CENSUS`, and not `LEA_DEBUG`: that one
    /// is a firehose (it prints a line per semsurf waiter, i.e. per frame),
    /// so anyone who wanted this line would have had to drown to read it.
    /// This reads a directory, and PROC_GONE is hot enough that it must not
    /// be free either.
    fn fd_census(&self) {
        // `is_some_and(|v| !v.is_empty())`, NOT `is_some()`. An EMPTY value is
        // still a SET variable, and the shell that starts this backend passes
        // `LEA_FD_CENSUS="${LEA_FD_CENSUS:-}"` (rig.sh) -- the careful-looking
        // idiom llm.md names as a trap -- so `is_some()` switched the census
        // on for every rig anybody ever brought up, whether or not they asked.
        // Measured 2026-08-21: a desktop guest nobody had set the variable for
        // was writing census lines into nvrm.log, and this is a PROC_GONE
        // path, which the doc above says must not be free.
        //
        // LEA_OBJLOG, three lines of code away in session.rs, has always had
        // the correct test. This was one missed site and not a pattern -- the
        // other six switches compare against a value or parse one.
        if std::env::var_os("LEA_FD_CENSUS").is_none_or(|v| v.is_empty()) {
            return;
        }
        let mut t = [0usize; 7];
        for s in self.sessions.values() {
            for (acc, n) in t.iter_mut().zip(s.census()) {
                *acc += n;
            }
        }
        let (mut total, mut ctl) = (0usize, 0usize);
        if let Ok(rd) = std::fs::read_dir("/proc/self/fd") {
            for e in rd.flatten() {
                total += 1;
                if std::fs::read_link(e.path())
                    .is_ok_and(|p| p.to_string_lossy().contains("nvidiactl"))
                {
                    ctl += 1;
                }
            }
        }
        let named: usize = t[0] + t[2] + t[3] + t[4] + t[5] + t[6];
        // `total`, not `ctl`, and no window term. The old expression was
        // `ctl - named - window.len()` and subtracted three different units
        // from each other: `ctl` counts only the fds whose link says
        // nvidiactl, `named` counts session-held fds of EVERY node
        // (/dev/nvidia0 and the two uvm nodes as well), and `window` counts
        // guest memory MAPPINGS, which are not fds at all. It therefore read
        // negative whenever a session held anything but ctl fds, which is
        // always: measured 2026-08-20, -21 on a bare boot and -203 on a
        // desktop, for a figure the doc above calls "held outside every
        // session" -- a count of things, which cannot be less than zero.
        // Still i64 and still printed signed: if this ever does go negative
        // the sessions are claiming fds the process does not have, and that
        // is worth seeing rather than clamping away.
        eprintln!(
            "vhost-user-nvrm: fd census: {} sessions hold {named} \
             (mirror {} of {} ever, event_ctls {} pooled_waiters {} armed {} pending {} osdesc {}) \
             | window {} maps \
             | process has {total} fds, {ctl} of them nvidiactl \
             | outside every session {}",
            self.sessions.len(),
            t[0], t[1], t[2], t[3], t[4], t[5], t[6],
            self.window.len(),
            total as i64 - named as i64,
        );
        for (id, s) in &self.sessions {
            let c = s.census();
            eprintln!(
                "vhost-user-nvrm:   session {id} ({}): mirror {} of {} ever, \
                 event_ctls {}, pooled_waiters {}, armed {}",
                s.proc_name(), c[0], c[1], c[2], c[3], c[4],
            );
        }
    }

    /// This guest process's session, created on demand.
    ///
    /// It is created on the process's first word and torn down on the
    /// `KIND_PROC_GONE` of its last FD. If creation fails (it opens no
    /// devices, so in practice only memory can defeat it), the guest gets
    /// an error rather than being quietly attached to someone else's
    /// session.
    fn session_for(&mut self, sub_id: u32) -> Option<&mut Session> {
        if !self.sessions.contains_key(&sub_id) {
            // The guest assigns the IDs, so it must not be allowed to
            // assign arbitrarily many: every session holds host FDs and
            // pools. The guest module would never go past a few dozen (one
            // ID per process with an open node) -- this bound catches a
            // guest that sends something else, and is deliberately far away
            // from anything a real run needs.
            const MAX_SESSIONS: usize = 1024;
            if self.sessions.len() >= MAX_SESSIONS {
                eprintln!(
                    "vhost-user-nvrm: {MAX_SESSIONS} guest processes at once -- \
                     further ones refused (ID {sub_id})"
                );
                return None;
            }
            match Session::detached_proc(sub_id, self.vram.clone()) {
                Ok(s) => {
                    dlog!("new session for guest process {sub_id}");
                    self.sessions.insert(sub_id, s);
                }
                Err(e) => {
                    eprintln!("vhost-user-nvrm: session for guest process {sub_id}: {e:#}");
                    return None;
                }
            }
        }
        self.sessions.get_mut(&sub_id)
    }

    /// Give back every window mapping this guest process still had blended
    /// in, and unmap them from the VMM.
    ///
    /// Nothing else ever did. `KIND_MAP_RELEASE` names one offset at a
    /// time, so a process that dies without tearing down leaves all of its
    /// mappings behind, and each leftover costs twice: a DUPLICATED
    /// `/dev/nvidia*` FD (`take_pending_map` clones it), which is why the FD
    /// census counted FDs that belonged to no session at all, and -- worse
    /// -- its slice of the host-visible window, which no later mapping can
    /// reuse. Running out of holes in that window is what killed CS2 at the
    /// 129th mapping (see `HOST_VISIBLE_SIZE`), so a leak here is not a
    /// bookkeeping detail but the same wall with a slower fuse.
    ///
    /// Removed FIRST, unmapped second: if the VMM refuses the unmap there is
    /// nothing sensible left to do with the entry, and keeping it would mean
    /// keeping the FD too.
    fn release_window_of(&mut self, guest_proc: u32) {
        let offs = window_of_proc(&self.window, guest_proc);
        if offs.is_empty() {
            return;
        }
        let taken: Vec<(u64, u64)> = offs
            .into_iter()
            .filter_map(|off| self.window.remove(&off).map(|m| (off, m.len)))
            .collect();
        let n = taken.len();
        if let Some(backend) = self.backend.as_ref() {
            for (off, len) in taken {
                let msg = VhostUserMMap {
                    shmid: SHM_ID_HOST_VISIBLE,
                    padding: [0; 7],
                    fd_offset: 0,
                    shm_offset: off,
                    len,
                    flags: 0,
                };
                if let Err(e) = backend.shmem_unmap(&msg) {
                    eprintln!(
                        "vhost-user-nvrm: SHMEM_UNMAP of dead process {guest_proc}'s \
                         window+{off:#x}: {e}"
                    );
                }
            }
        }
        dlog!("PROC_GONE {guest_proc}: gave back {n} window mappings");
    }

    /// The guest process has exited -- its session falls, and with it
    /// tokens, host FDs, pools and arenas. That is why the dense ID may be
    /// reused: its meaning ends here.
    fn on_proc_gone(&mut self, req: &Req) -> Vec<u8> {
        if req.guest_proc == 0 {
            return err_rsp(req.seq, libc::EINVAL);
        }
        // The poll registrations go BEFORE the session: the session's drop
        // closes the fds, and a registration must not outlive its fd.
        self.unregister_proc(req.guest_proc);
        // ... and the window mappings it never released itself.
        self.release_window_of(req.guest_proc);
        match self.sessions.remove(&req.guest_proc) {
            Some(s) => {
                dlog!("guest process {} exited, session torn down", req.guest_proc);
                // The one line that says whether the channel did anything
                // for this process -- measured 2026-08-15 as "26 kernel
                // callbacks registered in a GNOME session, none delivered"
                // before the channel existed, and this line is what would
                // have shown it. Only when it registered at all: most
                // processes never touch an event.
                let (r, f, u, d) = s.event_stats();
                if r + f + u + d != 0 {
                    eprintln!(
                        "vhost-user-nvrm: session {} events: registered {r} fired {f} \
                         unmatched {u} dataless {d} | device: delivered {} wakes {} \
                         dropped_noslot {} dropped_noqueue {}",
                        req.guest_proc,
                        self.ev_delivered,
                        self.ev_wakes,
                        self.ev_dropped_noslot,
                        self.ev_dropped_noqueue
                    );
                }
            }
            None => dlog!("PROC_GONE for unknown guest process {}", req.guest_proc),
        }
        self.fd_census();
        Rsp { seq: req.seq, ..Rsp::default() }.as_bytes().to_vec()
    }

    /// Answer one message. The return value is the finished response bytes.
    fn handle(&mut self, msg: &[u8]) -> Vec<u8> {
        let Some(req) = Req::from_bytes(msg) else {
            dlog!("message shorter than Req ({} bytes)", msg.len());
            return err_rsp(0, libc::EPROTO);
        };

        match req.kind {
            proto::KIND_GET_TABLES => self.on_get_tables(&req),
            proto::KIND_MAP_RELEASE => self.on_map_release(&req),
            proto::KIND_PROC_GONE => self.on_proc_gone(&req),
            k if k == Kind::MapPrepare as u32 => self.on_map_prepare(&req, msg),
            _ => {
                // Everything else goes unchanged to the guest process's
                // session -- the device adds nothing on this path, except
                // that it keeps its poll set in step with the fds.
                if req.kind == Kind::Close as u32 {
                    self.unregister_token(req.guest_proc, req.target_token);
                }
                let mem = self.mem.clone();
                // The fd that sits in the INLINE struct, and below it the one
                // inside the aux buffer. Both must be resolved HERE, before the
                // session borrow: the owner may be a different guest process
                // (NVKMS imports an object the X server exported), and from
                // inside `Session` there is no path to a sibling. `RawFd` is
                // Copy, so the shared borrow of `self.sessions` ends on these
                // lines -- the same reason `mem` is cloned above.
                //
                // `get`, never `session_for`: the latter CREATES on demand and
                // is capped, so resolving through it would let the guest mint
                // empty sessions out of a field it controls.
                let fd_field_fd = if req.fd_field_token != proto::NONE_U64
                    && req.fd_field_proc != proto::NONE_U32
                {
                    self.sessions
                        .get(&req.fd_field_proc)
                        .and_then(|s| s.mirror_raw(req.fd_field_token))
                } else {
                    None
                };
                // DIAGNOSTIC 2026-08-17, and it is a READER, not a fix. The
                // EGLImage import that fails names an fd owned by a DIFFERENT
                // guest process -- Xwayland imports what a client exported --
                // and a lookup that cannot see it answers EBADF, which
                // NVIDIA's GL stack reports as GL_OUT_OF_MEMORY. Measured
                // 2026-08-17: 139936 such refusals in ONE session while the
                // VRAM ledger stood at 287 of 4096 MiB, so it is not the cap.
                //
                // CORRECTED 2026-08-20, and the correction matters more than
                // the diagnostic. It used to ask only whether the CALLER's own
                // mirror holds the token, which stopped being the right
                // question at protocol v6: the resolution just above goes
                // through `fd_field_proc`, so a token that is absent from the
                // caller and present in the process that field names is the
                // HEALTHY cross-process import, not a miss. Asked the old way
                // it reported 20 CROSS-SESSION misses in a session that
                // refused NOTHING -- no `session N:` line anywhere in
                // 1_392_006 traced calls -- which reads exactly like the bug
                // it was added to find, and cost a reader most of a session.
                // It now fires only when the call really is about to be
                // refused, which is all three of: the field is actually
                // translated at all (`fd_field_off` set -- without it
                // session.rs never looks the token up and cannot refuse), the
                // device could not resolve it, and neither can the caller's
                // own mirror, which is what session.rs falls back to before it
                // returns EBADF.
                if req.fd_field_token != proto::NONE_U64
                    && req.fd_field_off != proto::NONE_U32
                    && fd_field_fd.is_none()
                    && self
                        .sessions
                        .get(&req.guest_proc)
                        .and_then(|s| s.mirror_raw(req.fd_field_token))
                        .is_none()
                {
                    let mut owner = None;
                    for (p, s) in self.sessions.iter() {
                        if *p != req.guest_proc && s.mirror_raw(req.fd_field_token).is_some() {
                            owner = Some(*p);
                            break;
                        }
                    }
                    self.fd_field_misses += 1;
                    // Rate-limited: the guest retries, and 140k lines would
                    // bury the answer they are supposed to give.
                    if self.fd_field_misses <= 20 || self.fd_field_misses % 5000 == 0 {
                        match owner {
                            Some(o) => eprintln!(
                                "vhost-user-nvrm: fd_field_token {:#x} asked by proc {} lives in \
                                 proc {}, which `fd_field_proc` did not name -- CROSS-SESSION \
                                 (miss {})",
                                req.fd_field_token, req.guest_proc, o, self.fd_field_misses
                            ),
                            None => eprintln!(
                                "vhost-user-nvrm: fd_field_token {:#x} asked by proc {} is in NO \
                                 session -- STALE (miss {})",
                                req.fd_field_token, req.guest_proc, self.fd_field_misses
                            ),
                        }
                    }
                }
                let aux_fd = if req.aux_fd_field_token != proto::NONE_U64
                    && req.aux_fd_field_proc != proto::NONE_U32
                {
                    self.sessions
                        .get(&req.aux_fd_field_proc)
                        .and_then(|s| s.mirror_raw(req.aux_fd_field_token))
                } else {
                    None
                };
                let Some(session) = self.session_for(req.guest_proc) else {
                    return err_rsp(req.seq, libc::ENOMEM);
                };
                session.set_guest_mem(mem);
                let out = match session.handle_msg_with(msg, aux_fd, fd_field_fd) {
                    Ok(r) => r.bytes,
                    Err(e) => {
                        eprintln!("vhost-user-nvrm: session {}: {e:#}", req.guest_proc);
                        err_rsp(req.seq, libc::EPROTO)
                    }
                };
                // What the session opened for events goes into the poll set
                // -- after the borrow of `session` has ended, on the
                // device's own epoll (no backend lock involved).
                let new = session.take_pollables();
                let unwatch = session.take_unwatch();
                let ctl_unwatch = session.take_ctl_unwatch();
                if !new.is_empty() {
                    self.register_pollables(req.guest_proc, new);
                }
                if !unwatch.is_empty() {
                    // Poller first, close second: `unwatch` owns the slot
                    // fds, and dropping it after the poller forgot them is
                    // what keeps a reused fd NUMBER out of the watch list.
                    use std::os::fd::AsRawFd;
                    let fds: Vec<std::os::fd::RawFd> =
                        unwatch.iter().map(|s| s.as_raw_fd()).collect();
                    self.waiters.unwatch(&fds);
                }
                // Event ctls of freed RM clients: epoll first, then the
                // drop at the end of this scope closes them. Same order,
                // same reason as the slot fds above.
                for (h_client, ctl) in ctl_unwatch {
                    self.unregister_event_ctl(req.guest_proc, h_client);
                    drop(ctl);
                }
                out
            }
        }
    }

    /// One chunk of the table stream. `addr` = offset, `map_len` = the
    /// maximum length wanted. Answer: `token` = total length (so the guest
    /// module knows how often it must come back), `inline_len` = length of
    /// this chunk.
    fn on_get_tables(&mut self, req: &Req) -> Vec<u8> {
        let total = self.tables.bytes.len() as u64;
        let off = req.addr;
        if off > total {
            return err_rsp(req.seq, libc::EINVAL);
        }
        // The guest proposes, the host caps: never more than one
        // maximum-size message at a time.
        let want = req.map_len.min(proto::MAX_PAYLOAD as u64).max(1);
        let end = (off + want).min(total);
        let chunk = &self.tables.bytes[off as usize..end as usize];
        dlog!("GET_TABLES {off}+{} of {total}", chunk.len());
        let rsp = Rsp {
            seq: req.seq,
            ret: 0,
            token: total,
            inline_len: chunk.len() as u32,
            ..Rsp::default()
        };
        let mut out = Vec::with_capacity(Rsp::WIRE_LEN + chunk.len());
        out.extend_from_slice(rsp.as_bytes());
        out.extend_from_slice(chunk);
        out
    }

    /// Place a mapping into the window.
    ///
    /// First the session (it checks the token and registers the mapping),
    /// then SHMEM_MAP at the offset the guest named. The guest manages the
    /// window because only it knows what is still free in its own address
    /// space -- but that does not let it decide WHAT lies there.
    fn on_map_prepare(&mut self, req: &Req, msg: &[u8]) -> Vec<u8> {
        let mem = self.mem.clone();
        let Some(session) = self.session_for(req.guest_proc) else {
            return err_rsp(req.seq, libc::ENOMEM);
        };
        session.set_guest_mem(mem);
        let reply = match session.handle_msg(msg) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("vhost-user-nvrm: MapPrepare: {e:#}");
                return err_rsp(req.seq, libc::EPROTO);
            }
        };
        let Some(rsp) = Rsp::from_bytes(&reply.bytes) else {
            return err_rsp(req.seq, libc::EIO);
        };
        if rsp.ret != 0 {
            return reply.bytes; // session already refused (unknown token or similar)
        }
        let blob_id = rsp.token;

        // Fetch the mapping from the SAME session that just registered it
        // -- after that the borrow ends and `backend` may be borrowed.
        let taken = self
            .sessions
            .get_mut(&req.guest_proc)
            .and_then(|s| s.take_pending_map(blob_id));
        let Some((fd, len, dev)) = taken else {
            dlog!("MapPrepare: blob_id {blob_id:#x} cannot be fetched");
            return err_rsp(req.seq, libc::EIO);
        };
        let Some(backend) = self.backend.as_ref() else {
            dlog!("MapPrepare without a backend channel");
            return err_rsp(req.seq, libc::EIO);
        };

        let off = req.addr;
        if off % 4096 != 0
            || len % 4096 != 0
            || off.checked_add(len).is_none_or(|e| e > HOST_VISIBLE_SIZE)
        {
            dlog!("MapPrepare: window slot {off:#x}+{len:#x} unusable");
            return err_rsp(req.seq, libc::EINVAL);
        }
        // Overlap check: the guest picks the slot, the host does not let it
        // overwrite anyone else's mapping.
        if self.overlaps(off, len) {
            dlog!("MapPrepare: {off:#x}+{len:#x} overlaps an existing mapping");
            return err_rsp(req.seq, libc::EBUSY);
        }

        // Probe the mmap HERE before involving the VMM. The fd and length
        // come from an arbitrary guest process, and the driver may refuse
        // the mmap for reasons that are its right -- measured 2026-08-16,
        // twice: Steam's vulkandriverquery mmaps the node without a prior
        // RM map ioctl and gets "NVRM: VM: invalid mmap context"
        // (nv-mmap.c:546), a harmless EINVAL on bare metal. Handed to
        // cloud-hypervisor instead, the failed mmap counted as a corrupted
        // request, the worker exited and the device stopped until reset
        // (OPEN-QUESTIONS nr 9): ONE bad mmap from ANY guest process
        // bricked every future NVKMS session. The probe is safe to repeat:
        // the driver's mmap path only READS the context under the file-VA
        // read lock (nv_acquire_file_va(.., NV_FALSE)) -- nothing is
        // consumed, and the probe VMA is gone before the real one is made.
        let probe = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                0,
            )
        };
        if probe == libc::MAP_FAILED {
            let errno = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EINVAL);
            dlog!("MapPrepare: probe mmap refused (errno {errno}) -- refused to the GUEST, the VMM never sees it");
            return err_rsp(req.seq, errno);
        }
        // SAFETY: probe is the mapping created just above, len unchanged.
        unsafe { libc::munmap(probe, len as usize) };

        let msg_map = VhostUserMMap {
            shmid: SHM_ID_HOST_VISIBLE,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset: off,
            len,
            flags: VhostUserMMapFlags::WRITABLE.bits(),
        };
        if let Err(e) = backend.shmem_map(&msg_map, &fd) {
            eprintln!("vhost-user-nvrm: SHMEM_MAP: {e}");
            return err_rsp(req.seq, libc::EIO);
        }
        self.window.insert(off, WindowMap { len, guest_proc: req.guest_proc, _fd: fd });
        dlog!("MapPrepare -> window+{off:#x}, {len} bytes, cache {:#x}", cache_for(dev));

        // Here `token` carries the CACHEABILITY, not a mapping id: the
        // host knows the NVOS33 (RM_MAP_MEMORY parameter block) flags, so
        // the guest module does not have to guess.
        Rsp { seq: req.seq, ret: 0, token: cache_for(dev) as u64, ..Rsp::default() }
            .as_bytes()
            .to_vec()
    }

    /// Take a mapping back out of the window and drop the host fd behind it.
    ///
    /// Keyed on the window offset alone, not on `(offset, guest_proc)`: the
    /// guest MODULE is the only sender of this kind, it owns the window
    /// bitmap for the whole VM, and the VM is the unit the host isolates
    /// (docs/ARCHITECTURE.md §2). A guest kernel that releases another
    /// process's slot is lying to itself, not to the host.
    fn on_map_release(&mut self, req: &Req) -> Vec<u8> {
        let Some(backend) = self.backend.as_ref() else {
            return err_rsp(req.seq, libc::EIO);
        };
        let Some(entry) = self.window.remove(&req.addr) else {
            // Not an error: at process exit the guest also tears down
            // mappings that never came about.
            dlog!("MAP_RELEASE: {:#x} was not blended in", req.addr);
            return Rsp { seq: req.seq, ..Rsp::default() }.as_bytes().to_vec();
        };
        let msg = VhostUserMMap {
            shmid: SHM_ID_HOST_VISIBLE,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset: req.addr,
            len: entry.len,
            flags: 0,
        };
        if let Err(e) = backend.shmem_unmap(&msg) {
            eprintln!("vhost-user-nvrm: SHMEM_UNMAP: {e}");
            return err_rsp(req.seq, libc::EIO);
        }
        dlog!("MAP_RELEASE window+{:#x}, {} bytes", req.addr, entry.len);
        // entry (and with it the host FD) is dropped here.
        Rsp { seq: req.seq, ..Rsp::default() }.as_bytes().to_vec()
    }

    fn overlaps(&self, off: u64, len: u64) -> bool {
        window_overlaps(&self.window, off, len)
    }

    fn process_queue(&mut self, vring: &NvVring) -> std::io::Result<()> {
        let mem = self
            .mem
            .as_ref()
            .ok_or_else(|| std::io::Error::other("kick before SET_MEM_TABLE"))?
            .memory();
        let mut used_any = false;
        loop {
            let chain = vring
                .get_mut()
                .get_queue_mut()
                .pop_descriptor_chain(mem.clone());
            let Some(chain) = chain else { break };
            let head = chain.head_index();

            let mut reader = chain
                .clone()
                .reader(&*mem)
                .map_err(|e| std::io::Error::other(format!("reader: {e}")))?;
            let mut writer = chain
                .writer(&*mem)
                .map_err(|e| std::io::Error::other(format!("writer: {e}")))?;

            let avail = reader.available_bytes();
            let resp = if !(Req::WIRE_LEN..=proto::MAX_MSG).contains(&avail) {
                dlog!("request of {avail} bytes refused");
                err_rsp(0, libc::EMSGSIZE)
            } else {
                let mut msg = vec![0u8; avail];
                match std::io::Read::read_exact(&mut reader, &mut msg) {
                    Ok(()) => self.handle(&msg),
                    Err(e) => {
                        dlog!("request unreadable: {e}");
                        err_rsp(0, libc::EIO)
                    }
                }
            };

            // If the response does not fit into the guest's buffer, it gets
            // a clean error instead of a truncated message.
            let resp = if resp.len() > writer.available_bytes() {
                dlog!(
                    "response {} bytes > response buffer {}",
                    resp.len(),
                    writer.available_bytes()
                );
                err_rsp(Rsp::from_bytes(&resp).map(|r| r.seq).unwrap_or(0), libc::EMSGSIZE)
            } else {
                resp
            };
            let n = resp.len().min(writer.available_bytes());
            std::io::Write::write_all(&mut writer, &resp[..n])?;
            vring
                .add_used(head, n as u32)
                .map_err(|e| std::io::Error::other(format!("add_used: {e}")))?;
            used_any = true;
        }
        if used_any {
            vring
                .signal_used_queue()
                .map_err(|e| std::io::Error::other(format!("signal: {e}")))?;
        }
        Ok(())
    }
}

fn err_rsp(seq: u32, errno: i32) -> Vec<u8> {
    Rsp { seq, ret: -errno, ..Rsp::default() }.as_bytes().to_vec()
}

impl VhostUserBackendMut for NvrmDevice {
    type Bitmap = ();
    type Vring = NvVring;

    /// Queue 0 requests, queue 1 events. cloud-hypervisor derives the
    /// count from `queue_sizes=[...]` on the command line
    /// (generic_vhost_user.rs:166, 241-253) -- `[256,256]`, or SET_VRING
    /// for index 1 never arrives.
    fn num_queues(&self) -> usize {
        2
    }

    fn max_queue_size(&self) -> usize {
        256
    }

    fn features(&self) -> u64 {
        (1 << VIRTIO_F_VERSION_1)
            | (1 << VHOST_USER_F_PROTOCOL_FEATURES)
            | (1 << VIRTIO_RING_F_INDIRECT_DESC)
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::REPLY_ACK
            | VhostUserProtocolFeatures::BACKEND_REQ
            | VhostUserProtocolFeatures::SHMEM
    }

    /// The host-visible window: region 1, HOST_VISIBLE_SIZE. That a non-GPU device
    /// gets one at all rests on cloud-hypervisor's shmem support being
    /// genuinely generic (`patches/0001-generic-vhost-user-shmem.patch`);
    /// why index 0 stays empty is on `SHM_ID_HOST_VISIBLE` above.
    fn get_shmem_config(&self) -> std::io::Result<VhostUserShMemConfig> {
        Ok(VhostUserShMemConfig::new(2, &[0, HOST_VISIBLE_SIZE]))
    }

    fn set_backend_req_fd(&mut self, backend: vhost::vhost_user::Backend) {
        self.backend = Some(backend);
    }

    fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx = enabled;
    }

    fn update_memory(&mut self, mem: Mem) -> std::io::Result<()> {
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        evset: EventSet,
        vrings: &[Self::Vring],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        if evset != EventSet::IN {
            return Ok(());
        }
        match device_event {
            0 => self.process_queue(&vrings[0]),
            // The guest kicked the event queue: it (re-)posted inbufs.
            // Nothing to do now -- they are consumed lazily by `push_event`.
            1 => {
                if let Some(evq) = vrings.get(1) {
                    let q = evq.get_ref();
                    let vq = q.get_queue();
                    dlog!("evq kick: ready={} next_avail={} next_used={} avail_idx(guest)={:?}",
                          vq.ready(), vq.next_avail(), vq.next_used(),
                          self.mem.as_ref().map(|m| vq.avail_idx(&*m.memory(), std::sync::atomic::Ordering::Acquire).map(|i| i.0)));
                }
                Ok(())
            }
            // Our own epoll set has something: an RM event fired on the
            // host. Only meaningful once both vrings exist.
            EVENT_LISTENER_U16 => match vrings.get(1) {
                Some(evq) => self.on_poll(evq),
                None => {
                    dlog!("event poll before the event queue exists");
                    Ok(())
                }
            },
            other => {
                dlog!("unexpected event {other}");
                Ok(())
            }
        }
    }
}

/// Serve as a vhost-user device on `socket` until the VMM hangs up.
pub fn serve(socket: &str) -> anyhow::Result<()> {
    let backend = Arc::new(RwLock::new(NvrmDevice::new()?));
    backend.write().unwrap().register_waiter_notify();
    let mut daemon = VhostUserDaemon::new(
        "vhost-user-nvrm".into(),
        backend.clone(),
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .map_err(|e| anyhow::anyhow!("vhost-user daemon: {e:?}"))?;

    // The device's own epoll set joins the worker's epoll HERE -- after the
    // daemon has built its handlers, before any request can hold the
    // backend's write lock. `register_listener` takes `num_queues()` under
    // the read lock (event_loop.rs:120); at this point nobody holds the
    // write lock, so this is the one moment it cannot deadlock. The fd is
    // copied out first, so our own read guard is gone before the call.
    let poll_fd = backend
        .read()
        .map_err(|_| anyhow::anyhow!("backend lock poisoned"))?
        .poll
        .as_raw_fd();
    let handlers = daemon.get_epoll_handlers();
    let Some(h) = handlers.first() else {
        anyhow::bail!("vhost-user daemon has no epoll handler");
    };
    h.register_listener(poll_fd, EventSet::IN, EVENT_LISTENER)
        .map_err(|e| anyhow::anyhow!("register event listener: {e}"))?;

    eprintln!(
        "vhost-user-nvrm: virtio-nvrm on {socket} (device_type {VIRTIO_ID_NVRM}), driver {}",
        nvrm_sys::DRIVER_VERSION
    );
    daemon
        .serve(socket)
        .map_err(|e| anyhow::anyhow!("vhost-user: {e:?}"))?;
    eprintln!("vhost-user-nvrm: VMM hung up");

    // EXIT here rather than returning, and it is not a shortcut.
    //
    // Returning unwinds through the daemon's destructor, which joins its
    // vring worker -- and that worker sits in `epoll_wait` with nothing
    // left to wake it, because the thing that used to wake it is the VMM
    // that just went away. Measured 2026-08-16 by SIGKILLing a guest that
    // held 1792 MiB: the message below appeared in the log, and the
    // process then lived on with its main thread in `futex_do_wait` and
    // the worker in `do_epoll_wait`, FOREVER.
    //
    // What that costs is not a stray process. RM frees a client's memory
    // when the process holding it dies, so a backend that never dies
    // never gives the card back: the host's free memory stayed 1.9 GB
    // short until the backend was killed by hand, and an orchestrator
    // restarting a crashed VM would leak that much per crash.
    //
    // Exiting is the honest end of this program's life. One backend serves
    // exactly one VM (see the module header and `vram.rs`), that VM is
    // gone, and everything worth releasing is released by the kernel on
    // process death -- which is measurably true: killing the backend by
    // hand freed the card completely. There is nothing to flush; the log
    // is stderr and `eprintln!` has already written it.
    std::process::exit(0);
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn win(entries: &[(u64, u64)]) -> BTreeMap<u64, WindowMap> {
        entries
            .iter()
            .map(|&(off, len)| {
                let fd = std::fs::File::open("/dev/null").expect("/dev/null");
                (off, WindowMap { len, guest_proc: 0, _fd: fd })
            })
            .collect()
    }

    /// Like `win`, but each entry names the guest process that placed it.
    fn win_owned(entries: &[(u64, u64, u32)]) -> BTreeMap<u64, WindowMap> {
        entries
            .iter()
            .map(|&(off, len, guest_proc)| {
                let fd = std::fs::File::open("/dev/null").expect("/dev/null");
                (off, WindowMap { len, guest_proc, _fd: fd })
            })
            .collect()
    }

    /// PROC_GONE gives back the dead process's window mappings and NOBODY
    /// else's.
    ///
    /// Until 2026-08-18 nothing gave them back at all: MAP_RELEASE names
    /// one offset, and a process that dies names none. Each leftover held a
    /// duplicated device FD -- the FDs the census found outside every
    /// session -- and its slice of the host-visible window, which no later
    /// mapping can reuse. The window running out of holes is what killed CS2
    /// at its 129th mapping.
    #[test]
    fn proc_gone_takes_only_that_processes_window_mappings() {
        let mut w = win_owned(&[
            (0x0000, 0x1000, 7),
            (0x1000, 0x1000, 9),
            (0x2000, 0x1000, 7),
            (0x3000, 0x1000, 0),
        ]);
        let mine = window_of_proc(&w, 7);
        assert_eq!(mine, vec![0x0000, 0x2000], "both of process 7's, in offset order");
        for off in mine {
            assert!(w.remove(&off).is_some(), "and each one is really there");
        }
        assert_eq!(w.len(), 2, "the other two processes keep theirs");
        assert!(w.contains_key(&0x1000) && w.contains_key(&0x3000));

        // A process that mapped nothing costs nothing -- PROC_GONE is a hot
        // path and most processes never place a window mapping at all.
        assert!(window_of_proc(&w, 12345).is_empty());
        // And guest_proc 0 is a real owner here, not a wildcard: device-wide
        // requests carry it, and matching it loosely would unmap the world.
        assert_eq!(window_of_proc(&w, 0), vec![0x3000]);
    }

    /// The window is empty, so nothing can collide with anything.
    #[test]
    fn an_empty_window_refuses_nothing() {
        assert!(!window_overlaps(&win(&[]), 0, 0x1000));
        assert!(!window_overlaps(&win(&[]), 0x4000_0000, 0x1000));
    }

    /// Touching at the boundary is NOT an overlap: a mapping that ends
    /// exactly where the next begins is the dense packing the guest is
    /// supposed to achieve, and refusing it would waste the window.
    #[test]
    fn end_to_end_mappings_do_not_collide() {
        let w = win(&[(0, 0x1000), (0x2000, 0x1000)]);
        assert!(!window_overlaps(&w, 0x1000, 0x1000), "the gap between the two");
        assert!(!window_overlaps(&w, 0x3000, 0x1000), "immediately after the last");
    }

    /// Every way one range can meet another, in one place: same start,
    /// starting inside, ending inside, and swallowing it whole.
    #[test]
    fn the_four_shapes_of_an_overlap_are_all_caught() {
        let w = win(&[(0x2000, 0x2000)]); // [0x2000, 0x4000)
        assert!(window_overlaps(&w, 0x2000, 0x1000), "same start");
        assert!(window_overlaps(&w, 0x3000, 0x2000), "starts inside, ends after");
        assert!(window_overlaps(&w, 0x1000, 0x2000), "starts before, ends inside");
        assert!(window_overlaps(&w, 0x1000, 0x4000), "swallows it whole");
        assert!(!window_overlaps(&w, 0x1000, 0x1000), "ends exactly at its start");
        assert!(!window_overlaps(&w, 0x4000, 0x1000), "starts exactly at its end");
    }

    /// The reason only the last mapping before the query needs checking:
    /// a hit must not be missed just because a LATER mapping sits between
    /// the query and the end of the map.
    #[test]
    fn a_hit_far_down_the_map_is_still_found() {
        let w = win(&[(0, 0x1000), (0x2000, 0x1000), (0x8000, 0x4000)]);
        assert!(window_overlaps(&w, 0x9000, 0x1000), "inside the last one");
        assert!(!window_overlaps(&w, 0x4000, 0x4000), "the hole between the second and third");
    }
}

/// The device WITHOUT a VM: `NvrmDevice::new` builds the descriptor
/// tables, an epoll set and the waiter poller, and touches no GPU, no
/// vhost-user socket and no guest memory. That makes the message handlers
/// that answer out of the device's own books -- GET_TABLES, MAP_RELEASE,
/// PROC_GONE -- testable here, which they are not from outside the module:
/// `handle`, `err_rsp` and the fields they read are private.
#[cfg(test)]
mod device_tests {
    use super::*;

    fn dev() -> NvrmDevice {
        NvrmDevice::new().expect("NvrmDevice::new must work without a GPU")
    }

    fn answer(d: &mut NvrmDevice, req: Req) -> Rsp {
        Rsp::from_bytes(&d.handle(req.as_bytes())).expect("every answer starts with a Rsp")
    }

    /// One GET_TABLES round trip: the response header plus the chunk that
    /// follows it.
    fn get_tables(d: &mut NvrmDevice, addr: u64, map_len: u64) -> (Rsp, Vec<u8>) {
        let req = Req { seq: 3, kind: proto::KIND_GET_TABLES, addr, map_len, ..Req::default() };
        let out = d.handle(req.as_bytes());
        let rsp = Rsp::from_bytes(&out).expect("every answer starts with a Rsp");
        let body = out[Rsp::WIRE_LEN..].to_vec();
        assert_eq!(rsp.seq, 3, "the answer echoes the sequence number");
        assert_eq!(
            body.len(),
            rsp.inline_len as usize,
            "inline_len must describe exactly the bytes that follow the header"
        );
        (rsp, body)
    }

    /// An `addr` past the end of the stream is refused with EINVAL.
    ///
    /// `on_get_tables` slices `tables.bytes[off..end]` with a number the
    /// GUEST chose. Without this check the slice panics -- and a panic in
    /// the vhost-user worker takes the whole daemon, and with it the VM's
    /// GPU, down. `addr == total` is NOT past the end: it is the empty
    /// tail a guest lands on when the stream divides evenly.
    #[test]
    fn get_tables_refuses_an_offset_past_the_end_of_the_stream() {
        let mut d = dev();
        let total = d.tables.bytes.len() as u64;
        for addr in [total + 1, total + 4096, u64::MAX] {
            let (rsp, body) = get_tables(&mut d, addr, 4096);
            assert_eq!(rsp.ret, -libc::EINVAL, "addr {addr} of {total} accepted");
            assert!(body.is_empty());
        }
        let (rsp, body) = get_tables(&mut d, total, 4096);
        assert_eq!(rsp.ret, 0, "the empty tail is a legal read");
        assert_eq!((rsp.inline_len, body.len()), (0, 0));
    }

    /// A `map_len` of 0 still yields one byte, and a chunk that reaches
    /// the end of the stream is short.
    ///
    /// Both are the guest module's loop condition. A zero-length chunk
    /// would advance the guest's offset by nothing -- a loop that never
    /// ends, bought with a `map_len` the guest controls -- and a last
    /// chunk that was padded up to the asked-for length would append
    /// garbage to the table stream.
    #[test]
    fn get_tables_never_answers_with_zero_bytes_and_shortens_the_last_chunk() {
        let mut d = dev();
        let total = d.tables.bytes.len() as u64;
        assert!(total > 10);

        let (rsp, body) = get_tables(&mut d, 0, 0);
        assert_eq!((rsp.ret, rsp.inline_len), (0, 1), "map_len 0 still makes progress");
        assert_eq!(body, d.tables.bytes[..1]);

        let (rsp, body) = get_tables(&mut d, total - 10, 4096);
        assert_eq!((rsp.ret, body.len()), (0, 10), "the last chunk is what is left, not what was asked");
        assert_eq!(body, d.tables.bytes[total as usize - 10..]);
    }

    /// Paging the real stream reproduces `tables.bytes` byte for byte,
    /// `token` names the total length in every answer, and the reassembled
    /// bytes carry the header and the checksum the guest verifies them
    /// against.
    ///
    /// This is the one message the guest cannot survive getting wrong
    /// quietly: the module holds no NVIDIA constant of its own and
    /// interprets these bytes for every ioctl it forwards. A paging bug
    /// that dropped or duplicated a chunk would not fail here, it would
    /// mis-describe some ioctl several kilobytes in.
    ///
    /// The chunk size is deliberately small (and not `MAX_PAYLOAD`): the
    /// whole stream is presently under 7 KiB, so a maximum-size request
    /// would take exactly one chunk and page nothing.
    #[test]
    fn get_tables_pages_reassemble_into_the_checksummed_stream() {
        use nvrm_wire::tables as t;

        let mut d = dev();
        let want = d.tables.bytes.clone();
        let total = want.len() as u64;
        const CHUNK: u64 = 1000;
        assert!(total > 3 * CHUNK && total % CHUNK != 0, "stream of {total} bytes");

        let mut got: Vec<u8> = Vec::new();
        let mut lens: Vec<usize> = Vec::new();
        while (got.len() as u64) < total {
            let (rsp, chunk) = get_tables(&mut d, got.len() as u64, CHUNK);
            assert_eq!(rsp.ret, 0);
            assert_eq!(rsp.token, total, "token is the total length, in every chunk");
            assert!(!chunk.is_empty(), "a zero-length chunk never terminates the loop");
            got.extend_from_slice(&chunk);
            lens.push(chunk.len());
            assert!(lens.len() < 4096, "not converging");
        }
        let last = lens.pop().unwrap();
        assert!(lens.len() >= 3, "the stream took only {} full chunks", lens.len());
        assert!(lens.iter().all(|&n| n == CHUNK as usize), "a middle chunk was not full");
        assert_eq!(last, total as usize - lens.len() * CHUNK as usize);
        assert!(last < CHUNK as usize, "the last chunk is short");
        assert_eq!(got, want, "the pages do not reassemble the stream");

        // ... and what reassembled is a table stream that checks out.
        let word = |i: usize| u32::from_le_bytes(got[4 * i..4 * i + 4].try_into().unwrap());
        assert_eq!(word(0), t::TABLE_MAGIC, "magic 'NVRT'");
        assert_eq!(word(1), t::TABLE_VERSION);
        assert_eq!(word(2) as usize, got.len(), "total_len covers the header too");
        assert_eq!(word(3), t::fnv1a32(&got[t::HDR_LEN..]), "checksum over the body");
        assert_eq!(word(3), d.tables.checksum, "and it is the one the device logged");
    }

    /// However large a `map_len` the guest asks for, one chunk is at most
    /// `MAX_PAYLOAD`.
    ///
    /// Uncapped, a `map_len` of `u64::MAX` would build a response longer
    /// than the buffer the guest posted, and `process_queue` would have to
    /// turn the whole read into EMSGSIZE -- the guest would never get its
    /// tables and could forward nothing at all.
    ///
    /// The stream as built today is SHORTER than one maximum chunk
    /// (~7 KiB), so the cap cannot bite on it and a test against the real
    /// tables would prove nothing. The handler serves whatever
    /// `tables.bytes` holds, so the bytes are lengthened here instead --
    /// which is exactly the situation the next table growth creates.
    #[test]
    fn get_tables_caps_one_chunk_at_max_payload() {
        let mut d = dev();
        let real = d.tables.bytes.len();
        let grown = 2 * proto::MAX_PAYLOAD + 1234;
        assert!(real < grown);
        // A pattern, not zeros: a paging bug that repeated or skipped a
        // chunk would be invisible against a uniform filler.
        let filler: Vec<u8> = (real..grown).map(|i| (i % 251) as u8).collect();
        d.tables.bytes.extend_from_slice(&filler);
        let want = d.tables.bytes.clone();
        assert_eq!(want.len(), grown);

        for map_len in [u64::MAX, proto::MAX_PAYLOAD as u64 + 1, 1 << 40] {
            let (rsp, body) = get_tables(&mut d, 0, map_len);
            assert_eq!(body.len(), proto::MAX_PAYLOAD, "map_len {map_len} was not capped");
            assert_eq!(rsp.inline_len as usize, proto::MAX_PAYLOAD);
            assert_eq!(rsp.token, grown as u64);
        }

        // And the guest's loop still terminates on it, in three chunks.
        let mut got: Vec<u8> = Vec::new();
        let mut lens: Vec<usize> = Vec::new();
        while got.len() < grown {
            let (_, chunk) = get_tables(&mut d, got.len() as u64, u64::MAX);
            assert!(!chunk.is_empty());
            got.extend_from_slice(&chunk);
            lens.push(chunk.len());
            assert!(lens.len() < 16, "not converging");
        }
        assert_eq!(lens, vec![proto::MAX_PAYLOAD, proto::MAX_PAYLOAD, 1234]);
        assert_eq!(got, want);
    }

    /// A message too short to hold a `Req` is EPROTO, with sequence 0.
    ///
    /// There is nothing else it could be: the sequence number lives inside
    /// the bytes that did not arrive, so the device cannot echo one, and
    /// it must not read the fields either. `process_queue` bounds the size
    /// before calling `handle`, but `handle` is also the fuzz target's
    /// entry point (`fuzz/fuzz_targets/handle_msg.rs`) and must stand on
    /// its own.
    #[test]
    fn a_message_shorter_than_a_request_is_eproto() {
        let mut d = dev();
        let full = Req { seq: 77, kind: proto::KIND_PROC_GONE, guest_proc: 5, ..Req::default() };
        for n in [0, 1, 4, Req::WIRE_LEN - 1] {
            let rsp = Rsp::from_bytes(&d.handle(&full.as_bytes()[..n])).expect("a Rsp comes back");
            assert_eq!(rsp.ret, -libc::EPROTO, "{n} bytes were accepted as a request");
            assert_eq!(rsp.seq, 0, "no sequence number arrived, so none is echoed");
        }
        // The exact length is enough; nothing beyond the header is needed.
        let rsp = answer(&mut d, full);
        assert_eq!((rsp.ret, rsp.seq), (0, 77));
    }

    /// MAP_RELEASE without a VMM channel answers EIO and KEEPS the mapping
    /// in the book.
    ///
    /// `self.backend` is the vhost-user BACKEND channel, the one the
    /// device sends SHMEM_MAP/SHMEM_UNMAP on; the VMM hands it over at
    /// `set_backend_req_fd`, so a device built by a unit test does not
    /// have one. Releasing a window mapping IS that message -- the entry
    /// here is only the host's record of what the VMM blended in -- so
    /// without the channel there is nothing to do but refuse. Forgetting
    /// the entry anyway would be the worse failure: the VMM would keep the
    /// mapping, the offset would never be reusable, and nothing would ever
    /// name it again. That ordering is what the second assertion pins.
    #[test]
    fn map_release_without_a_vmm_channel_is_eio_and_keeps_the_entry() {
        let mut d = dev();
        assert!(d.backend.is_none(), "a unit test has no VMM to talk to");
        d.window.insert(
            0x1000,
            WindowMap {
                len: 0x2000,
                guest_proc: 5,
                _fd: std::fs::File::open("/dev/null").expect("/dev/null"),
            },
        );

        let rsp = answer(
            &mut d,
            Req { seq: 4, kind: proto::KIND_MAP_RELEASE, addr: 0x1000, ..Req::default() },
        );
        assert_eq!(rsp.ret, -libc::EIO);
        assert_eq!(rsp.seq, 4);
        assert!(
            d.window.contains_key(&0x1000),
            "a release the device could not carry out must not forget the mapping"
        );
    }

    /// PROC_GONE refuses `guest_proc == 0` and shrugs at a process it
    /// never heard of.
    ///
    /// 0 is the "not stated" key: every caller that names no process
    /// shares that one session, so acting on a PROC_GONE for it would tear
    /// down a session that belongs to nobody in particular and to all of
    /// them at once. An UNKNOWN process, on the other hand, is the normal
    /// case -- a guest process that never forwarded anything still sends
    /// its PROC_GONE on exit -- so it must be a plain success, or every
    /// such exit would log an error the guest cannot act on.
    #[test]
    fn proc_gone_refuses_process_zero_but_not_an_unknown_process() {
        let mut d = dev();
        let rsp = answer(
            &mut d,
            Req { seq: 11, kind: proto::KIND_PROC_GONE, guest_proc: 0, ..Req::default() },
        );
        assert_eq!(rsp.ret, -libc::EINVAL, "guest_proc 0 is 'not stated', not a process");
        assert_eq!(rsp.seq, 11);

        let rsp = answer(
            &mut d,
            Req { seq: 12, kind: proto::KIND_PROC_GONE, guest_proc: 4242, ..Req::default() },
        );
        assert_eq!(rsp.ret, 0, "an absent session is not an error");
        assert_eq!(rsp.seq, 12);
        assert!(d.sessions.is_empty(), "and nothing was created on the way");
    }

    /// `err_rsp` puts the errno in `ret` NEGATED, echoes the sequence
    /// number, and carries no payload.
    ///
    /// The sign is the whole protocol: `Rsp.ret` is the return of `ioctl(2)`
    /// itself, so the guest module reads `ret < 0` as `-errno` and hands
    /// that straight to its caller. A positive value there would read as a
    /// successful ioctl whose return happened to be nonzero.
    #[test]
    fn err_rsp_negates_the_errno_and_echoes_the_sequence() {
        for (seq, errno) in
            [(0u32, libc::EINVAL), (7, libc::EPROTO), (u32::MAX, libc::EIO), (3, libc::ENOMEM)]
        {
            let bytes = err_rsp(seq, errno);
            assert_eq!(bytes.len(), Rsp::WIRE_LEN, "an error answer carries no payload");
            let rsp = Rsp::from_bytes(&bytes).unwrap();
            assert_eq!(rsp.seq, seq);
            assert_eq!(rsp.ret, -errno, "errno {errno} must arrive negated");
            assert!(rsp.ret < 0);
            assert_eq!((rsp.token, rsp.inline_len, rsp.aux_len, rsp.scm_fd_count), (0, 0, 0, 0));
        }
    }
}
