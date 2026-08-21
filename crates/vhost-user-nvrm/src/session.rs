// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! One guest process, seen from the host: the `Session`.
//!
//! The device (`nvrm.rs`) keys a session on `Req.guest_proc`, the dense id
//! the guest module assigns per process. A session owns the host-side
//! mirrors of the `/dev/nvidia*` files that process opened (`Mirror`,
//! token -> host fd), its pending window mappings, its UVM pool and
//! OS-descriptor arenas (`host_pool.rs`), its share of the VRAM ledger
//! (`vram.rs`), and the event/waiter substitutions described below.
//!
//! Vocabulary used throughout this file, once:
//!   - RM: NVIDIA's Resource Manager, the kernel driver behind
//!     `/dev/nvidiactl` and `/dev/nvidiaN`. Its ioctls are called
//!     "escapes" (`NV_ESC_*`); the parameter blocks are the `NVOS*`
//!     structs from nvos.h -- NVOS64/NVOS21 for RM_ALLOC (a new object of
//!     class `hClass`), NVOS54 for RM_CONTROL (a command `cmd` on an
//!     object, with an embedded params buffer), NVOS00 for RM_FREE,
//!     NVOS02 for RM_ALLOC_MEMORY, NVOS32 for RM_VID_HEAP_CONTROL,
//!     NVOS33 for RM_MAP_MEMORY, NVOS41 for RM_GET_EVENT_DATA, NVOS10
//!     for RM_ALLOC_EVENT.
//!   - UVM: the unified-memory driver behind `/dev/nvidia-uvm`; its
//!     commands are raw numbers without `_IOC` encoding.
//!   - OFD: an open file description. RM binds a client to the OFD it was
//!     created on, which is why the host runs every call on the mirrored fd
//!     and never on one of its own.
//!   - OS descriptor: `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` (hClass 0x71),
//!     memory RM PINS rather than copies -- the guest sends its pages as GPA
//!     runs and the host assembles them into an arena.
//!   - NVKMS: NVIDIA's modesetting kernel module in the guest, which
//!     reaches this backend through the module's kernel path and whose
//!     ring-0 callbacks (events, semaphore-surface waiters) the host must
//!     substitute with OS events it owns, because a guest kernel pointer
//!     means nothing here.
//!
//! The shape is a flat, explicit dispatch: `handle_msg` -> `on_*`, and for
//! an ioctl `prepare()` (every check against a possibly lying guest, every
//! translation) followed by `execute()` (the syscall through the
//! `NvSyscalls` seam, and the bookkeeping that depends on its answer). No RM
//! client of its own, no MAP_MEMORY special case -- the guest opens its own
//! fds and both roles are mirrored alike.

use std::os::fd::RawFd;

use anyhow::Result;

use nvrm_abi::iowr_raw;
use nvrm_abi::share;
use nvrm_abi::sys;
use nvrm_abi::xlate::Dev;
use nvrm_wire::{self as proto, DevTag, Kind, Req, Rsp, NONE_U32, NONE_U64};

use crate::guest_words::{GuestAddr, GuestLen};
use crate::host_pool::{GpaRun, PoolState};
use crate::mirror::Mirror;
use crate::syscalls::{NvSyscalls, RealSyscalls};

type Mem = vm_memory::GuestMemoryAtomic<vm_memory::GuestMemoryMmap<()>>;

/// `UVM_ALLOC_SEMAPHORE_POOL` (uvm_ioctl.h:757, UVM_IOCTL_BASE(68)). The
/// host answers it itself instead of forwarding -- otherwise the real UVM
/// would place the pool at GPU VA `base` and collide with the external
/// mapping the host puts there. `rmStatus` sits at offset 9240; the number
/// comes from the bindgen struct, not from a hand count.
const UVM_ALLOC_SEMAPHORE_POOL: u32 = 68;
const SEMAPHORE_POOL_RMSTATUS_OFF: usize =
    std::mem::offset_of!(sys::UVM_ALLOC_SEMAPHORE_POOL_PARAMS, rmStatus);

/// Where `rmStatus` sits in the parameter block of a UVM command, for the
/// failure log line and for the faked answers. UVM commands are raw numbers
/// with no `_IOC` size, so the offset cannot be derived from the request;
/// it comes from the bindgen struct of each command (uvm_ioctl.h). `None` =
/// a command this backend does not know the layout of (the log then prints
/// status 0).
fn uvm_status_off(nr: u32) -> Option<usize> {
    use nvrm_abi::xlate::uvm;
    use std::mem::offset_of;
    Some(match nr {
        uvm::INITIALIZE => offset_of!(sys::UVM_INITIALIZE_PARAMS, rmStatus),
        uvm::REGISTER_GPU_VASPACE => offset_of!(sys::UVM_REGISTER_GPU_VASPACE_PARAMS, rmStatus),
        uvm::UNREGISTER_GPU_VASPACE => offset_of!(sys::UVM_UNREGISTER_GPU_VASPACE_PARAMS, rmStatus),
        uvm::REGISTER_CHANNEL => offset_of!(sys::UVM_REGISTER_CHANNEL_PARAMS, rmStatus),
        uvm::UNREGISTER_CHANNEL => offset_of!(sys::UVM_UNREGISTER_CHANNEL_PARAMS, rmStatus),
        uvm::MAP_EXTERNAL_ALLOCATION => offset_of!(sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS, rmStatus),
        uvm::FREE => offset_of!(sys::UVM_FREE_PARAMS, rmStatus),
        uvm::REGISTER_GPU => offset_of!(sys::UVM_REGISTER_GPU_PARAMS, rmStatus),
        uvm::PAGEABLE_MEM_ACCESS => offset_of!(sys::UVM_PAGEABLE_MEM_ACCESS_PARAMS, rmStatus),
        uvm::SET_PREFERRED_LOCATION => offset_of!(sys::UVM_SET_PREFERRED_LOCATION_PARAMS, rmStatus),
        uvm::UNSET_PREFERRED_LOCATION => offset_of!(sys::UVM_UNSET_PREFERRED_LOCATION_PARAMS, rmStatus),
        uvm::ENABLE_READ_DUPLICATION => offset_of!(sys::UVM_ENABLE_READ_DUPLICATION_PARAMS, rmStatus),
        uvm::DISABLE_READ_DUPLICATION => offset_of!(sys::UVM_DISABLE_READ_DUPLICATION_PARAMS, rmStatus),
        uvm::SET_ACCESSED_BY => offset_of!(sys::UVM_SET_ACCESSED_BY_PARAMS, rmStatus),
        uvm::UNSET_ACCESSED_BY => offset_of!(sys::UVM_UNSET_ACCESSED_BY_PARAMS, rmStatus),
        uvm::MIGRATE => offset_of!(sys::UVM_MIGRATE_PARAMS, rmStatus),
        uvm::MAP_DYNAMIC_PARALLELISM_REGION => {
            offset_of!(sys::UVM_MAP_DYNAMIC_PARALLELISM_REGION_PARAMS, rmStatus)
        }
        uvm::ALLOC_SEMAPHORE_POOL => SEMAPHORE_POOL_RMSTATUS_OFF,
        uvm::PAGEABLE_MEM_ACCESS_ON_GPU => {
            offset_of!(sys::UVM_PAGEABLE_MEM_ACCESS_ON_GPU_PARAMS, rmStatus)
        }
        uvm::VALIDATE_VA_RANGE => offset_of!(sys::UVM_VALIDATE_VA_RANGE_PARAMS, rmStatus),
        uvm::CREATE_EXTERNAL_RANGE => offset_of!(sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS, rmStatus),
        uvm::MM_INITIALIZE => offset_of!(sys::UVM_MM_INITIALIZE_PARAMS, rmStatus),
        _ => return None,
    })
}

/// The offsets the "managed light" answers write into (see `Action::FakeManaged`
/// and the execute branch): every one is an `offset_of!` of the bindgen struct
/// so that the numbers quoted in the branch cannot drift from uvm_ioctl.h.
const MIGRATE_SEMAPHORE_ADDRESS_OFF: usize = std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, semaphoreAddress);
const MIGRATE_SEMAPHORE_PAYLOAD_OFF: usize = std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, semaphorePayload);
const MIGRATE_USER_SPACE_START_OFF: usize = std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, userSpaceStart);
const MIGRATE_USER_SPACE_LENGTH_OFF: usize = std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, userSpaceLength);
const MIGRATE_FLAGS_OFF: usize = std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, flags);
const _: () = {
    // The literal offsets the managed-compat branch has always used, now
    // pinned to the structs: rmStatus @16 for (UN)SET_PREFERRED_LOCATION's
    // sibling commands 43..45, @36 for SET_PREFERRED_LOCATION, @32 for
    // (UN)SET_ACCESSED_BY, @72 for MIGRATE; the two MIGRATE output words at
    // 56/64, semaphore address/payload at 40/48.
    assert!(SEMAPHORE_POOL_RMSTATUS_OFF == 9240);
    assert!(std::mem::offset_of!(sys::UVM_UNSET_PREFERRED_LOCATION_PARAMS, rmStatus) == 16);
    assert!(std::mem::offset_of!(sys::UVM_ENABLE_READ_DUPLICATION_PARAMS, rmStatus) == 16);
    assert!(std::mem::offset_of!(sys::UVM_DISABLE_READ_DUPLICATION_PARAMS, rmStatus) == 16);
    assert!(std::mem::offset_of!(sys::UVM_SET_PREFERRED_LOCATION_PARAMS, rmStatus) == 36);
    assert!(std::mem::offset_of!(sys::UVM_SET_ACCESSED_BY_PARAMS, rmStatus) == 32);
    assert!(std::mem::offset_of!(sys::UVM_UNSET_ACCESSED_BY_PARAMS, rmStatus) == 32);
    assert!(std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, rmStatus) == 72);
    assert!(MIGRATE_USER_SPACE_START_OFF == 56 && MIGRATE_USER_SPACE_LENGTH_OFF == 64);
    assert!(MIGRATE_SEMAPHORE_ADDRESS_OFF == 40 && MIGRATE_SEMAPHORE_PAYLOAD_OFF == 48);
    assert!(MIGRATE_FLAGS_OFF == 32);
};

/// A session's finished answer. Until the SEQPACKET transport was removed
/// it also carried SCM_RIGHTS FDs, for a transport that shared a kernel
/// with the guest; across a VM boundary there is no FD to pass.
#[derive(Default)]
pub struct Reply {
    pub bytes: Vec<u8>,
}

pub struct Session {
    mirror: Mirror,
    /// Resolved by the DEVICE for this one message, from the session named
    /// by `aux_fd_field_proc`. Set on every dispatch, read once in
    /// `prepare`. Never read without `aux_fd_field_token != NONE_U64`.
    aux_fd_host: Option<RawFd>,
    /// The fd behind `Req::fd_field_token`, resolved by the DEVICE against
    /// the session named in `fd_field_proc`. `None` = not stated, and then
    /// the lookup below falls back to this session's own mirror.
    fd_field_host: Option<RawFd>,
    scratch: Vec<u8>,
    aux: Vec<u8>,
    /// Collects what `reply()` produced, until the carrier picks it up.
    out: Reply,
    /// blob_id -> (token, length, device). Lives between MapPrepare and the
    /// moment the device fetches the mapping via
    /// [`Session::take_pending_map`] -- the device decides the cacheability
    /// from the `dev` half.
    pending_maps: std::collections::HashMap<u64, PendingMap>,
    /// The per-session id of a pending MapPrepare mapping: high 32 bits are
    /// the session's own id, the low 32 count up, and the value is handed
    /// back to the guest in `Rsp.token`. The name is inherited from the
    /// virtio-gpu era and names nothing blob-like today.
    next_blob_id: u64,
    /// Backing client, semaphore pools, 0x71 arenas.
    pool: PoolState,
    /// This session's guest RAM -- set by the device before every exec.
    /// Needed to assemble guest pages into one host VA.
    mem: Option<Mem>,
    /// Translates a uvm FD token (which the guest received at open time) to
    /// the host uvm FD -- for UvmPoolBack. Exactly one uvm FD per session
    /// is expected.
    uvm_token: Option<u64>,
    /// Which guest process owns this session (the guest module's dense ID).
    /// 0 = not stated; such callers share one session.
    sub_id: u32,
    /// What the guest said about itself (name, guest PID). Display and RM
    /// attribution only; nothing enforced hangs off it.
    proc: proto::ProcInfo,
    /// The seam between deciding and doing (OPEN-QUESTIONS nr 5): the
    /// three syscalls this session issues at all. A ledger in tests, the
    /// driver in production.
    sys: Box<dyn NvSyscalls>,
    /// This session's share of the VM's VRAM cap. Off unless
    /// `LEA_VRAM_LIMIT_MIB` is set; its Drop returns whatever the guest
    /// never freed itself.
    vram: crate::vram::Books,
    /// The control nodes this session's substituted OS events hang off,
    /// ONE PER RM CLIENT, and the next id to hand out. See
    /// `alloc_os_event_id`.
    ///
    /// WHY per client and not one per session: NV_ESC_RM_GET_EVENT_DATA
    /// hands back only `hObject/NotifyIndex/info32/info16` (`NvUnixEvent`,
    /// nvos.h:1927-1938) -- no hClient -- and RM handles are unique only
    /// per client. NVKMS runs two clients with their own handle
    /// allocators (`nvEvoGlobal.clientHandle`, nvkms-rm.c:1737, and
    /// `device->hRmClient`, nvkms-kapi-sync.c:152), so `hObject` alone
    /// cannot say whose event fired. One ctl fd per client makes
    /// `(nvfp, hObject)` unique -- which is RM's own bookkeeping
    /// (`allocate_os_event` hangs off the nvfp, osapi.c:596-640).
    event_ctls: std::collections::HashMap<u32, nvrm_abi::NvDevice>,
    /// Session-wide, not per client: RM checks `(hParent, fd)` for
    /// uniqueness (osapi.c:620-627), and one counter is simply never
    /// wrong.
    next_event_id: u32,
    /// `(h_client, h_event)` -> what the guest asked for. Filled after a
    /// successful substituted alloc, emptied on RM_FREE of the event or
    /// of its client.
    events: std::collections::HashMap<(u32, u32), EventReg>,
    /// Free semaphore-surface waiter slots, per RM client (the OS event id
    /// is registered under `(hClient, id)` on the host, so a slot cannot
    /// serve another client). See the semsurf block in `prepare`, marker
    /// (1a''').
    waiter_pool: std::collections::HashMap<u32, Vec<WaiterSlot>>,
    /// Armed waiters by OS-event id: RM will fire these exactly once.
    waiters: std::collections::HashMap<u32, ActiveWaiter>,
    /// Slot fds on their way out (client freed): the device tells the
    /// waiter poller first, then drops these, which closes them. Holding
    /// them here is what keeps the fd NUMBERS from being reused while the
    /// poller still knows them.
    pending_unwatch: Vec<WaiterSlot>,
    /// Event ctls on their way out, same contract as `pending_unwatch` and
    /// for the same reason: the device takes the fd out of its epoll set
    /// FIRST, and only then are these dropped (which closes them).
    ///
    /// Why they must go out at all: an event ctl is a SECOND
    /// `/dev/nvidiactl` fd for one RM client, and RM keeps a client alive
    /// for as long as any fd that created it is open. Until 2026-08-17
    /// this map was emptied in two error paths only, so a client the guest
    /// had long freed stayed alive on the host. Measured after a few hours
    /// of desktop: 2574 open `nvidiactl` fds against 7 guest processes
    /// still holding the GPU, and new buffer imports failing with
    /// GL_OUT_OF_MEMORY because RM was full -- first CS2's window stayed
    /// empty, eventually Steam's did too.
    pending_ctl_unwatch: Vec<(u32, nvrm_abi::NvDevice)>,
    /// fds this session opened (or saw) that the device shall poll, until
    /// the device collects them with [`Session::take_pollables`].
    pending_pollables: Vec<Pollable>,
    /// Counters for the summary line at PROC_GONE (`event_stats`).
    ev_registered: u64,
    ev_fired: u64,
    ev_unmatched: u64,
    ev_dataless: u64,
    /// How many failing calls this session has already reported. Only for
    /// rate-limiting the failure line in `execute`.
    fail_logs: u64,
    /// How often each `LEA_CTRL_DUMP` command was already dumped.
    ctrl_dumps: std::collections::HashMap<u32, u32>,
    /// Which door each per-client holding was let go by, and how many were
    /// ever taken. Measured 2026-08-18: 2003 open `nvidiactl` FDs
    /// against ONE live guest process, growing by 2 per GL client and flat
    /// at idle (OPEN-QUESTIONS 31). Sessions themselves are reaped
    /// correctly -- 1079 created against MAX_SESSIONS 1024 without a single
    /// refusal -- so the FDs sit in a session that is still alive, and the
    /// question is only WHICH release path never runs. These three counters
    /// answer that in one log line (`note_ctl_taken`) instead of a night of
    /// guessing.
    ctl_taken: u64,
    ctl_freed_by_rmfree: u64,
    ctl_freed_by_close: u64,
    /// `h_client` -> the guest FD token the client was ALLOCATED on.
    ///
    /// RM ties a client's lifetime to the file that created it: when that
    /// file closes, RM tears the client down and no `NV_ESC_RM_FREE` ever
    /// crosses the boundary (`on_close` says the same for the VRAM ledger,
    /// which has always released on this door). The per-client holdings
    /// -- the event ctl and the waiter slots -- did NOT, so they survived
    /// until the whole guest process exited. Only the client's own token
    /// can say which of them a close takes with it, so it is noted here at
    /// the alloc and read back in `close_clients_of_token`.
    client_token: std::collections::HashMap<u32, u64>,
    /// Next `event_ctls.len()` at which to say something. Doubles, so a
    /// healthy session is silent and a leaking one reports log(n) times.
    ctl_warn_at: usize,
    /// `/dev/nvidia0`, registered against the ctl FD of the client that
    /// needed it. Exists for the one call in this session that may NOT
    /// ride on ctl: the rewritten OS-descriptor alloc (see
    /// `osdesc_gpu_fd`). Opened at the first such call and kept, because
    /// the registration binds it to that client.
    osdesc_gpu: Option<nvrm_abi::NvDevice>,
}

/// Largest single mapping accepted through `MapPrepare`.
///
/// A plausibility bound against a lying guest, not a resource limit: the
/// host-visible window is 8 GiB (`nvrm::HOST_VISIBLE_SIZE`), and the value
/// dates from when the window was 256 MiB. Measured: the largest mapping
/// real workloads ask for is 56 MiB (NVENC), and CS2's 128 mappings are 32
/// MiB or smaller. A guest that needs one mapping above this gets EINVAL
/// from `on_map_prepare`, and raising the number is a decision to make with
/// that measurement in hand -- the window has room for it.
const MAX_MAP_LEN: u64 = 256 << 20;

/// `uvm_linux_ioctl.h` -- raw number, no _IOC encoding. Only the tests below
/// name it; it stays in the non-test build as documentation of the value, the
/// way `UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE` does.
#[allow(dead_code)]
const UVM_INITIALIZE: u32 = 0x3000_0001;
/// `uvm_types.h:67`.
const UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE: u64 = 0x2;

/// A registered mapping that has not been fetched yet.
pub struct PendingMap {
    pub token: u64,
    pub len: u64,
    /// Which node -- the cacheability the guest is told hangs off this.
    pub dev: Dev,
}

/// One substituted kernel-callback event -- the books behind (1a') in
/// `prepare`. Everything the guest needs to route the firing back to ITS
/// callback, saved before the host overwrote the params.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventReg {
    /// NVOS64.hRoot @0 of the alloc (`alloc_status_off`).
    pub h_client: u32,
    /// NVOS64.hObjectNew @8 -- known only AFTER execute; 0 until then.
    pub h_event: u32,
    /// 0x7e or 0x78, exactly as the guest asked (the host sent 0x79).
    pub class: u32,
    /// NV0005.notifyIndex @12, unstripped (flags and subdevice included).
    pub notify_index: u32,
    /// NV0005.data @16 BEFORE the id overwrote it: the guest's
    /// NVOS10_EVENT_KERNEL_CALLBACK_EX* in guest kernel VA.
    pub guest_data: u64,
    /// `req.target_token` the alloc rode on (log only for this class).
    pub token: u64,
    /// The fd:id given to NV_ESC_ALLOC_OS_EVENT on this client's event ctl.
    pub id: u32,
}

/// One semaphore-surface waiter slot: its own event ctl with ONE OS event
/// allocated on it. Its own, because RM fires waiters dataless
/// (`sem_surf.c:1591`) -- the wake is a flag on the FILE, and a file shared
/// between waiters could not say which one fired. The id is bound to the
/// `(hClient, fd)` pair at alloc time and lives as long as the client, so
/// the slot is pooled per client and reused registration after
/// registration.
#[derive(Debug)]
pub struct WaiterSlot {
    ctl: nvrm_abi::NvDevice,
    id: u32,
}

impl std::os::fd::AsRawFd for WaiterSlot {
    fn as_raw_fd(&self) -> RawFd {
        self.ctl.as_raw_fd()
    }
}

/// One armed waiter: RM holds a listener that will post to `slot` once.
#[derive(Debug)]
struct ActiveWaiter {
    slot: WaiterSlot,
    h_client: u32,
    /// The NVOS10_EVENT_KERNEL_CALLBACK_EX* the guest kernel sent -- goes
    /// back in the firing's `addr` so the guest can make the call.
    guest_kc: u64,
    /// `req.target_token` the registration rode on (log only).
    token: u64,
}

/// What `prepare` saw of a semsurf REGISTER_WAITER; `execute` arms it (RM
/// said NV_OK) or gives the slot back.
#[derive(Debug)]
pub struct PendingWaiter {
    slot: WaiterSlot,
    h_client: u32,
    guest_kc: u64,
    token: u64,
}

/// A host fd the device shall watch. The SESSION reports it (it opened
/// the fd), the DEVICE registers it -- the device owns the epoll set, and
/// nothing in a session may reach the backend's locks (nvrm.rs `on_poll`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pollable {
    /// This session's private `/dev/nvidiactl` for RM client `h_client`:
    /// readable = a substituted event fired; drained with
    /// [`Session::drain_os_events`].
    EventCtl { h_client: u32, fd: RawFd },
    /// A guest fd that ran NV_ESC_ALLOC_OS_EVENT itself (0x79 path): RM
    /// wakes THAT file (nv.c:4036-4086); the guest drains it with its own
    /// passed-through NV_ESC_RM_GET_EVENT_DATA. `owner` is the session the
    /// token belongs to when it is NOT the caller's (the alloc form names
    /// the fd in NV0005.data, resolved through aux_fd_field_proc); `None`
    /// means the calling session.
    Client { token: u64, fd: RawFd, owner: Option<u32> },
    /// An armed semaphore-surface waiter: NOT for the epoll set -- the
    /// device hands it to the waiter poller (`waiters.rs`, which says why
    /// epoll cannot be trusted with these). Readable once = the waiter
    /// fired; the device retires it with [`Session::semsurf_wake`].
    Waiter { id: u32, fd: RawFd },
}

/// One drained firing, ready to become a KIND_EVENT_FIRED Req.
#[derive(Clone, Copy, Debug)]
pub struct Fired {
    pub reg: EventReg,
    /// NvUnixEvent.info32 (0 on the substituted path, carried anyway).
    pub info32: u32,
}

// The event-path layouts come from the vendor headers through bindgen
// (nvrm-sys build.rs allowlists NvUnixEvent and NV0005_ALLOC_PARAMETERS),
// so there is exactly one description of them in this tree.
const NV0005_HCLASS: usize = std::mem::offset_of!(sys::NV0005_ALLOC_PARAMETERS, hClass);
const NV0005_NOTIFYINDEX: usize = std::mem::offset_of!(sys::NV0005_ALLOC_PARAMETERS, notifyIndex);
const NV0005_DATA: usize = std::mem::offset_of!(sys::NV0005_ALLOC_PARAMETERS, data);

/// ctrl00da.h: the two semaphore-surface waiter controls. REGISTER params
/// are 4x NvU64 {index, waitValue, newValue, notificationHandle@24},
/// UNREGISTER drops newValue (handle @16). The offsets are asserted below
/// against the vendor structs, the way the NV0005 ones are.
const CTRL_SEMSURF_REGISTER_WAITER: u32 = sys::NV_SEMAPHORE_SURFACE_CTRL_CMD_REGISTER_WAITER;
const CTRL_SEMSURF_UNREGISTER_WAITER: u32 = sys::NV_SEMAPHORE_SURFACE_CTRL_CMD_UNREGISTER_WAITER;
const _: () = {
    assert!(std::mem::offset_of!(
        sys::NV_SEMAPHORE_SURFACE_CTRL_REGISTER_WAITER_PARAMS, notificationHandle) == 24);
    assert!(std::mem::offset_of!(
        sys::NV_SEMAPHORE_SURFACE_CTRL_UNREGISTER_WAITER_PARAMS, notificationHandle) == 16);
};

const _: () = {
    assert!(std::mem::size_of::<sys::NvUnixEvent>() == 16);
    assert!(std::mem::size_of::<sys::NV0005_ALLOC_PARAMETERS>() == 24);
    // The numbers the (1a') comment has quoted since 2026-08-08.
    assert!(NV0005_HCLASS == 8 && NV0005_NOTIFYINDEX == 12 && NV0005_DATA == 16);
    assert!(std::mem::size_of::<sys::NVOS41_PARAMETERS>() == 16);
};

/// The `LEA_DEBUG` level, read ONCE per process: 0 = unset or empty (an
/// empty value is deliberately "off" -- launchers pass the variable through
/// as `LEA_DEBUG="${LEA_DEBUG:-}"`, and `is_some()` would switch a firehose
/// on), 1 = any other value (per-call diagnostics), 2 = `LEA_DEBUG=2` (every
/// forwarded call, for sequence diffs against a native trace).
///
/// Cached because some of the readers sit on per-frame paths (a semaphore-
/// surface waiter registration is one per fence): `std::env::var` takes a
/// lock and scans `environ`, and docs/TESTING.md §2 measured what that costs
/// against the 12 us of a forwarded ioctl.
pub(crate) fn debug_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var_os("LEA_DEBUG") {
        None => 0,
        Some(v) if v.is_empty() => 0,
        Some(v) if v == "2" => 2,
        Some(_) => 1,
    })
}

/// `LEA_MANAGED_COMPAT=1`: the "managed light" answers (see
/// `Action::FakeManaged`). A HOST switch, read once.
fn managed_compat() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LEA_MANAGED_COMPAT").ok().as_deref() == Some("1"))
}

/// `LEA_GPU_NAME_RAW`: leave the driver's own product string in place
/// instead of the mediated `Leandro ...` name. Read once.
///
/// Compared against `"1"` for the same reason `debug_level` treats an empty
/// value as off: launchers pass knobs through as `VAR="${VAR:-}"`, and mere
/// presence would switch the raw name on for every VM that inherits the
/// empty variable -- and `=0` would read as ON, which no one writing it
/// means. `LEA_MANAGED_COMPAT` is the sibling switch and reads the same way.
fn gpu_name_raw() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LEA_GPU_NAME_RAW").ok().as_deref() == Some("1"))
}

