// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Virtio request dispatch, shared-window mappings, and event delivery.
//!
//! Queue 0 carries requests and replies; queue 1 carries host events.
//! Sessions are keyed by guest process ID. IDs and cross-process FD owners
//! are guest-controlled; isolation and resource policy apply to the whole VM.

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
use nvrm_sys::RmAbi;

type Mem = GuestMemoryAtomic<GuestMemoryMmap<()>>;
type NvVring = VringRwLock<Mem>;

const VIRTIO_F_VERSION_1: u64 = 32;
const VHOST_USER_F_PROTOCOL_FEATURES: u64 = 30;

/// Cacheability values shared with virtio-gpu and the guest module.
const MAP_CACHE_CACHED: u32 = 0x01;
const MAP_CACHE_UNCACHED: u32 = 0x02;

/// Host-visible region ID. cloud-hypervisor uses the list index; slot 0 is empty.
const SHM_ID_HOST_VISIBLE: u8 = 1;

/// Virtual address space reserved for RM mappings. Backing is installed on demand.
/// 8 GiB accommodates the measured multi-GiB graphics working sets.
const HOST_VISIBLE_SIZE: u64 = 8 << 30;

/// One-shot latch for `LEA_TEST_SHMEM_MAP_OOB` (see `on_map_prepare`). Fires
/// once per process so the test costs exactly one mapping and everything
/// after it is the recovery being measured, not a second injection.
static TEST_OOB_FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// GPU mappings use uncached access because this path cannot distinguish
/// registers from framebuffer memory. Control-node system memory is cached.
fn cache_for(dev: nvrm_abi::xlate::Dev) -> u32 {
    match dev {
        nvrm_abi::xlate::Dev::Gpu => MAP_CACHE_UNCACHED,
        _ => MAP_CACHE_CACHED,
    }
}

/// Experimental virtio type. Must match VIRTIO_ID_NVRM in the generated
/// header and the hypervisor device_type argument.
pub const VIRTIO_ID_NVRM: u32 = 60;

/// Indirect descriptors allow a 1 MiB auxiliary buffer in a 256-entry queue.
const VIRTIO_RING_F_INDIRECT_DESC: u64 = 28;

/// Use the session's cached debug level; an empty LEA_DEBUG disables logging.
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
    /// Owner used to release leftover mappings on PROC_GONE.
    guest_proc: u32,
    _fd: std::fs::File,
}

/// Window offsets owned by this guest process.
fn window_of_proc(
    window: &std::collections::BTreeMap<u64, WindowMap>,
    guest_proc: u32,
) -> Vec<u64> {
    window
        .iter()
        .filter(|(_, m)| m.guest_proc == guest_proc)
        .map(|(&off, _)| off)
        .collect()
}

/// Whether [off, off + len) overlaps an existing mapping.
/// Existing mappings are disjoint, so only the last start before the query
/// end can overlap. Callers validate both ranges against HOST_VISIBLE_SIZE.
fn window_overlaps(
    window: &std::collections::BTreeMap<u64, WindowMap>,
    off: u64,
    len: u64,
) -> bool {
    window
        .range(..off + len)
        .next_back()
        .is_some_and(|(&o, m)| o + m.len > off)
}

/// The worker reserves event IDs 0 and 1 for queues, and 2 for exit.
/// Its handle_event interface receives this ID as u16.
const EVENT_LISTENER: u64 = 3;
const EVENT_LISTENER_U16: u16 = EVENT_LISTENER as u16;

/// One fd in the device's epoll set, resolved from the `data` word.
#[derive(Clone, Copy, Debug)]
enum PollSrc {
    /// A session's per-client event ctl: readable = drain with
    /// `Session::drain_os_events`, one `KIND_EVENT_FIRED` per firing.
    EventCtl {
        guest_proc: u32,
        h_client: u32,
        fd: RawFd,
    },
    /// A guest fd that allocated an OS event itself: readable = tell the
    /// guest to wake the fd behind `token`.
    Client {
        guest_proc: u32,
        token: u64,
        fd: RawFd,
    },
    /// Waiter completions, delivered through a persistent eventfd counter.
    /// NVIDIA waiter FDs use poll(2) in waiters.rs because epoll consumes wakes.
    WaiterNotify,
}

/// The dedupe key of a `PollSrc`: `(guest_proc, token, h_client)` with the
/// unused half at its sentinel (`NONE_U64` for an event ctl, 0 for a
/// client fd -- 0 is not a client handle RM hands out).
type PollKey = (u32, u64, u32);

pub struct NvrmDevice<A: RmAbi> {
    mem: Option<Mem>,
    /// Guest process ID to session. ID 0 shares the unspecified-process session.
    /// Separate sessions prevent GPU VA and token collisions between processes.
    sessions: BTreeMap<u32, Session<A>>,
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
    /// VM-wide VRAM policy and accounting, shared by all sessions.
    vram: std::sync::Arc<crate::vram::Ledger>,
    pins: std::sync::Arc<crate::host_pool::PinBudget>,