/// Is this control on the `LEA_CTRL_DUMP` list? Comma-separated NVOS54.cmd
/// values in hex, with or without `0x`. Parsed once -- this sits on the
/// ioctl path.
fn ctrl_dump_wanted(cmd: u32) -> bool {
    static LIST: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let list = LIST.get_or_init(|| {
        std::env::var("LEA_CTRL_DUMP")
            .unwrap_or_default()
            .split(',')
            .filter_map(|t| {
                let t = t.trim().trim_start_matches("0x");
                (!t.is_empty()).then(|| u32::from_str_radix(t, 16).ok()).flatten()
            })
            .collect()
    });
    !list.is_empty() && list.contains(&cmd)
}

/// Where `status` sits in the parameter struct of an `NV_ESC_RM_ALLOC`,
/// or `None` if this is neither of the two forms.
///
/// WARNING: there are TWO. NVOS64 (48 bytes) has status @40, NVOS21 (32
/// bytes) has it @28 -- and `xlate::embedded_ptr` accepts both, so a
/// guest can choose. Reading @40 out of a 32-byte buffer is a
/// guest-triggered panic in the daemon; treating the 32-byte form as "not
/// an alloc" would be a way around every check that hangs off the status.
/// The first three handles (hRoot, hObjectParent, hObjectNew) are at 0/4/8
/// in both.
fn alloc_status_off(inline_len: usize) -> Option<usize> {
    match inline_len {
        n if n >= 48 => Some(40),
        32 => Some(28),
        _ => None,
    }
}

/// A refusal out of [`Session::prepare`]: errno for the Rsp, prose for the
/// log line. Its own type rather than a `reply_err` in the middle of
/// deciding, so that prepare does not BUILD an answer but RETURNS a
/// decision -- tests can inspect it directly.
struct Refusal {
    errno: i32,
    why: String,
}

impl Refusal {
    fn new(errno: i32, why: String) -> Self {
        Refusal { errno, why }
    }
    fn msg(errno: i32, why: &str) -> Self {
        Refusal { errno, why: why.to_string() }
    }
}

/// What is to be DONE after the checks -- the `prepare()`/`execute()`
/// seam of OPEN-QUESTIONS nr 5.
enum Action {
    /// The real ioctl on the target FD.
    Forward,
    /// UVM_ALLOC_SEMAPHORE_POOL: fake success -- otherwise the real UVM
    /// would create the pool and collide with the external mapping the host
    /// attaches at that GPU VA.
    FakeSemaphorePool,
    /// "Managed light" (LEA_MANAGED_COMPAT=1): semantic no-op answers for
    /// the managed-only commands; the evidence sits at the execute branch.
    FakeManaged,
    /// The VM is at its VRAM cap -- `LEA_VRAM_LIMIT_MIB`, or the guest
    /// half of `LEA_VRAM_PROFILE_MIB`. Answer exactly the
    /// way the card answers when it is full -- ioctl 0, NVOS64.status =
    /// NV_ERR_NO_MEMORY -- so that libcuda produces an ordinary CUDA OOM.
    /// Measured reference at the execute branch.
    FakeVramFull,
}

/// The finished decision from [`Session::prepare`]: control data only. The
/// translated buffers stay in `self.scratch`/`self.aux` -- the plan does
/// NOT take them along, because the addresses written into them (embedded
/// pointer, nested) point at exactly those allocations, and nothing touches
/// the buffers between prepare and execute.
struct Plan {
    seq: u32,
    target_fd: i32,
    request: libc::c_ulong,
    action: Action,
    ioctl_nr: u32,
    dev_tag: u32,
    inline_len: usize,
    aux_len: usize,
    /// The arena of a 0x71 alloc, until execute pins it to the created
    /// handle (or drops it with the plan, if RM refuses).
    osdesc_arena: Option<crate::host_pool::Arena>,
    /// The FD this call rides on. Only the VRAM books need it: closing an
    /// FD frees the clients behind it, and with them their memory.
    target_token: u64,
    /// Bytes of FB already reserved on the ledger for this call, to be
    /// settled once RM has spoken. 0 = nothing reserved.
    vram_reserved: u64,
    /// Offset of `status` in this alloc's parameter struct -- 40 for
    /// NVOS64, 28 for NVOS21. Only set when `vram_reserved != 0` or the
    /// cap refused, i.e. only where it is read.
    vram_status_off: usize,
    /// Bytes 16..48 of the guest's NVOS64 block, saved when a kernel-form
    /// 0x71 alloc was rewritten into the NVOS02 shape RM accepts. Restored
    /// before the reply so the caller reads back the struct it sent, with
    /// only `status` (offset 40 in BOTH structs) coming from RM.
    osdesc_nvos64_tail: Option<Vec<u8>>,
    /// A substituted kernel-callback event (1a'), with `h_event` still 0:
    /// execute fills it from NVOS64.hObjectNew and files the registration,
    /// or gives the id back if RM refused.
    event_reg: Option<EventReg>,
    /// A substituted semaphore-surface waiter (the semsurf block in
    /// `prepare`, marker (1a''')): execute arms it on NV_OK or gives the
    /// slot back.
    waiter_reg: Option<PendingWaiter>,
    /// A translated semsurf UNREGISTER_WAITER: on NV_OK the waiter was
    /// cancelled before it fired, and execute recycles this id's slot.
    waiter_unreg: Option<u32>,
    /// This alloc came through NV_ESC_RM_VID_HEAP_CONTROL, not
    /// NV_ESC_RM_ALLOC. Same ledger, same decisions -- the parameter block
    /// is a different struct, so `execute` has to read the settle fields
    /// from different offsets and out of `scratch` rather than `aux`.
    vram_vidheap: bool,
    /// Where FB_GET_INFO's (V1) answer array lands in `aux`, and how many
    /// entries it holds: `(aux_off, fbInfoListSize)`.
    ///
    /// Recorded in prepare because that is the only place the nested
    /// descriptor is in scope; read in execute, which is the only place
    /// RM's answer exists. The V2 form needs no such note -- its array is
    /// inside the params buffer, at a fixed offset.
    fb_info_list: Option<(usize, usize)>,
}

impl Session {
    /// Session for ONE guest process. The id is fixed from the start, so
    /// the first RM client already carries it -- even if the guest sent no
    /// name with its open. It is embedded in every blob_id the session
    /// hands out, so no session can fetch another session's mapping.
    ///
    /// `vram` is the VM's ledger, shared by every session of this backend:
    /// the cap is a property of the VM, not of a guest process (the host
    /// cannot enforce anything finer -- docs/FUTURE.md).
    pub fn detached_proc(sub_id: u32, vram: std::sync::Arc<crate::vram::Ledger>) -> Result<Self> {
        let mut s = Self::build(sub_id, vram)?;
        s.next_blob_id = (sub_id as u64) << 32 | 1;
        s.sub_id = sub_id;
        Ok(s)
    }

    fn build(sub_id: u32, vram: std::sync::Arc<crate::vram::Ledger>) -> Result<Self> {
        Ok(Self {
            mirror: Mirror::new(),
            aux_fd_host: None,
            fd_field_host: None,
            scratch: Vec::with_capacity(proto::MAX_PAYLOAD),
            aux: Vec::with_capacity(proto::MAX_PAYLOAD),
            out: Reply::default(),
            pending_maps: Default::default(),
            // 0 stays free: the guest reads it as a failed registration.
            next_blob_id: 1,
            pool: PoolState::default(),
            mem: None,
            uvm_token: None,
            sub_id: 0,
            proc: proto::ProcInfo::default(),
            sys: Box::new(RealSyscalls),
            vram: crate::vram::Books::new(sub_id, vram),
            osdesc_gpu: None,
            event_ctls: Default::default(),
            // 0 stays free so an untouched field never looks like an id.
            next_event_id: 1,
            events: Default::default(),
            waiter_pool: Default::default(),
            waiters: Default::default(),
            pending_unwatch: Vec::new(),
            pending_ctl_unwatch: Vec::new(),
            pending_pollables: Vec::new(),
            ev_registered: 0,
            ev_fired: 0,
            ev_unmatched: 0,
            ev_dataless: 0,
            fail_logs: 0,
            ctrl_dumps: Default::default(),
            ctl_taken: 0,
            ctl_freed_by_rmfree: 0,
            ctl_freed_by_close: 0,
            client_token: Default::default(),
            // Far above any healthy session (a client or two), far below the
            // thousands that were measured.
            ctl_warn_at: 32,
        })
    }

    /// Turn a ring-0 callback event into an OS event this backend owns.
    ///
    /// This is the wall the whole display path stood behind, and it is
    /// not about display at all. NVKMS registers a non-stall interrupt
    /// callback while it allocates its device, and RM refuses
    /// NV01_EVENT_KERNEL_CALLBACK(_EX) to anything below
    /// RS_PRIV_LEVEL_KERNEL -- "we can not trust the function pointer"
    /// (event_api.c:74-88). Unlike the hotplug and DP-IRQ registrations
    /// further down, THAT failure is fatal: nvRmAllocDeviceEvo takes
    /// `goto failure` and the device is gone, so no display common object
    /// is ever allocated and numHeads is never even asked for. Measured
    /// 2026-08-08 as a chain of 31 calls that ends right there.
    ///
    /// RM is right to refuse, and faking the privilege would not help: the
    /// pointer is a GUEST kernel address and would mean nothing here. So
    /// the object is substituted instead. The host registers an OS event of
    /// its own -- a (hClient, file, id) triple, where the id is a number
    /// this session picks and the file is the channel RM later wakes -- and
    /// rewrites the alloc to NV01_EVENT_OS_EVENT naming that id.
    ///
    /// The guest's callback pointer stays in the guest, where it means
    /// something. Delivering the firing back to it is the second half:
    /// the ctl fd this registers on is reported to the device as a
    /// [`Pollable::EventCtl`], the device polls it, and
    /// [`Session::drain_os_events`] turns each firing into what the guest
    /// needs to call its own pointer (`EventReg`, saved by `prepare`).
    fn alloc_os_event_id(&mut self, h_client: u32) -> Result<u32, Refusal> {
        use std::collections::hash_map::Entry;
        use std::os::fd::AsRawFd;
        // One lookup, and the ctl FD comes straight out of it. The pollable is
        // announced only for a ctl this call OPENED -- announcing it for one
        // that was already in the map would have the device poll the same fd
        // twice. `fresh` carries that across the end of the match because the
        // Entry borrows `event_ctls`, and `pending_pollables` is a second
        // field of the same `self`.
        let mut fresh = false;
        let fd = match self.event_ctls.entry(h_client) {
            Entry::Occupied(slot) => slot.get().as_raw_fd(),
            Entry::Vacant(slot) => {
                let ctl = Self::open_event_ctl()
                    .map_err(|e| Refusal::new(libc::EIO, format!("event ctl: {e}")))?;
                let fd = ctl.as_raw_fd();
                slot.insert(ctl);
                fresh = true;
                fd
            }
        };
        if fresh {
            self.pending_pollables.push(Pollable::EventCtl { h_client, fd });
            self.note_ctl_taken();
        }
        let id = self.next_event_id;
        self.next_event_id += 1;
        self.alloc_os_event_on(fd, h_client, id)?;
        Ok(id)
    }

    /// The alloc itself, shared with the waiter slots: one NV_ESC_ALLOC_OS_
    /// EVENT of `(h_client, id)` on `fd`.
    fn alloc_os_event_on(&mut self, fd: RawFd, h_client: u32, id: u32) -> Result<(), Refusal> {
        let mut p = nvrm_abi::nvgpu::IoctlAllocOsEvent {
            h_client,
            // hDevice is unused by allocate_os_event; it stores hClient, the
            // file and the id, and looks at nothing else (osapi.c:597-635).
            h_device: 0,
            fd: id,
            status: 0,
        };
        let r = unsafe {
            self.sys.ioctl(
                fd,
                iowr_raw(
                    nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT,
                    std::mem::size_of::<nvrm_abi::nvgpu::IoctlAllocOsEvent>() as u32,
                ) as libc::c_ulong,
                &mut p as *mut _ as *mut u8,
                std::mem::size_of::<nvrm_abi::nvgpu::IoctlAllocOsEvent>(),
            )
        };
        if r != 0 {
            return Err(Refusal::new(libc::EIO, format!("ALLOC_OS_EVENT ret {r}")));
        }
        if p.status != sys::NV_OK {
            return Err(Refusal::new(
                libc::EIO,
                format!("ALLOC_OS_EVENT status {:#x}", p.status),
            ));
        }
        Ok(())
    }

    /// A waiter slot for `h_client`: pooled, or freshly opened and bound.
    fn waiter_slot_get(&mut self, h_client: u32) -> Result<WaiterSlot, Refusal> {
        use std::os::fd::AsRawFd;
        if let Some(s) = self.waiter_pool.get_mut(&h_client).and_then(Vec::pop) {
            return Ok(s);
        }
        let ctl = Self::open_event_ctl()
            .map_err(|e| Refusal::new(libc::EIO, format!("waiter ctl: {e}")))?;
        let id = self.next_event_id;
        self.next_event_id += 1;
        self.alloc_os_event_on(ctl.as_raw_fd(), h_client, id)?;
        self.note_ctl_taken();
        Ok(WaiterSlot { ctl, id })
    }

    /// A waiter fired (the poller reported its fd): retire it, recycle the
    /// slot, and hand back what the KIND_EVENT_FIRED needs --
    /// `(h_client, guest_kc, token)`. `None` = already retired (a stale or
    /// duplicate wake), which the device drops in silence.
    pub fn semsurf_wake(&mut self, id: u32) -> Option<(u32, u64, u64)> {
        let w = self.waiters.remove(&id)?;
        let out = (w.h_client, w.guest_kc, w.token);
        self.waiter_pool.entry(w.h_client).or_default().push(w.slot);
        self.ev_fired += 1;
        Some(out)
    }

    /// Slot fds the device shall remove from the waiter poller and then
    /// drop (dropping closes them). See `pending_unwatch`.
    pub fn take_unwatch(&mut self) -> Vec<WaiterSlot> {
        std::mem::take(&mut self.pending_unwatch)
    }

    /// Event ctls whose RM client is gone. The device unregisters each one
    /// from its epoll set and only THEN drops it -- see
    /// `pending_ctl_unwatch`; dropping first would leave a registration on
    /// an fd number the next `open` can hand out again.
    pub fn take_ctl_unwatch(&mut self) -> Vec<(u32, nvrm_abi::NvDevice)> {
        std::mem::take(&mut self.pending_ctl_unwatch)
    }

    /// The guest process this session belongs to, for the census line.
    pub fn proc_name(&self) -> String {
        self.proc.comm_str().to_string()
    }

    /// What this session holds, by source, for the device's census:
    /// `[mirror.len, mirror.ever, event_ctls, pooled waiter slots, armed
    /// waiters, pending unwatch, osdesc gpu fd]`.
    ///
    /// Written because OPEN-QUESTIONS 31 could name the NUMBER of leaked
    /// FDs but not their OWNER, and two nights went into guessing at it. The
    /// census is summed over all sessions and printed against the process's
    /// real FD count: whatever the sum does not explain is not in a session,
    /// and that single line decides where to look.
    pub fn census(&self) -> [usize; 7] {
        [
            self.mirror.len(),
            self.mirror.ever(),
            self.event_ctls.len(),
            self.waiter_pool.values().map(Vec::len).sum::<usize>(),
            self.waiters.len(),
            self.pending_unwatch.len() + self.pending_ctl_unwatch.len(),
            usize::from(self.osdesc_gpu.is_some()),
        ]
    }

    /// Everything this session holds FOR one RM client, given up at once.
    /// Returns how many ctl FDs that was, for the counters.
    ///
    /// Handed to the device rather than dropped here: the device owns the
    /// epoll registration, and a FD closed before the poller forgot it
    /// would have its NUMBER reused under the poller's feet
    /// (`pending_unwatch`, `pending_ctl_unwatch`, waiters.rs).
    ///
    /// The waiter slots die with the client for a reason of their own:
    /// their OS events are `(hClient, id)` pairs RM has just torn down, and
    /// a pooled slot reused after RM hands the same hClient value to a NEW
    /// client would register waiters against an id RM no longer knows.
    fn release_client(&mut self, h_client: u32) -> usize {
        let mut freed = 0;
        self.events.retain(|&(c, _), _| c != h_client);
        self.client_token.remove(&h_client);
        if let Some(ctl) = self.event_ctls.remove(&h_client) {
            freed += 1;
            self.pending_ctl_unwatch.push((h_client, ctl));
        }
        for s in self.waiter_pool.remove(&h_client).unwrap_or_default() {
            freed += 1;
            self.pending_unwatch.push(s);
        }
        let dead: Vec<u32> = self
            .waiters
            .iter()
            .filter(|(_, w)| w.h_client == h_client)
            .map(|(&id, _)| id)
            .collect();
        for id in dead {
            if let Some(w) = self.waiters.remove(&id) {
                freed += 1;
                self.pending_unwatch.push(w.slot);
            }
        }
        freed
    }

    /// The guest closed one of its FDs. RM tears down every client that was
    /// created on that file and no `NV_ESC_RM_FREE` crosses the boundary for
    /// them, so this session's per-client holdings have to be given up here
    /// or they stay until the whole guest process exits.
    ///
    /// That is exactly what OPEN-QUESTIONS 31 measured: 2003 open
    /// `nvidiactl` FDs against ONE live guest process, +2 per GL client,
    /// flat at idle, and sessions themselves reaped correctly. The VRAM
    /// ledger has always released on this door (`close_token`); the event
    /// ctls and waiter slots did not.
    fn close_clients_of_token(&mut self, token: u64) {
        let dying: Vec<u32> = self
            .client_token
            .iter()
            .filter(|(_, &t)| t == token)
            .map(|(&c, _)| c)
            .collect();
        for c in dying {
            self.ctl_freed_by_close += self.release_client(c) as u64;
        }
    }

    /// One more `/dev/nvidiactl` was taken for a per-client holding. Says so
    /// once per doubling, with the two release counters beside it, so a log
    /// from a long desktop run answers "which door never runs" by itself.
    fn note_ctl_taken(&mut self) {
        self.ctl_taken += 1;
        let held = self.event_ctls.len() + self.waiter_pool.values().map(Vec::len).sum::<usize>();
        if held >= self.ctl_warn_at {
            self.ctl_warn_at = held.saturating_mul(2);
            eprintln!(
                "vhost-user-nvrm: session {} ({}) holds {held} ctl FDs for its clients \
                 ({} event ctls, {} pooled waiter slots, {} armed) -- \
                 taken {}, freed {} by RM_FREE, {} by FD close",
                self.sub_id,
                self.proc.comm_str(),
                self.event_ctls.len(),
                self.waiter_pool.values().map(Vec::len).sum::<usize>(),
                self.waiters.len(),
                self.ctl_taken,
                self.ctl_freed_by_rmfree,
                self.ctl_freed_by_close,
            );
        }
    }

    /// The node an event ctl is opened on. In production `/dev/nvidiactl`;
    /// under test `/dev/null` -- the ledger (`NvSyscalls`) never lets an
    /// ioctl reach it, and the fd only has to exist so that the bookkeeping
    /// around it (`Pollable`, `EventReg`, `drain_os_events`) can be
    /// exercised without a GPU.
    fn open_event_ctl() -> nvrm_abi::Result<nvrm_abi::NvDevice> {
        #[cfg(test)]
        {
            nvrm_abi::NvDevice::open("/dev/null")
        }
        #[cfg(not(test))]
        {
            nvrm_abi::NvDevice::open_ctl()
        }
    }

    /// Give an OS-event id back to RM: NV_ESC_FREE_OS_EVENT on the ctl of
    /// `h_client` (nvgpu.rs `IoctlFreeOsEvent`, same 16-byte layout as the
    /// alloc). Best effort -- an id that stays registered costs RM one list
    /// entry until the ctl closes with the session; a failure here is
    /// logged, never surfaced to the guest.
    fn free_os_event(&mut self, h_client: u32, id: u32) {
        use std::os::fd::AsRawFd;
        let Some(ctl) = self.event_ctls.get(&h_client) else { return };
        let fd = ctl.as_raw_fd();
        let mut p = nvrm_abi::nvgpu::IoctlFreeOsEvent { h_client, h_device: 0, fd: id, status: 0 };
        let r = unsafe {
            self.sys.ioctl(
                fd,
                iowr_raw(
                    nvrm_abi::nvgpu::NV_ESC_FREE_OS_EVENT,
                    std::mem::size_of::<nvrm_abi::nvgpu::IoctlFreeOsEvent>() as u32,
                ) as libc::c_ulong,
                &mut p as *mut _ as *mut u8,
                std::mem::size_of::<nvrm_abi::nvgpu::IoctlFreeOsEvent>(),
            )
        };
        if r != 0 || p.status != sys::NV_OK {
            eprintln!(
                "vhost-user-nvrm: FREE_OS_EVENT id {id} client {h_client:#x}: ret {r} \
                 status {:#x} -- id stays registered until the session ends",
                p.status
            );
        }
    }

    /// The fds the device has not registered yet. Emptied by the call.
    pub fn take_pollables(&mut self) -> Vec<Pollable> {
        std::mem::take(&mut self.pending_pollables)
    }

    /// `(registered, fired, unmatched, dataless)` -- for the summary line
    /// the device prints at PROC_GONE.
    pub fn event_stats(&self) -> (u64, u64, u64, u64) {
        (self.ev_registered, self.ev_fired, self.ev_unmatched, self.ev_dataless)
    }

    /// Does this client still own an event ctl? False after a drain gave
    /// up on it -- the device then forgets the poll registration, so that
    /// a later alloc's fresh ctl is not deduplicated away.
    pub fn has_event_ctl(&self, h_client: u32) -> bool {
        self.event_ctls.contains_key(&h_client)
    }

    /// Drain every firing RM has queued on the event ctl of `h_client`.
    ///
    /// Called by the device when that ctl reports readable. RM's queue is
    /// per file (`nvfp->event_data_head`, nv.c:2318-2321) and the fd is
    /// polled LEVEL-triggered: whatever is left in the queue makes the fd
    /// readable again on the next `epoll_wait`, so this loops until RM says
    /// `MoreEvents == 0`. Not draining completely would be a busy loop
    /// in the worker thread that also serves queue 0.
    ///
    /// Each entry is one NV_ESC_RM_GET_EVENT_DATA (osapi.c:2955-2967 ->
    /// :504-535): RM copies ONE `NvUnixEvent` to `pEvent` -- an address in
    /// THIS process, which is why the escape is safe here and must be
    /// table-ruled when the guest sends it (`EMB_LEN_FIXED` in the
    /// descriptor table). Every entry names `hObject` = the hEvent RM
    /// created for the substituted alloc, looked up in `events` under this
    /// client.
    ///
    /// Two answers are not events: `NV_ERR_OPERATING_SYSTEM` (osapi.c:519-
    /// 524) means the queue is empty -- the fd was readable because of a
    /// DATALESS post, which `nvidia_poll` clears itself on the poll
    /// (nv.c:2320) and which cannot be attributed to a registration; it is
    /// counted and nothing more. A failed ioctl (`ret != 0`) is a broken
    /// ctl: it is closed so that the level-triggered fd cannot spin the
    /// worker, and RM tears its events down with the file.
    pub fn drain_os_events(&mut self, h_client: u32) -> Vec<Fired> {
        use std::os::fd::AsRawFd;
        let mut out = Vec::new();
        let Some(ctl) = self.event_ctls.get(&h_client) else { return out };
        let fd = ctl.as_raw_fd();
        // Bounded: RM's queue is finite, but a ctl that always says
        // MoreEvents keeps queue 0 waiting; anything left re-triggers.
        for _ in 0..256 {
            let mut ev = sys::NvUnixEvent::default();
            let mut p = sys::NVOS41_PARAMETERS {
                pEvent: &mut ev as *mut sys::NvUnixEvent as *mut libc::c_void,
                MoreEvents: 0,
                status: 0,
            };
            let r = unsafe {
                self.sys.ioctl(
                    fd,
                    iowr_raw(
                        sys::NV_ESC_RM_GET_EVENT_DATA,
                        std::mem::size_of::<sys::NVOS41_PARAMETERS>() as u32,
                    ) as libc::c_ulong,
                    &mut p as *mut _ as *mut u8,
                    std::mem::size_of::<sys::NVOS41_PARAMETERS>(),
                )
            };
            if r != 0 {
                eprintln!(
                    "vhost-user-nvrm: GET_EVENT_DATA on event ctl of client {h_client:#x} \
                     returned {r} -- closing that ctl, its events are lost"
                );
                self.event_ctls.remove(&h_client);
                self.events.retain(|&(c, _), _| c != h_client);
                break;
            }
            if p.status != sys::NV_OK {
                if p.status == sys::NV_ERR_OPERATING_SYSTEM {
                    // The dataless answer: RM has nothing queued -- the fd
                    // is drained, and a `break` here leaves it quiet.
                    self.ev_dataless += 1;
                    break;
                }
                // Any OTHER status leaves RM's queue where it is, and the
                // fd level-readable -- a `break` here would re-fire the
                // outer epoll forever, with the worker that also serves
                // queue 0 spinning on it. Same answer as an ioctl failure:
                // this ctl is done, and the log says so.
                eprintln!(
                    "vhost-user-nvrm: GET_EVENT_DATA client {h_client:#x}: status {:#x} -- \
                     closing that ctl, its events are lost",
                    p.status
                );
                self.event_ctls.remove(&h_client);
                self.events.retain(|&(c, _), _| c != h_client);
                break;
            }
            self.ev_fired += 1;
            match self.events.get(&(h_client, ev.hObject)) {
                Some(reg) => {
                    // RM strips notifyIndex to `15:0` before it posts
                    // (event_notification.c:849); the registration keeps
                    // the unstripped word. A mismatch is a bookkeeping
                    // error worth a line, not a reason to drop the firing.
                    if reg.notify_index & 0xffff != ev.NotifyIndex && debug_level() >= 1 {
                        eprintln!(
                            "vhost-user-nvrm: event {:#x}/{:#x}: notifyIndex {:#x} registered, \
                             {:#x} fired",
                            h_client, ev.hObject, reg.notify_index, ev.NotifyIndex
                        );
                    }
                    out.push(Fired { reg: *reg, info32: ev.info32 });
                }
                None => self.ev_unmatched += 1,
            }
            if p.MoreEvents == 0 {
                break;
            }
        }
        out
    }

    /// A `/dev/nvidia0` FD registered against `ctl_fd`, opened at first
    /// use and kept for the session.
    ///
    /// Needed because the two escapes involved in an OS-descriptor alloc
    /// disagree about their node: an NV04_ALLOC of hClass 0x71 is
    /// NV_CTL_DEVICE_ONLY (escape.c:486) and NV_ESC_RM_ALLOC_MEMORY, the
    /// shape RM accepts from a userspace process, is NV_ACTUAL_DEVICE_ONLY
    /// (escape.c:399). So the rewritten call cannot ride on the FD it
    /// arrived on, no matter how correct its payload is.
    ///
    /// REGISTER_FD is what makes the client behind `ctl_fd` visible on this
    /// node; without it RM answers 0x23 INVALID_CLIENT (the same pairing
    /// `host_pool::Backing` needs for its own 0x27).
    ///
    /// Index 0 because that is the one card this backend serves -- the same
    /// assumption `Backing::new` makes. A second GPU would need the index
    /// the guest named at open time, which this request does not carry.
    fn osdesc_gpu_fd(&mut self, ctl_fd: i32) -> Result<i32, Refusal> {
        use std::os::fd::AsRawFd;
        if self.osdesc_gpu.is_none() {
            let gpu = nvrm_abi::NvDevice::open_gpu(0)
                .map_err(|e| Refusal::new(libc::EIO, format!("0x71 osdesc: open gpu: {e}")))?;
            let mut p = nvrm_abi::nvgpu::IoctlRegisterFd { ctl_fd };
            // REGISTER_FD has no status field: the driver answers as errno.
            let r = unsafe {
                self.sys.ioctl(
                    gpu.as_raw_fd(),
                    iowr_raw(
                        nvrm_abi::nvgpu::NV_ESC_REGISTER_FD,
                        std::mem::size_of::<nvrm_abi::nvgpu::IoctlRegisterFd>() as u32,
                    ) as libc::c_ulong,
                    &mut p as *mut _ as *mut u8,
                    std::mem::size_of::<nvrm_abi::nvgpu::IoctlRegisterFd>(),
                )
            };
            if r != 0 {
                return Err(Refusal::new(
                    libc::EIO,
                    format!("0x71 osdesc: REGISTER_FD returned {r}"),
                ));
            }
            self.osdesc_gpu = Some(gpu);
        }
        Ok(self.osdesc_gpu.as_ref().unwrap().as_raw_fd())
    }

    /// The name under which RM lists this guest process: `comm[guest-pid]`.
    /// Both parts are the guest's own statements, both true -- the name
    /// alone would be ambiguous (two `python3`), the PID alone says nothing.
    fn sub_name(&self) -> String {
        if self.proc.pid == 0 && self.proc.comm[0] == 0 {
            return format!("guest-{}", self.sub_id);
        }
        format!("{}[{}]", self.proc.comm_str(), self.proc.pid)
    }

    /// Swap out the three syscalls (OPEN-QUESTIONS nr 5).
    ///
    /// For fuzz targets and integration tests: the production path sets
    /// `RealSyscalls` in the constructor and never calls this.
    pub fn set_syscalls(&mut self, sys: Box<dyn NvSyscalls>) {
        self.sys = sys;
    }

    /// Register an already-open FD under a fresh token.
    ///
    /// Likewise only for fuzz targets and tests: in production, tokens
    /// arise exclusively in `on_open`, where a real device node is opened.
    /// Without this route every fuzz message would end at EBADF and the
    /// translation paths behind it would stay untested.
    pub fn insert_token(&mut self, fd: std::os::fd::OwnedFd) -> u64 {
        self.mirror.insert(fd)
    }

    /// The device hands in the current guest RAM before every exec. Cheap
    /// (an Arc clone), and required as soon as a UvmPoolBack arrives.
    pub fn set_guest_mem(&mut self, mem: Option<Mem>) {
        self.mem = mem;
    }

    /// Host fd behind a token IN THIS SESSION's mirror. The device uses it
    /// to resolve a token that belongs to a DIFFERENT session -- see
    /// `aux_fd_field_proc` in nvrm-wire.
    pub fn mirror_raw(&self, token: u64) -> Option<RawFd> {
        self.mirror.raw(token)
    }

    /// Process one message without any device-resolved fds -- the entry
    /// point of the fuzz target and the tests. `None` for both fds means
    /// "nobody resolved a cross-session fd for this message", and it is
    /// deliberately NOT "look in my own mirror": that fallback is exactly
    /// the wrong-session lookup `aux_fd_field_proc` was built to end.
    pub fn handle_msg(&mut self, bytes: &[u8]) -> Result<Reply> {
        self.handle_msg_with(bytes, None, None)
    }

    /// Process one message and return the answer. The carrier decides how
    /// it reaches the guest. An error HERE is a transport error; guest
    /// errors come back as an Rsp with `ret < 0`. `aux_fd_host` and
    /// `fd_field_host` are the host fds the DEVICE resolved for this message
    /// from the sessions named in `aux_fd_field_proc` / `fd_field_proc`
    /// (a token means nothing without the session that minted it).
    pub fn handle_msg_with(
        &mut self,
        bytes: &[u8],
        aux_fd_host: Option<RawFd>,
        fd_field_host: Option<RawFd>,
    ) -> Result<Reply> {
        self.aux_fd_host = aux_fd_host;
        self.fd_field_host = fd_field_host;
        capture(bytes);
        let req = Req::from_bytes(bytes)
            .ok_or_else(|| anyhow::anyhow!("message shorter than the Req header"))?;
        let payload = &bytes[Req::WIRE_LEN..];
        // The payload must come out of the caller's buffer before the
        // handlers reach for self.scratch -- otherwise on_ioctl borrows twice.
        match Kind::from_u32(req.kind) {
            Some(Kind::Hello) => self.on_hello(&req)?,
            Some(Kind::Open) => self.on_open(&req, payload)?,
            Some(Kind::Close) => self.on_close(&req)?,
            Some(Kind::Ioctl) => self.on_ioctl(&req, payload)?,
            Some(Kind::MapPrepare) => self.on_map_prepare(&req)?,
            Some(Kind::UvmPoolBack) => self.on_uvm_pool_back(&req, payload)?,
            None => self.reply_err(req.seq, libc::EPROTO, "unknown Kind")?,
        }
        Ok(std::mem::take(&mut self.out))
    }

    fn on_hello(&mut self, req: &Req) -> Result<()> {
        if req.ioctl_nr != proto::PROTO_VERSION {
            return self.reply_err(req.seq, libc::EPROTO, "protocol version");
        }
        self.reply(Rsp { seq: req.seq, ..Rsp::default() }, &[], &[])
    }

    fn on_open(&mut self, req: &Req, payload: &[u8]) -> Result<()> {
        use nvrm_abi::NvDevice;

        // Who is opening travels along with the first Open. It is optional:
        // a caller that sends nothing keeps the default. Adopt it only the
        // FIRST time: a later Open by the same process carries the same
        // data, and a differing name would be a contradiction -- not
        // something to adopt silently.
        if req.inline_len as usize >= proto::ProcInfo::WIRE_LEN
            && payload.len() >= proto::ProcInfo::WIRE_LEN
            && self.proc.pid == 0
            && self.proc.comm[0] == 0
        {
            if let Some(info) = proto::ProcInfo::from_bytes(payload) {
                self.proc = info;
                // From here on this process can appear in the VM's own
                // process list: before the identity arrives there is no
                // guest PID `nvidia-smi` could resolve.
                self.vram.announce(self.proc.pid);
                if self.sub_id != 0 {
                    eprintln!(
                        "vhost-user-nvrm: guest process {} = {} (guest PID {})",
                        self.sub_id,
                        self.proc.comm_str(),
                        self.proc.pid
                    );
                }
            }
        }

        // WARNING: for `Gpu`, `ioctl_nr` is the GPU index and therefore a
        // guest word: without a bound the host builds
        // `/dev/nvidia<arbitrary u32>` from it and tries to open that -- a
        // guest could thus make the host perform one filesystem access per
        // message under a name of its own choosing. Found by the fuzzer,
        // which produced `/dev/nvidia1048577`. NV_MAX_DEVICES == 32
        // (nvlimits.h:37) is the number the driver itself knows.
        const NV_MAX_DEVICES: u32 = 32;
        if req.dev_tag == DevTag::Gpu as u32 && req.ioctl_nr >= NV_MAX_DEVICES {
            return self.reply_err(req.seq, libc::EINVAL, "GPU index beyond NV_MAX_DEVICES");
        }

        let dev = match DevTag::from_u32(req.dev_tag) {
            Some(DevTag::Ctl) => NvDevice::open_ctl(),
            Some(DevTag::Gpu) => NvDevice::open_gpu(req.ioctl_nr),
            Some(DevTag::Uvm) => NvDevice::open("/dev/nvidia-uvm"),
            Some(DevTag::UvmTools) => NvDevice::open("/dev/nvidia-uvm-tools"),
            None => return self.reply_err(req.seq, libc::EINVAL, "dev_tag"),
        };
        let dev = match dev {
            Ok(d) => d,
            Err(e) => return self.reply_err(req.seq, libc::EACCES, &format!("open: {e}")),
        };

        // Ownership of the real FD passes to the mirror; as long as the
        // token lives, the FD lives.
        let owned = dev.into_owned_fd();
        let token = self.mirror.insert(owned);

        // Remember the uvm FD -- UvmPoolBack needs it (the guest's GPU and
        // VASpace are registered on it). Exactly one per session expected.
        if matches!(DevTag::from_u32(req.dev_tag), Some(DevTag::Uvm)) {
            self.uvm_token = Some(token);
        }

        // Nothing to mirror back: the guest creates a placeholder FD of its
        // own and routes via the token.
        self.reply(Rsp { seq: req.seq, token, ..Rsp::default() }, &[], &[])
    }

    /// Register a mapping that the guest is about to fetch through the
    /// host-visible window.
    ///
    /// This only keeps books. The actual `mmap` is done later by
    /// cloud-hypervisor, on the device FD the host passes along via
    /// SHMEM_MAP -- which it may, because RM hangs the mmap context off the
    /// **OFD** and not off the process (`nv-mmap.c:770`, `:521-548`). The
    /// binding that does exist (a foreign client yields `0x23
    /// INVALID_CLIENT`) bites at ioctl `0x4e`, and the guest just forwarded
    /// that one here.
    fn on_map_prepare(&mut self, req: &Req) -> Result<()> {
        if self.mirror.raw(req.target_token).is_none() {
            return self.reply_err(req.seq, libc::EBADF, "MapPrepare on an unknown token");
        }
        if req.map_len == 0 || req.map_len > MAX_MAP_LEN {
            return self.reply_err(req.seq, libc::EINVAL, "MapPrepare: implausible length");
        }
        // The node decides the cacheability the guest is told; an unknown
        // tag has none.
        let Some(dev) = dev_of(req.dev_tag) else {
            return self.reply_err(req.seq, libc::EINVAL, "MapPrepare: dev_tag");
        };
        let id = self.next_blob_id;
        // Only the low 32 bits count up; the high ones hold the session's
        // own id (`sub_id`), so ids from different sessions cannot collide.
        //
        // The low half skips 0 when it wraps. For every session but one the
        // wrap only repeats an id; for `sub_id == 0` the whole blob_id would
        // become 0, and `build()` keeps 0 free because the guest reads it as
        // a failed registration -- it would ignore a mapping the host had
        // registered.
        let low = match (id + 1) & 0xffff_ffff {
            0 => 1,
            n => n,
        };
        self.next_blob_id = (self.next_blob_id & !0xffff_ffff) | low;
        self.pending_maps.insert(
            id,
            PendingMap { token: req.target_token, len: req.map_len, dev },
        );
        self.reply(Rsp { seq: req.seq, token: id, ..Rsp::default() }, &[], &[])
    }

    /// Back the semaphore pool with guest pages.
    ///
    /// `payload` carries EXCLUSIVELY the GPA runs (16 bytes each) that the
    /// guest derived from /proc/self/pagemap for the pool at `req.addr`.
    /// `req.map_len` is the pool length, `req.target_token` the uvm FD.
    fn on_uvm_pool_back(&mut self, req: &Req, payload: &[u8]) -> Result<()> {
        let Some(mem) = self.mem.clone() else {
            return self.reply_err(req.seq, libc::EIO, "UvmPoolBack without guest RAM");
        };
        let uvm_fd = match self.mirror.raw(req.target_token) {
            Some(f) => f,
            None => return self.reply_err(req.seq, libc::EBADF, "UvmPoolBack: unknown uvm token"),
        };
        let Some(runs) = GpaRun::decode(payload, req.gpa_run_count as usize) else {
            return self.reply_err(req.seq, libc::EINVAL, "UvmPoolBack: runs unreadable");
        };
        // addr and map_len are guest words -- from here on they carry the
        // type whose arithmetic exists only checked (guest_words.rs).
        match self.pool.back_pool(
            &mem,
            uvm_fd,
            GuestAddr::new(req.addr),
            GuestLen::new(req.map_len),
            &runs,
        ) {
            Ok(()) => {
                eprintln!(
                    "vhost-user-nvrm: pool @{:#x} ({} KiB, {} runs) attached to GPU VA",
                    req.addr, req.map_len >> 10, runs.len()
                );
                self.reply(Rsp { seq: req.seq, ..Rsp::default() }, &[], &[])
            }
            Err(e) => {
                eprintln!("vhost-user-nvrm: UvmPoolBack @{:#x}: {e:#}", req.addr);
                self.reply_err(req.seq, libc::EIO, "UvmPoolBack failed")
            }
        }
    }

    /// Fetch the mapping registered under a blob_id: a private copy of the
    /// device FD (which may go to the VMM), the length, and the node.
    pub fn take_pending_map(&mut self, blob_id: u64) -> Option<(std::fs::File, u64, Dev)> {
        let PendingMap { token, len, dev } = self.pending_maps.remove(&blob_id)?;
        let raw = self.mirror.raw(token)?;
        // SAFETY: raw is a valid FD held by the mirror; try_clone duplicates
        // it, and the mirror keeps its own.
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
        borrowed.try_clone_to_owned().ok().map(|o| (o.into(), len, dev))
    }

    fn on_close(&mut self, req: &Req) -> Result<()> {
        self.mirror.remove(req.target_token);
        // Dropping the last reference closes the host FD, and RM tears
        // down every client that was created on it -- no RM_FREE crosses
        // the boundary for those. Whatever they held is free again.
        self.vram.close_token(req.target_token);
        // ... and so does everything this session holds FOR those clients.
        self.close_clients_of_token(req.target_token);
        self.reply(Rsp { seq: req.seq, ..Rsp::default() }, &[], &[])
    }

    fn on_ioctl(&mut self, req: &Req, payload: &[u8]) -> Result<()> {
        match self.prepare(req, payload) {
            Ok(plan) => self.execute(plan),
            Err(Refusal { errno, why }) => self.reply_err(req.seq, errno, &why),
        }
    }