    /// Nested epoll for RM events. Registering FDs directly with the worker
    /// from handle_event would reacquire the backend RwLock and deadlock.
    /// Register this epoll FD once, before serving requests.
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
    event_error: Option<String>,
}

impl<A: RmAbi> NvrmDevice<A> {
    fn new() -> anyhow::Result<Self> {
        let tables = nvrm_abi::table::build::<A>();
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
            sessions: BTreeMap::new(),
            fd_field_misses: 0,
            tables,
            backend: None,
            window: BTreeMap::new(),
            // A refused profile ends the backend here, before the socket
            // exists: the guest then fails to start with the reason in
            // this log, rather than coming up under a policy nobody chose.
            vram: crate::vram::Ledger::new().map_err(|e| anyhow::anyhow!("{e}"))?,
            pins: crate::host_pool::PinBudget::from_env()?,
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
            event_error: None,
        })
    }

    /// An eventfd retains readiness until read, so it is safe in epoll.
    fn register_waiter_notify(&mut self) -> std::io::Result<()> {
        let fd = self.waiters.notify_fd();
        let id = self.next_poll_id;
        self.next_poll_id += 1;
        self.poll
            .ctl(ControlOperation::Add, fd, EpollEvent::new(EventSet::IN, id))?;
        self.poll_srcs.insert(id, PollSrc::WaiterNotify);
        dlog!("poll +{id}: WaiterNotify fd {fd}");
        Ok(())
    }

    // Event registration.

    fn poll_key(guest_proc: u32, p: &Pollable) -> PollKey {
        match p {
            Pollable::EventCtl { h_client, .. } => (guest_proc, proto::NONE_U64, *h_client),
            Pollable::Client { token, owner, .. } => (owner.unwrap_or(guest_proc), *token, 0),
            // Never in the epoll set; register_pollables diverts these
            // before the key is asked for.
            Pollable::Waiter { .. } => unreachable!("waiter fds bypass the epoll set"),
        }
    }

    /// Register event FDs. Event controls are level-triggered and drained here;
    /// client FDs are edge-triggered and drained by the guest to avoid a busy loop.
    fn register_pollables(&mut self, guest_proc: u32, new: Vec<Pollable>) -> std::io::Result<()> {
        for p in new {
            // Waiters use the dedicated poller; registration retains the FD.
            if let Pollable::Waiter { registration, fd } = p {
                self.waiters.watch(crate::waiters::Watch {
                    fired: crate::waiters::Fired {
                        guest_proc,
                        registration,
                    },
                    fd,
                })?;
                continue;
            }
            let key = Self::poll_key(guest_proc, &p);
            if self.poll_ids.contains_key(&key) {
                continue;
            }
            let (fd, src, set) = match p {
                Pollable::EventCtl { h_client, fd } => (
                    fd,
                    PollSrc::EventCtl {
                        guest_proc,
                        h_client,
                        fd,
                    },
                    EventSet::IN,
                ),
                Pollable::Client { token, fd, owner } => (
                    fd,
                    // The firing names the token's OWNER, or the guest
                    // looks the token up in the wrong process.
                    PollSrc::Client {
                        guest_proc: owner.unwrap_or(guest_proc),
                        token,
                        fd,
                    },
                    EventSet::IN | EventSet::EDGE_TRIGGERED,
                ),
                Pollable::Waiter { .. } => unreachable!("diverted above"),
            };
            let id = self.next_poll_id;
            self.next_poll_id += 1;
            match self
                .poll
                .ctl(ControlOperation::Add, fd, EpollEvent::new(set, id))
            {
                Ok(()) => {
                    self.poll_srcs.insert(id, src);
                    self.poll_ids.insert(key, id);
                    dlog!("poll +{id}: {src:?} fd {fd}");
                }
                Err(e) => {
                    eprintln!("vhost-user-nvrm: event poll: cannot watch fd {fd} ({src:?}): {e}")
                }
            }
        }
        Ok(())
    }

    /// Remove the kernel registration and its lookup entries. A hung-up FD
    /// left in level-triggered epoll would spin. fd_hint supplies the current
    /// mirror FD; the stored FD remains valid for event controls.
    fn unregister_id(&mut self, id: u64, fd_hint: Option<RawFd>) {
        let Some(src) = self.poll_srcs.remove(&id) else {
            return;
        };
        let (key, fd) = match src {
            PollSrc::EventCtl {
                guest_proc,
                h_client,
                fd,
            } => ((guest_proc, proto::NONE_U64, h_client), fd),
            PollSrc::Client {
                guest_proc,
                token,
                fd,
            } => ((guest_proc, token, 0), fd),
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
        let _ = self.poll.ctl(
            ControlOperation::Delete,
            fd,
            EpollEvent::new(EventSet::IN, id),
        );
        dlog!("poll -{id}: {src:?}");
    }

    /// Unregister before closing the mirror FD. A duplicate held by the VMM
    /// can keep the epoll registration alive after the original FD closes.
    fn unregister_token(&mut self, guest_proc: u32, token: u64) {
        let Some(&id) = self.poll_ids.get(&(guest_proc, token, 0)) else {
            return;
        };
        let fd = self
            .sessions
            .get(&guest_proc)
            .and_then(|s| s.mirror_raw(token));
        self.unregister_id(id, fd);
    }

    /// Unregister a freed client's event control before the caller closes it.
    fn unregister_event_ctl(&mut self, guest_proc: u32, h_client: u32) {
        let key = (guest_proc, proto::NONE_U64, h_client);
        let Some(&id) = self.poll_ids.get(&key) else {
            return;
        };
        self.unregister_id(id, None);
    }

    /// Everything a session ever reported, on its way out.
    fn unregister_proc(&mut self, guest_proc: u32) -> std::io::Result<()> {
        self.waiters.cancel_process(guest_proc)?;
        let ids: Vec<u64> =
            self.poll_srcs
                .iter()
                .filter(|(_, s)| match **s {
                    PollSrc::EventCtl { guest_proc: g, .. }
                    | PollSrc::Client { guest_proc: g, .. } => g == guest_proc,
                    PollSrc::WaiterNotify => false,
                })
                .map(|(&id, _)| id)
                .collect();
        for id in ids {
            let fd = match self.poll_srcs.get(&id) {
                Some(PollSrc::Client { token, .. }) => self
                    .sessions
                    .get(&guest_proc)
                    .and_then(|s| s.mirror_raw(*token)),
                // The event ctl closes with the session and takes its
                // registration along; the fd is not duplicated anywhere.
                _ => None,
            };
            self.unregister_id(id, fd);
        }
        Ok(())
    }

    // Event delivery.

    fn next_seq(&mut self) -> u32 {
        self.ev_seq = self.ev_seq.wrapping_add(1);
        self.ev_seq
    }

    /// Deliver one event without blocking request processing.
    /// Returns false and increments a drop counter if delivery fails.
    fn push_event(&mut self, evq: &NvVring, req: &Req) -> bool {
        if !evq.get_ref().is_enabled() {
            self.ev_dropped_noqueue += 1;
            return false;
        }
        let Some(mem) = self.mem.as_ref().map(|m| m.memory()) else {
            self.ev_dropped_noqueue += 1;
            return false;
        };
        let chain = evq
            .get_mut()
            .get_queue_mut()
            .pop_descriptor_chain(mem.clone());
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

    /// Drain bounded event batches so request processing cannot starve.
    /// Level triggering schedules any remainder. Signal the guest once per pass.
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
                    if matches!(self.poll_srcs.get(&id), Some(PollSrc::WaiterNotify)) {
                        return Err(std::io::Error::other("waiter notification FD failed"));
                    }
                    // The fd died under us; the kernel drops it from the set
                    // when the last reference goes, but forget it now.
                    dlog!("poll id {id}: fd hung up / errored ({set:?})");
                    self.unregister_id(id, None);
                    continue;
                }
                match self.poll_srcs.get(&id).copied() {
                    Some(PollSrc::Client {
                        guest_proc, token, ..
                    }) => {
                        let seq = self.next_seq();
                        let req = Req {
                            seq,
                            kind: proto::KIND_EVENT_FIRED,
                            ioctl_nr: sys::NV01_EVENT_OS_EVENT,
                            target_token: token,
                            guest_proc,
                            ..Req::default()
                        };
                        dlog!(
                            "EVENT_FIRED class {:#x} proc {guest_proc} tok {token}",
                            sys::NV01_EVENT_OS_EVENT
                        );
                        wrote_any |= self.push_event(evq, &req);
                        self.ev_wakes += 1;
                    }
                    Some(PollSrc::EventCtl {
                        guest_proc,
                        h_client,
                        ..
                    }) => {
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
                        // A failed drain can close the control. Remove its registration so a
                        // replacement under the same client key can be registered.
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
                                f.reg.class,
                                f.reg.token,
                                f.reg.h_client,
                                f.reg.h_event,
                                f.reg.notify_index,
                                f.reg.id
                            );
                            wrote_any |= self.push_event(evq, &req);
                        }
                    }
                    Some(PollSrc::WaiterNotify) => {
                        // Retire completed waiters and return their callback data. A zero
                        // hEvent distinguishes these from allocated event objects.
                        for w in self.waiters.take_fired()? {
                            let woken = self
                                .sessions
                                .get_mut(&w.guest_proc)
                                .and_then(|s| s.semsurf_wake(w.registration));
                            let Some((h_client, kc, token)) = woken else {
                                dlog!(
                                    "waiter id {} of proc {}: no books -- dropped",
                                    w.registration.event_id,
                                    w.guest_proc
                                );
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
                                fd_field_token: w.registration.event_id as u64,
                                embedded_ptr_off: 0,
                                addr: kc,
                                guest_proc: w.guest_proc,
                                ..Req::default()
                            };
                            dlog!(
                                "EVENT_FIRED semsurf proc {} hClient {h_client:#x} kc {kc:#x} id {}",
                                w.guest_proc, w.registration.event_id
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

    /// Compare session-owned FDs with /proc/self/fd, gated by LEA_FD_CENSUS.
    fn fd_census(&self) {
        // An empty LEA_FD_CENSUS disables the directory scan.
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
        // Subtract all session FDs from all process FDs. Keep the signed result
        // to expose inconsistent counts; window FDs are outside the sessions.
        eprintln!(
            "vhost-user-nvrm: fd census: {} sessions hold {named} \
             (mirror {} of {} ever, event_ctls {} pooled_waiters {} armed {} pending {} osdesc {}) \
             | window {} maps \
             | process has {total} fds, {ctl} of them nvidiactl \
             | outside every session {}",
            self.sessions.len(),
            t[0],
            t[1],
            t[2],
            t[3],
            t[4],
            t[5],
            t[6],
            self.window.len(),
            total as i64 - named as i64,
        );
        for (id, s) in &self.sessions {
            let c = s.census();
            eprintln!(
                "vhost-user-nvrm:   session {id} ({}): mirror {} of {} ever, \
                 event_ctls {}, pooled_waiters {}, armed {}",
                s.proc_name(),
                c[0],
                c[1],
                c[2],
                c[3],
                c[4],
            );
        }
    }

    /// Get or create the session. Refuse new sessions at the VM-wide limit.
    fn session_for(&mut self, sub_id: u32) -> Option<&mut Session<A>> {
        if !self.sessions.contains_key(&sub_id) {
            // Bound guest-controlled session creation and its associated host resources.
            const MAX_SESSIONS: usize = 1024;
            if self.sessions.len() >= MAX_SESSIONS {
                eprintln!(
                    "vhost-user-nvrm: {MAX_SESSIONS} guest processes at once -- \
                     further ones refused (ID {sub_id})"
                );
                return None;
            }
            match Session::<A>::detached_proc(sub_id, self.vram.clone(), self.pins.clone()) {
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

    /// Release a window slot after SHMEM_UNMAP succeeds. Requires REPLY_ACK.
    fn release_window(&mut self, off: u64) -> std::io::Result<()> {
        let backend = self
            .backend
            .as_ref()
            .ok_or_else(|| std::io::Error::other("SHMEM_UNMAP without a backend channel"))?;
        let Some(entry) = self.window.get(&off) else {
            return Ok(());
        };
        let msg = VhostUserMMap {
            shmid: SHM_ID_HOST_VISIBLE,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset: off,
            len: entry.len,
            flags: 0,
        };
        backend.shmem_unmap(&msg)?;
        self.window.remove(&off);
        Ok(())
    }

    /// Release a process's mappings. Failed unmaps retain their FDs and reserved slots.
    fn release_window_of(&mut self, guest_proc: u32) -> std::io::Result<()> {
        let mut failure = None;
        for off in window_of_proc(&self.window, guest_proc) {
            if let Err(e) = self.release_window(off) {
                eprintln!("vhost-user-nvrm: SHMEM_UNMAP proc {guest_proc} window+{off:#x}: {e}");
                failure = Some(e);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Unregister events, release mappings, and remove an exited session.
    fn on_proc_gone(&mut self, req: &Req) -> Vec<u8> {
        if req.guest_proc == 0 {
            return err_rsp(req.seq, libc::EINVAL);
        }
        // The poll registrations go BEFORE the session: the session's drop
        // closes the fds, and a registration must not outlive its fd.
        if let Err(e) = self.unregister_proc(req.guest_proc) {
            self.event_error = Some(e.to_string());
            return err_rsp(req.seq, libc::EIO);
        }
        // ... and the window mappings it never released itself.
        if self.release_window_of(req.guest_proc).is_err() {
            return err_rsp(req.seq, libc::EIO);
        }
        match self.sessions.remove(&req.guest_proc) {
            Some(s) => {
                dlog!("guest process {} exited, session torn down", req.guest_proc);
                // Report event delivery only for sessions that registered events.
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
        self.pins.retry_cleanup();
        if self.pins.used_bytes() != 0 {
            eprintln!(
                "vhost-user-nvrm: guest-page registrations: {} bytes charged, {} retained, {} cleanup-pending",
                self.pins.used_bytes(), self.pins.retained_bytes(), self.pins.quarantined_bytes()
            );
        }
        self.fd_census();
        Rsp {
            seq: req.seq,
            ..Rsp::default()
        }
        .as_bytes()
        .to_vec()
    }

    fn resolve_fd(&self, guest_proc: u32, token: u64) -> Option<RawFd> {
        self.sessions.get(&guest_proc)?.mirror_raw(token)
    }

    /// Answer one message. The return value is the finished response bytes.
    fn handle(&mut self, msg: &[u8]) -> Vec<u8> {
        let Some(req) = Req::from_bytes(msg) else {
            dlog!("message shorter than Req ({} bytes)", msg.len());
            return err_rsp(0, libc::EPROTO);
        };

        if self.event_error.is_some() {
            return err_rsp(req.seq, libc::EIO);
        }
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
                // Resolve inline and auxiliary FD owners before borrowing the caller
                // session. Lookup must not create sessions from guest-controlled fields.
                let fd_field_fd = if req.fd_field_token != proto::NONE_U64
                    && req.fd_field_proc != proto::NONE_U32
                {
                    self.resolve_fd(req.fd_field_proc, req.fd_field_token)
                } else {
                    None
                };
                // Log unresolved translated FDs. An explicit owner must resolve in that
                // session; only an unspecified owner may use the caller's mirror.
                if req.fd_field_token != proto::NONE_U64
                    && req.fd_field_off != proto::NONE_U32
                    && fd_field_fd.is_none()
                    && (req.fd_field_proc != proto::NONE_U32
                        || self
                            .resolve_fd(req.guest_proc, req.fd_field_token)
                            .is_none())
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
                    self.resolve_fd(req.aux_fd_field_proc, req.aux_fd_field_token)
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
                let new = session.take_pollables();
                let unwatch = session.take_unwatch();
                let ctl_unwatch = session.take_ctl_unwatch();
                let ids: Vec<u32> = unwatch.iter().map(|r| r.event_id()).collect();
                if let Err(e) = self.waiters.cancel_slots(req.guest_proc, &ids) {
                    self.event_error = Some(e.to_string());
                    return err_rsp(req.seq, libc::EIO);
                }
                // The old poll is finished before these slots become reusable.
                self.sessions
                    .get_mut(&req.guest_proc)
                    .unwrap()
                    .finish_unwatch(unwatch);
                if let Err(e) = self.register_pollables(req.guest_proc, new) {
                    self.event_error = Some(e.to_string());
                    return err_rsp(req.seq, libc::EIO);
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

    /// Return a bounded table chunk: addr is the offset, map_len the requested
    /// length, and the response token carries the total stream length.
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

    /// Validate and consume a session mapping, then install it at the
    /// guest-selected window offset through SHMEM_MAP.
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

        // Reject invalid RM mapping contexts before calling the VMM. Its mmap
        // path reads the context without consuming it, so this probe is repeatable.
        // The probe VMA is released before the VMM creates the final mapping.
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

        let mut msg_map = VhostUserMMap {
            shmid: SHM_ID_HOST_VISIBLE,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset: off,
            len,
            flags: VhostUserMMapFlags::WRITABLE.bits(),
        };
        // Inject one out-of-window SHMEM_MAP to test VMM refusal and recovery.
        // An empty LEA_TEST_SHMEM_MAP_OOB disables the hook.
        if std::env::var_os("LEA_TEST_SHMEM_MAP_OOB").is_some_and(|v| !v.is_empty())
            && !TEST_OOB_FIRED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            msg_map.shm_offset = HOST_VISIBLE_SIZE;
            // Copied out first: VhostUserMMap is packed, so a format macro
            // cannot borrow the field.
            let bad = HOST_VISIBLE_SIZE;
            eprintln!(
                "vhost-user-nvrm: LEA_TEST_SHMEM_MAP_OOB -- asking the VMM to map at \
                 {bad:#x}, one window past the end, on purpose"
            );
        }
        if let Err(e) = backend.shmem_map(&msg_map, &fd) {
            eprintln!("vhost-user-nvrm: SHMEM_MAP: {e}");
            return err_rsp(req.seq, libc::EIO);
        }
        self.window.insert(
            off,
            WindowMap {
                len,
                guest_proc: req.guest_proc,
                _fd: fd,
            },
        );
        dlog!(
            "MapPrepare -> window+{off:#x}, {len} bytes, cache {:#x}",
            cache_for(dev)
        );

        // Here `token` carries the CACHEABILITY, not a mapping id: the
        // host knows the NVOS33 (RM_MAP_MEMORY parameter block) flags, so
        // the guest module does not have to guess.
        Rsp {
            seq: req.seq,
            ret: 0,
            token: cache_for(dev) as u64,
            ..Rsp::default()
        }
        .as_bytes()
        .to_vec()
    }

    /// Release a VM-wide window offset. Guest process IDs are not a trust boundary.
    fn on_map_release(&mut self, req: &Req) -> Vec<u8> {
        if let Err(e) = self.release_window(req.addr) {
            eprintln!("vhost-user-nvrm: SHMEM_UNMAP: {e}");
            return err_rsp(req.seq, libc::EIO);
        }
        Rsp {
            seq: req.seq,
            ..Rsp::default()
        }
        .as_bytes()
        .to_vec()
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

            if let Some(error) = &self.event_error {
                return Err(std::io::Error::other(format!(
                    "waiter event path failed: {error}"
                )));
            }

            // If the response does not fit into the guest's buffer, it gets
            // a clean error instead of a truncated message.
            let resp = if resp.len() > writer.available_bytes() {
                dlog!(
                    "response {} bytes > response buffer {}",
                    resp.len(),
                    writer.available_bytes()
                );
                err_rsp(
                    Rsp::from_bytes(&resp).map(|r| r.seq).unwrap_or(0),
                    libc::EMSGSIZE,
                )
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
    Rsp {
        seq,
        ret: -errno,
        ..Rsp::default()
    }
    .as_bytes()
    .to_vec()
}

impl<A: RmAbi> VhostUserBackendMut for NvrmDevice<A> {
    type Bitmap = ();
    type Vring = NvVring;

    /// Queue 0 carries requests; queue 1 carries events.
    /// The hypervisor must configure `queue_sizes=[256,256]`.
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

    /// Region 1 is the host-visible window; region 0 is unused.
    /// Requires the generic-vhost-user SHMEM hypervisor patch.
    fn get_shmem_config(&self) -> std::io::Result<VhostUserShMemConfig> {
        Ok(VhostUserShMemConfig::new(2, &[0, HOST_VISIBLE_SIZE]))
    }

    fn set_backend_req_fd(&mut self, backend: vhost::vhost_user::Backend) {
        self.backend = Some(backend);
    }

    fn set_event_idx(&mut self, _enabled: bool) {}

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
                    dlog!(
                        "evq kick: ready={} next_avail={} next_used={} avail_idx(guest)={:?}",
                        vq.ready(),
                        vq.next_avail(),
                        vq.next_used(),
                        self.mem.as_ref().map(|m| vq
                            .avail_idx(&*m.memory(), std::sync::atomic::Ordering::Acquire)
                            .map(|i| i.0))
                    );
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

/// Serve one VM using the detected host driver ABI.
/// Guest userspace must match that driver version; no guest-version check exists.
pub fn serve(socket: &str) -> anyhow::Result<()> {
    let version = nvrm_sys::detect()?;
    eprintln!("vhost-user-nvrm: driver {} ABI", version.as_str());
    struct Serve<'a>(&'a str);
    impl nvrm_sys::AbiVisitor for Serve<'_> {
        type Out = anyhow::Result<()>;
        fn visit<A: RmAbi>(self) -> Self::Out {
            serve_with::<A>(self.0)
        }
    }
    nvrm_sys::dispatch(version, Serve(socket))
}

fn serve_with<A: RmAbi>(socket: &str) -> anyhow::Result<()> {
    // The card, and which VM this process serves (from the socket path),
    // before the ledger reads its profile: the encoder share and the VM's
    // own UUID come out of both (grid.rs).
    crate::grid::set_card(socket, crate::host_pool::card());
    let backend = Arc::new(RwLock::new(NvrmDevice::<A>::new()?));
    backend.write().unwrap().register_waiter_notify()?;
    let mut daemon = VhostUserDaemon::new(
        "vhost-user-nvrm".into(),
        backend.clone(),
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .map_err(|e| anyhow::anyhow!("vhost-user daemon: {e:?}"))?;

    // Register before serving: register_listener reads the backend lock.
    // Copy the FD out first so no backend lock remains held during registration.
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

    // Avoid joining a vring worker blocked in epoll_wait after VMM disconnect.
    // Process exit releases RM clients and memory. Orderly worker shutdown is
    // a separate lifecycle fix; returning here previously leaked the backend.
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
                (
                    off,
                    WindowMap {
                        len,
                        guest_proc: 0,
                        _fd: fd,
                    },
                )
            })
            .collect()
    }

    fn win_owned(entries: &[(u64, u64, u32)]) -> BTreeMap<u64, WindowMap> {
        entries
            .iter()
            .map(|&(off, len, guest_proc)| {
                let fd = std::fs::File::open("/dev/null").expect("/dev/null");
                (
                    off,
                    WindowMap {
                        len,
                        guest_proc,
                        _fd: fd,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn proc_gone_takes_only_that_processes_window_mappings() {
        let mut w = win_owned(&[
            (0x0000, 0x1000, 7),
            (0x1000, 0x1000, 9),
            (0x2000, 0x1000, 7),
            (0x3000, 0x1000, 0),
        ]);
        let mine = window_of_proc(&w, 7);
        assert_eq!(
            mine,
            vec![0x0000, 0x2000],
            "both of process 7's, in offset order"
        );
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

    #[test]
    fn an_empty_window_refuses_nothing() {
        assert!(!window_overlaps(&win(&[]), 0, 0x1000));
        assert!(!window_overlaps(&win(&[]), 0x4000_0000, 0x1000));
    }

    #[test]
    fn end_to_end_mappings_do_not_collide() {
        let w = win(&[(0, 0x1000), (0x2000, 0x1000)]);
        assert!(
            !window_overlaps(&w, 0x1000, 0x1000),
            "the gap between the two"
        );
        assert!(
            !window_overlaps(&w, 0x3000, 0x1000),
            "immediately after the last"
        );
    }

    #[test]
    fn the_four_shapes_of_an_overlap_are_all_caught() {
        let w = win(&[(0x2000, 0x2000)]); // [0x2000, 0x4000)
        assert!(window_overlaps(&w, 0x2000, 0x1000), "same start");
        assert!(
            window_overlaps(&w, 0x3000, 0x2000),
            "starts inside, ends after"
        );
        assert!(
            window_overlaps(&w, 0x1000, 0x2000),
            "starts before, ends inside"
        );
        assert!(window_overlaps(&w, 0x1000, 0x4000), "swallows it whole");
        assert!(
            !window_overlaps(&w, 0x1000, 0x1000),
            "ends exactly at its start"
        );
        assert!(
            !window_overlaps(&w, 0x4000, 0x1000),
            "starts exactly at its end"
        );
    }

    #[test]
    fn a_hit_far_down_the_map_is_still_found() {
        let w = win(&[(0, 0x1000), (0x2000, 0x1000), (0x8000, 0x4000)]);
        assert!(window_overlaps(&w, 0x9000, 0x1000), "inside the last one");
        assert!(
            !window_overlaps(&w, 0x4000, 0x4000),
            "the hole between the second and third"
        );
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;

    fn unmap_reply(
        d: &mut NvrmDevice<nvrm_sys::DefaultAbi>,
        status: u64,
    ) -> std::thread::JoinHandle<()> {
        use std::os::fd::BorrowedFd;
        use vhost::vhost_user::FrontendReqHandler;

        struct UnmapHandler(u64);
        impl VhostUserFrontendReqHandler for UnmapHandler {
            fn shmem_unmap(&self, _: &VhostUserMMap) -> std::io::Result<u64> {
                Ok(self.0)
            }
        }
        let mut frontend = FrontendReqHandler::new(Arc::new(UnmapHandler(status))).unwrap();
        frontend.set_reply_ack_flag(true);
        // SAFETY: frontend owns this FD until it moves into the response thread.
        let fd = unsafe { BorrowedFd::borrow_raw(frontend.get_tx_raw_fd()) };
        let socket = std::os::unix::net::UnixStream::from(fd.try_clone_to_owned().unwrap());
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let backend = vhost::vhost_user::Backend::from_stream(socket);
        backend.set_shmem_flag(true);
        backend.set_reply_ack_flag(true);
        d.backend = Some(backend);
        std::thread::spawn(move || {
            assert_eq!(frontend.handle_request().unwrap(), status);
        })
    }

    #[test]
    fn failed_unmap_keeps_slot_until_a_successful_retry() {
        let mut d = dev();
        d.window.insert(
            0x1000,
            WindowMap {
                len: 0x1000,
                guest_proc: 7,
                _fd: std::fs::File::open("/dev/null").unwrap(),
            },
        );
        let peer = unmap_reply(&mut d, 1);
        let req = Req {
            kind: proto::KIND_MAP_RELEASE,
            addr: 0x1000,
            ..Req::default()
        };
        assert_eq!(answer(&mut d, req).ret, -libc::EIO);
        peer.join().unwrap();
        assert!(d.overlaps(0x1000, 0x1000));

        let peer = unmap_reply(&mut d, 0);
        assert_eq!(answer(&mut d, req).ret, 0);
        peer.join().unwrap();
        assert!(d.window.is_empty());
    }

    #[test]
    fn process_cleanup_keeps_mappings_when_vmm_refuses_unmap() {
        let mut d = dev();
        assert!(d.session_for(7).is_some());
        d.window.insert(
            0x1000,
            WindowMap {
                len: 0x1000,
                guest_proc: 7,
                _fd: std::fs::File::open("/dev/null").unwrap(),
            },
        );
        let peer = unmap_reply(&mut d, 1);
        let req = Req {
            kind: proto::KIND_PROC_GONE,
            guest_proc: 7,
            ..Req::default()
        };
        assert_eq!(answer(&mut d, req).ret, -libc::EIO);
        peer.join().unwrap();
        assert!(d.window.contains_key(&0x1000));
        assert!(d.sessions.contains_key(&7));

        let peer = unmap_reply(&mut d, 0);
        assert_eq!(answer(&mut d, req).ret, 0);
        peer.join().unwrap();
        assert!(d.window.is_empty());
        assert!(!d.sessions.contains_key(&7));
    }

    fn dev() -> NvrmDevice<nvrm_sys::DefaultAbi> {
        NvrmDevice::new().expect("NvrmDevice::new must work without a GPU")
    }

    fn answer(d: &mut NvrmDevice<nvrm_sys::DefaultAbi>, req: Req) -> Rsp {
        Rsp::from_bytes(&d.handle(req.as_bytes())).expect("every answer starts with a Rsp")
    }

    fn get_tables(
        d: &mut NvrmDevice<nvrm_sys::DefaultAbi>,
        addr: u64,
        map_len: u64,
    ) -> (Rsp, Vec<u8>) {
        let req = Req {
            seq: 3,
            kind: proto::KIND_GET_TABLES,
            addr,
            map_len,
            ..Req::default()
        };
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

    #[test]
    fn get_tables_never_answers_with_zero_bytes_and_shortens_the_last_chunk() {
        let mut d = dev();
        let total = d.tables.bytes.len() as u64;
        assert!(total > 10);

        let (rsp, body) = get_tables(&mut d, 0, 0);
        assert_eq!(
            (rsp.ret, rsp.inline_len),
            (0, 1),
            "map_len 0 still makes progress"
        );
        assert_eq!(body, d.tables.bytes[..1]);

        let (rsp, body) = get_tables(&mut d, total - 10, 4096);
        assert_eq!(
            (rsp.ret, body.len()),
            (0, 10),
            "the last chunk is what is left, not what was asked"
        );
        assert_eq!(body, d.tables.bytes[total as usize - 10..]);
    }

    #[test]
    fn get_tables_pages_reassemble_into_the_checksummed_stream() {
        use nvrm_wire::tables as t;

        let mut d = dev();
        let want = d.tables.bytes.clone();
        let total = want.len() as u64;
        const CHUNK: u64 = 1000;
        assert!(
            total > 3 * CHUNK && total % CHUNK != 0,
            "stream of {total} bytes"
        );

        let mut got: Vec<u8> = Vec::new();
        let mut lens: Vec<usize> = Vec::new();
        while (got.len() as u64) < total {
            let (rsp, chunk) = get_tables(&mut d, got.len() as u64, CHUNK);
            assert_eq!(rsp.ret, 0);
            assert_eq!(
                rsp.token, total,
                "token is the total length, in every chunk"
            );
            assert!(
                !chunk.is_empty(),
                "a zero-length chunk never terminates the loop"
            );
            got.extend_from_slice(&chunk);
            lens.push(chunk.len());
            assert!(lens.len() < 4096, "not converging");
        }
        let last = lens.pop().unwrap();
        assert!(
            lens.len() >= 3,
            "the stream took only {} full chunks",
            lens.len()
        );
        assert!(
            lens.iter().all(|&n| n == CHUNK as usize),
            "a middle chunk was not full"
        );
        assert_eq!(last, total as usize - lens.len() * CHUNK as usize);
        assert!(last < CHUNK as usize, "the last chunk is short");
        assert_eq!(got, want, "the pages do not reassemble the stream");

        // ... and what reassembled is a table stream that checks out.
        let word = |i: usize| u32::from_le_bytes(got[4 * i..4 * i + 4].try_into().unwrap());
        assert_eq!(word(0), t::TABLE_MAGIC, "magic 'NVRT'");
        assert_eq!(word(1), t::TABLE_VERSION);
        assert_eq!(
            word(2) as usize,
            got.len(),
            "total_len covers the header too"
        );
        assert_eq!(
            word(3),
            t::fnv1a32(&got[t::HDR_LEN..]),
            "checksum over the body"
        );
        assert_eq!(
            word(3),
            d.tables.checksum,
            "and it is the one the device logged"
        );
    }

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
            assert_eq!(
                body.len(),
                proto::MAX_PAYLOAD,
                "map_len {map_len} was not capped"
            );
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

    #[test]
    fn a_message_shorter_than_a_request_is_eproto() {
        let mut d = dev();
        let full = Req {
            seq: 77,
            kind: proto::KIND_PROC_GONE,
            guest_proc: 5,
            ..Req::default()
        };
        for n in [0, 1, 4, Req::WIRE_LEN - 1] {
            let rsp = Rsp::from_bytes(&d.handle(&full.as_bytes()[..n])).expect("a Rsp comes back");
            assert_eq!(
                rsp.ret,
                -libc::EPROTO,
                "{n} bytes were accepted as a request"
            );
            assert_eq!(rsp.seq, 0, "no sequence number arrived, so none is echoed");
        }
        // The exact length is enough; nothing beyond the header is needed.
        let rsp = answer(&mut d, full);
        assert_eq!((rsp.ret, rsp.seq), (0, 77));
    }

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
            Req {
                seq: 4,
                kind: proto::KIND_MAP_RELEASE,
                addr: 0x1000,
                ..Req::default()
            },
        );
        assert_eq!(rsp.ret, -libc::EIO);
        assert_eq!(rsp.seq, 4);
        assert!(
            d.window.contains_key(&0x1000),
            "a release the device could not carry out must not forget the mapping"
        );
    }

    #[test]
    fn proc_gone_refuses_process_zero_but_not_an_unknown_process() {
        let mut d = dev();
        let rsp = answer(
            &mut d,
            Req {
                seq: 11,
                kind: proto::KIND_PROC_GONE,
                guest_proc: 0,
                ..Req::default()
            },
        );
        assert_eq!(
            rsp.ret,
            -libc::EINVAL,
            "guest_proc 0 is 'not stated', not a process"
        );
        assert_eq!(rsp.seq, 11);

        let rsp = answer(
            &mut d,
            Req {
                seq: 12,
                kind: proto::KIND_PROC_GONE,
                guest_proc: 4242,
                ..Req::default()
            },
        );
        assert_eq!(rsp.ret, 0, "an absent session is not an error");
        assert_eq!(rsp.seq, 12);
        assert!(d.sessions.is_empty(), "and nothing was created on the way");
    }

    #[test]
    fn err_rsp_negates_the_errno_and_echoes_the_sequence() {
        for (seq, errno) in [
            (0u32, libc::EINVAL),
            (7, libc::EPROTO),
            (u32::MAX, libc::EIO),
            (3, libc::ENOMEM),
        ] {
            let bytes = err_rsp(seq, errno);
            assert_eq!(
                bytes.len(),
                Rsp::WIRE_LEN,
                "an error answer carries no payload"
            );
            let rsp = Rsp::from_bytes(&bytes).unwrap();
            assert_eq!(rsp.seq, seq);
            assert_eq!(rsp.ret, -errno, "errno {errno} must arrive negated");
            assert!(rsp.ret < 0);
            assert_eq!(
                (rsp.token, rsp.inline_len, rsp.aux_len, rsp.scm_fd_count),
                (0, 0, 0, 0)
            );
        }
    }
}