    /// DECIDING (the `prepare()` half of OPEN-QUESTIONS nr 5): every check against a
    /// possibly lying guest, plus the finished assembly of the buffers (fd
    /// translation, embedded pointer, nested addresses, UVM init flag, 0x71
    /// arena).
    ///
    /// The step markers -- (1), (1a'), (2b) and so on, here and in
    /// `execute` -- name CONCERNS, not file order: a number is one
    /// translation concern, letters are its sub-cases, and primes mark the
    /// same concern's other half in the other function, so (1a') in
    /// prepare pairs with (1a'') in execute. Do not renumber them; they
    /// are cited from other files. The buffers themselves stay session state
    /// (`self.scratch`/`self.aux`) -- the [`Plan`] carries control data
    /// only, and the addresses written in stay valid because nothing
    /// touches the buffers between prepare and execute.
    ///
    /// GPU-free: everything here goes through the [`NvSyscalls`] seam, so
    /// the tests run it against a ledger. Two things DO issue an ioctl from
    /// inside prepare, both through that seam: the substituted event of
    /// (1a') and the semsurf waiter slot (an `NV_ESC_ALLOC_OS_EVENT` on a
    /// ctl this backend owns). The forwarded call itself never runs here.
    /// (The 0x71 path builds an arena, i.e. an mmap onto guest RAM -- a
    /// memfd in tests, no GPU.)
    fn prepare(&mut self, req: &Req, payload: &[u8]) -> std::result::Result<Plan, Refusal> {
        let mut inline_len = req.inline_len as usize;
        let mut aux_len = req.aux_len as usize;

        if payload.len() < inline_len + aux_len
            || inline_len > proto::MAX_PAYLOAD
            || aux_len > proto::MAX_AUX
        {
            return Err(Refusal::new(libc::EINVAL, format!(
                "Length: inline {inline_len} aux {aux_len} payload {}", payload.len())));
        }

        // The device tag decides how the request number is read (frontend
        // `_IOC` encoding vs. UVM raw number) and how the call is logged. A
        // tag this backend does not know is a message it cannot interpret --
        // refused, not read as the ctl node (which it silently was until
        // 2026-08-18). No guest module sends one; the fuzzer does.
        let Some(dev) = dev_of(req.dev_tag) else {
            return Err(Refusal::new(libc::EINVAL, format!("dev_tag {} unknown", req.dev_tag)));
        };

        // Denied controls: do not execute them at all.
        //
        // `NV0000_CTRL_CMD_SET_SUB_PROCESS_ID` writes exactly the field the
        // host assigns itself moments later, and does so without any check
        // (client_resource.c:4856-4872, NON_PRIVILEGED). A guest process
        // could otherwise pose as a different one. Which commands are
        // denied, and why, is documented at xlate::blocked_ctrls().
        //
        // Here in the host is where the denial takes effect -- the guest
        // module gets the list through the table only to save work. The
        // check stands BEFORE the token lookup, so it also fires when the
        // guest sends a nonsense token.
        if req.ioctl_nr == sys::NV_ESC_RM_CONTROL && inline_len >= 12 {
            let cmd = u32::from_le_bytes(payload[8..12].try_into().unwrap());
            if nvrm_abi::xlate::ctrl_blocked(cmd) {
                return Err(Refusal::new(
                    libc::EPERM,
                    format!("control {cmd:#x} is never forwarded"),
                ));
            }
        }

        // Routing FD: the host FD behind this call's token.
        let target_fd = match self.mirror.raw(req.target_token) {
            Some(f) => f,
            None => return Err(Refusal::msg(libc::EBADF, "unknown token")),
        };

        self.scratch.clear();
        self.scratch.extend_from_slice(&payload[..inline_len]);
        self.aux.clear();
        self.aux.extend_from_slice(&payload[inline_len..inline_len + aux_len]);

        // (1) Translate the fd field: guest token -> host FD number.
        if req.fd_field_off != NONE_U32 {
            let off = req.fd_field_off as usize;
            if off + 4 > inline_len {
                return Err(Refusal::msg(libc::EINVAL, "fd_field_off"));
            }
            let host_num: i32 = if req.fd_field_token == NONE_U64 {
                // fd == -1 in the guest (OS_DESCRIPTOR): leave unchanged.
                i32::from_le_bytes(self.scratch[off..off + 4].try_into().unwrap())
            } else if let Some(f) = self.fd_field_host {
                // Resolved by the device against `fd_field_proc`: the fd
                // belongs to ANOTHER guest process. Measured 2026-08-17: an
                // EGLImage import names an fd the X server exported, and
                // looking it up here -- in the importer's mirror -- answered
                // EBADF 139936 times in one session, which NVIDIA's GL stack
                // reports as GL_OUT_OF_MEMORY and which cost Steam and CS2
                // their windows.
                f
            } else {
                // Not stated: the caller's own mirror, which is right for
                // every path that does not import across processes.
                match self.mirror.raw(req.fd_field_token) {
                    Some(f) => f,
                    None => return Err(Refusal::msg(libc::EBADF, "fd_field_token")),
                }
            };
            self.scratch[off..off + 4].copy_from_slice(&host_num.to_le_bytes());
        }

        // (2) Embedded pointer: aux lies in the host buffer, and its address
        //     goes into the inline struct. self.aux lives as long as the
        //     ioctl runs.
        //
        //     IMPORTANT: `aux_len` is chosen by the guest, but the length
        //     the driver dereferences this pointer with sits in the inline
        //     struct (NVOS54.paramsSize, or the hClass table for RM_ALLOC).
        //     Believing `aux_len` alone gets the daemon heap overrun:
        //     paramsSize = 1 MiB with aux_len = 0 is enough. The host
        //     therefore recomputes the expected length itself and compares.

        // (0x79 return channel) A guest client that registers an OS event
        // of its own names the fd this call rides on as its wake channel:
        // RM stores the nvfp of THIS file (osapi.c:596-611, 2929-2939) and
        // wakes exactly its waitqueue (nv.c:4045, 4085). vkcube trace:
        // `ioctl gpu 0xce ... 23/31`, later `poll gpu 31 3`. So the device
        // watches this host fd and, when it turns readable, tells the guest
        // to make ITS fd (the token) readable. Reported before the checks
        // that follow -- a refused ALLOC_OS_EVENT leaves the fd registered
        // for nothing, and the device dedupes.
        if !dev.is_uvm() && req.ioctl_nr == nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT {
            self.pending_pollables.push(Pollable::Client {
                token: req.target_token,
                fd: target_fd,
                owner: None,
            });
        }

        // (2b') 0x71 alloc with GPA runs in aux. The guest sent the pages
        // behind NVOS02.pMemory as runs (not as a params buffer); the host
        // assembles them into a host VA and writes that in place of the
        // guest VA. RM pins them, and the arena lives until the RM_FREE.
        let mut osdesc_arena = None;
        let mut osdesc_nvos64_tail: Option<Vec<u8>> = None;
        let mut rewrite_ioctl_nr: Option<u32> = None;
        let mut rewrite_target_fd: Option<i32> = None;
        if req.gpa_run_count > 0 {
            // TWO shapes, because the guest has two callers.
            //
            // A process allocates OS-described memory with
            // NV_ESC_RM_ALLOC_MEMORY (NVOS02): the address and the limit are
            // in the INLINE block and aux carries runs and nothing else.
            //
            // NVKMS uses NV04_ALLOC (NVOS64) with hClass 0x71 and puts
            // NV_OS_DESC_MEMORY_ALLOCATION_PARAMS in the PARAMS buffer -- so
            // that call keeps its params and aux is params ++ runs. This is
            // the PRIME import: the guest's display device allocated the
            // buffer, NVIDIA imports it, and the pages are guest RAM this
            // process already has mapped.
            //
            // No protocol bump: NV_ESC_RM_ALLOC with runs used to be refused
            // outright by exactly the branch below, so an older backend
            // answers a newer guest with a clean error rather than a
            // misreading.
            let kern_form = req.ioctl_nr == sys::NV_ESC_RM_ALLOC;
            if !kern_form && (req.ioctl_nr != sys::NV_ESC_RM_ALLOC_MEMORY || inline_len < 44) {
                return Err(Refusal::msg(libc::EINVAL, "gpa_run_count only for 0x71"));
            }
            let Some(mem) = self.mem.clone() else {
                return Err(Refusal::msg(libc::EIO, "0x71 runs without guest RAM"));
            };
            // Where the runs start, and where the limit is read from.
            const OSDESC_PARAMS: usize =
                std::mem::size_of::<sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS>();
            const OSDESC_DESC: usize =
                std::mem::offset_of!(sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS, descriptor);
            const OSDESC_LIMIT: usize =
                std::mem::offset_of!(sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS, limit);
            if kern_form && self.aux.len() < OSDESC_PARAMS {
                return Err(Refusal::msg(libc::EINVAL, "0x71 kernel form: params too short"));
            }
            let run_bytes = if kern_form { &self.aux[OSDESC_PARAMS..] } else { &self.aux[..] };
            let Some(runs) = GpaRun::decode(run_bytes, req.gpa_run_count as usize) else {
                return Err(Refusal::msg(libc::EINVAL, "0x71: runs unreadable"));
            };
            // NVOS02: pMemory @24, limit @32 (nvos.h:285-295). `limit` is
            // the LAST byte address, so the length is limit+1 -- and `limit`
            // is a guest word: at u64::MAX the sum wraps to 0. Arena::build
            // does catch the zero, but this computation must not lean on the
            // next barrier.
            let limit = if kern_form {
                u64::from_le_bytes(
                    self.aux[OSDESC_LIMIT..OSDESC_LIMIT + 8].try_into().unwrap(),
                )
            } else {
                u64::from_le_bytes(self.scratch[32..40].try_into().unwrap())
            };
            let Some(total) = limit.checked_add(1) else {
                return Err(Refusal::msg(libc::EINVAL, "0x71: limit == u64::MAX"));
            };
            match self.pool.arena_for_osdesc(&mem, &runs, GuestLen::new(total)) {
                Ok((arena, va)) => {
                    if kern_form {
                        self.aux[OSDESC_DESC..OSDESC_DESC + 8]
                            .copy_from_slice(&va.to_le_bytes());
                        // Whoever writes the address writes what KIND it is.
                        // The guest asked with OS_DMA_BUF_PTR because that is
                        // what it had; what RM gets here is a plain virtual
                        // address in this process, and saying otherwise makes
                        // RM read a dma_buf pointer out of a VA. Measured as
                        // status 0x56 (NV_ERR_NOT_SUPPORTED) on an alloc whose
                        // pages had already arrived correctly.
                        const OSDESC_TYPE: usize = std::mem::offset_of!(
                            sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS,
                            descriptorType
                        );
                        self.aux[OSDESC_TYPE..OSDESC_TYPE + 4].copy_from_slice(
                            &sys::NVOS32_DESCRIPTOR_TYPE_VIRTUAL_ADDRESS.to_le_bytes(),
                        );
                        // And the surface type with it. The path that is
                        // known to work -- a process allocating OS-described
                        // memory through NV_ESC_RM_ALLOC_MEMORY -- is
                        // converted by RM's own deprecated layer, which sets
                        // NVOS32_TYPE_IMAGE beside the virtual address
                        // (rmapi_deprecated_allocmemory.c:246). NVKMS asks
                        // with TYPE_PRIMARY because it is describing a
                        // scanout buffer; once the descriptor has become a
                        // plain host address, IMAGE is what matches it.
                        const OSDESC_STYPE: usize = std::mem::offset_of!(
                            sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS,
                            type_
                        );
                        self.aux[OSDESC_STYPE..OSDESC_STYPE + 4]
                            .copy_from_slice(&sys::NVOS32_TYPE_IMAGE.to_le_bytes());
                    } else {
                        self.scratch[24..32].copy_from_slice(&va.to_le_bytes());
                    }
                    osdesc_arena = Some(arena);
                }
                Err(e) => {
                    eprintln!("vhost-user-nvrm: 0x71 arena: {e:#}");
                    return Err(Refusal::msg(libc::EIO, "0x71 arena failed"));
                }
            }
            if kern_form {
                // And now the SHAPE, which is the part RM actually judges.
                //
                // osmemdesc.c decides on descriptorType: VIRTUAL_ADDRESS is
                // unconditionally NV_ERR_NOT_SUPPORTED there, and
                // OS_DMA_BUF_PTR/OS_SGT_PTR are reserved for clients at
                // RS_PRIV_LEVEL_KERNEL -- which this backend, a userspace
                // process, is not. Measured: status 0x56 on an alloc whose
                // pages had already arrived correctly.
                //
                // What DOES work is the shape a process uses, and has used
                // in this project for months: NV_ESC_RM_ALLOC_MEMORY with
                // NVOS02. RM's own deprecated layer converts it and sets
                // descriptorType VIRTUAL_ADDRESS together with
                // NVOS32_TYPE_IMAGE (rmapi_deprecated_allocmemory.c:246).
                //
                // The two structs agree where it matters: hRoot, hParent,
                // hObjectNew and hClass at 0/4/8/12, and `status` at 40 in
                // both. So only the middle is rewritten, and the guest's
                // original middle is put back before the reply.
                const NVOS02_FLAGS: usize = 16;
                const NVOS02_PMEM: usize = 24;
                const NVOS02_LIMIT: usize = 32;
                // The ESCAPE takes nv_ioctl_nvos02_parameters_with_fd, not the
                // bare NVOS02: the fd of the node the allocation belongs to
                // sits behind the struct, and the _IOC size says 56. Sending
                // 44 is a request that names one struct and carries another,
                // and the driver answers -EINVAL.
                // NVOS02 is 48 bytes, not 44: pMemory and limit are
                // NV_ALIGN_BYTES(8), so `status` sits at 40 and the struct is
                // padded out to 48. The fd follows THERE. Putting it at 44
                // passes the _IOC size check and hands the frontend a garbage
                // fd -- measured as ret -1 / EINVAL with a correct payload.
                const NVOS02_FD: usize = 48;
                const NVOS02_LEN: usize = 56;
                if self.scratch.len() < 48 {
                    return Err(Refusal::msg(libc::EINVAL, "0x71 kernel form: inline too short"));
                }
                osdesc_nvos64_tail = Some(self.scratch[16..40].to_vec());
                let va = u64::from_le_bytes(
                    self.aux[OSDESC_DESC..OSDESC_DESC + 8].try_into().unwrap(),
                );
                // The flags are not decoration: RmAllocOsDescriptor reads
                // them FIRST and answers NV_ERR_INVALID_FLAGS before it has
                // looked at a single page (escape.c:206). Zero would already
                // fail there -- MAPPING_DEFAULT is not NO_MAP.
                const OSDESC_ATTR: usize =
                    std::mem::offset_of!(sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS, attr);
                const OSDESC_ATTR2: usize =
                    std::mem::offset_of!(sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS, attr2);
                let attr = u32::from_le_bytes(
                    self.aux[OSDESC_ATTR..OSDESC_ATTR + 4].try_into().unwrap(),
                );
                let attr2 = u32::from_le_bytes(
                    self.aux[OSDESC_ATTR2..OSDESC_ATTR2 + 4].try_into().unwrap(),
                );
                let (flags, lost) = nvos02_flags_for_osdesc(attr, attr2);
                if let Some(name) = lost {
                    eprintln!(
                        "vhost-user-nvrm: 0x71 osdesc: guest asked {name}, sending WRITE_BACK \
                         -- the OS page-array path takes nothing else (osmemdesc.c:344; \
                         attr {attr:#x})"
                    );
                }
                self.scratch[NVOS02_FLAGS..NVOS02_FLAGS + 8].fill(0);
                self.scratch[NVOS02_FLAGS..NVOS02_FLAGS + 4]
                    .copy_from_slice(&flags.to_le_bytes());
                self.scratch[NVOS02_PMEM..NVOS02_PMEM + 8].copy_from_slice(&va.to_le_bytes());
                self.scratch[NVOS02_LIMIT..NVOS02_LIMIT + 8]
                    .copy_from_slice(&limit.to_le_bytes());
                if self.scratch.len() < NVOS02_LEN {
                    self.scratch.resize(NVOS02_LEN, 0);
                }
                // And the NODE, which is the second half of the shape.
                // NV_ESC_RM_ALLOC_MEMORY is NV_ACTUAL_DEVICE_ONLY
                // (escape.c:399) while an NV04_ALLOC of hClass 0x71 is
                // NV_CTL_DEVICE_ONLY (escape.c:486) -- the two escapes are
                // mutually exclusive about their node, so the call the guest
                // sent on ctl can never be answered on the FD it arrived on.
                // The frontend refuses it as NV_ERR_INVALID_ARGUMENT, which
                // nv.c:2805 turns into a bare -EINVAL with no status written.
                let gpu_fd = self.osdesc_gpu_fd(target_fd)?;
                self.scratch[NVOS02_FD..NVOS02_FD + 4]
                    .copy_from_slice(&(gpu_fd as u32).to_le_bytes());
                rewrite_target_fd = Some(gpu_fd);
                inline_len = NVOS02_LEN;
                rewrite_ioctl_nr = Some(sys::NV_ESC_RM_ALLOC_MEMORY);
                // The runs were ours; the params were only ever a source.
                self.aux.clear();
                aux_len = 0;
            } else {
                // aux carried the runs, it is not a params buffer: do not send back.
                self.aux.clear();
                aux_len = 0;
            }
        }

        // A rewritten 0x71 alloc has no embedded pointer any more: NVOS64
        // carries pAllocParms at offset 16, NVOS02 carries `flags` there, and
        // the params it pointed at have already been consumed into the runs
        // and the host VA. Measured as "embedded_ptr_off set, but this call
        // carries no embedded pointer" -- the guest described the struct it
        // sent, and this backend changed it.
        if req.embedded_ptr_off != NONE_U32 && rewrite_ioctl_nr.is_none() {
            let off = req.embedded_ptr_off as usize;
            if off + 8 > inline_len {
                return Err(Refusal::msg(libc::EINVAL, "embedded_ptr_off"));
            }
            let need = unsafe {
                nvrm_abi::xlate::embedded_ptr(
                    dev, req.ioctl_nr, self.scratch.as_ptr(), inline_len as u32)
            };
            match need {
                Ok(Some(e)) => {
                    if e.ptr_off as usize != off || (e.len as usize) > self.aux.len() {
                        return Err(Refusal::new(libc::EINVAL, format!(
                            "aux too small: driver reads {} bytes @{}, guest sent {} @{}",
                            e.len, e.ptr_off, self.aux.len(), off)));
                    }
                }
                Ok(None) => {
                    return Err(Refusal::msg(libc::EINVAL,
                        "embedded_ptr_off set, but this call carries no embedded pointer"));
                }
                Err(()) => {
                    return Err(Refusal::msg(libc::ENOTSUP,
                        "embedded length not determinable (unknown hClass)"));
                }
            }
            let addr = self.aux.as_mut_ptr() as u64;
            self.scratch[off..off + 8].copy_from_slice(&addr.to_le_bytes());
        }

        // (1a') A ring-0 callback event, which RM will refuse. Substituted
        //       for an OS event this backend owns -- see
        //       `alloc_os_event_id` for why this decides
        //       whether NVKMS gets a device at all.
        //
        //       The class is read from the INLINE block (NVOS64.hClass @12),
        //       because that is the one RM dispatches on, and rewritten in
        //       both places: RM looks at the outer class for the privilege
        //       check and at the params for the event's own type.
        let mut event_reg: Option<EventReg> = None;
        if req.ioctl_nr == sys::NV_ESC_RM_ALLOC
            && inline_len >= 16
            && self.aux.len() >= std::mem::size_of::<sys::NV0005_ALLOC_PARAMETERS>()
        {
            let hclass = u32::from_le_bytes(self.scratch[12..16].try_into().unwrap());
            if hclass == sys::NV01_EVENT_KERNEL_CALLBACK
                || hclass == sys::NV01_EVENT_KERNEL_CALLBACK_EX
            {
                let h_client = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                // NV0005_ALLOC_PARAMETERS (cl0005.h:40): hParentClient@0,
                // hSrcResource@4, hClass@8, notifyIndex@12, data@16 (NvP64)
                // -- offsets from the mirror struct, asserted against these
                // numbers. `data` held a guest kernel pointer on the way in
                // and holds the id of an event this process owns on the way
                // out. The pointer and the unstripped notifyIndex are saved
                // FIRST: they are what the guest gets back with the firing
                // (KIND_EVENT_FIRED `addr` / `nested_count`).
                let guest_data = u64::from_le_bytes(
                    self.aux[NV0005_DATA..NV0005_DATA + 8].try_into().unwrap(),
                );
                let notify_index = u32::from_le_bytes(
                    self.aux[NV0005_NOTIFYINDEX..NV0005_NOTIFYINDEX + 4].try_into().unwrap(),
                );
                let id = self.alloc_os_event_id(h_client)?;
                self.aux[NV0005_HCLASS..NV0005_HCLASS + 4]
                    .copy_from_slice(&sys::NV01_EVENT_OS_EVENT.to_le_bytes());
                self.aux[NV0005_DATA..NV0005_DATA + 8].copy_from_slice(&(id as u64).to_le_bytes());
                self.scratch[12..16]
                    .copy_from_slice(&sys::NV01_EVENT_OS_EVENT.to_le_bytes());
                event_reg = Some(EventReg {
                    h_client,
                    h_event: 0,
                    class: hclass,
                    notify_index,
                    guest_data,
                    token: req.target_token,
                    id,
                });
                eprintln!(
                    "vhost-user-nvrm: kernel callback event {hclass:#x} -> OS event id {id} \
                     (client {h_client:#x}, notifyIndex {notify_index:#x}, cb {guest_data:#x})"
                );
            }
        }

        // (1a''') The OTHER door for guest kernel callbacks, and it is a
        //         CONTROL, not an alloc: a semaphore-surface waiter
        //         (NV_SEMAPHORE_SURFACE_CTRL_CMD_REGISTER_WAITER). NVKMS
        //         puts a pointer to its NVOS10_EVENT_KERNEL_CALLBACK_EX in
        //         `notificationHandle` (nvkms-kapi-sync.c:432); RM reads
        //         that for a USERSPACE client -- which this backend is --
        //         as an OS-event id (osUserHandleToKernelPtr) and refuses.
        //         nvidia-drm then arms no timer either
        //         (nvidia-drm-fence.c:1059-1076), and the fence behind
        //         every GBM compositor frame polls forever. Measured
        //         2026-08-16: weston frozen in its first eglSwapBuffers.
        //
        //         So: substitute an OS event of our own, one PRIVATE fd per
        //         waiter, because the firing is dataless (sem_surf.c:1591)
        //         and only the file itself can say who fired. A kernel VA
        //         cannot fit in 32 bits and a user client's id cannot
        //         exceed them (RM casts to NvU32, os.c:1744) -- that is the
        //         whole discriminator, and user-space registrations pass
        //         through untouched.
        let mut waiter_reg: Option<PendingWaiter> = None;
        let mut waiter_unreg: Option<u32> = None;
        if req.ioctl_nr == sys::NV_ESC_RM_CONTROL && inline_len >= 32 {
            let cmd = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
            if cmd == CTRL_SEMSURF_REGISTER_WAITER && self.aux.len() >= 32 {
                let kc = u64::from_le_bytes(self.aux[24..32].try_into().unwrap());
                if kc > u32::MAX as u64 {
                    let h_client = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                    let slot = self.waiter_slot_get(h_client)?;
                    self.aux[24..32].copy_from_slice(&(slot.id as u64).to_le_bytes());
                    // Per waiter registration, i.e. per frame -- hence the
                    // cached level.
                    if debug_level() >= 1 {
                        eprintln!(
                            "vhost-user-nvrm: semsurf waiter kc {kc:#x} -> OS event id {} \
                             (client {h_client:#x})",
                            slot.id
                        );
                    }
                    waiter_reg = Some(PendingWaiter {
                        slot,
                        h_client,
                        guest_kc: kc,
                        token: req.target_token,
                    });
                }
            } else if cmd == CTRL_SEMSURF_UNREGISTER_WAITER && self.aux.len() >= 24 {
                let kc = u64::from_le_bytes(self.aux[16..24].try_into().unwrap());
                if kc > u32::MAX as u64 {
                    let h_client = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                    // RM matches the unregister by the SAME handle value the
                    // registration carried, so the translation must agree
                    // with itself. No armed waiter = it already fired; the
                    // untranslated pointer then draws OBJECT_NOT_FOUND from
                    // RM, which is exactly the "too late to cancel" answer
                    // nvidia-drm expects on that path.
                    let id = self
                        .waiters
                        .iter()
                        .find(|(_, w)| w.h_client == h_client && w.guest_kc == kc)
                        .map(|(&id, _)| id);
                    if let Some(id) = id {
                        self.aux[16..24].copy_from_slice(&(id as u64).to_le_bytes());
                        waiter_unreg = Some(id);
                    }
                }
            }
        }

        // (1b) FD field INSIDE the aux buffer (NvP64, 8 bytes): guest token
        //      -> host FD. The counterpart to (1) for fds that sit in the
        //      alloc params (NV0005.data for NV01_EVENT_OS_EVENT).
        if req.aux_fd_field_off != NONE_U32 {
            let off = req.aux_fd_field_off as usize;
            // WIDTH, and the host decides it from ITS OWN table rather than
            // believing the guest. Eight bytes for an alloc's NvP64
            // (NV0005.data); FOUR for a control's NvS32 (ctrl0000unix.h).
            // Writing eight where the field is four would overwrite the
            // neighbour -- for NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD
            // that neighbour is `flags`, and RM would then read a flag word
            // built from a file descriptor.
            let ctrl_fd = if req.ioctl_nr == sys::NV_ESC_RM_CONTROL && inline_len >= 32 {
                let cmd = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
                nvrm_abi::xlate::ctrl_fd_offset(cmd)
            } else {
                None
            };
            let width = match ctrl_fd {
                // The guest may only name the offset the host's own table
                // names. Anything else is a guest that disagrees with the
                // table it was handed, and that is a refusal, not a fixup.
                Some(o) if o as usize == off => 4,
                Some(_) => return Err(Refusal::msg(libc::EINVAL, "aux_fd_field_off vs table")),
                None => 8,
            };
            if off + width > self.aux.len() {
                return Err(Refusal::msg(libc::EINVAL, "aux_fd_field_off"));
            }
            if req.aux_fd_field_token != NONE_U64 {
                // STRICTLY what the device resolved, in the session the
                // guest named as the owner. There is no fall-back to
                // `self.mirror`: tokens are per-session and this call is
                // routinely NOT the owner (NVKMS importing an object the X
                // server exported), so a fall-back would silently look one
                // up in the wrong mirror -- and with per-session counters
                // starting at 1 it could even find a DIFFERENT file.
                let host_num = match self.aux_fd_host {
                    Some(f) => f,
                    None => return Err(Refusal::msg(libc::EBADF, "aux_fd_field_token")),
                };
                if width == 4 {
                    self.aux[off..off + 4].copy_from_slice(&host_num.to_le_bytes());
                } else {
                    self.aux[off..off + 8].copy_from_slice(&(host_num as i64).to_le_bytes());
                    // (0x79 return channel, the alloc form) The Vulkan
                    // driver never calls NV_ESC_ALLOC_OS_EVENT: it allocs
                    // NV01_EVENT_OS_EVENT with the fd in NV0005.data (20
                    // per fencetime run) and then poll(2)s /dev/nvidia0.
                    // RM wakes the nvfp of the fd named in `data`
                    // (event_notification/osapi: the file, not the caller),
                    // so THAT fd is the wake channel -- and it may live in
                    // another session (aux_fd_field_proc). The device
                    // dedupes; a refused alloc leaves a harmless watch.
                    self.pending_pollables.push(Pollable::Client {
                        token: req.aux_fd_field_token,
                        fd: host_num,
                        owner: if req.aux_fd_field_proc != NONE_U32 {
                            Some(req.aux_fd_field_proc)
                        } else {
                            None
                        },
                    });
                }
            }
            // NONE_U64: leave the value (a negative fd) unchanged.
        }

        // (2b) SECOND-level pointers: every descriptor points at a range
        //      inside the same aux buffer. Write that address in.
        //
        //      (2) applies here too: `d.len` comes from the guest, while the
        //      length RM uses the written pointer with sits in a field
        //      INSIDE the params buffer. The host resolves it independently.
        let mut fb_info_list: Option<(usize, usize)> = None;
        if req.nested_count as usize > proto::MAX_NESTED {
            return Err(Refusal::msg(libc::EINVAL, "nested_count > MAX_NESTED"));
        }
        if req.nested_count > 0 {
            let specs = if req.ioctl_nr == nvrm_abi::sys::NV_ESC_RM_CONTROL && inline_len >= 32
            {
                let cmd = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
                nvrm_abi::xlate::nested_ptrs(cmd)
            } else {
                &[][..]
            };
            if specs.len() != req.nested_count as usize {
                return Err(Refusal::new(libc::EINVAL, format!(
                    "nested_count {} does not match {} annotated pointers",
                    req.nested_count, specs.len())));
            }
            for (i, sp) in specs.iter().enumerate() {
                let d = req.nested[i];
                let po = d.ptr_off as usize;
                let ao = d.aux_off as usize;
                if d.ptr_off != sp.ptr_off {
                    return Err(Refusal::msg(libc::EINVAL, "nested ptr_off differs"));
                }
                let want = unsafe { sp.len.resolve(self.aux.as_ptr(), self.aux.len() as u32) };
                let Some(want) = want else {
                    return Err(Refusal::msg(libc::EINVAL, "nested length not determinable"));
                };
                if want > d.len {
                    return Err(Refusal::new(libc::EINVAL, format!(
                        "nested @{po}: driver reads {want} bytes, guest sent {}", d.len)));
                }
                if po + 8 > self.aux.len() || ao + d.len as usize > self.aux.len() {
                    return Err(Refusal::msg(libc::EINVAL,
                        "nested descriptor outside aux"));
                }
                let addr = unsafe { self.aux.as_mut_ptr().add(ao) } as u64;
                self.aux[po..po + 8].copy_from_slice(&addr.to_le_bytes());

                // The graphics stack's FB question. Note WHERE the answer
                // will land, so execute can cap it the way the V2 form is
                // already capped -- see vram::CMD_FB_GET_INFO for what an
                // uncapped answer here costs.
                if self.vram.enabled()
                    && req.ioctl_nr == nvrm_abi::sys::NV_ESC_RM_CONTROL
                    && inline_len >= 32
                    && u32::from_le_bytes(self.scratch[8..12].try_into().unwrap())
                        == crate::vram::CMD_FB_GET_INFO
                {
                    let asked = u32::from_le_bytes(
                        self.aux[..4].try_into().unwrap()) as usize;
                    fb_info_list = Some((ao, asked));
                }
            }
        }

        // (2c) UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE is NOT forced any
        // more. History, because the reason was real once:
        //
        // Without the flag UVM binds the va_space to the mm of the process
        // that called UVM_INITIALIZE (uvm.c:953), and a later mmap from a
        // DIFFERENT process returns -EOPNOTSUPP (uvm.c:784-788). In the
        // window era that was this daemon vs cloud-hypervisor, so the flag
        // was set unconditionally, with the note "the price (pageable
        // access, HMM, ATS off) is demonstrably zero for these workloads"
        // and "the flag is pure IN, so the guest notices nothing".
        //
        // Both halves stopped being true, measured 2026-08-15 with the
        // tracer's answer payloads (OPEN-QUESTIONS nr 11):
        //   - flags is IN/OUT: the guest read 0x2 back where the host
        //     native run reads 0x0;
        //   - UVM_PAGEABLE_MEM_ACCESS answered 0 in the guest, 1 natively --
        //     the Vulkan raytracing init REQUIRES pageable access, skips
        //     PAGEABLE_MEM_ACCESS_ON_GPU when it is off, and vkCreateDevice
        //     with VK_KHR_acceleration_structure returns
        //     INITIALIZATION_FAILED. CS2 enables that extension whenever
        //     the driver offers it: "Failed to initialize Vulkan".
        //
        // And the reason is gone: guest UVM mmaps take the pool path
        // (nvrm_node_mmap -> nvrm_mmap_pool), never the host-visible
        // window -- no foreign process mmaps this daemon's UVM fd any more.
        // The guest's own flags pass through unchanged, as every other IN
        // field does.
        let _ = UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE; // kept as documentation of the value

        // (3) The real ioctl. Frontend: rebuild the _IOC encoding.
        //     UVM: raw number, no _IOC encoding.
        let request: libc::c_ulong = if dev.is_uvm() {
            req.ioctl_nr as libc::c_ulong
        } else if inline_len > ((1 << 14) - 1) {
            // XFER direction: the payload does not fit into _IOC_SIZE
            // (14 bits). It would have to be wrapped into an
            // nv_ioctl_xfer_t. No forwarded workload produces this today
            // (the controls are small) - hence refuse loudly rather than
            // guess at a half-built encoding.
            return Err(Refusal::msg(libc::EMSGSIZE, "XFER repack not built yet"));
        } else {
            // The rewritten number, when a kernel-form 0x71 alloc was turned
            // into the NVOS02 shape: the _IOC encoding carries BOTH the
            // number and the size, so encoding the original here sends RM a
            // request that names one struct and carries another. Measured as
            // ret -22 on a call whose payload was already correct.
            iowr_raw(rewrite_ioctl_nr.unwrap_or(req.ioctl_nr), inline_len as u32)
                as libc::c_ulong
        };

        // Do NOT forward ALLOC_SEMAPHORE_POOL -- otherwise the real UVM
        // would create the pool at GPU VA `base` and collide with the
        // external mapping the guest requests moments later via
        // UvmPoolBack. Fake success, rmStatus = NV_OK.
        let fake_semaphore_pool =
            dev.is_uvm() && req.ioctl_nr == UVM_ALLOC_SEMAPHORE_POOL;

        // "Managed light", opt-in (LEA_MANAGED_COMPAT=1): managed ranges
        // never exist across the VM boundary -- the guest pages hang there
        // as an EXTERNAL_RANGE, and the managed-only commands refuse that
        // with 0x1e (measured with DISABLE_READ_DUPLICATION). Sysmem-
        // resident pages HAVE no read duplication and nothing to migrate --
        // so the answers here are semantic no-ops, not guesses:
        //  - ENABLE/DISABLE_READ_DUPLICATION (44/45): rmStatus @16 = NV_OK.
        //  - MIGRATE (51): rmStatus @72 = NV_OK, userSpaceStart/Length
        //    @56/@64 = 0. Per uvm_ioctl.h:595-597 a NOTHING_TO_DO plus a
        //    non-zero pair would mean "userspace completes the migration
        //    itself (move_pages)"; zero plus NV_OK says it is already done.
        //    In the guest these are anonymous pages that stay where they
        //    are. The writes, and why NV_OK rather than NOTHING_TO_DO, are
        //    at the execute branch.
        // The default (without the variable) stays: forward, get 0x1e, and
        // let cuMemAllocManaged fail loudly.
        let fake_managed = dev.is_uvm()
            && matches!(req.ioctl_nr, 42 | 43 | 44 | 45 | 46 | 47 | 51)
            && managed_compat();
        // (2e) The VM's VRAM cap (LEA_VRAM_LIMIT_MIB), off by default.
        //
        // Reserve BEFORE the ioctl, settle after: an allocation that is
        // only counted once RM has handed it out is an allocation that
        // walked past the cap, and one request is enough -- the guest
        // picks the size.
        //
        // WARNING: this runs whether or not a cap is set. Counting and
        // enforcing used to be one thing, because the cap was the only
        // consumer; the VM's own process list needs the numbers in the
        // DEFAULT configuration, where no one capped anything. `reserve`
        // therefore accepts unconditionally when no limit is set -- and
        // the price of the bookkeeping is now paid by every run, which is
        // what the measurement after this change has to show.
        //
        // The limit itself is still read once, at startup
        // (crate::vram::limit_bytes).
        let mut vram_reserved = 0u64;
        let mut vram_full = false;
        let mut vram_status_off = 0usize;
        if req.ioctl_nr == sys::NV_ESC_RM_ALLOC {
            // Both alloc forms carry hClass @12; only `status` moves.
            if let Some(st_off) = alloc_status_off(inline_len) {
                let hclass = u32::from_le_bytes(self.scratch[12..16].try_into().unwrap());
                // The params are the aux buffer, whose length the
                // embedded-pointer check above already held against what
                // the driver reads for this class.
                if let Some(bytes) = crate::vram::request_bytes(hclass, &self.aux) {
                    vram_status_off = st_off;
                    if self.vram.reserve(bytes) {
                        vram_reserved = bytes;
                    } else {
                        vram_full = true;
                    }
                }
            }
        }

        // (2e') The SAME cap, through the other door. NVOS32 is entirely
        // inline -- no embedded pointer -- so the parameter block is
        // `scratch`, and the reserve/settle mechanics below are the
        // NVOS64 branch's, unchanged.
        let mut vram_vidheap = false;
        if req.ioctl_nr == sys::NV_ESC_RM_VID_HEAP_CONTROL {
            if let Some(bytes) = crate::vram::vidheap_request_bytes(&self.scratch[..inline_len]) {
                vram_vidheap = true;
                vram_status_off = crate::vram::V_STATUS_OFF;
                if self.vram.reserve(bytes) {
                    vram_reserved = bytes;
                } else {
                    vram_full = true;
                }
            }
        }

        let action = if fake_semaphore_pool {
            Action::FakeSemaphorePool
        } else if fake_managed {
            Action::FakeManaged
        } else if vram_full {
            Action::FakeVramFull
        } else {
            Action::Forward
        };
        Ok(Plan {
            seq: req.seq,
            target_fd: rewrite_target_fd.unwrap_or(target_fd),
            request,
            action,
            ioctl_nr: rewrite_ioctl_nr.unwrap_or(req.ioctl_nr),
            dev_tag: req.dev_tag,
            inline_len,
            aux_len,
            osdesc_arena,
            osdesc_nvos64_tail,
            target_token: req.target_token,
            vram_reserved,
            vram_status_off,
            event_reg,
            waiter_reg,
            waiter_unreg,
            vram_vidheap,
            fb_info_list,
        })
    }

    /// DOING: the three [`NvSyscalls`] calls (or the faked answers), the
    /// follow-up work that depends on the result, and the reply. Reads and
    /// writes the buffers that [`Session::prepare`] assembled.
    ///
    /// The fake branches check their field lengths HERE, not in prepare:
    /// there, checking and writing the answer are one inseparable match,
    /// and neither involves a syscall -- the refusal looks the same to the
    /// guest either way.
    fn execute(&mut self, mut plan: Plan) -> Result<()> {
        let inline_len = plan.inline_len;
        let aux_len = plan.aux_len;

        let ret = if matches!(plan.action, Action::FakeVramFull) {
            // The VM is at its cap. Answer the way a full card answers.
            //
            // Measured (probe/suites/test_vram_churn.py native, tracer):
            // at the OOM point the ioctl returns 0 and NVOS64.status is
            // 0x51 = NV_ERR_NO_MEMORY -- libcuda turns that into
            // CUDA_ERROR_OUT_OF_MEMORY and torch into OutOfMemoryError.
            // An errno instead would surface as an AcceleratorError, i.e.
            // a crash rather than a limit.
            //
            // `vram_status_off` is 40 (NVOS64) or 28 (NVOS21); prepare
            // set it, and only for a length that holds it.
            let o = plan.vram_status_off;
            self.scratch[o..o + 4].copy_from_slice(&sys::NV_ERR_NO_MEMORY.to_le_bytes());
            // Not every refusal: a guest that keeps asking would otherwise
            // write the backend log full, and libcuda does retry.
            if let Some(n) = self.vram.count_refusal() {
                eprintln!(
                    "vhost-user-nvrm: VRAM cap reached ({n}. refusal) -- {} of {} MiB in use, \
                     allocation answered with NV_ERR_NO_MEMORY ({})",
                    self.vram.used() >> 20,
                    self.vram.limit() >> 20,
                    self.vram.knob()
                );
            }
            0
        } else if matches!(plan.action, Action::FakeSemaphorePool) {
            if SEMAPHORE_POOL_RMSTATUS_OFF + 4 <= inline_len {
                self.scratch[SEMAPHORE_POOL_RMSTATUS_OFF..SEMAPHORE_POOL_RMSTATUS_OFF + 4]
                    .copy_from_slice(&sys::NV_OK.to_le_bytes());
            }
            0
        } else if matches!(plan.action, Action::FakeManaged) {
            match plan.ioctl_nr {
                // UNSET_PREFERRED_LOCATION / ENABLE / DISABLE_READ_DUPLICATION:
                // rmStatus @16 in all three (uvm_ioctl.h, guarded above).
                43..=45 if inline_len >= 20 => {
                    self.scratch[16..20].copy_from_slice(&sys::NV_OK.to_le_bytes());
                }
                // SET_PREFERRED_LOCATION: a residency hint -- with the pages
                // sysmem-resident the location is fixed, so the hint is a
                // no-op. rmStatus @36.
                42 if inline_len >= 40 => {
                    self.scratch[36..40].copy_from_slice(&sys::NV_OK.to_le_bytes());
                }
                // SET/UNSET_ACCESSED_BY: access hints for migratable ranges
                // -- with the pages sysmem-resident there is nothing to hint
                // at. rmStatus @32.
                46 | 47 if inline_len >= 36 => {
                    self.scratch[32..36].copy_from_slice(&sys::NV_OK.to_le_bytes());
                }
                // MIGRATE: rmStatus @72, userSpaceStart/Length @56/@64,
                // semaphoreAddress/Payload @40/@48 (UVM_MIGRATE_PARAMS).
                51 if inline_len >= 80 => {
                    let base = self.scratch[0..8].to_vec();
                    let length = self.scratch[8..16].to_vec();
                    self.scratch[MIGRATE_USER_SPACE_START_OFF..MIGRATE_USER_SPACE_START_OFF + 8]
                        .copy_from_slice(&[0u8; 8]);
                    self.scratch[MIGRATE_USER_SPACE_LENGTH_OFF..MIGRATE_USER_SPACE_LENGTH_OFF + 8]
                        .copy_from_slice(&[0u8; 8]);
                    // NV_OK, not NOTHING_TO_DO: with NOTHING_TO_DO libcuda
                    // retries the ioctl endlessly (measured: 2.4 million
                    // calls in 120 s). There IS nothing to migrate -- the
                    // pages are sysmem-resident and coherently mapped;
                    // "migration complete" is the truthful answer for this
                    // architecture.
                    self.scratch[72..76].copy_from_slice(&sys::NV_OK.to_le_bytes());
                    // Async protocol: libcuda waits (spinning, with len=0
                    // MIGRATE flushes) for UVM to write `semaphorePayload`
                    // to `semaphoreAddress` -- an address that lies in the
                    // semaphore pool the host backed. Without that write the
                    // prefetch sync hangs forever, so the host performs it.
                    let sema = u64::from_le_bytes(
                        self.scratch[MIGRATE_SEMAPHORE_ADDRESS_OFF..MIGRATE_SEMAPHORE_ADDRESS_OFF + 8]
                            .try_into().unwrap(),
                    );
                    let pay = u32::from_le_bytes(
                        self.scratch[MIGRATE_SEMAPHORE_PAYLOAD_OFF..MIGRATE_SEMAPHORE_PAYLOAD_OFF + 4]
                            .try_into().unwrap(),
                    );
                    if sema != 0 && !self.pool.write_guest_u32(GuestAddr::new(sema), pay) {
                        eprintln!(
                            "vhost-user-nvrm: MIGRATE semaphore {sema:#x} not in any \
                             pool -- guest will hang"
                        );
                    }
                    if debug_level() >= 2 {
                        let flags = u32::from_le_bytes(
                            self.scratch[MIGRATE_FLAGS_OFF..MIGRATE_FLAGS_OFF + 4].try_into().unwrap(),
                        );
                        eprintln!(
                            "vhost-user-nvrm: MIGRATE base={:#x} len={:#x} flags={flags:#x} \
                             sema={sema:#x} payload={pay:#x}",
                            u64::from_le_bytes(base.try_into().unwrap()),
                            u64::from_le_bytes(length.try_into().unwrap()),
                        );
                    }
                }
                _ => {
                    return self.reply_err(plan.seq, libc::EINVAL, "managed compat: params too short");
                }
            }
            0
        } else {
            // The length is the initialized part of the scratch buffer:
            // `inline_len` bytes copied from the guest, plus whatever a
            // translation grew it to. The driver ignores it; the ledger in
            // the test build must not read past it.
            unsafe {
                let n = self.scratch.len();
                self.sys.ioctl(plan.target_fd, plan.request, self.scratch.as_mut_ptr(), n)
            }
        };
        // errno belongs to the call that just returned and to nothing else.
        // Everything below may make syscalls of its own (an FREE_OS_EVENT on
        // the (1a') failure path, log writes, the capture hook), each of which
        // can overwrite it -- until 2026-08-18 the errno was read at the very
        // end of this function, after all of them.
        let ioctl_errno = if ret < 0 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
        } else {
            0
        };

        // After a successful 0x71, pin the arena to the created handle
        // (NVOS02: hObjectNew @8, status @40).
        if let Some(arena) = plan.osdesc_arena.take() {
            let st = u32::from_le_bytes(self.scratch[40..44].try_into().unwrap());
            if ret == 0 && st == sys::NV_OK {
                let hnew = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
                self.pool.keep_osdesc_arena(hnew, arena);
            }
        }

        // (1a'', the other half of the substitution) RM has spoken about the
        // substituted event: file the registration under the handle it
        // created (hObjectNew @8 in both alloc forms), or hand the OS-event
        // id back if it refused -- an id RM never fires for would sit in its
        // list until the ctl closes.
        if let Some(mut r) = plan.event_reg.take() {
            let st_off = alloc_status_off(inline_len);
            let ok = ret == 0
                && st_off.is_some_and(|o| {
                    o + 4 <= self.scratch.len()
                        && u32::from_le_bytes(self.scratch[o..o + 4].try_into().unwrap())
                            == sys::NV_OK
                });
            if ok && self.scratch.len() >= 12 {
                r.h_event = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
                self.events.insert((r.h_client, r.h_event), r);
                self.ev_registered += 1;
            } else {
                self.free_os_event(r.h_client, r.id);
            }
        }

        // (1a''', the other half) RM has spoken about the substituted
        // waiter. NV_OK is the only "a firing is coming": arm the slot and
        // report it to the poller. Every other verdict -- ALREADY_SIGNALLED
        // included, which RM answers WITHOUT registering a notification
        // (ctrl00da.h:189-196) -- returns the slot to the pool. NVOS54:
        // status @28.
        if let Some(p) = plan.waiter_reg.take() {
            use std::os::fd::AsRawFd;
            let ok = ret == 0
                && inline_len >= 32
                && u32::from_le_bytes(self.scratch[28..32].try_into().unwrap()) == sys::NV_OK;
            if ok {
                self.pending_pollables
                    .push(Pollable::Waiter { id: p.slot.id, fd: p.slot.ctl.as_raw_fd() });
                self.waiters.insert(
                    p.slot.id,
                    ActiveWaiter {
                        h_client: p.h_client,
                        guest_kc: p.guest_kc,
                        token: p.token,
                        slot: p.slot,
                    },
                );
                self.ev_registered += 1;
            } else {
                self.waiter_pool.entry(p.h_client).or_default().push(p.slot);
            }
        }
        if let Some(id) = plan.waiter_unreg.take() {
            // NV_OK = cancelled before it fired; the listener is gone and
            // the slot is free again. Any other verdict means the waiter
            // fired (the poller owns the retirement) or was never RM's.
            let ok = ret == 0
                && inline_len >= 32
                && u32::from_le_bytes(self.scratch[28..32].try_into().unwrap()) == sys::NV_OK;
            if ok {
                if let Some(w) = self.waiters.remove(&id) {
                    self.waiter_pool.entry(w.h_client).or_default().push(w.slot);
                }
            }
        }

        // The guest sent an NVOS64 block and must read one back. Only the
        // middle was rewritten into the NVOS02 shape RM accepts; `status`
        // sits at 40 in both structs, so RM's verdict survives the restore
        // and everything else is the caller's own again.
        if let Some(tail) = plan.osdesc_nvos64_tail.take() {
            // 16..40 only. `status` is at 40 and is the one field that must
            // NOT come back from the saved copy -- restoring it would hand
            // the caller its own zero instead of RM's verdict.
            if self.scratch.len() >= 40 && tail.len() >= 24 {
                self.scratch[16..40].copy_from_slice(&tail[..24]);
            }
        }

        // A refusal decided while reading RM's ANSWER is carried to the end
        // of this function instead of returning from the middle of it.
        // Everything below this point books what the ioctl DID -- it has
        // already run, and the books are the host's, not the guest's
        // answer. Returning early made those two share a fate they do not
        // have (see the note above the check at the end).
        let mut refusal: Option<(i32, &'static str)> = None;

        if ret == 0 && plan.ioctl_nr == sys::NV_ESC_RM_CONTROL && inline_len >= 32 {
            let cmd = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
            // The card the guest sees: size and name, and the two are NOT
            // the same question.
            //
            // The SIZE is only rewritten under a cap. Without one the VM
            // has the whole card, and reporting anything else would be the
            // lie.
            //
            // The NAME is rewritten always. It is not a claim about how
            // much memory there is -- it is a claim about what the guest is
            // talking to, and that is a mediated device either way. Nobody
            // should be able to sit in one of these VMs and not notice; the
            // card says so, capped or not. The cap only decides whether the
            // profile size joins the name.
            if self.vram.enabled() && cmd == crate::vram::CMD_FB_GET_INFO_V2 {
                let st = u32::from_le_bytes(self.scratch[28..32].try_into().unwrap());
                if st == sys::NV_OK {
                    crate::vram::rewrite_fb_info(
                        &mut self.aux, self.vram.limit(), self.vram.used());
                }
            }
            // The same answer through the V1 door, which is the one the
            // graphics stack uses. `fb_info_list` is only Some under a cap
            // and only for this command, so the check is the plan's.
            if let Some((off, asked)) = plan.fb_info_list {
                let st = u32::from_le_bytes(self.scratch[28..32].try_into().unwrap());
                if st == sys::NV_OK && off <= self.aux.len() {
                    let (limit, used) = (self.vram.limit(), self.vram.used());
                    crate::vram::rewrite_fb_info_list(
                        &mut self.aux[off..], asked, limit, used);
                }
            }
            // LEA_GPU_NAME_RAW=1 leaves the driver's own name in place --
            // the discriminator for whether a client branches on the
            // product string (none has been caught doing so).
            if cmd == crate::vram::CMD_GPU_GET_NAME_STRING && !gpu_name_raw() {
                let st = u32::from_le_bytes(self.scratch[28..32].try_into().unwrap());
                if st == sys::NV_OK {
                    crate::vram::rewrite_gpu_name(&mut self.aux, self.vram.profile());
                }
            }
            // (3c) The VM's own process list.
            //
            // `nvidia-smi` builds it from GPU_GET_PIDS plus one
            // GPU_GET_PID_INFO per PID. Both are forwarded and RM answers
            // with the HOST's table -- measured 2026-08-06: the guest
            // receives eight host PIDs with their per-process FB usage and
            // prints "No running processes found" only because it cannot
            // resolve those PIDs in its own /proc. Replacing the table
            // therefore closes a leak as much as it adds a feature. On the
            // way BACK, in place: both structures are flat, so nothing
            // about the protocol changes. NVOS54: cmd @8, status @28.
            if cmd == crate::vram::CMD_GPU_GET_PIDS || cmd == crate::vram::CMD_GPU_GET_PID_INFO {
                let st = u32::from_le_bytes(self.scratch[28..32].try_into().unwrap());
                // Only rewrite an answer RM actually gave. On a failure the
                // guest should see the failure, not a table we invented.
                if st == sys::NV_OK {
                    let roster = self.vram.roster();
                    let n = if cmd == crate::vram::CMD_GPU_GET_PIDS {
                        crate::vram::rewrite_get_pids(&mut self.aux, &roster)
                    } else {
                        crate::vram::rewrite_get_pid_info(&mut self.aux, &roster)
                    };
                    if n.is_none() {
                        // The buffer is not the structure the command
                        // names. Forwarding RM's answer would leak host
                        // PIDs, so refuse instead of guessing.
                        eprintln!(
                            "vhost-user-nvrm: {cmd:#x} with a {}-byte params buffer -- \
                             not the documented struct, answering EINVAL",
                            self.aux.len()
                        );
                        refusal = Some((libc::EINVAL, "process list: params too short"));
                    }
                }
            }
        }

        // Settle what prepare reserved on the VRAM ledger: keep the charge
        // only if RM really handed out device memory.
        //
        // The attr is read BACK, not as it was sent: with LOCATION_ANY the
        // guest leaves the choice to RM, and RM writes what it chose into
        // the same field (measured: 0x18000000 in -> 0x11800000 out for a
        // VIDMEM block; on a failed alloc the field stays untouched, which
        // is why `ok` is checked first).
        if plan.vram_reserved != 0 {
            // hRoot @0, hObjectParent @4, hObjectNew @8 in both alloc
            // forms; `status` is the one field that moves (see
            // alloc_status_off).
            let o = plan.vram_status_off;
            let st = u32::from_le_bytes(self.scratch[o..o + 4].try_into().unwrap());
            let ok = ret == 0 && st == sys::NV_OK;
            // hRoot @0 and hObjectParent @4 sit at the same offsets in
            // NVOS64 and NVOS32; the handle and the written-back attr do
            // not, and NVOS32 keeps both inline.
            let (attr_out, handle) = if plan.vram_vidheap {
                let p = &self.scratch[..inline_len];
                (crate::vram::vidheap_attr_out(p), crate::vram::vidheap_handle(p))
            } else {
                let attr = if self.aux.len() >= 28 {
                    u32::from_le_bytes(self.aux[24..28].try_into().unwrap())
                } else {
                    0
                };
                (attr, u32::from_le_bytes(self.scratch[8..12].try_into().unwrap()))
            };
            self.vram.settle(ok, attr_out, crate::vram::Charge {
                token: plan.target_token,
                root: u32::from_le_bytes(self.scratch[0..4].try_into().unwrap()),
                parent: u32::from_le_bytes(self.scratch[4..8].try_into().unwrap()),
                handle,
                bytes: plan.vram_reserved,
            });
        }

        // And the release through the same door the alloc came in by.
        // NVOS32_FUNCTION_FREE names the memory handle in the very field
        // ALLOC_SIZE wrote it into, so `free_object` needs nothing new --
        // it takes that handle and everything that hung below it.
        //
        // Deliberately not conditional on NVOS32.status, for the reason the
        // NVOS00 path below states: a charge released once too often costs
        // a VM some of its cap, a charge never released strangles it.
        // Without this half the cap would not be a cap but a slow trap.
        if ret == 0 && plan.ioctl_nr == sys::NV_ESC_RM_VID_HEAP_CONTROL {
            let p = &self.scratch[..inline_len];
            if crate::vram::vidheap_function(p) == Some(sys::NVOS32_FUNCTION_FREE) {
                let hroot = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                self.vram.free_object(hroot, crate::vram::vidheap_handle(p));
            }
        }

        // The object ledger, the host half of it: every ALLOC and every
        // FREE the host RM actually saw, with the pair it books objects
        // under. The guest keeps the same ledger behind `display > 1`
        // (kapi_ledger); the two together are the only way to tell a handle
        // the guest lost track of from one the host never let go.
        obj_ledger(plan.ioctl_nr, &self.scratch[..inline_len], ret);

        // A client root is the one object with no parent, so
        // `hObjectParent == 0` names it in both alloc forms (the comment at
        // the VRAM settle above says the offsets). Note which guest FD it
        // was created on -- that FD, not an RM_FREE, is what usually ends
        // this client's life, and `close_clients_of_token` needs the link.
        if ret == 0 && plan.ioctl_nr == sys::NV_ESC_RM_ALLOC && inline_len >= 12 {
            let st_ok = alloc_status_off(inline_len)
                .filter(|&o| o + 4 <= inline_len)
                .map(|o| u32::from_le_bytes(self.scratch[o..o + 4].try_into().unwrap()))
                == Some(sys::NV_OK);
            let parent = u32::from_le_bytes(self.scratch[4..8].try_into().unwrap());
            let h_new = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
            if st_ok && parent == 0 && h_new != 0 {
                self.client_token.insert(h_new, plan.target_token);
            }
        }

        // When the guest frees an OS descriptor, the matching arena drops
        // with it (NVOS00: hObjectOld @8).
        if ret == 0 && plan.ioctl_nr == sys::NV_ESC_RM_FREE && inline_len >= 12 {
            let hroot = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
            let hold = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
            self.pool.drop_osdesc_arena(hold);
            // ... and so does its VRAM charge, together with the charges of
            // everything that hung below it. Deliberately not conditional
            // on NVOS00.status: a charge that is released once too often
            // costs a VM some of its cap, a charge that is never released
            // strangles it for good.
            self.vram.free_object(hroot, hold);
            // ... and a substituted event's registration. Freeing the
            // EVENT object leaves RM's (hClient, fd) triple behind, so the
            // id is given back explicitly; freeing the CLIENT takes every
            // one of its events with it (`rm_client_free_os_events`,
            // osapi.c:539-544), and the books just follow.
            if hold == hroot {
                self.ctl_freed_by_rmfree += self.release_client(hroot) as u64;
            } else if let Some(r) = self.events.remove(&(hroot, hold)) {
                self.free_os_event(r.h_client, r.id);
            }
        }

        // (3a') A freshly created RM client receives the identity of the
        //       guest process that owns it (NVOS64: hClass @12,
        //       hObjectNew @8, status @40).
        //
        //       Why right here and not later: `subProcessID` feeds RM's
        //       decision whether two channels may share a USERD page
        //       (kernel_fifo.c:508-511). Set only after the first channel,
        //       the page is already handed out.
        //
        //       If the call fails, the client stays at 0 -- that is, at the
        //       behavior without this step at all. Hence log loudly, but do
        //       not fail the guest's call.
        if ret == 0 && self.sub_id != 0 && plan.ioctl_nr == sys::NV_ESC_RM_ALLOC && inline_len >= 44
        {
            let hclass = u32::from_le_bytes(self.scratch[12..16].try_into().unwrap());
            let st = u32::from_le_bytes(self.scratch[40..44].try_into().unwrap());
            if hclass == sys::NV01_ROOT_CLIENT as u32 && st == sys::NV_OK {
                let hclient = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
                let name = self.sub_name();
                let (r, s) =
                    self.sys.set_sub_process_id(plan.target_fd, hclient, self.sub_id, &name);
                if r != 0 || s != 0 {
                    eprintln!(
                        "vhost-user-nvrm: SET_SUB_PROCESS_ID({hclient:#x} -> {} \"{name}\") \
                         failed: ret {r} status {s:#x} -- client stays at 0",
                        self.sub_id
                    );
                } else if debug_level() >= 2 {
                    eprintln!(
                        "vhost-user-nvrm: client {hclient:#x} -> subProcessID {} \"{name}\"",
                        self.sub_id
                    );
                }
            }
        }

        // (3b) Grant objects that UVM duplicates later to the same user.
        //      Reasoning and evidence live at share::grant_dup_same_user()
        //      and share::uvm_dupes_class().
        if ret == 0 && plan.ioctl_nr == 0x2b && inline_len >= 48 {
            // NVOS64: hRoot @0, hObjectNew @8, hClass @12, status @40.
            let st = u32::from_le_bytes(self.scratch[40..44].try_into().unwrap());
            let hclass = u32::from_le_bytes(self.scratch[12..16].try_into().unwrap());
            if st == 0 && share::uvm_dupes_class(hclass) {
                let hclient = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                let hobject = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
                let (r, s) = self.sys.grant_dup_same_user(plan.target_fd, hclient, hobject);
                if r != 0 || s != 0 {
                    eprintln!("vhost-user-nvrm: DUP_OBJECT grant for {hobject:#x} \
                               (hClass {hclass:#x}) failed: ret {r} status {s:#x}");
                } else if debug_level() >= 2 {
                    eprintln!("vhost-user-nvrm: DUP_OBJECT granted: {hobject:#x} (hClass {hclass:#x})");
                }
            }
        }

        {
            // A FAILING call is logged always, and that is a deliberate
            // change of default. Every hunt in this repo has come down to
            // "which RM call failed, and with what status", and until now
            // that line sat behind LEA_DEBUG -- so the only way to see a
            // failure was to turn on a firehose that prints per frame and
            // changes the timing of the thing being measured. A non-zero
            // status is rare by construction; if it is not, that IS the
            // finding. LEA_DEBUG=2 still logs EVERY call, for sequence
            // diffs against a direct trace.
            //
            // The env lookup is cached (`debug_level`): this runs on the
            // ioctl path, and `std::env::var` takes a lock every call.
            let verbose = debug_level() >= 2;
            // RM_CONTROL: cmd @ 8, status @ 28 in NVOS54.
            // RM_ALLOC: hClass @ 12 in NVOS64 (status left 0 here).
            let sub = if plan.ioctl_nr == 0x2a && inline_len >= 32 {
                u32::from_le_bytes(self.scratch[8..12].try_into().unwrap())
            } else if plan.ioctl_nr == 0x2b && inline_len >= 16 {
                u32::from_le_bytes(self.scratch[12..16].try_into().unwrap())
            } else { 0 };
            let st = if plan.ioctl_nr == 0x2a && inline_len >= 32 {
                u32::from_le_bytes(self.scratch[28..32].try_into().unwrap())
            } else if plan.ioctl_nr == 0x2b && inline_len >= 44 {
                // NVOS64: status @40 (nvos.h:480-490)
                u32::from_le_bytes(self.scratch[40..44].try_into().unwrap())
            } else if plan.ioctl_nr == 0x27 && inline_len >= 44 {
                // NVOS02: hRoot/hParent/hNew/hClass @0..16, flags @16,
                // pMemory @24, limit @32, status @40 (nvos.h:285-295)
                u32::from_le_bytes(self.scratch[40..44].try_into().unwrap())
            } else if plan.ioctl_nr == 0x4e && inline_len >= 44 {
                // NVOS33: hClient/hDevice/hMemory @0/4/8, offset @16,
                // length @24, pLinearAddress @32, status @40 (nvos.h:1845-1856)
                u32::from_le_bytes(self.scratch[40..44].try_into().unwrap())
            } else if dev_of(plan.dev_tag).is_some_and(|d| d.is_uvm()) {
                // rmStatus offset per UVM command, from the bindgen structs.
                match uvm_status_off(plan.ioctl_nr) {
                    Some(o) if o + 4 <= inline_len =>
                        u32::from_le_bytes(self.scratch[o..o + 4].try_into().unwrap()),
                    _ => 0,
                }
            } else { 0 };
            // A named control's ANSWER, not just its status. `LEA_CTRL_DUMP`
            // takes a comma-separated list of NVOS54.cmd values in hex.
            //
            // Why this exists: OPEN-QUESTIONS 26/35 measured that the
            // EGLImage acquire fails without a single RM call failing, so
            // the remaining class is number 12's -- an answer that looks
            // valid and is wrong. A status column cannot see that; only the
            // bytes can. The three capability controls eglcore issues right
            // before it divides by a zero field (number 32) are the first
            // customers: 0x20801206, 0x2080121b, 0x20801701.
            //
            // First two per session per command: these run once at client
            // init, and a third would only repeat the second.
            if plan.ioctl_nr == 0x2a && st == 0 && ctrl_dump_wanted(sub) {
                let n = *self.ctrl_dumps.entry(sub).or_insert(0);
                if n < 2 {
                    self.ctrl_dumps.insert(sub, n + 1);
                    let shown = self.aux.len().min(96);
                    let hex: String = self.aux[..shown]
                        .chunks(4)
                        .map(|w| {
                            let mut b = [0u8; 4];
                            b[..w.len()].copy_from_slice(w);
                            format!("{:08x} ", u32::from_le_bytes(b))
                        })
                        .collect();
                    eprintln!(
                        "vhost-user-nvrm: CTRL {sub:#x} answer to {} (proc {}): \
                         {} of {} bytes as u32: {hex}",
                        self.proc.comm_str(),
                        self.sub_id,
                        shown,
                        self.aux.len(),
                    );
                }
            }
            if ret != 0 || st != 0 || verbose {
                // Rate-limited, so a client that retries a failing call
                // every frame cannot bury the first one -- which is the
                // only one that says WHEN it started.
                self.fail_logs += 1;
                if verbose || self.fail_logs <= 50 || self.fail_logs % 1000 == 0 {
                    eprintln!(
                        "vhost-user-nvrm: dev {} nr {:#x} cmd {sub:#x} ret {ret} \
                         status {st:#x} (proc {} {}, failure {})",
                        plan.dev_tag,
                        plan.ioctl_nr,
                        self.sub_id,
                        self.proc.comm_str(),
                        self.fail_logs,
                    );
                }
                // A HARD ioctl failure (ret != 0, as opposed to an RM status)
                // does not say which host FD it rode on, and for the one-shot
                // escapes that is the whole question: NV_ESC_ATTACH_GPUS_TO_FD
                // answers EINVAL when the FD already carries GPUs
                // (vendor/.../nv.c, `nvlfp->num_attached_gpus != 0`), so a
                // spurious one means either the guest attached twice on one FD
                // or WE routed a fresh guest FD onto a host FD that had been
                // attached before -- and only the FD number tells those apart.
                // Not rate-limited with the line above: measured 2026-08-20, a
                // desktop session traced at LEA_DEBUG=2 produced 2 of these in
                // 1_392_006 calls, so they cannot bury anything.
                if ret != 0 {
                    eprintln!(
                        "vhost-user-nvrm:   ^ hard ioctl failure rode host fd {} (token {:#x})",
                        plan.target_fd, plan.target_token
                    );
                }
                // The failing attach alone cannot answer number 45: "already
                // attached" is only distinguishable from "RM refused an id" by
                // comparing the failing call's FD against the SUCCEEDING one
                // just before it. Measured 2026-08-20, the two always arrive
                // as a pair -- one `ret 0`, then one `ret -1` -- so the
                // succeeding call has to name its FD too. Every attach, not
                // every call: a desktop session issued 9834 of them beside
                // 710_767 traced lines, so this is a rounding error on the
                // firehose and silent without it.
                if plan.ioctl_nr == nvrm_abi::nvgpu::NV_ESC_ATTACH_GPUS_TO_FD {
                    eprintln!(
                        "vhost-user-nvrm:   attach_gpus ret {ret} host fd {} token {:#x} \
                         (proc {} {})",
                        plan.target_fd,
                        plan.target_token,
                        self.sub_id,
                        self.proc.comm_str(),
                    );
                }
            }
        }

        // The refusal, once everything the ioctl did has been booked.
        //
        // It is a statement about RM's ANSWER (the params buffer is not the
        // structure the command names, so the table cannot be rewritten and
        // forwarding it would leak host PIDs), not about the call: the call
        // ran, and the VRAM settle, the object ledger, the client/token
        // notes and the failure log all describe what it did. They are
        // unconditional by design -- an accounting entry that depends on
        // whether the guest liked the answer is an accounting entry that
        // eventually does not balance. What the GUEST sees is unchanged:
        // the same errno, the same empty payload.
        if let Some((errno, why)) = refusal {
            return self.reply_err(plan.seq, errno, why);
        }

        let out_ret = if ret < 0 { -ioctl_errno } else { ret };

        // (4) Reply: header + the written-back inline + aux. The guest reads
        //     the RM status out of the payload itself.
        let rsp = Rsp {
            seq: plan.seq,
            ret: out_ret,
            inline_len: inline_len as u32,
            aux_len: aux_len as u32,
            ..Rsp::default()
        };
        // Take the buffers out briefly so reply(&self) may borrow them, then
        // hang them back in - that way the allocation is preserved.
        let scratch = std::mem::take(&mut self.scratch);
        let aux = std::mem::take(&mut self.aux);
        let r = self.reply(rsp, &scratch, &aux);
        self.scratch = scratch;
        self.aux = aux;
        r
    }

    // ---- reply helpers ----

    fn reply(&mut self, rsp: Rsp, inline: &[u8], aux: &[u8]) -> Result<()> {
        let out = &mut self.out.bytes;
        out.clear();
        out.reserve(Rsp::WIRE_LEN + inline.len() + aux.len());
        out.extend_from_slice(rsp.as_bytes());
        out.extend_from_slice(inline);
        out.extend_from_slice(aux);
        Ok(())
    }

    /// A guest error as a plausible Rsp. `ret = -errno`, so the guest can
    /// set its own errno from it. The session carries on - a single ioctl
    /// error is not fatal to the transport.
    fn reply_err(&mut self, seq: u32, errno: i32, ctx: &str) -> Result<()> {
        eprintln!("vhost-user-nvrm: guest error seq={seq}: {ctx} (errno {errno})");
        self.reply(Rsp { seq, ret: -errno, ..Rsp::default() }, &[], &[])
    }
}

/// One line per `NV_ESC_RM_ALLOC` / `NV_ESC_RM_FREE` the host RM saw, with
/// the `(client, handle)` pair it books an object under, its class, and RM's
/// own verdict.
///
/// Why the host side needs its own: the guest's handle generator hands out a
/// number the moment IT considers the handle free. When that disagrees with
/// what RM still holds, the alloc comes back `NV_ERR_INSERT_DUPLICATE_NAME`
/// (0x19), and only a ledger on both ends can say which entry was lost and
/// where. `LEA_OBJLOG=1` switches it on.
///
/// WARNING: the environment variable is read ONCE, for the reason
/// `capture()` states -- `std::env::var_os` takes a lock and scans
/// `environ`, which per forwarded ioctl would distort the very latency this
/// backend is measured by.
fn obj_ledger(ioctl_nr: u32, inline: &[u8], ret: i32) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    // Any non-empty value switches it on. NOT `is_some()`: lea_backend_start passes
    // the variable through as `LEA_OBJLOG="${LEA_OBJLOG:-}"`, so an unset
    // variable arrives here SET AND EMPTY.
    if !*ON.get_or_init(|| {
        std::env::var_os("LEA_OBJLOG").is_some_and(|v| !v.is_empty())
    }) {
        return;
    }
    let u32_at = |o: usize| -> u32 {
        inline.get(o..o + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    // NVOS64 (alloc): hRoot@0, hObjectParent@4, hObjectNew@8, hClass@12,
    // status@40. NVOS00 (free): hRoot@0, hObjectParent@4, hObjectOld@8,
    // status@12.
    let (verb, hclass, status) = match ioctl_nr {
        n if n == sys::NV_ESC_RM_ALLOC && inline.len() >= 44 => ("ALLOC", u32_at(12), u32_at(40)),
        n if n == sys::NV_ESC_RM_FREE && inline.len() >= 16 => ("FREE ", 0, u32_at(12)),
        _ => return,
    };
    eprintln!(
        "vhost-user-nvrm: ledger: {verb} client {:#x} parent {:#x} handle {:#x} \
         class {hclass:#x} -> status {status:#x} (ret {ret})",
        u32_at(0),
        u32_at(4),
        u32_at(8),
    );
}

/// Raw capture for the fuzz corpus; off when `LEA_CAPTURE_DIR` is unset.
///
/// Why it sits in the production path at all: a corpus of SYNTHETIC
/// messages fuzzes what the author imagined. The guest sends something
/// else -- and exactly that difference is what a corpus is worth.
///
/// WARNING: the environment variable is read ONCE, not per message.
/// `std::env::var_os` scans `environ` linearly and takes a lock while doing
/// so -- against 12.2 us per forwarded ioctl that would not be a rounding
/// error but a distortion of the very number this box measures. Switched
/// off, the capture therefore costs one comparison against `None`.
///
/// The file name carries a hash of the content, not a counter: the same
/// message twice yields one file, and a run can extend an existing corpus
/// without overwriting it.
fn capture(bytes: &[u8]) {
    use std::sync::OnceLock;
    static DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let Some(dir) = DIR
        .get_or_init(|| std::env::var_os("LEA_CAPTURE_DIR").map(Into::into))
        .as_ref()
    else {
        return;
    };
    let h = nvrm_wire::tables::fnv1a32(bytes);
    let path = dir.join(format!("{:08x}-{}", h, bytes.len()));
    if !path.exists() {
        let _ = std::fs::write(path, bytes);
    }
}

/// The NVOS02 `flags` for the OS-descriptor path, and the one thing the
/// guest's NVOS32 `attr` still has a say in.
///
/// This is not the translation it looks like. The path admits exactly ONE
/// combination, and it is dictated twice over:
///
///   * LOCATION must be PCI and MAPPING must be NO_MAP, or
///     `RmAllocOsDescriptor` (escape.c:206) answers NV_ERR_INVALID_FLAGS
///     before it has looked at anything else.
///   * COHERENCY must be CACHED or WRITE_BACK and PHYSICALITY must not be
///     CONTIGUOUS -- `osCreateMemFromOsDescriptor` refuses everything else
///     for a page array (osmemdesc.c:337-347), with the reason written out
///     above the check: what arrives there is anonymous user memory, it IS
///     write-back cacheable, and RM has no say over where its pages sit.
///
/// So a guest asking WRITE_COMBINE for a scanout buffer cannot be honoured
/// through this route -- not by our choice but because RM refuses the
/// combination. What goes out instead is the truth about these pages: an
/// ordinary write-back mapping of guest RAM in this process. The name of
/// what was asked for comes back so the caller can log it; the substitution
/// is never silent.
///
/// Worth knowing while reading this: our OS02 flags become NVOS32 `attr` in
/// RmAllocOsDescriptor, and osdescConstruct turns that straight back into
/// OS02 flags (os_desc_mem.c:75, "Bug 860684") for the check that judges
/// them. Both ends of that round trip must hold.
fn nvos02_flags_for_osdesc(attr: u32, attr2: u32) -> (u32, Option<&'static str>) {
    use nvrm_abi::nvgpu::{nvos02_flags as f02, nvos32_attr as a32, nvos32_attr2 as a2};

    // CACHED and WRITE_BACK are the two the path takes, and RM folds both
    // into OS32 WRITE_BACK anyway (escape.c:215-219). Anything else is a
    // substitution, and is named as one.
    let lost = match a32::COHERENCY.get(attr) {
        sys::NVOS32_ATTR_COHERENCY_CACHED | sys::NVOS32_ATTR_COHERENCY_WRITE_BACK => None,
        sys::NVOS32_ATTR_COHERENCY_UNCACHED => Some("UNCACHED"),
        sys::NVOS32_ATTR_COHERENCY_WRITE_COMBINE => Some("WRITE_COMBINE"),
        sys::NVOS32_ATTR_COHERENCY_WRITE_THROUGH => Some("WRITE_THROUGH"),
        sys::NVOS32_ATTR_COHERENCY_WRITE_PROTECT => Some("WRITE_PROTECT"),
        _ => Some("(unknown)"),
    };
    // The one field the guest still decides: whether the GPU may cache it.
    // RmAllocOsDescriptor passes it through untouched (escape.c:232-235).
    let gpu_cacheable = if a2::GPU_CACHEABLE.get(attr2) == sys::NVOS32_ATTR2_GPU_CACHEABLE_YES {
        sys::NVOS02_FLAGS_GPU_CACHEABLE_YES
    } else {
        sys::NVOS02_FLAGS_GPU_CACHEABLE_NO
    };

    let flags = f02::PHYSICALITY.set(sys::NVOS02_FLAGS_PHYSICALITY_NONCONTIGUOUS)
        | f02::LOCATION.set(sys::NVOS02_FLAGS_LOCATION_PCI)
        | f02::COHERENCY.set(sys::NVOS02_FLAGS_COHERENCY_WRITE_BACK)
        | f02::GPU_CACHEABLE.set(gpu_cacheable)
        | f02::MAPPING.set(sys::NVOS02_FLAGS_MAPPING_NO_MAP);
    (flags, lost)
}

/// The wire's `DevTag` as xlate's `Dev`; `None` for a tag this backend
/// does not know (refused by the callers, never guessed).
fn dev_of(tag: u32) -> Option<Dev> {
    Some(match DevTag::from_u32(tag)? {
        DevTag::Ctl => Dev::Ctl,
        DevTag::Gpu => Dev::Gpu,
        DevTag::Uvm => Dev::Uvm,
        DevTag::UvmTools => Dev::UvmTools,
    })
}

// ===========================================================================
// Tests: the guest-lies cases, without a GPU
// ===========================================================================
// Made possible by the seam from OPEN-QUESTIONS nr 5. Every test checks TWO
// things: that the session refuses, AND that no ioctl was issued while doing
// so. Checking only the error response would not suffice -- it would look
// exactly the same if the driver had been the one to refuse.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscalls::{FakeCall, FakeSyscalls};
    use std::sync::Arc;

    /// A session with a ledger instead of a driver, plus a token pointing
    /// at a harmless FD (a memfd -- an ioctl on it would give ENOTTY, but
    /// the fake never lets it get that far).
    fn session() -> (Session, Arc<FakeSyscalls>, u64) {
        let fake = Arc::new(FakeSyscalls::default());
        let mut s = Session::detached_proc(7, crate::vram::Ledger::off()).unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        assert!(fd >= 0);
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let token = s.mirror.insert(owned);
        (s, fake, token)
    }

    /// Build a message: Req header plus inline plus aux.
    fn msg(req: &Req, inline: &[u8], aux: &[u8]) -> Vec<u8> {
        let mut v = req.as_bytes().to_vec();
        v.extend_from_slice(inline);
        v.extend_from_slice(aux);
        v
    }

    /// The guest closed one of its FDs (KIND_CLOSE names it by token).
    fn close_req(token: u64) -> Req {
        Req {
            seq: 1,
            kind: Kind::Close as u32,
            dev_tag: DevTag::Ctl as u32,
            target_token: token,
            ..Req::default()
        }
    }

    fn ioctl_req(token: u64, nr: u32, inline_len: usize, aux_len: usize) -> Req {
        Req {
            seq: 1,
            kind: Kind::Ioctl as u32,
            dev_tag: DevTag::Ctl as u32,
            ioctl_nr: nr,
            target_token: token,
            inline_len: inline_len as u32,
            aux_len: aux_len as u32,
            ..Req::default()
        }
    }

    /// The errno out of a reply (Rsp.ret == -errno), or None on success.
    fn errno_of(reply: &Reply) -> Option<i32> {
        let rsp = Rsp::from_bytes(&reply.bytes).expect("reply shorter than Rsp");
        (rsp.ret < 0).then(|| -rsp.ret)
    }

    // ---- framing ----------------------------------------------------------

    #[test]
    fn short_message_is_a_transport_error_not_a_reply() {
        let (mut s, fake, _) = session();
        assert!(s.handle_msg(&[0u8; Req::WIRE_LEN - 1]).is_err());
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn unknown_kind_gets_eproto_and_no_syscall() {
        let (mut s, fake, _) = session();
        // Exactly the device-only kinds this session must never see.
        for kind in [proto::KIND_GET_TABLES, proto::KIND_MAP_RELEASE, proto::KIND_PROC_GONE, 99] {
            let req = Req { seq: 1, kind, ..Req::default() };
            let r = s.handle_msg(&msg(&req, &[], &[])).unwrap();
            assert_eq!(errno_of(&r), Some(libc::EPROTO), "Kind {kind}");
        }
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn hello_checks_the_protocol_version() {
        let (mut s, _, _) = session();
        let bad = Req { seq: 1, kind: Kind::Hello as u32,
                        ioctl_nr: proto::PROTO_VERSION + 1, ..Req::default() };
        assert_eq!(errno_of(&s.handle_msg(&msg(&bad, &[], &[])).unwrap()), Some(libc::EPROTO));
        let good = Req { ioctl_nr: proto::PROTO_VERSION, ..bad };
        assert_eq!(errno_of(&s.handle_msg(&msg(&good, &[], &[])).unwrap()), None);
    }

    // ---- the length lies --------------------------------------------------

    #[test]
    fn payload_shorter_than_announced_is_rejected() {
        let (mut s, fake, tok) = session();
        // inline_len claims 32, only 16 bytes sent.
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        let r = s.handle_msg(&msg(&req, &[0u8; 16], &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));

        // The same via the aux part.
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 4096);
        let r = s.handle_msg(&msg(&req, &[0u8; 32], &[0u8; 8])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));

        assert_eq!(fake.ioctl_count(), 0, "no ioctl on a too-short message");
    }

    /// The cap must fire even when the guest ACTUALLY sends the bytes --
    /// otherwise the test only checks the length consistency in front of it
    /// and the cap could be removed unnoticed.
    #[test]
    fn oversized_inline_and_aux_are_rejected() {
        let (mut s, fake, tok) = session();
        let n = proto::MAX_PAYLOAD + 1;
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, n, 0);
        let r = s.handle_msg(&msg(&req, &vec![0u8; n], &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL), "inline over MAX_PAYLOAD");

        let a = proto::MAX_AUX + 1;
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, a);
        let r = s.handle_msg(&msg(&req, &[0u8; 32], &vec![0u8; a])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL), "aux over MAX_AUX");

        assert_eq!(fake.ioctl_count(), 0);
    }

    /// WARNING: the lie the comment in `prepare` warns about: `aux_len` is
    /// chosen by the guest, but the length RM dereferences the pointer with
    /// sits in the inline struct (NVOS54.paramsSize). Believing aux_len
    /// alone gets the daemon heap read past its end.
    #[test]
    fn lying_params_size_is_caught() {
        let (mut s, fake, tok) = session();
        let mut inline = vec![0u8; 32];
        inline[8..12].copy_from_slice(&0x2080_0110u32.to_le_bytes()); // some cmd
        inline[16..24].copy_from_slice(&0xdead_beefu64.to_le_bytes()); // params != 0
        inline[24..28].copy_from_slice(&4096u32.to_le_bytes()); // paramsSize LIES
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 16);
        req.embedded_ptr_off = 16;
        let r = s.handle_msg(&msg(&req, &inline, &[0u8; 16])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL), "4096 expected, 16 sent");
        assert_eq!(fake.ioctl_count(), 0);
    }

    /// An `embedded_ptr_off` that does not point where the driver expects
    /// the pointer is rejected.
    ///
    /// WARNING: honesty note: the bounds check `off + 8 > inline_len` is
    /// unreachable today -- `xlate::embedded_ptr` returns `ptr_off == 16`
    /// for both commands that carry a pointer, and the comparison
    /// `e.ptr_off != off` catches any other value first. Remove the bounds
    /// check and this test stays green. It stays regardless: it protects
    /// the write `scratch[off..off+8]` at the end of the block the moment a
    /// command with a different ptr_off is added.
    #[test]
    fn embedded_ptr_off_outside_inline_is_rejected() {
        let (mut s, fake, tok) = session();
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        req.embedded_ptr_off = 28; // 28+8 > 32, and != ptr_off 16
        let r = s.handle_msg(&msg(&req, &[0u8; 32], &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn fd_field_off_outside_inline_is_rejected() {
        let (mut s, fake, tok) = session();
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        req.fd_field_off = 30; // 30+4 > 32
        let r = s.handle_msg(&msg(&req, &[0u8; 32], &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));

        // And an fd field pointing at an unknown token: EBADF.
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        req.fd_field_off = 0;
        req.fd_field_token = 0xdead;
        let r = s.handle_msg(&msg(&req, &[0u8; 32], &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EBADF));
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn aux_fd_field_off_outside_aux_is_rejected() {
        let (mut s, fake, tok) = session();
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 8);
        req.aux_fd_field_off = 4; // 4+8 > 8
        let r = s.handle_msg(&msg(&req, &[0u8; 32], &[0u8; 8])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn nested_count_mismatch_is_rejected() {
        let (mut s, fake, tok) = session();
        // Over MAX_NESTED.
        //
        // WARNING: honesty note: this bound is unreachable today -- remove
        // it and the cross-check against `xlate` (below) catches the same
        // case, because no command has more than MAX_NESTED pointers. What
        // it protects is the index access `req.nested[i]` into a
        // fixed-size array, the moment the xlate table ever gains a fifth
        // pointer. The invariant behind that is tested:
        // `nested_ptrs_stay_within_max_nested` in nvrm-abi::xlate.
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 64);
        req.nested_count = proto::MAX_NESTED as u32 + 1;
        let r = s.handle_msg(&msg(&req, &[0u8; 32], &[0u8; 64])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));

        // Matching the number, but the command has no second-level pointers
        // at all -- that is 0 annotated against 1 claimed.
        let mut inline = vec![0u8; 32];
        inline[8..12].copy_from_slice(&0x2080_0110u32.to_le_bytes());
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 64);
        req.nested_count = 1;
        let r = s.handle_msg(&msg(&req, &inline, &[0u8; 64])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn xfer_repack_is_refused_loudly() {
        let (mut s, fake, tok) = session();
        // inline_len > 14 bits: no longer fits into the _IOC encoding.
        let n = (1 << 14) as usize;
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, n, 0);
        let r = s.handle_msg(&msg(&req, &vec![0u8; n], &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EMSGSIZE), "refuse loudly rather than guess");
        assert_eq!(fake.ioctl_count(), 0);
    }

    // ---- deny list and routing --------------------------------------------

    /// The denial must fire BEFORE the token lookup -- otherwise a denied
    /// command would escape simply by the guest sending a nonsense token.
    #[test]
    fn blocked_controls_are_refused_before_the_token_lookup() {
        let (mut s, fake, _) = session();
        for &cmd in nvrm_abi::xlate::blocked_ctrls() {
            let mut inline = vec![0u8; 32];
            inline[8..12].copy_from_slice(&cmd.to_le_bytes());
            // Deliberately a token that does NOT exist.
            let req = ioctl_req(0xdead_beef, sys::NV_ESC_RM_CONTROL, 32, 0);
            let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
            assert_eq!(errno_of(&r), Some(libc::EPERM),
                       "{cmd:#x} must give EPERM, not EBADF");
        }
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn unknown_token_gets_ebadf() {
        let (mut s, fake, _) = session();
        let req = ioctl_req(0xdead_beef, sys::NV_ESC_RM_CONTROL, 32, 0);
        let r = s.handle_msg(&msg(&req, &[0u8; 32], &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EBADF));
        assert_eq!(fake.ioctl_count(), 0);
    }

    // ---- the flags of the rewritten OS-descriptor alloc --------------------

    /// The one that matters: zero flags -- what this path sent until now --
    /// are refused twice before RM has looked at a single page, because
    /// MAPPING_DEFAULT is not NO_MAP (escape.c:207) and COHERENCY_UNCACHED
    /// is not what anonymous user memory is (osmemdesc.c:344).
    #[test]
    fn osdesc_flags_are_the_one_combination_the_path_takes() {
        use nvrm_abi::nvgpu::nvos02_flags as f02;
        for attr in [0u32, 0xffff_ffff, 0x4200_0000] {
            let (flags, _) = nvos02_flags_for_osdesc(attr, 0);
            assert_ne!(flags, 0, "attr {attr:#x}");
            assert_eq!(f02::MAPPING.get(flags), sys::NVOS02_FLAGS_MAPPING_NO_MAP);
            assert_eq!(f02::LOCATION.get(flags), sys::NVOS02_FLAGS_LOCATION_PCI);
            assert_eq!(f02::COHERENCY.get(flags), sys::NVOS02_FLAGS_COHERENCY_WRITE_BACK);
            assert_eq!(
                f02::PHYSICALITY.get(flags),
                sys::NVOS02_FLAGS_PHYSICALITY_NONCONTIGUOUS,
                "the page-array path refuses CONTIGUOUS outright (osmemdesc.c:346)"
            );
        }
    }

    /// What the guest asked for is answered, not swallowed: the two
    /// cacheabilities the path carries pass silently, every other one comes
    /// back by name so the substitution reaches the log.
    #[test]
    fn osdesc_flags_name_the_coherency_that_was_substituted() {
        use nvrm_abi::nvgpu::nvos32_attr as a32;
        for asked in [sys::NVOS32_ATTR_COHERENCY_CACHED, sys::NVOS32_ATTR_COHERENCY_WRITE_BACK] {
            let (_, lost) = nvos02_flags_for_osdesc(a32::COHERENCY.set(asked), 0);
            assert_eq!(lost, None, "asked {asked} is carried as asked");
        }
        for asked in [
            sys::NVOS32_ATTR_COHERENCY_UNCACHED,
            sys::NVOS32_ATTR_COHERENCY_WRITE_COMBINE,
            sys::NVOS32_ATTR_COHERENCY_WRITE_THROUGH,
            sys::NVOS32_ATTR_COHERENCY_WRITE_PROTECT,
        ] {
            let (_, lost) = nvos02_flags_for_osdesc(a32::COHERENCY.set(asked), 0);
            assert!(lost.is_some(), "asked {asked} must be reported as substituted");
        }
    }

    /// And the guest's claim about physicality changes nothing -- what RM
    /// will describe are pages locked out of a user address space, and that
    /// is never one physical range, whatever the caller wrote.
    #[test]
    fn osdesc_physicality_ignores_what_the_guest_claims() {
        use nvrm_abi::nvgpu::{nvos02_flags as f02, nvos32_attr as a32};
        let claims_contiguous = a32::PHYSICALITY.set(sys::NVOS32_ATTR_PHYSICALITY_CONTIGUOUS);
        let (flags, _) = nvos02_flags_for_osdesc(claims_contiguous, 0);
        assert_eq!(
            f02::PHYSICALITY.get(flags),
            sys::NVOS02_FLAGS_PHYSICALITY_NONCONTIGUOUS
        );
    }

    #[test]
    fn osdesc_gpu_cacheable_follows_attr2() {
        use nvrm_abi::nvgpu::{nvos02_flags as f02, nvos32_attr2 as a2};
        let (yes, _) =
            nvos02_flags_for_osdesc(0, a2::GPU_CACHEABLE.set(sys::NVOS32_ATTR2_GPU_CACHEABLE_YES));
        assert_eq!(f02::GPU_CACHEABLE.get(yes), sys::NVOS02_FLAGS_GPU_CACHEABLE_YES);
        let (no, _) =
            nvos02_flags_for_osdesc(0, a2::GPU_CACHEABLE.set(sys::NVOS32_ATTR2_GPU_CACHEABLE_NO));
        assert_eq!(f02::GPU_CACHEABLE.get(no), sys::NVOS02_FLAGS_GPU_CACHEABLE_NO);
    }

    #[test]
    fn gpa_runs_only_on_the_osdesc_alloc() {
        let (mut s, fake, tok) = session();
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 16);
        req.gpa_run_count = 1;
        let r = s.handle_msg(&msg(&req, &[0u8; 32], &[0u8; 16])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL), "GPA runs only for 0x71");
        assert_eq!(fake.ioctl_count(), 0);
    }

    // ---- what passes, and how it would reach the driver --------------------

    /// The counter-proof to everything above: a clean call goes through,
    /// with exactly the buffer the guest sent, and the _IOC number that
    /// matches the length.
    #[test]
    fn a_clean_call_reaches_the_driver_unchanged() {
        let (mut s, fake, tok) = session();
        let mut inline = vec![0u8; 32];
        inline[8..12].copy_from_slice(&0x2080_0110u32.to_le_bytes());
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&r), None, "a clean call must not be refused");
        assert_eq!(fake.ioctl_count(), 1);
        match &fake.calls()[0] {
            FakeCall::Ioctl { request, inline: got, .. } => {
                assert_eq!(*request as u32, nvrm_abi::iowr_raw(sys::NV_ESC_RM_CONTROL, 32));
                assert_eq!(&got[..32], &inline[..]);
            }
            other => panic!("expected an ioctl, got {other:?}"),
        }
    }

    /// The fd field is translated: the guest sends its token, the driver
    /// sees the HOST FD number. If that is lost, RM looks the event up by a
    /// guest-local number and silently finds nothing.
    #[test]
    fn fd_fields_are_translated_to_host_numbers() {
        let (mut s, fake, tok) = session();
        let host_fd = s.mirror.raw(tok).unwrap();
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        req.fd_field_off = 0;
        req.fd_field_token = tok;
        let mut inline = vec![0u8; 32];
        inline[0..4].copy_from_slice(&0x4141_4141u32.to_le_bytes()); // guest garbage
        inline[8..12].copy_from_slice(&0x2080_0110u32.to_le_bytes());
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&r), None);
        let got = fake.last_inline().unwrap();
        assert_eq!(i32::from_le_bytes(got[0..4].try_into().unwrap()), host_fd);
    }

    #[test]
    /// The guest's UVM_INITIALIZE flags cross UNCHANGED. The multi-process
    /// flag used to be forced here; it cost pageable memory access and with
    /// it the Vulkan raytracing device (OPEN-QUESTIONS nr 11), and its
    /// reason -- a foreign process mmapping this daemon's UVM fd -- is gone.
    fn uvm_initialize_flags_pass_through() {
        let (mut s, fake, tok) = session();
        let mut req = ioctl_req(tok, UVM_INITIALIZE, 16, 0);
        req.dev_tag = DevTag::Uvm as u32;
        let r = s.handle_msg(&msg(&req, &[0u8; 16], &[])).unwrap();
        assert_eq!(errno_of(&r), None);
        let got = fake.last_inline().unwrap();
        // The ledger records what the caller had, not a fixed 256: a UVM
        // request carries no size in its number, and reading past the
        // 16 bytes of UVM_INITIALIZE_PARAMS reads the uninitialized tail of
        // the scratch Vec.
        assert_eq!(got.len(), 16, "the ledger read past the guest's payload");
        let flags = u64::from_le_bytes(got[0..8].try_into().unwrap());
        assert_eq!(flags & UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE, 0,
                   "the multi-process flag must not be forced onto the guest's UVM init");
        // UVM crosses the boundary as a RAW number, not _IOC-encoded.
        match &fake.calls()[0] {
            FakeCall::Ioctl { request, .. } => assert_eq!(*request as u32, UVM_INITIALIZE),
            other => panic!("{other:?}"),
        }
    }

    /// The GPU index at open time is a guest word. Without a bound the host
    /// pastes it into a file name and tries to open that.
    #[test]
    fn open_rejects_a_gpu_index_beyond_the_driver_limit() {
        let (mut s, fake, _) = session();
        for nr in [32u32, 1_048_577, u32::MAX] {
            let req = Req { seq: 1, kind: Kind::Open as u32, dev_tag: DevTag::Gpu as u32,
                            ioctl_nr: nr, ..Req::default() };
            let r = s.handle_msg(&msg(&req, &[], &[])).unwrap();
            assert_eq!(errno_of(&r), Some(libc::EINVAL), "GPU index {nr} accepted");
        }
        assert_eq!(fake.ioctl_count(), 0);
    }

    /// The fuzz corpus as a regression probe -- no nightly, no libFuzzer,
    /// in every `cargo test`.
    ///
    /// The files in `fuzz/corpus/handle_msg` were captured from a real gate
    /// run (9287 messages, reduced by `cargo fuzz cmin` to
    /// coverage-equivalent representatives) and extended with what the
    /// fuzzer found on its own. Running them here costs milliseconds and
    /// catches exactly the regression a restructuring makes likely: a
    /// message that used to pass now panics.
    ///
    /// The minimised corpus IS in git (126 representatives, committed
    /// 2026-08-18); a larger one is captured on a rig
    /// with `LEA_CAPTURE_DIR` and minimised with `cargo fuzz cmin`. Without
    /// it this test has nothing to replay and says so on stderr instead of
    /// passing in silence -- it is a bonus on a rig, not a gate in a clean
    /// checkout.
    #[test]
    fn the_fuzz_corpus_still_goes_through() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fuzz/corpus/handle_msg");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("fuzz corpus: {} absent -- nothing replayed (not a failure)", dir.display());
            return;
        };
        let mut n = 0;
        for e in entries.flatten() {
            let Ok(bytes) = std::fs::read(e.path()) else { continue };
            let (mut s, _fake, _tok) = session();
            // A second token, as the fuzz target provides: otherwise the
            // translation paths end at EBADF already.
            let fd = unsafe { libc::memfd_create(c"leandro-corpus".as_ptr(), 0) };
            if fd >= 0 {
                s.mirror.insert(unsafe {
                    <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
                });
            }
            // A panic = test failure. Err is allowed (transport error).
            let _ = s.handle_msg(&bytes);
            n += 1;
        }
        assert!(n > 0, "corpus directory {} is empty", dir.display());
        eprintln!("fuzz corpus: {n} messages replayed");
    }

    /// A fresh RM client receives the guest process's identity -- and does
    /// so IMMEDIATELY, because RM hangs USERD separation off subProcessID
    /// (kernel_fifo.c:508-511).
    #[test]
    fn a_fresh_root_client_gets_its_sub_process_id() {
        let fake = Arc::new(FakeSyscalls {
            // NVOS64: hObjectNew @8, status @40. Fake success.
            writes_back: vec![(8, 0xabcd_1234u32.to_le_bytes().to_vec()),
                              (40, sys::NV_OK.to_le_bytes().to_vec())],
            ..Default::default()
        });
        let mut s = Session::detached_proc(7, crate::vram::Ledger::off()).unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned);

        let mut inline = vec![0u8; 48];
        inline[12..16].copy_from_slice(&(sys::NV01_ROOT_CLIENT as u32).to_le_bytes());
        let req = ioctl_req(tok, sys::NV_ESC_RM_ALLOC, 48, 0);
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&r), None);

        let set = fake.calls().into_iter().find_map(|c| match c {
            FakeCall::SetSubProcessId { hclient, sub_id, name, .. } => Some((hclient, sub_id, name)),
            _ => None,
        });
        assert_eq!(set, Some((0xabcd_1234, 7, "guest-7".to_string())));
    }

    // ---- the cut itself (the prepare/execute seam, OPEN-QUESTIONS nr 5) ----

    /// `prepare` returns a DECISION, not a finished answer: a refusal is
    /// directly inspectable as a Refusal (errno and reason), without
    /// parsing the response bytes -- that is what the cut buys over going
    /// through handle_msg.
    #[test]
    fn prepare_hands_back_the_refusal_itself() {
        let (mut s, fake, tok) = session();
        let cmd = nvrm_abi::xlate::blocked_ctrls()[0];
        let mut inline = vec![0u8; 32];
        inline[8..12].copy_from_slice(&cmd.to_le_bytes());
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        let payload = inline.clone();
        let refusal = s.prepare(&req, &payload).err().expect("a denied control must refuse");
        assert_eq!(refusal.errno, libc::EPERM);
        assert!(refusal.why.contains("never forwarded"), "{}", refusal.why);
        assert_eq!(fake.ioctl_count(), 0, "prepare issues no syscall");
    }

    /// A clean call yields a forward plan with a finished _IOC request
    /// number -- inspectable BEFORE anything is executed. (That execute
    /// then issues the plan unchanged is covered by the counter-proofs
    /// above, which go through handle_msg.)
    #[test]
    fn prepare_builds_a_forward_plan_with_the_ioc_request() {
        let (mut s, fake, tok) = session();
        let inline = vec![0u8; 32];
        let req = ioctl_req(tok, 0x2a, 32, 0);
        let plan = s.prepare(&req, &inline).ok().expect("a clean call must pass");
        assert!(matches!(plan.action, Action::Forward));
        assert_eq!(plan.request, iowr_raw(0x2a, 32) as libc::c_ulong);
        assert_eq!((plan.inline_len, plan.aux_len), (32, 0));
        assert_eq!(fake.ioctl_count(), 0, "prepare itself executes nothing");
    }

    // ---- the VRAM cap ------------------------------------------------------

    /// A session on a ledger the test keeps a handle to, so it can read
    /// the VM's counter from outside.
    fn capped_session(limit: u64) -> (Session, Arc<FakeSyscalls>, u64, Arc<crate::vram::Ledger>) {
        let led = crate::vram::Ledger::for_test(limit);
        let fake = Arc::new(FakeSyscalls::default());
        let mut s = Session::detached_proc(7, led.clone()).unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-vram-test".as_ptr(), 0) };
        assert!(fd >= 0);
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let token = s.mirror.insert(owned);
        (s, fake, token, led)
    }

    /// One RM_ALLOC of `bytes` VRAM: NVOS64 inline (hClass @12,
    /// pAllocParms @16) plus the 128-byte NV_MEMORY_ALLOCATION_PARAMS the
    /// churn run was measured sending (flags 0x1c101, attr 0x18000000).
    fn vram_alloc_msg(tok: u64, handle: u32, bytes: u64) -> Vec<u8> {
        let mut inline = vec![0u8; 48];
        inline[0..4].copy_from_slice(&0xc1d8_3a38u32.to_le_bytes()); // hRoot
        inline[4..8].copy_from_slice(&0x5c00_0002u32.to_le_bytes()); // hObjectParent
        inline[8..12].copy_from_slice(&handle.to_le_bytes()); // hObjectNew
        inline[12..16].copy_from_slice(&0x40u32.to_le_bytes()); // NV01_MEMORY_LOCAL_USER
        // pAllocParms: the guest's own VA. Non-zero is what marks the call
        // as carrying params at all (xlate::embedded_ptr); the host
        // overwrites it with the address of its aux buffer.
        inline[16..24].copy_from_slice(&0x7f00_0000_0000u64.to_le_bytes());
        let mut aux = vec![0u8; 128];
        aux[8..12].copy_from_slice(&0x1c101u32.to_le_bytes()); // flags
        aux[24..28].copy_from_slice(&0x1800_0000u32.to_le_bytes()); // attr, LOCATION_VIDMEM
        aux[64..72].copy_from_slice(&bytes.to_le_bytes()); // size
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_ALLOC, 48, 128);
        req.embedded_ptr_off = 16;
        msg(&req, &inline, &aux)
    }

    fn free_msg(tok: u64, handle: u32) -> Vec<u8> {
        let mut inline = vec![0u8; 16];
        inline[0..4].copy_from_slice(&0xc1d8_3a38u32.to_le_bytes());
        inline[4..8].copy_from_slice(&0x5c00_0002u32.to_le_bytes());
        inline[8..12].copy_from_slice(&handle.to_le_bytes());
        let req = ioctl_req(tok, sys::NV_ESC_RM_FREE, 16, 0);
        msg(&req, &inline, &[])
    }

    /// NVOS64.status out of a reply's inline payload.
    fn rm_status(reply: &Reply) -> u32 {
        let off = Rsp::WIRE_LEN + 40;
        u32::from_le_bytes(reply.bytes[off..off + 4].try_into().unwrap())
    }

    /// The whole point: over the cap, the guest sees the answer a full
    /// card gives -- ioctl 0 plus NV_ERR_NO_MEMORY -- and the driver is
    /// never asked. An errno here would reach torch as an
    /// AcceleratorError, i.e. a crash instead of an OOM.
    #[test]
    fn over_the_cap_the_guest_gets_the_answer_a_full_card_gives() {
        let (mut s, fake, tok, led) = capped_session(8 << 20);

        let r = s.handle_msg(&vram_alloc_msg(tok, 0xaa, 4 << 20)).unwrap();
        assert_eq!(rm_status(&r), sys::NV_OK);
        assert_eq!(led.used(), 4 << 20);
        assert_eq!(fake.ioctl_count(), 1);

        let r = s.handle_msg(&vram_alloc_msg(tok, 0xbb, 4 << 20)).unwrap();
        assert_eq!(rm_status(&r), sys::NV_OK);
        assert_eq!(led.used(), 8 << 20);
        assert_eq!(fake.ioctl_count(), 2);

        // Full. The third one must be refused -- without an ioctl.
        let r = s.handle_msg(&vram_alloc_msg(tok, 0xcc, 4 << 20)).unwrap();
        let rsp = Rsp::from_bytes(&r.bytes).unwrap();
        assert_eq!(rsp.ret, 0, "the ioctl itself succeeds, as it does natively");
        assert_eq!(rm_status(&r), sys::NV_ERR_NO_MEMORY);
        assert_eq!(fake.ioctl_count(), 2, "a refused allocation reaches no driver");
        assert_eq!(led.used(), 8 << 20, "a refusal charges nothing");

        // Free one, and there is room again.
        s.handle_msg(&free_msg(tok, 0xaa)).unwrap();
        assert_eq!(led.used(), 4 << 20);
        let r = s.handle_msg(&vram_alloc_msg(tok, 0xdd, 4 << 20)).unwrap();
        assert_eq!(rm_status(&r), sys::NV_OK);
        assert_eq!(led.used(), 8 << 20);
    }

    /// The counter-check that carries the whole default: with no cap set,
    /// nothing is refused, however much is asked for.
    ///
    /// The books DO count -- that changed when the process list became a
    /// second consumer of the same numbers. What must not change is that
    /// no allocation is turned away.
    #[test]
    fn with_the_cap_off_nothing_is_refused() {
        let (mut s, fake, tok) = session();
        for h in 0..8u32 {
            let r = s.handle_msg(&vram_alloc_msg(tok, h, 8 << 30)).unwrap();
            assert_eq!(rm_status(&r), sys::NV_OK);
        }
        assert_eq!(fake.ioctl_count(), 8, "every allocation is forwarded");
        assert_eq!(s.vram.owed(), 8 * (8u64 << 30), "and every one is counted");
    }

    /// The identity is adopted BEFORE the device is opened, and that is
    /// what makes it testable without one: a GPU index past the driver
    /// limit is refused, but the process has already announced itself.
    ///
    /// Without the announce there is no row, and the VM's process list
    /// stays empty however much the guest allocates -- the failure mode is
    /// silent, which is why it is pinned here rather than left to the gate.
    #[test]
    fn opening_announces_the_guest_process_before_touching_a_device() {
        let (mut s, fake, _tok, _led) = capped_session(64 << 20);
        let mut info = proto::ProcInfo { pid: 4711, _pad: 0, comm: [0; 16] };
        info.comm[..6].copy_from_slice(b"python");
        let req = Req {
            seq: 1,
            kind: Kind::Open as u32,
            dev_tag: DevTag::Gpu as u32,
            ioctl_nr: 9999, // past NV_MAX_DEVICES -- the open is refused
            inline_len: proto::ProcInfo::WIRE_LEN as u32,
            ..Req::default()
        };
        let r = s.handle_msg(&msg(&req, info.as_bytes(), &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL), "the open itself is refused");
        assert_eq!(fake.ioctl_count(), 0);

        let roster = s.vram.roster();
        assert_eq!(roster.len(), 1, "and the process is on the list regardless");
        assert_eq!(roster[0].guest_pid, 4711);
    }

    /// Closing the FD frees the clients on it -- no RM_FREE crosses the
    /// boundary for those, so the release has to hang off the close.
    #[test]
    fn closing_the_fd_returns_the_charges() {
        let (mut s, _fake, tok, led) = capped_session(64 << 20);
        for h in 0..4u32 {
            s.handle_msg(&vram_alloc_msg(tok, h, 4 << 20)).unwrap();
        }
        assert_eq!(led.used(), 16 << 20);

        let req = Req { seq: 1, kind: Kind::Close as u32, target_token: tok, ..Req::default() };
        s.handle_msg(&msg(&req, &[], &[])).unwrap();
        assert_eq!(led.used(), 0);
    }

    /// The path no message announces: the guest process dies, the device
    /// drops the session. Whatever it still owed goes back, or the VM's
    /// cap shrinks with every process that ever ran.
    #[test]
    fn a_dropped_session_returns_what_the_guest_never_freed() {
        let led = crate::vram::Ledger::for_test(64 << 20);
        {
            let fake = Arc::new(FakeSyscalls::default());
            let mut s = Session::detached_proc(7, led.clone()).unwrap();
            s.sys = Box::new(fake);
            let fd = unsafe { libc::memfd_create(c"leandro-vram-drop".as_ptr(), 0) };
            let owned =
                unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
            let tok = s.mirror.insert(owned);
            for h in 0..4u32 {
                s.handle_msg(&vram_alloc_msg(tok, h, 4 << 20)).unwrap();
            }
            assert_eq!(led.used(), 16 << 20);
        }
        assert_eq!(led.used(), 0, "the session's debt dies with it");
    }

    /// An allocation RM itself refuses must not stay charged -- otherwise
    /// a VM that runs into the real card's limit would afterwards be
    /// throttled by its own cap as well.
    #[test]
    fn an_allocation_rm_refuses_stays_uncharged() {
        let (mut s, _fake, tok, led) = capped_session(64 << 20);
        // The fake writes NV_ERR_NO_MEMORY into NVOS64.status, as a full
        // card does (measured: 0x51 at offset 40).
        let refusing = Arc::new(FakeSyscalls {
            writes_back: vec![(40, sys::NV_ERR_NO_MEMORY.to_le_bytes().to_vec())],
            ..Default::default()
        });
        s.sys = Box::new(refusing);
        let r = s.handle_msg(&vram_alloc_msg(tok, 0xaa, 4 << 20)).unwrap();
        assert_eq!(rm_status(&r), sys::NV_ERR_NO_MEMORY);
        assert_eq!(led.used(), 0);
    }

    /// The short alloc form. NVOS21 is 32 bytes and puts `status` at 28,
    /// not at 40 -- reading 40 out of a 32-byte buffer is a panic the
    /// guest can trigger at will, and skipping the form altogether would
    /// be a way around the cap. Neither: the cap holds and the daemon
    /// stands.
    #[test]
    fn the_short_alloc_form_is_capped_at_its_own_status_offset() {
        let (mut s, fake, tok, led) = capped_session(4 << 20);

        // NVOS21: hRoot/hParent/hNew @0/4/8, hClass @12, pAllocParms @16,
        // paramsSize @24, status @28.
        let short = |handle: u32, bytes: u64| {
            let mut inline = vec![0u8; 32];
            inline[0..4].copy_from_slice(&0xc1d8_3a38u32.to_le_bytes());
            inline[4..8].copy_from_slice(&0x5c00_0002u32.to_le_bytes());
            inline[8..12].copy_from_slice(&handle.to_le_bytes());
            inline[12..16].copy_from_slice(&0x40u32.to_le_bytes());
            inline[16..24].copy_from_slice(&0x7f00_0000_0000u64.to_le_bytes());
            let mut aux = vec![0u8; 128];
            aux[8..12].copy_from_slice(&0x1c101u32.to_le_bytes());
            aux[24..28].copy_from_slice(&0x1800_0000u32.to_le_bytes());
            aux[64..72].copy_from_slice(&bytes.to_le_bytes());
            let mut req = ioctl_req(tok, sys::NV_ESC_RM_ALLOC, 32, 128);
            req.embedded_ptr_off = 16;
            msg(&req, &inline, &aux)
        };
        let status28 = |r: &Reply| {
            let o = Rsp::WIRE_LEN + 28;
            u32::from_le_bytes(r.bytes[o..o + 4].try_into().unwrap())
        };

        let r = s.handle_msg(&short(0xaa, 4 << 20)).unwrap();
        assert_eq!(status28(&r), sys::NV_OK);
        assert_eq!(led.used(), 4 << 20);

        let r = s.handle_msg(&short(0xbb, 1 << 20)).unwrap();
        assert_eq!(status28(&r), sys::NV_ERR_NO_MEMORY, "the short form is capped too");
        assert_eq!(fake.ioctl_count(), 1, "and refused without an ioctl");
        // The 48-byte status field must NOT have been touched: it does not
        // exist in this message.
        assert_eq!(r.bytes.len(), Rsp::WIRE_LEN + 32 + 128);
    }

    /// The 4.2 GB that occupy nothing: NV50_MEMORY_VIRTUAL with
    /// ALLOC_FLAGS_VIRTUAL reserves address space. Charging it would
    /// exhaust any plausible cap on the first CUDA context.
    #[test]
    fn a_virtual_reservation_costs_nothing() {
        let (mut s, _fake, tok, led) = capped_session(64 << 20);
        let mut m = vram_alloc_msg(tok, 0xaa, 0xfb00_0000);
        // hClass 0x50a0 in the inline, VIRTUAL in the params flags.
        let hclass_off = Req::WIRE_LEN + 12;
        m[hclass_off..hclass_off + 4].copy_from_slice(&0x50a0u32.to_le_bytes());
        let flags_off = Req::WIRE_LEN + 48 + 8;
        m[flags_off..flags_off + 4].copy_from_slice(&0x8c415u32.to_le_bytes());
        let r = s.handle_msg(&m).unwrap();
        assert_eq!(rm_status(&r), sys::NV_OK);
        assert_eq!(led.used(), 0, "a virtual reservation occupies no FB");
    }

    // ---- the event return channel (ARCHITECTURE.md 6a; built for nr 10) ---

    /// A 0x7e alloc as NVKMS sends it: NVOS64 (48 B) with hClass 0x7e,
    /// NV0005 params with a notifyIndex and a callback pointer in `data`.
    fn kernel_callback_alloc(tok: u64, h_client: u32, notify: u32, cb: u64) -> Vec<u8> {
        let mut inline = vec![0u8; 48];
        inline[0..4].copy_from_slice(&h_client.to_le_bytes());
        inline[4..8].copy_from_slice(&0x5c00_0001u32.to_le_bytes());
        inline[12..16].copy_from_slice(&sys::NV01_EVENT_KERNEL_CALLBACK_EX.to_le_bytes());
        // pAllocParms @16 non-null: the guest VA the params sat at; the
        // host replaces it with its aux address.
        inline[16..24].copy_from_slice(&0x7fff_0000_1000u64.to_le_bytes());
        let mut aux = vec![0u8; std::mem::size_of::<sys::NV0005_ALLOC_PARAMETERS>()];
        aux[0..4].copy_from_slice(&h_client.to_le_bytes());
        aux[NV0005_HCLASS..NV0005_HCLASS + 4]
            .copy_from_slice(&sys::NV01_EVENT_KERNEL_CALLBACK_EX.to_le_bytes());
        aux[NV0005_NOTIFYINDEX..NV0005_NOTIFYINDEX + 4].copy_from_slice(&notify.to_le_bytes());
        aux[NV0005_DATA..NV0005_DATA + 8].copy_from_slice(&cb.to_le_bytes());
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_ALLOC, 48, aux.len());
        // pAllocParms @16 (NVOS64) -- the embedded pointer the table names
        // for class 0x7e; the fake never dereferences it, but prepare's
        // check must find the offset it expects.
        req.embedded_ptr_off = 16;
        msg(&req, &inline, &aux)
    }

    /// The (1a') substitution keeps books now: the guest's pointer and
    /// notifyIndex are saved BEFORE the id overwrites `data`, RM's
    /// hObjectNew is filed after the call, and the ctl the OS event hangs
    /// off is reported for polling -- once per client.
    #[test]
    fn a_substituted_event_is_registered_and_its_ctl_reported() {
        // NVOS64: hObjectNew @8 -- and NOTHING at 40: `status` stays 0 ==
        // NV_OK, and offset 8 is inside every 16-byte struct the fake also
        // writes into (the ALLOC_OS_EVENT block).
        let fake = Arc::new(FakeSyscalls {
            writes_back: vec![(8, 0x5c00_00e1u32.to_le_bytes().to_vec())],
            ..Default::default()
        });
        let mut s = Session::detached_proc(7, crate::vram::Ledger::off()).unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned);

        let h_client = 0xc1d8_0001u32;
        let r = s.handle_msg(&kernel_callback_alloc(tok, h_client, 0x1000_0007, 0xffff_8881_2345_6780)).unwrap();
        assert_eq!(errno_of(&r), None);

        // The ledger: ALLOC_OS_EVENT with fd:1 on a fd that is NOT the
        // guest's token, then the alloc itself with hClass 0x79.
        let calls: Vec<_> = fake.calls().into_iter().filter_map(|c| match c {
            FakeCall::Ioctl { fd, request, inline } => Some((fd, nvrm_abi::ioc_nr(request as u32), inline)),
            _ => None,
        }).collect();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].1, nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT);
        assert_ne!(calls[0].0, fd, "the OS event hangs off the session's own ctl, not the guest's fd");
        assert_eq!(&calls[0].2[0..4], &h_client.to_le_bytes());
        assert_eq!(&calls[0].2[8..12], &1u32.to_le_bytes(), "first id is 1");
        assert_eq!(calls[1].1, sys::NV_ESC_RM_ALLOC);
        assert_eq!(&calls[1].2[12..16], &sys::NV01_EVENT_OS_EVENT.to_le_bytes());

        // The guest reads the id back in `data` (this is what
        // vdisp_event_on_missing_parent keys on).
        let aux = &r.bytes[Rsp::WIRE_LEN + 48..];
        assert_eq!(&aux[NV0005_DATA..NV0005_DATA + 8], &1u64.to_le_bytes());
        assert_eq!(&aux[NV0005_HCLASS..NV0005_HCLASS + 4], &sys::NV01_EVENT_OS_EVENT.to_le_bytes());

        // The books.
        let reg = s.events.get(&(h_client, 0x5c00_00e1)).copied().expect("registered under RM's handle");
        assert_eq!(reg, EventReg {
            h_client, h_event: 0x5c00_00e1, class: sys::NV01_EVENT_KERNEL_CALLBACK_EX,
            notify_index: 0x1000_0007, guest_data: 0xffff_8881_2345_6780, token: tok, id: 1,
        });
        assert_eq!(s.event_stats(), (1, 0, 0, 0));
        let ctl_fd = calls[0].0;
        assert_eq!(s.take_pollables(), vec![Pollable::EventCtl { h_client, fd: ctl_fd }]);
        assert!(s.take_pollables().is_empty(), "reported once, then taken");

        // A second event on the SAME client reuses the ctl (id 2, no new
        // pollable); a different client gets a ctl of its own.
        let r = s.handle_msg(&kernel_callback_alloc(tok, h_client, 3, 0x1)).unwrap();
        assert_eq!(errno_of(&r), None);
        assert!(s.take_pollables().is_empty());
        let r = s.handle_msg(&kernel_callback_alloc(tok, h_client + 1, 3, 0x2)).unwrap();
        assert_eq!(errno_of(&r), None);
        let p = s.take_pollables();
        assert_eq!(p.len(), 1);
        assert!(matches!(p[0], Pollable::EventCtl { h_client: c, fd } if c == h_client + 1 && fd != ctl_fd));
        assert_eq!(s.event_ctls.len(), 2);
        assert_eq!(s.event_stats().0, 3);
    }

    /// RM refused the substituted alloc: nothing is filed, and the id goes
    /// back with FREE_OS_EVENT.
    #[test]
    fn a_refused_substituted_event_gives_its_id_back() {
        let fake = Arc::new(FakeSyscalls { ioctl_ret: -1, ..Default::default() });
        let mut s = Session::detached_proc(7, crate::vram::Ledger::off()).unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned);
        // ret -1 also fails ALLOC_OS_EVENT itself -> refusal, no alloc sent.
        let r = s.handle_msg(&kernel_callback_alloc(tok, 0xc100_0001, 7, 0x10)).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EIO));
        assert_eq!(fake.ioctl_count(), 1);
        assert!(s.events.is_empty());

        // Now ALLOC_OS_EVENT succeeds and the RM alloc fails: the fake
        // cannot tell the two apart by return code, so let it succeed and
        // write NV_ERR_INVALID_CLASS into NVOS64.status @40 -- but only for
        // a 48-byte buffer, which the fake cannot know. So use the 32-byte
        // NVOS21 form (status @28 -- also past the 16-byte ALLOC_OS_EVENT
        // block). writes_back is unbounded there; keep this test on the
        // return path the fake CAN express: ioctl_ret != 0 on the alloc is
        // impossible to isolate, so drive the refusal through `execute`
        // directly with a hand-built plan.
        let fake = Arc::new(FakeSyscalls::default());
        s.sys = Box::new(fake.clone());
        let mut m = kernel_callback_alloc(tok, 0xc100_0002, 7, 0x10);
        let req = Req::from_bytes(&m).unwrap();
        let payload = m.split_off(Req::WIRE_LEN);
        let plan = s.prepare(&req, &payload).ok().expect("prepare accepts");
        assert!(plan.event_reg.is_some());
        // Pretend RM refused: force a status that is not NV_OK into scratch
        // before execute reads it back (the fake writes nothing at 40).
        s.scratch[40..44].copy_from_slice(&sys::NV_ERR_INVALID_CLASS.to_le_bytes());
        s.execute(plan).unwrap();
        assert!(s.events.is_empty(), "a refused alloc registers nothing");
        let frees: Vec<_> = fake.calls().into_iter().filter(|c| matches!(c,
            FakeCall::Ioctl { request, .. }
                if nvrm_abi::ioc_nr(*request as u32) == nvrm_abi::nvgpu::NV_ESC_FREE_OS_EVENT)).collect();
        assert_eq!(frees.len(), 1, "the id was given back: {:?}", fake.calls());
        assert_eq!(s.event_stats(), (0, 0, 0, 0));
    }

    /// RM_FREE of the event takes the registration and the id with it;
    /// RM_FREE of the client takes every registration of that client, and
    /// no FREE_OS_EVENT (RM does that itself).
    #[test]
    fn freeing_the_event_or_its_client_clears_the_books() {
        let fake = Arc::new(FakeSyscalls {
            writes_back: vec![(8, 0x5c00_00e1u32.to_le_bytes().to_vec())],
            ..Default::default()
        });
        let mut s = Session::detached_proc(7, crate::vram::Ledger::off()).unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned);
        let c1 = 0xc1d8_0001u32;
        s.handle_msg(&kernel_callback_alloc(tok, c1, 7, 0x10)).unwrap();
        assert_eq!(s.events.len(), 1);

        // Free the event object: NVOS00 hRoot @0, hObjectOld @8.
        let mut inline = vec![0u8; 16];
        inline[0..4].copy_from_slice(&c1.to_le_bytes());
        inline[8..12].copy_from_slice(&0x5c00_00e1u32.to_le_bytes());
        let n_before = fake.ioctl_count();
        s.handle_msg(&msg(&ioctl_req(tok, sys::NV_ESC_RM_FREE, 16, 0), &inline, &[])).unwrap();
        assert!(s.events.is_empty());
        let after: Vec<_> = fake.calls()[n_before..].iter().filter_map(|c| match c {
            FakeCall::Ioctl { request, .. } => Some(nvrm_abi::ioc_nr(*request as u32)),
            _ => None,
        }).collect();
        assert_eq!(after, vec![sys::NV_ESC_RM_FREE, nvrm_abi::nvgpu::NV_ESC_FREE_OS_EVENT]);

        // Two events on two clients, then free ONE client: its event is
        // gone, the other's stays, and no FREE_OS_EVENT is sent. A fake
        // WITHOUT writes_back here: the one above would also rewrite
        // NVOS00.hObjectOld @8 of the FREE itself, which the hook reads
        // after the call.
        let fake = Arc::new(FakeSyscalls::default());
        s.sys = Box::new(fake.clone());
        s.handle_msg(&kernel_callback_alloc(tok, c1, 7, 0x10)).unwrap();
        s.handle_msg(&kernel_callback_alloc(tok, c1 + 1, 7, 0x11)).unwrap();
        assert!(s.events.contains_key(&(c1, 0)) && s.events.contains_key(&(c1 + 1, 0)));
        let mut inline = vec![0u8; 16];
        inline[0..4].copy_from_slice(&c1.to_le_bytes());
        inline[8..12].copy_from_slice(&c1.to_le_bytes());
        let n_before = fake.ioctl_count();
        assert_eq!(s.event_ctls.len(), 2, "one ctl per client, before the free");
        s.handle_msg(&msg(&ioctl_req(tok, sys::NV_ESC_RM_FREE, 16, 0), &inline, &[])).unwrap();
        assert_eq!(s.events.len(), 1, "the other client's event stays");
        assert!(s.events.contains_key(&(c1 + 1, 0)));
        assert_eq!(fake.ioctl_count(), n_before + 1, "RM frees the client's os events itself");

        // And the ctl fd of the freed client leaves too. Until 2026-08-17
        // it did not, and RM therefore kept the client alive on the host
        // for as long as the session ran (`pending_ctl_unwatch`).
        assert_eq!(s.event_ctls.len(), 1, "the freed client's ctl is gone");
        assert!(s.event_ctls.contains_key(&(c1 + 1)), "the other client keeps its own");
        let out = s.take_ctl_unwatch();
        assert_eq!(out.len(), 1, "handed to the device, not dropped here");
        assert_eq!(out[0].0, c1, "and it names the client whose ctl it is");
        assert!(s.take_ctl_unwatch().is_empty(), "reported once, then taken");
    }

    /// The other door a client leaves by: the guest closes the FD it was
    /// allocated on. RM tears the client down there and sends no RM_FREE,
    /// so nothing but the token can say which holdings die with it.
    ///
    /// Without this the ctl FDs survived until the guest PROCESS
    /// exited, which is the leak of OPEN-QUESTIONS 31 -- 2003 open
    /// `nvidiactl` FDs against one live guest process. The VRAM ledger had
    /// released on this door since it was written (`close_token`); the
    /// per-client holdings had not.
    #[test]
    fn closing_the_fd_a_client_was_allocated_on_gives_its_ctl_back() {
        let fake = Arc::new(FakeSyscalls::default());
        let mut s = Session::detached_proc(7, crate::vram::Ledger::off()).unwrap();
        s.sys = Box::new(fake.clone());
        let mktok = |s: &mut Session| {
            let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
            let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
            s.mirror.insert(owned)
        };
        let tok_a = mktok(&mut s);
        let tok_b = mktok(&mut s);
        let (c_a, c_b) = (0xc1d8_0001u32, 0xc1d8_0002u32);

        // Each client is ALLOCATED on its own FD: NVOS21 hRoot @0,
        // hObjectParent @4 (0 = a root), hObjectNew @8, status @28.
        let mut root = |tok: u64, h: u32| {
            let mut inline = vec![0u8; 32];
            inline[8..12].copy_from_slice(&h.to_le_bytes());
            s.handle_msg(&msg(&ioctl_req(tok, sys::NV_ESC_RM_ALLOC, 32, 0), &inline, &[]))
                .unwrap();
        };
        root(tok_a, c_a);
        root(tok_b, c_b);
        assert_eq!(s.client_token.get(&c_a), Some(&tok_a), "noted at the alloc");
        assert_eq!(s.client_token.get(&c_b), Some(&tok_b));

        // Give each of them an event ctl, the FD that used to leak.
        s.handle_msg(&kernel_callback_alloc(tok_a, c_a, 7, 0x10)).unwrap();
        s.handle_msg(&kernel_callback_alloc(tok_b, c_b, 7, 0x11)).unwrap();
        assert_eq!(s.event_ctls.len(), 2, "one ctl per client");
        let _ = s.take_ctl_unwatch();

        // Close ONLY the first FD. No RM_FREE crosses -- this is the point.
        s.handle_msg(&msg(&close_req(tok_a), &[], &[])).unwrap();
        assert_eq!(s.event_ctls.len(), 1, "the closed FD's client gives its ctl back");
        assert!(s.event_ctls.contains_key(&c_b), "the other FD's client keeps its own");
        assert!(!s.client_token.contains_key(&c_a), "and its note goes with it");
        let out = s.take_ctl_unwatch();
        assert_eq!(out.len(), 1, "handed to the device, not dropped here");
        assert_eq!(out[0].0, c_a, "and it names the right client");
        assert_eq!(s.ctl_freed_by_close, 1, "counted on the close door");
        assert_eq!(s.ctl_freed_by_rmfree, 0, "and not on the other one");

        // The second FD too, so a session that closes everything keeps
        // nothing: that is the property the leak violated.
        s.handle_msg(&msg(&close_req(tok_b), &[], &[])).unwrap();
        assert!(s.event_ctls.is_empty(), "nothing held after both FDs are closed");
        assert!(s.client_token.is_empty());
    }

    /// Draining: one NV_ESC_RM_GET_EVENT_DATA per queued firing on the
    /// client's ctl, matched against the books by (hClient, hObject). The
    /// fake cannot write through NVOS41.pEvent, so RM's answer is the
    /// zeroed NvUnixEvent -- hObject 0, MoreEvents 0, status NV_OK -- which
    /// counts as fired+unmatched unless a registration for handle 0 sits
    /// in the books; with one there, it comes back as a `Fired`.
    #[test]
    fn drain_asks_get_event_data_and_matches_by_client_and_handle() {
        let fake = Arc::new(FakeSyscalls::default());
        let mut s = Session::detached_proc(7, crate::vram::Ledger::off()).unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned);
        let c1 = 0xc1d8_0001u32;
        // hObjectNew stays 0 (no writes_back): registered under (c1, 0).
        s.handle_msg(&kernel_callback_alloc(tok, c1, 0x1000_0007, 0x10)).unwrap();
        assert!(s.events.contains_key(&(c1, 0)));

        // Unknown client: nothing to drain, no ioctl.
        let n = fake.ioctl_count();
        assert!(s.drain_os_events(0xdead).is_empty());
        assert_eq!(fake.ioctl_count(), n);

        let fired = s.drain_os_events(c1);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].reg.guest_data, 0x10);
        assert_eq!(fired[0].reg.class, sys::NV01_EVENT_KERNEL_CALLBACK_EX);
        assert_eq!(fired[0].info32, 0);
        let last = fake.calls().last().cloned().unwrap();
        match last {
            FakeCall::Ioctl { fd: on, request, inline } => {
                assert_eq!(nvrm_abi::ioc_nr(request as u32), sys::NV_ESC_RM_GET_EVENT_DATA);
                assert_eq!(nvrm_abi::ioc_size(request as u32) as usize,
                           std::mem::size_of::<sys::NVOS41_PARAMETERS>());
                assert_eq!(inline.len(), 16);
                assert_ne!(on, fd, "drained on the session's ctl, never the guest's fd");
                // pEvent points at a NvUnixEvent in THIS process, not at 0.
                assert_ne!(u64::from_le_bytes(inline[0..8].try_into().unwrap()), 0);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(s.event_stats(), (1, 1, 0, 0));

        // A firing for a handle nobody registered is counted, not delivered.
        s.events.clear();
        assert!(s.drain_os_events(c1).is_empty());
        assert_eq!(s.event_stats(), (1, 2, 1, 0));
    }

    /// The 0x79 path: a guest client that runs NV_ESC_ALLOC_OS_EVENT itself
    /// names the fd it rode on as its wake channel -- reported as
    /// `Pollable::Client` with the token, whether or not RM accepts.
    #[test]
    fn a_guest_os_event_alloc_reports_its_fd_for_polling() {
        let (mut s, fake, tok) = session();
        let fd = s.mirror.raw(tok).unwrap();
        let mut inline = vec![0u8; 16];
        inline[8..12].copy_from_slice(&23u32.to_le_bytes());
        let mut req = ioctl_req(tok, nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT, 16, 0);
        req.dev_tag = DevTag::Gpu as u32;
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&r), None);
        assert_eq!(fake.ioctl_count(), 1);
        assert_eq!(s.take_pollables(), vec![Pollable::Client { token: tok, fd, owner: None }]);
        // A UVM ioctl with the same raw number is not an OS event.
        let mut req = ioctl_req(tok, nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT, 16, 0);
        req.dev_tag = DevTag::Uvm as u32;
        s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert!(s.take_pollables().is_empty());
    }

    // ---- semaphore-surface waiters (the Wayland fence path) ---------------

    /// One semsurf control as the guest kernel sends it: NVOS54 (32 B) with
    /// the cmd, params in aux with the notification handle at `off`.
    fn semsurf_control(tok: u64, h_client: u32, cmd: u32, aux_len: usize, off: usize, handle: u64) -> Vec<u8> {
        let mut inline = vec![0u8; 32];
        inline[0..4].copy_from_slice(&h_client.to_le_bytes());
        inline[4..8].copy_from_slice(&0x5c00_00dau32.to_le_bytes());
        inline[8..12].copy_from_slice(&cmd.to_le_bytes());
        inline[16..24].copy_from_slice(&0x7fff_0000_2000u64.to_le_bytes());
        inline[24..28].copy_from_slice(&(aux_len as u32).to_le_bytes());
        let mut aux = vec![0u8; aux_len];
        aux[off..off + 8].copy_from_slice(&handle.to_le_bytes());
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, aux_len);
        req.embedded_ptr_off = 16;
        msg(&req, &inline, &aux)
    }

    /// REGISTER_WAITER with a kernel-VA handle: the handle is substituted
    /// by an OS event id on a private ctl, the waiter is armed, and its fd
    /// is reported for the poller -- NOT for the epoll set.
    #[test]
    fn a_semsurf_waiter_is_substituted_and_armed() {
        let (mut s, fake, tok) = session();
        let h_client = 0xc1d8_0002u32;
        let kc = 0xffff_8881_dead_be00u64;

        let r = s.handle_msg(&semsurf_control(
            tok, h_client, CTRL_SEMSURF_REGISTER_WAITER, 32, 24, kc)).unwrap();
        assert_eq!(errno_of(&r), None);

        // The ledger: ALLOC_OS_EVENT of (h_client, id 1) on a PRIVATE fd,
        // then the control itself with the id where the pointer was.
        let calls: Vec<_> = fake.calls().into_iter().filter_map(|c| match c {
            FakeCall::Ioctl { fd, request, inline } =>
                Some((fd, nvrm_abi::ioc_nr(request as u32), inline)),
            _ => None,
        }).collect();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].1, nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT);
        assert_eq!(&calls[0].2[0..4], &h_client.to_le_bytes());
        assert_eq!(&calls[0].2[8..12], &1u32.to_le_bytes());
        assert_eq!(calls[1].1, sys::NV_ESC_RM_CONTROL);

        // The guest's params went out with the id, and came back with it.
        let aux = &r.bytes[Rsp::WIRE_LEN + 32..];
        assert_eq!(&aux[24..32], &1u64.to_le_bytes());

        // Armed, and reported for the POLLER (the waiter fd must never
        // enter the epoll set -- waiters.rs says why).
        let w = s.waiters.get(&1).expect("armed under its id");
        assert_eq!((w.h_client, w.guest_kc), (h_client, kc));
        use std::os::fd::AsRawFd;
        let slot_fd = w.slot.ctl.as_raw_fd();
        assert_eq!(s.take_pollables(), vec![Pollable::Waiter { id: 1, fd: slot_fd }]);

        // The wake retires it and recycles the slot.
        assert_eq!(s.semsurf_wake(1), Some((h_client, kc, tok)));
        assert!(s.waiters.is_empty());
        assert_eq!(s.waiter_pool[&h_client].len(), 1);
        assert_eq!(s.semsurf_wake(1), None, "one-shot: the second wake finds nothing");
    }

    /// A user-space registration carries an OS-event id (NvU32 -- RM casts,
    /// os.c:1744); it passes through byte for byte, no slot, no pollable.
    #[test]
    fn a_user_space_waiter_handle_passes_through() {
        let (mut s, fake, tok) = session();
        let r = s.handle_msg(&semsurf_control(
            tok, 0xc1d8_0003, CTRL_SEMSURF_REGISTER_WAITER, 32, 24, 23)).unwrap();
        assert_eq!(errno_of(&r), None);
        assert_eq!(fake.ioctl_count(), 1, "no ALLOC_OS_EVENT for a user handle");
        let aux = &r.bytes[Rsp::WIRE_LEN + 32..];
        assert_eq!(&aux[24..32], &23u64.to_le_bytes(), "untouched");
        assert!(s.waiters.is_empty());
        assert!(s.take_pollables().is_empty());
    }

    /// UNREGISTER_WAITER on an armed waiter: the same substitution -- RM
    /// matches the listener by the handle value the registration carried --
    /// and NV_OK recycles the slot, so the next registration reuses id and
    /// fd instead of opening a third ctl.
    #[test]
    fn an_unregistered_waiter_recycles_its_slot() {
        let (mut s, fake, tok) = session();
        let h_client = 0xc1d8_0004u32;
        let kc = 0xffff_8881_dead_bf00u64;
        s.handle_msg(&semsurf_control(
            tok, h_client, CTRL_SEMSURF_REGISTER_WAITER, 32, 24, kc)).unwrap();
        s.take_pollables();

        let r = s.handle_msg(&semsurf_control(
            tok, h_client, CTRL_SEMSURF_UNREGISTER_WAITER, 24, 16, kc)).unwrap();
        assert_eq!(errno_of(&r), None);
        let aux = &r.bytes[Rsp::WIRE_LEN + 32..];
        assert_eq!(&aux[16..24], &1u64.to_le_bytes(), "translated to the same id");
        assert!(s.waiters.is_empty(), "cancelled");
        assert_eq!(s.waiter_pool[&h_client].len(), 1);

        // Re-register: the pooled slot serves again -- no new ALLOC_OS_EVENT.
        let before = fake.calls().iter().filter(|c| matches!(c,
            FakeCall::Ioctl { request, .. }
                if nvrm_abi::ioc_nr(*request as u32) == nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT
        )).count();
        s.handle_msg(&semsurf_control(
            tok, h_client, CTRL_SEMSURF_REGISTER_WAITER, 32, 24, kc)).unwrap();
        let after = fake.calls().iter().filter(|c| matches!(c,
            FakeCall::Ioctl { request, .. }
                if nvrm_abi::ioc_nr(*request as u32) == nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT
        )).count();
        assert_eq!(before, after, "the slot was reused, not re-allocated");
        assert!(s.waiters.contains_key(&1));

        // An unregister for a handle nobody armed leaves the pointer alone:
        // RM answers OBJECT_NOT_FOUND, which is the truthful "too late".
        let r = s.handle_msg(&semsurf_control(
            tok, h_client, CTRL_SEMSURF_UNREGISTER_WAITER, 24, 16, 0xffff_8881_0000_0100)).unwrap();
        let aux = &r.bytes[Rsp::WIRE_LEN + 32..];
        assert_eq!(&aux[16..24], &0xffff_8881_0000_0100u64.to_le_bytes());
    }

    // ---- errno: the guest gets the errno of ITS call ---------------------

    /// A failed forwarded ioctl answers `-errno` -- the plain case, which
    /// establishes that the fake's errno reaches the wire at all.
    #[test]
    fn a_failed_ioctl_answers_its_own_errno() {
        let (mut s, fake, tok) = session();
        fake.ioctl_rets.lock().unwrap().push_back((-1, libc::ENOSPC));
        let inline = vec![0u8; 32];
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::ENOSPC));
        assert_eq!(fake.ioctl_count(), 1);
    }

    /// The errno must be the errno of the FORWARDED call, not of whatever
    /// syscall ran after it. The (1a') path is where that goes wrong: RM
    /// refuses the substituted event alloc (ioctl -1, ENOSPC), the session
    /// then gives the OS-event id back with FREE_OS_EVENT -- a second ioctl
    /// that sets errno again (EPERM here). Until 2026-08-18 `execute` read
    /// `last_os_error()` at its very end and the guest was told EPERM.
    #[test]
    fn a_later_syscall_does_not_replace_the_errno_of_the_forwarded_call() {
        let (mut s, fake, tok) = session();
        {
            let mut q = fake.ioctl_rets.lock().unwrap();
            q.push_back((0, 0)); // ALLOC_OS_EVENT in prepare
            q.push_back((-1, libc::ENOSPC)); // the forwarded RM_ALLOC
            q.push_back((0, libc::EPERM)); // FREE_OS_EVENT on the failure path
        }
        let r = s.handle_msg(&kernel_callback_alloc(tok, 0xc100_0001, 7, 0x10)).unwrap();
        assert_eq!(fake.ioctl_count(), 3, "alloc-os-event, the alloc, free-os-event: {:?}", fake.calls());
        assert_eq!(errno_of(&r), Some(libc::ENOSPC), "the guest must see the alloc's errno");
        assert!(s.events.is_empty());
    }

    // ---- UVM parameter layouts: hand-quoted numbers vs the vendor structs --

    /// `uvm_status_off` is the offset the failure log line and the faked
    /// answers read `rmStatus` from. Every value comes from `offset_of!` of
    /// the bindgen struct; this pins that each one lies inside the size
    /// `xlate::uvm_param_size` sends, and that the two tables name the same
    /// command set. A status read outside the struct is a read of whatever
    /// follows the guest's block in `scratch`.
    #[test]
    fn uvm_status_offsets_lie_inside_the_sizes_xlate_sends() {
        use nvrm_abi::xlate::{uvm, uvm_param_size};
        let known = [
            uvm::INITIALIZE, uvm::PAGEABLE_MEM_ACCESS, uvm::MM_INITIALIZE,
            uvm::REGISTER_GPU_VASPACE, uvm::UNREGISTER_GPU_VASPACE, uvm::REGISTER_CHANNEL,
            uvm::UNREGISTER_CHANNEL, uvm::MAP_EXTERNAL_ALLOCATION, uvm::FREE, uvm::REGISTER_GPU,
            uvm::MAP_DYNAMIC_PARALLELISM_REGION, uvm::ALLOC_SEMAPHORE_POOL,
            uvm::PAGEABLE_MEM_ACCESS_ON_GPU, uvm::SET_PREFERRED_LOCATION,
            uvm::UNSET_PREFERRED_LOCATION, uvm::ENABLE_READ_DUPLICATION,
            uvm::DISABLE_READ_DUPLICATION, uvm::SET_ACCESSED_BY, uvm::UNSET_ACCESSED_BY,
            uvm::MIGRATE, uvm::VALIDATE_VA_RANGE, uvm::CREATE_EXTERNAL_RANGE,
        ];
        for nr in known {
            let off = uvm_status_off(nr).unwrap_or_else(|| panic!("{nr:#x} has no status offset"));
            let size = uvm_param_size(nr).unwrap_or_else(|| panic!("{nr:#x} has no size")) as usize;
            assert!(off + 4 <= size, "{nr:#x}: rmStatus @{off} outside the {size}-byte block");
        }
        // DEINITIALIZE has no parameter block, hence no status; a number
        // nobody knows answers None rather than a guess.
        assert_eq!(uvm_status_off(uvm::DEINITIALIZE), None);
        assert_eq!(uvm_status_off(0x7fff), None);
        // And the two the fake answers write to, as literals the way the
        // execute branch quotes them.
        assert_eq!(uvm_status_off(uvm::MIGRATE), Some(72));
        assert_eq!(uvm_status_off(uvm::ALLOC_SEMAPHORE_POOL), Some(9240));
    }

    /// The process-list refusal, pinned before and after the restructure of
    /// `execute`'s tail: a GPU_GET_PIDS answer whose params buffer is not
    /// the documented structure is refused with EINVAL, and the guest gets
    /// the refusal rather than RM's answer -- which still carries HOST
    /// PIDs, which is what makes forwarding it a leak rather than a
    /// nuisance.
    ///
    /// A characterization test: it passes on both sides of the change. It
    /// is here because "the refusal semantics stay identical" is the whole
    /// claim of that change, and a claim that nothing asserts is a hope.
    #[test]
    fn a_process_list_that_is_not_the_struct_is_refused() {
        let (mut s, fake, tok) = session();
        let mut inline = vec![0u8; 32];
        inline[8..12].copy_from_slice(&crate::vram::CMD_GPU_GET_PIDS.to_le_bytes());
        inline[16..24].copy_from_slice(&0xdead_beefu64.to_le_bytes()); // params != 0
        inline[24..28].copy_from_slice(&16u32.to_le_bytes()); // paramsSize, honest
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 16);
        req.embedded_ptr_off = 16;
        // 16 bytes is far short of the NV2080_CTRL_GPU_GET_PIDS_PARAMS the
        // command names, so the rewrite cannot happen.
        let r = s.handle_msg(&msg(&req, &inline, &[0u8; 16])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL), "short process list refused");
        // The ioctl DID run -- the refusal is about RM's answer, not about
        // the guest's request, and that is why the bookkeeping after it
        // must not be skipped.
        assert_eq!(fake.ioctl_count(), 1);
        // A refusal carries no payload: nothing of RM's answer reaches the
        // guest.
        let rsp = Rsp::from_bytes(&r.bytes).unwrap();
        assert_eq!((rsp.inline_len, rsp.aux_len), (0, 0));
        assert_eq!(r.bytes.len(), Rsp::WIRE_LEN);
    }

    /// The blob_id counter must not hand out 0 when the low half wraps.
    ///
    /// The high 32 bits carry the session's `sub_id`, so for every session
    /// but one a wrapped low half is merely a repeat. For `sub_id == 0` --
    /// the first guest process of a backend, and the fuzz target's session
    /// -- the whole id becomes 0, and 0 is documented in `build()` as the
    /// value that stays free because the guest reads it as a failed
    /// registration. The guest would then take a successful MapPrepare for
    /// a failure and never fetch the mapping, while the host holds the FD
    /// under an id nobody asks for.
    ///
    /// 4 billion mappings is not a run, it is a long-lived backend; the
    /// counter is set up next to the wrap rather than counted there.
    #[test]
    fn a_wrapping_blob_id_never_hands_out_zero() {
        let mut s = Session::detached_proc(0, crate::vram::Ledger::off()).unwrap();
        s.sys = Box::new(Arc::new(FakeSyscalls::default()));
        let fd = unsafe { libc::memfd_create(c"leandro-blobid-test".as_ptr(), 0) };
        assert!(fd >= 0);
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned);

        let prepare = |s: &mut Session, seq: u32| -> u64 {
            let mp = Req {
                seq,
                kind: Kind::MapPrepare as u32,
                dev_tag: DevTag::Ctl as u32,
                target_token: tok,
                map_len: 4096,
                ..Req::default()
            };
            let r = s.handle_msg(mp.as_bytes()).unwrap();
            let rsp = Rsp::from_bytes(&r.bytes).unwrap();
            assert_eq!(rsp.ret, 0, "MapPrepare refused");
            rsp.token
        };

        // The last id of the low half, then the one that follows it.
        s.next_blob_id = 0xffff_ffff;
        assert_eq!(prepare(&mut s, 1), 0xffff_ffff);
        let wrapped = prepare(&mut s, 2);
        assert_ne!(wrapped, 0, "MapPrepare handed out blob_id 0");
        assert!(!s.pending_maps.contains_key(&0), "a mapping registered under 0");
        // And the session id in the high half is untouched by the wrap.
        assert_eq!(wrapped >> 32, 0);
    }

    /// A `dev_tag` this backend does not know is refused with EINVAL and
    /// no ioctl runs. Until 2026-08-18 it was read as the ctl node: the
    /// request number of a UVM call would then have been `_IOC`-encoded and
    /// sent to whatever file the token named. No guest module sends such a
    /// tag; the fuzzer does, and a message that cannot be interpreted is
    /// not one to guess about.
    #[test]
    fn an_unknown_dev_tag_is_refused_before_any_syscall() {
        let (mut s, fake, tok) = session();
        let inline = vec![0u8; 32];
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        req.dev_tag = 7;
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));
        assert_eq!(fake.ioctl_count(), 0, "refused before the ioctl");
        // MapPrepare with the same nonsense: refused too, nothing registered.
        let mp = Req {
            seq: 2,
            kind: Kind::MapPrepare as u32,
            dev_tag: 7,
            target_token: tok,
            map_len: 4096,
            ..Req::default()
        };
        let r = s.handle_msg(mp.as_bytes()).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));
        assert!(s.pending_maps.is_empty());
    }
}
