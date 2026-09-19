// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Host state for one guest process, keyed by `Req.guest_proc`.
//!
//! Owns device FD tokens, pending mappings, guest-memory backing, VRAM
//! accounting, and event substitutions. `prepare` validates and translates;
//! `execute` forwards the ioctl and applies its result. Event preparation
//! also allocates host OS events through `NvSyscalls`.
//!
//! RM is NVIDIA's Resource Manager. Its frontend ioctls use `NVOS*`
//! parameter blocks; UVM uses raw command numbers. Guest kernel callbacks
//! are replaced with host OS events and delivered back through the event queue.
//! Process IDs and handles are guest-supplied bookkeeping keys. The isolation
//! boundary is the VM, not an individual process within it.

use nvrm_sys::RmAbi;
use std::marker::PhantomData;
use std::os::fd::{OwnedFd, RawFd};
use std::sync::Arc;

use anyhow::Result;

use nvrm_abi::iowr_raw;
use nvrm_abi::share;
use nvrm_abi::sys;
use nvrm_abi::xlate::Dev;
use nvrm_wire::{self as proto, DevTag, Kind, Req, Rsp, NONE_U32, NONE_U64};

use crate::guest_words::{GuestAddr, GuestLen};
use crate::host_pool::{GpaRun, PinBudget, PoolState};
use crate::mirror::Mirror;
use crate::syscalls::{NvSyscalls, RealSyscalls};
use crate::waiters::RegistrationId;

type Mem = vm_memory::GuestMemoryAtomic<vm_memory::GuestMemoryMmap<()>>;

/// Emulate semaphore-pool allocation: forwarding would collide with the
/// external mapping installed at the same GPU VA. Derive rmStatus from bindgen.
const UVM_ALLOC_SEMAPHORE_POOL: u32 = 68;
const SEMAPHORE_POOL_RMSTATUS_OFF: usize =
    std::mem::offset_of!(sys::UVM_ALLOC_SEMAPHORE_POOL_PARAMS, rmStatus);

/// UVM status offsets for diagnostics and emulated replies, from bindgen.
/// Raw UVM request numbers carry no size. Unknown layouts return None
/// and log status zero.
fn uvm_status_off<A: RmAbi>(nr: u32) -> Option<usize> {
    use nvrm_abi::xlate::uvm;
    use std::mem::offset_of;
    Some(match nr {
        uvm::INITIALIZE => offset_of!(sys::UVM_INITIALIZE_PARAMS, rmStatus),
        uvm::REGISTER_GPU_VASPACE => offset_of!(sys::UVM_REGISTER_GPU_VASPACE_PARAMS, rmStatus),
        uvm::UNREGISTER_GPU_VASPACE => offset_of!(sys::UVM_UNREGISTER_GPU_VASPACE_PARAMS, rmStatus),
        uvm::REGISTER_CHANNEL => offset_of!(sys::UVM_REGISTER_CHANNEL_PARAMS, rmStatus),
        uvm::UNREGISTER_CHANNEL => A::UVM_UNREGISTER_CHANNEL_PARAMS_OFF_rmStatus,
        uvm::MAP_EXTERNAL_ALLOCATION => {
            offset_of!(sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS, rmStatus)
        }
        uvm::FREE => A::UVM_FREE_PARAMS_OFF_rmStatus,
        uvm::REGISTER_GPU => offset_of!(sys::UVM_REGISTER_GPU_PARAMS, rmStatus),
        uvm::PAGEABLE_MEM_ACCESS => offset_of!(sys::UVM_PAGEABLE_MEM_ACCESS_PARAMS, rmStatus),
        uvm::SET_PREFERRED_LOCATION => offset_of!(sys::UVM_SET_PREFERRED_LOCATION_PARAMS, rmStatus),
        uvm::UNSET_PREFERRED_LOCATION => {
            offset_of!(sys::UVM_UNSET_PREFERRED_LOCATION_PARAMS, rmStatus)
        }
        uvm::ENABLE_READ_DUPLICATION => {
            offset_of!(sys::UVM_ENABLE_READ_DUPLICATION_PARAMS, rmStatus)
        }
        uvm::DISABLE_READ_DUPLICATION => {
            offset_of!(sys::UVM_DISABLE_READ_DUPLICATION_PARAMS, rmStatus)
        }
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
const MIGRATE_SEMAPHORE_ADDRESS_OFF: usize =
    std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, semaphoreAddress);
const MIGRATE_SEMAPHORE_PAYLOAD_OFF: usize =
    std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, semaphorePayload);
const MIGRATE_USER_SPACE_START_OFF: usize =
    std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, userSpaceStart);
const MIGRATE_USER_SPACE_LENGTH_OFF: usize =
    std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, userSpaceLength);
const MIGRATE_FLAGS_OFF: usize = std::mem::offset_of!(sys::UVM_MIGRATE_PARAMS, flags);
const _: () = {
    // Pin managed-compat status/output offsets to the vendor structs.
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

/// Serialized response for the transport.
#[derive(Default)]
pub struct Reply {
    pub bytes: Vec<u8>,
}

pub struct Session<A: RmAbi> {
    /// Selects the driver ABI without imposing Send/Sync bounds on its marker type.
    _abi: PhantomData<fn() -> A>,
    mirror: Mirror,
    /// Aux FD resolved by the device for this request and its stated owner.
    aux_fd_host: Option<RawFd>,
    /// Inline FD resolved by the device. Only an omitted owner permits local lookup.
    fd_field_host: Option<RawFd>,
    scratch: Vec<u8>,
    aux: Vec<u8>,
    /// Collects what `reply()` produced, until the carrier picks it up.
    out: Reply,
    /// Mapping ID to token, length, and device until the VMM fetches it.
    pending_maps: std::collections::HashMap<u64, PendingMap>,
    /// Mapping ID: session in the high 32 bits, per-session counter in the low 32.
    next_blob_id: u64,
    /// Backing client, semaphore pools, 0x71 arenas.
    pool: PoolState,
    /// Guest RAM supplied by the device before execution, for GPA translation.
    mem: Option<Mem>,
    /// Which guest process owns this session (the guest module's dense ID).
    /// 0 = not stated; such callers share one session.
    sub_id: u32,
    /// What the guest said about itself (name, guest PID). Display and RM
    /// attribution only; nothing enforced hangs off it.
    proc: proto::ProcInfo,
    /// System calls; replaced by a recorder in tests and fuzz targets.
    sys: Box<dyn NvSyscalls>,
    /// This session's VRAM accounting. Drop returns outstanding charges.
    vram: crate::vram::Books,
    /// One event ctl per RM client: GET_EVENT_DATA returns hObject without hClient.
    /// Separate ctls disambiguate identical handles in different clients (osapi.c:596-640).
    event_ctls: std::collections::HashMap<u32, nvrm_abi::NvDevice>,
    /// Session-wide OS-event counter. The extra bit range lets exhaustion fail without reuse.
    next_event_id: u64,
    next_waiter_generation: u64,
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
    /// Cancelled slots stay outside the free pool until the poller acknowledges.
    pending_unwatch: Vec<WaiterRetirement>,
    /// Owns retired event ctls until the device unregisters them from epoll.
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
    /// Per-client FD acquisition and release counters for leak diagnostics.
    ctl_taken: u64,
    ctl_freed_by_rmfree: u64,
    ctl_freed_by_close: u64,
    /// RM client to the guest FD token that created it.
    /// Closing that file tears down the client without an RM_FREE request.
    client_token: std::collections::HashMap<u32, u64>,
    /// Next number of held ctls that triggers a warning; doubles after each warning.
    ctl_warn_at: usize,
    /// GPU FD registered to the first ctl used for an OS-descriptor allocation.
    /// Multiple ctl owners in one session need separate registrations.
    osdesc_gpu: Option<nvrm_abi::NvDevice>,
}

/// Plausibility limit for one mapping, independent of the 8 GiB window.
/// Measured workloads requested at most 56 MiB (NVENC); CS2 used mappings
/// of at most 32 MiB. Larger mappings currently return EINVAL.
const MAX_MAP_LEN: u64 = 256 << 20;

#[cfg(test)]
const UVM_INITIALIZE: u32 = 0x3000_0001;
#[cfg(test)]
const UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE: u64 = 0x2;

/// A registered mapping that has not been fetched yet.
pub struct PendingMap {
    pub token: u64,
    pub len: u64,
    /// Device node, which determines the guest mapping cacheability.
    pub dev: Dev,
}

/// Guest callback metadata saved before OS-event substitution (1a').
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventReg {
    /// NVOS64.hRoot @0 of the alloc (`alloc_status_off`).
    pub h_client: u32,
    /// NVOS64.hObjectNew@8; filled after successful allocation.
    pub h_event: u32,
    /// 0x7e or 0x78, exactly as the guest asked (the host sent 0x79).
    pub class: u32,
    /// NV0005.notifyIndex @12, unstripped (flags and subdevice included).
    pub notify_index: u32,
    /// Original NV0005.data@16: the guest callback pointer.
    pub guest_data: u64,
    /// `req.target_token` the alloc rode on (log only for this class).
    pub token: u64,
    /// The fd:id given to NV_ESC_ALLOC_OS_EVENT on this client's event ctl.
    pub id: u32,
}

/// A private event ctl with one OS event, pooled per RM client.
/// Waiter firings carry no data (sem_surf.c); the FD identifies the waiter.
/// The event ID is bound to (hClient, fd) for the client's lifetime.
#[derive(Debug)]
pub struct WaiterSlot {
    ctl: Arc<OwnedFd>,
    id: u32,
}

impl std::os::fd::AsRawFd for WaiterSlot {
    fn as_raw_fd(&self) -> RawFd {
        self.ctl.as_raw_fd()
    }
}

/// A slot quarantined until the device finishes poller cancellation.
#[derive(Debug)]
pub struct WaiterRetirement {
    slot: WaiterSlot,
    reuse_for: Option<u32>,
}

impl WaiterRetirement {
    pub fn event_id(&self) -> u32 {
        self.slot.id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WaiterTarget {
    surface: u32,
    index: u64,
    wait_value: u64,
}

impl WaiterTarget {
    fn from_params(inline: &[u8], aux: &[u8]) -> Self {
        Self {
            surface: u32::from_le_bytes(inline[4..8].try_into().unwrap()),
            index: u64::from_le_bytes(aux[..8].try_into().unwrap()),
            wait_value: u64::from_le_bytes(aux[8..16].try_into().unwrap()),
        }
    }
}

/// One armed waiter: RM holds a listener that will post to `slot` once.
#[derive(Debug)]
struct ActiveWaiter {
    slot: WaiterSlot,
    generation: u64,
    target: WaiterTarget,
    h_client: u32,
    /// Guest callback pointer, returned in the firing's addr field.
    guest_kc: u64,
    /// `req.target_token` the registration rode on (log only).
    token: u64,
}

/// What `prepare` saw of a semsurf REGISTER_WAITER; `execute` arms it (RM
/// said NV_OK) or gives the slot back.
#[derive(Debug)]
pub struct PendingWaiter {
    slot: WaiterSlot,
    generation: u64,
    target: WaiterTarget,
    h_client: u32,
    guest_kc: u64,
    token: u64,
}

/// Registration requested by the session and installed by the device.
/// Sessions must not take backend locks; the device owns poll registration.
#[derive(Clone, Debug)]
pub enum Pollable {
    /// This session's private `/dev/nvidiactl` for RM client `h_client`:
    /// readable = a substituted event fired; drained with
    /// [`Session::drain_os_events`].
    EventCtl { h_client: u32, fd: RawFd },
    /// RM wakes this guest-owned FD; the guest drains GET_EVENT_DATA.
    /// `owner` identifies another session for a translated NV0005.data FD;
    /// None means the caller.
    Client {
        token: u64,
        fd: RawFd,
        owner: Option<u32>,
    },
    /// One-shot waiter for the dedicated poller, not epoll (see waiters.rs).
    /// The device retires it with [`Session::semsurf_wake`].
    Waiter {
        registration: RegistrationId,
        fd: Arc<OwnedFd>,
    },
}

/// One drained firing, ready to become a KIND_EVENT_FIRED Req.
#[derive(Clone, Copy, Debug)]
pub struct Fired {
    pub reg: EventReg,
    /// NvUnixEvent.info32 (0 on the substituted path, carried anyway).
    pub info32: u32,
}

// Derive event layouts from bindgen to keep one source of field offsets.
const NV0005_HCLASS: usize = std::mem::offset_of!(sys::NV0005_ALLOC_PARAMETERS, hClass);
const NV0005_NOTIFYINDEX: usize = std::mem::offset_of!(sys::NV0005_ALLOC_PARAMETERS, notifyIndex);
const NV0005_DATA: usize = std::mem::offset_of!(sys::NV0005_ALLOC_PARAMETERS, data);

/// Semaphore waiter controls from ctrl00da.h. REGISTER uses handle@24;
/// UNREGISTER omits newValue and uses handle@16. Assertions pin both.
const CTRL_SEMSURF_REGISTER_WAITER: u32 = sys::NV_SEMAPHORE_SURFACE_CTRL_CMD_REGISTER_WAITER;
const CTRL_SEMSURF_UNREGISTER_WAITER: u32 = sys::NV_SEMAPHORE_SURFACE_CTRL_CMD_UNREGISTER_WAITER;
const _: () = {
    assert!(std::mem::offset_of!(sys::NVOS54_PARAMETERS, hObject) == 4);
    assert!(
        std::mem::offset_of!(sys::NV_SEMAPHORE_SURFACE_CTRL_REGISTER_WAITER_PARAMS, index) == 0
    );
    assert!(
        std::mem::offset_of!(
            sys::NV_SEMAPHORE_SURFACE_CTRL_REGISTER_WAITER_PARAMS,
            waitValue
        ) == 8
    );
    assert!(
        std::mem::offset_of!(
            sys::NV_SEMAPHORE_SURFACE_CTRL_UNREGISTER_WAITER_PARAMS,
            index
        ) == 0
    );
    assert!(
        std::mem::offset_of!(
            sys::NV_SEMAPHORE_SURFACE_CTRL_UNREGISTER_WAITER_PARAMS,
            waitValue
        ) == 8
    );
    assert!(
        std::mem::offset_of!(
            sys::NV_SEMAPHORE_SURFACE_CTRL_REGISTER_WAITER_PARAMS,
            notificationHandle
        ) == 24
    );
    assert!(
        std::mem::offset_of!(
            sys::NV_SEMAPHORE_SURFACE_CTRL_UNREGISTER_WAITER_PARAMS,
            notificationHandle
        ) == 16
    );
};

const _: () = {
    assert!(std::mem::size_of::<sys::NvUnixEvent>() == 16);
    assert!(std::mem::size_of::<sys::NV0005_ALLOC_PARAMETERS>() == 24);
    // The numbers the (1a') comment has quoted since 2026-08-08.
    assert!(NV0005_HCLASS == 8 && NV0005_NOTIFYINDEX == 12 && NV0005_DATA == 16);
    assert!(std::mem::size_of::<sys::NVOS41_PARAMETERS>() == 16);
};

/// Cached debug level: unset/empty = 0, `2` = every call, other values = 1.
/// Launchers pass empty values, so presence alone must not enable logging.
/// Cache the lookup because callers include the per-frame event path.
pub(crate) fn debug_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var_os("LEA_DEBUG") {
        None => 0,
        Some(v) if v.is_empty() => 0,
        Some(v) if v == "2" => 2,
        Some(_) => 1,
    })
}

/// Host-only managed compatibility switch; read once.
fn managed_compat() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LEA_MANAGED_COMPAT").ok().as_deref() == Some("1"))
}

/// Leave the driver's product name unchanged when `LEA_GPU_NAME_RAW=1`.
/// Read once. Empty values and `0` stay disabled, as with managed compat.
fn gpu_name_raw() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LEA_GPU_NAME_RAW").ok().as_deref() == Some("1"))
}

/// Match LEA_CTRL_DUMP command IDs, parsed once as comma-separated hex.
fn ctrl_dump_wanted(cmd: u32) -> bool {
    static LIST: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let list = LIST.get_or_init(|| {
        std::env::var("LEA_CTRL_DUMP")
            .unwrap_or_default()
            .split(',')
            .filter_map(|t| {
                let t = t.trim().trim_start_matches("0x");
                (!t.is_empty())
                    .then(|| u32::from_str_radix(t, 16).ok())
                    .flatten()
            })
            .collect()
    });
    !list.is_empty() && list.contains(&cmd)
}

/// Status offset for RM_ALLOC: NVOS64 uses byte 40, NVOS21 byte 28.
/// Both forms are accepted by `xlate::embedded_ptr`; neither may bypass
/// status-dependent checks. Their root/parent/object handles share 0/4/8.
fn alloc_status_off(inline_len: usize) -> Option<usize> {
    match inline_len {
        n if n >= 48 => Some(40),
        32 => Some(28),
        _ => None,
    }
}

/// Preparation failure: guest errno and a diagnostic reason.
#[derive(Debug)]
struct Refusal {
    errno: i32,
    why: String,
}

impl Refusal {
    fn new(errno: i32, why: String) -> Self {
        Refusal { errno, why }
    }
    fn msg(errno: i32, why: &str) -> Self {
        Refusal {
            errno,
            why: why.to_string(),
        }
    }
}

/// Execution decision from preparation (question 5).
enum Action {
    /// The real ioctl on the target FD.
    Forward,
    /// Emulate success; a real pool would collide with our external mapping.
    FakeSemaphorePool,
    /// "Managed light" (LEA_MANAGED_COMPAT=1): semantic no-op answers for
    /// the managed-only commands; the evidence sits at the execute branch.
    FakeManaged,
    /// Refuse at the VM cap using native OOM semantics: ioctl succeeds,
    /// RM status is NV_ERR_NO_MEMORY, and libcuda reports a CUDA OOM.
    FakeVramFull,
}

/// Control data from preparation. Translated pointers target the session's
/// scratch/aux allocations, which must stay fixed until execution.
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
    /// Offset of `status` in this alloc's parameter struct; 40 for
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
    /// NVOS32 allocation: settle from scratch using its distinct offsets.
    vram_vidheap: bool,
    /// The request as the guest sent it, whenever the ledger looked at
    /// it (reserved or refused). Kept for the log lines in execute: by
    /// then RM has written its answer over `attr`.
    vram_ask: Option<crate::vram::Ask>,
    /// Location/count of FB_GET_INFO V1's nested output array in aux.
    /// Capture during preparation for reply rewriting; V2 uses an inline array.
    fb_info_list: Option<(usize, usize)>,
}

impl<A: RmAbi> Session<A> {
    /// Create a session sharing the VM's VRAM and guest-page budgets.
    pub fn detached_proc(
        sub_id: u32,
        vram: Arc<crate::vram::Ledger>,
        pins: Arc<PinBudget>,
    ) -> Result<Self> {
        let mut s = Self::build(sub_id, vram, pins)?;
        s.next_blob_id = (sub_id as u64) << 32 | 1;
        s.sub_id = sub_id;
        Ok(s)
    }

    fn build(sub_id: u32, vram: Arc<crate::vram::Ledger>, pins: Arc<PinBudget>) -> Result<Self> {
        Ok(Self {
            _abi: PhantomData,
            mirror: Mirror::new(),
            aux_fd_host: None,
            fd_field_host: None,
            scratch: Vec::with_capacity(proto::MAX_PAYLOAD),
            aux: Vec::with_capacity(proto::MAX_PAYLOAD),
            out: Reply::default(),
            pending_maps: Default::default(),
            // 0 stays free: the guest reads it as a failed registration.
            next_blob_id: 1,
            pool: PoolState::with_budget(pins),
            mem: None,
            sub_id: 0,
            proc: proto::ProcInfo::default(),
            sys: Box::new(RealSyscalls),
            vram: crate::vram::Books::new(sub_id, vram),
            osdesc_gpu: None,
            event_ctls: Default::default(),
            // 0 stays free so an untouched field never looks like an id.
            next_event_id: 1,
            next_waiter_generation: 1,
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

    /// Substitute a guest kernel callback with a host OS event.
    ///
    /// RM rejects userspace kernel callbacks (event_api.c:74-88). The original
    /// guest pointer stays in EventReg and returns through the event queue.
    fn alloc_os_event_id(&mut self, h_client: u32) -> Result<u32, Refusal> {
        use std::collections::hash_map::Entry;
        use std::os::fd::AsRawFd;
        let id = self.take_event_id()?;
        // Announce only newly opened ctls so the device does not poll an FD twice.
        // `fresh` carries that decision beyond the map entry's mutable borrow.
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
            self.pending_pollables
                .push(Pollable::EventCtl { h_client, fd });
            self.note_ctl_taken();
        }
        self.alloc_os_event_on(fd, h_client, id)?;
        Ok(id)
    }

    /// IDs stay unique for this session, including after an allocation fails.
    fn take_event_id(&mut self) -> Result<u32, Refusal> {
        let id = u32::try_from(self.next_event_id)
            .map_err(|_| Refusal::msg(libc::ENOSPC, "OS-event id space exhausted"))?;
        self.next_event_id += 1;
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
        let id = self.take_event_id()?;
        let ctl = Self::open_event_ctl()
            .map_err(|e| Refusal::new(libc::EIO, format!("waiter ctl: {e}")))?;
        self.alloc_os_event_on(ctl.as_raw_fd(), h_client, id)?;
        self.note_ctl_taken();
        Ok(WaiterSlot {
            ctl: Arc::new(ctl.into_owned_fd()),
            id,
        })
    }

    fn take_waiter_generation(&mut self) -> Result<u64, Refusal> {
        let generation = self.next_waiter_generation;
        self.next_waiter_generation = generation
            .checked_add(1)
            .ok_or_else(|| Refusal::msg(libc::ENOSPC, "waiter generation space exhausted"))?;
        Ok(generation)
    }

    /// A completion proves its poll is finished; only the matching arm may recycle.
    pub fn semsurf_wake(&mut self, registration: RegistrationId) -> Option<(u32, u64, u64)> {
        if self.waiters.get(&registration.event_id)?.generation != registration.generation {
            return None;
        }
        let w = self.waiters.remove(&registration.event_id)?;
        let out = (w.h_client, w.guest_kc, w.token);
        self.waiter_pool.entry(w.h_client).or_default().push(w.slot);
        self.ev_fired += 1;
        Some(out)
    }

    pub fn take_unwatch(&mut self) -> Vec<WaiterRetirement> {
        std::mem::take(&mut self.pending_unwatch)
    }

    /// Return cancelled slots only after the poller acknowledges quiescence.
    pub fn finish_unwatch(&mut self, retired: Vec<WaiterRetirement>) {
        for retirement in retired {
            if let Some(client) = retirement.reuse_for {
                self.waiter_pool
                    .entry(client)
                    .or_default()
                    .push(retirement.slot);
            }
        }
    }

    /// Take retired event ctls. Unregister from epoll before dropping them.
    pub fn take_ctl_unwatch(&mut self) -> Vec<(u32, nvrm_abi::NvDevice)> {
        std::mem::take(&mut self.pending_ctl_unwatch)
    }

    /// The guest process this session belongs to, for the census line.
    pub fn proc_name(&self) -> String {
        self.proc.comm_str().to_string()
    }

    /// FD census: mirror live/ever, event ctls, pooled/armed waiters,
    /// pending removal, and cached GPU FD.
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

    /// Release one client's backing, registrations, and waiter slots.
    /// Return FDs to the device for poll removal before closing them.
    /// Pooled slots cannot survive their client: RM has destroyed their OS-event IDs.
    fn release_client(&mut self, h_client: u32) -> usize {
        let mut freed = 0;
        self.pool.drop_osdesc_client(h_client);
        self.events.retain(|&(c, _), _| c != h_client);
        self.client_token.remove(&h_client);
        if let Some(ctl) = self.event_ctls.remove(&h_client) {
            freed += 1;
            self.pending_ctl_unwatch.push((h_client, ctl));
        }
        for s in self.waiter_pool.remove(&h_client).unwrap_or_default() {
            freed += 1;
            self.pending_unwatch.push(WaiterRetirement {
                slot: s,
                reuse_for: None,
            });
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
                self.pending_unwatch.push(WaiterRetirement {
                    slot: w.slot,
                    reuse_for: None,
                });
            }
        }
        freed
    }

    /// Release clients created on a closed guest FD; RM does not emit RM_FREE for them.
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

    /// Count an acquired ctl and log growth at exponentially spaced thresholds.
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

    /// Open the event node. Tests use /dev/null and replace all ioctls.
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

    /// Best-effort NV_ESC_FREE_OS_EVENT on the client's ctl.
    /// Failures are logged; closing the ctl releases any remaining registration.
    fn free_os_event(&mut self, h_client: u32, id: u32) {
        use std::os::fd::AsRawFd;
        let Some(ctl) = self.event_ctls.get(&h_client) else {
            return;
        };
        let fd = ctl.as_raw_fd();
        let mut p = nvrm_abi::nvgpu::IoctlFreeOsEvent {
            h_client,
            h_device: 0,
            fd: id,
            status: 0,
        };
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

    /// Event counters for the process-exit summary.
    pub fn event_stats(&self) -> (u64, u64, u64, u64) {
        (
            self.ev_registered,
            self.ev_fired,
            self.ev_unmatched,
            self.ev_dataless,
        )
    }

    /// Whether a client still has an event ctl. After a failed drain, remove
    /// the old poll registration so a replacement ctl can be registered.
    pub fn has_event_ctl(&self, h_client: u32) -> bool {
        self.event_ctls.contains_key(&h_client)
    }

    /// Drain up to 256 events from one client's level-triggered ctl.
    ///
    /// GET_EVENT_DATA returns one NvUnixEvent at a time (osapi.c:504-535).
    /// The ctl identifies hClient; hObject identifies its registration.
    /// NV_ERR_OPERATING_SYSTEM means a dataless wake with no queued event.
    /// Other errors close the ctl to prevent a repeated level-triggered wake.
    pub fn drain_os_events(&mut self, h_client: u32) -> Vec<Fired> {
        use std::os::fd::AsRawFd;
        let mut out = Vec::new();
        let Some(ctl) = self.event_ctls.get(&h_client) else {
            return out;
        };
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
                    // The dataless answer: RM has nothing queued; the fd
                    // is drained, and a `break` here leaves it quiet.
                    self.ev_dataless += 1;
                    break;
                }
                // Other errors can leave the FD level-readable. Retire it instead of
                // spinning the device worker on repeated epoll wakeups.
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
                    out.push(Fired {
                        reg: *reg,
                        info32: ev.info32,
                    });
                }
                None => self.ev_unmatched += 1,
            }
            if p.MoreEvents == 0 {
                break;
            }
        }
        out
    }

    /// Open a GPU FD registered against ctl_fd for OS-descriptor allocations.
    ///
    /// The kernel-form allocation uses the ctl node, but its userspace NVOS02
    /// replacement requires a GPU node (escape.c:399,486). REGISTER_FD binds
    /// that node to the client. The current cache assumes one ctl owner and GPU 0.
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

    /// Guest-provided process name and PID for RM attribution.
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

    /// Register an open test/fuzz FD so messages can reach translation checks.
    /// Production tokens are created only by `on_open`.
    pub fn insert_token(&mut self, fd: std::os::fd::OwnedFd, kind: Dev) -> u64 {
        self.mirror.insert(fd, kind)
    }

    /// The device hands in the current guest RAM before every exec. Cheap
    /// (an Arc clone), and required as soon as a UvmPoolBack arrives.
    pub fn set_guest_mem(&mut self, mem: Option<Mem>) {
        self.mem = mem;
    }

    /// Resolve a token in this session only; the device selects the owner.
    pub fn mirror_raw(&self, token: u64) -> Option<RawFd> {
        self.mirror.raw(token)
    }

    /// Dispatch without device-resolved FDs, for tests and fuzzing.
    /// Explicit owners require handle_msg_with; omitted inline owners use this mirror.
    pub fn handle_msg(&mut self, bytes: &[u8]) -> Result<Reply> {
        self.handle_msg_with(bytes, None, None)
    }

    /// Dispatch one request with FDs resolved by the device against their owners.
    /// Transport failures return Err; guest errors return a reply with negative errno.
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
        // handlers reach for self.scratch; otherwise on_ioctl borrows twice.
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
        self.reply(
            Rsp {
                seq: req.seq,
                ..Rsp::default()
            },
            &[],
            &[],
        )
    }

    fn on_open(&mut self, req: &Req, payload: &[u8]) -> Result<()> {
        use nvrm_abi::NvDevice;

        // Adopt optional process identity on the first Open only; later Opens
        // must not replace it.
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
                self.vram.announce(self.proc.pid, &self.proc.comm_str());
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

        // For Gpu, ioctl_nr is a guest-supplied device index. Bound it before
        // constructing /dev/nvidiaN; NV_MAX_DEVICES is the driver's limit.
        const NV_MAX_DEVICES: u32 = 32;
        if req.dev_tag == DevTag::Gpu as u32 && req.ioctl_nr >= NV_MAX_DEVICES {
            return self.reply_err(req.seq, libc::EINVAL, "GPU index beyond NV_MAX_DEVICES");
        }

        let Some(kind) = dev_of(req.dev_tag) else {
            return self.reply_err(req.seq, libc::EINVAL, "dev_tag");
        };
        let dev = match kind {
            Dev::Ctl => NvDevice::open_ctl(),
            Dev::Gpu => NvDevice::open_gpu(req.ioctl_nr),
            Dev::Uvm => NvDevice::open("/dev/nvidia-uvm"),
            Dev::UvmTools => {
                return self.reply_err(req.seq, libc::ENOTSUP, "UVM tools ABI is unsupported")
            }
        };
        let dev = match dev {
            Ok(d) => d,
            Err(e) => return self.reply_err(req.seq, libc::EACCES, &format!("open: {e}")),
        };

        // Ownership of the real FD passes to the mirror; as long as the
        // token lives, the FD lives.
        let owned = dev.into_owned_fd();
        let token = self.mirror.insert(owned, kind);

        // Nothing to mirror back: the guest creates a placeholder FD of its
        // own and routes via the token.
        self.reply(
            Rsp {
                seq: req.seq,
                token,
                ..Rsp::default()
            },
            &[],
            &[],
        )
    }

    /// Record a mapping for the host-visible window. Cloud-hypervisor later
    /// mmaps the FD sent by SHMEM_MAP: RM binds that context to the OFD
    /// (`nv-mmap.c`), after the forwarded RM_MAP_MEMORY validates the client.
    fn on_map_prepare(&mut self, req: &Req) -> Result<()> {
        if self.mirror.raw(req.target_token).is_none() {
            return self.reply_err(req.seq, libc::EBADF, "MapPrepare on an unknown token");
        }
        if req.map_len == 0 || req.map_len > MAX_MAP_LEN {
            return self.reply_err(req.seq, libc::EINVAL, "MapPrepare: implausible length");
        }
        let Some(dev) = self.mirror.kind(req.target_token) else {
            return self.reply_err(req.seq, libc::EBADF, "MapPrepare: unknown token");
        };
        if dev_of(req.dev_tag) != Some(dev) {
            return self.reply_err(
                req.seq,
                libc::EINVAL,
                "MapPrepare: device tag differs from token",
            );
        }
        let id = self.next_blob_id;
        // The high half is the session ID. The low half wraps but skips zero:
        // blob ID 0 means registration failure to the guest.
        let low = match (id + 1) & 0xffff_ffff {
            0 => 1,
            n => n,
        };
        self.next_blob_id = (self.next_blob_id & !0xffff_ffff) | low;
        self.pending_maps.insert(
            id,
            PendingMap {
                token: req.target_token,
                len: req.map_len,
                dev,
            },
        );
        self.reply(
            Rsp {
                seq: req.seq,
                token: id,
                ..Rsp::default()
            },
            &[],
            &[],
        )
    }

    /// Back a semaphore pool with 16-byte GPA runs from the guest.
    /// req.addr/map_len name the pool; target_token names its UVM FD.
    fn on_uvm_pool_back(&mut self, req: &Req, payload: &[u8]) -> Result<()> {
        if self.mirror.kind(req.target_token) != Some(Dev::Uvm) {
            return self.reply_err(req.seq, libc::EBADF, "UvmPoolBack requires a UVM token");
        }
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
        // addr and map_len are guest words; from here on they carry the
        // type whose arithmetic exists only checked (guest_words.rs).
        match self.pool.back_pool_for::<A>(
            &mem,
            req.target_token,
            uvm_fd,
            GuestAddr::new(req.addr),
            GuestLen::new(req.map_len),
            &runs,
        ) {
            Ok(()) => {
                eprintln!(
                    "vhost-user-nvrm: pool @{:#x} ({} KiB, {} runs) attached to GPU VA",
                    req.addr,
                    req.map_len >> 10,
                    runs.len()
                );
                self.reply(
                    Rsp {
                        seq: req.seq,
                        ..Rsp::default()
                    },
                    &[],
                    &[],
                )
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
        borrowed
            .try_clone_to_owned()
            .ok()
            .map(|o| (o.into(), len, dev))
    }

    fn on_close(&mut self, req: &Req) -> Result<()> {
        if let Err(error) = self.pool.close_uvm(req.target_token) {
            eprintln!("vhost-user-nvrm: pool cleanup retained on token close: {error:#}");
        }
        self.mirror.remove(req.target_token);
        // Close retires our accounting; driver cleanup may be deferred.
        self.vram.close_token(req.target_token);
        // ... and so does everything this session holds FOR those clients.
        self.close_clients_of_token(req.target_token);
        self.reply(
            Rsp {
                seq: req.seq,
                ..Rsp::default()
            },
            &[],
            &[],
        )
    }

    fn on_ioctl(&mut self, req: &Req, payload: &[u8]) -> Result<()> {
        match self.prepare(req, payload) {
            Ok(plan) => self.execute(plan),
            Err(Refusal { errno, why }) => self.reply_err(
                req.seq,
                errno,
                &format!("ioctl dev={} nr={:#x}: {why}", req.dev_tag, req.ioctl_nr),
            ),
        }
    }

    /// Validate guest buffers and build the translated request (question 5).
    /// Step markers are shared with `execute` and other source references.
    /// `Plan` borrows addresses in scratch/aux; keep those buffers unchanged
    /// until execution. Guest-buffer validation finishes before event allocation.
    fn prepare(&mut self, req: &Req, payload: &[u8]) -> std::result::Result<Plan, Refusal> {
        let mut inline_len = req.inline_len as usize;
        let mut aux_len = req.aux_len as usize;

        if payload.len() < inline_len + aux_len
            || inline_len > proto::MAX_PAYLOAD
            || aux_len > proto::MAX_AUX
        {
            return Err(Refusal::new(
                libc::EINVAL,
                format!(
                    "Length: inline {inline_len} aux {aux_len} payload {}",
                    payload.len()
                ),
            ));
        }

        // Device tags distinguish frontend _IOC requests from raw UVM numbers.
        // An unknown tag cannot be routed safely.
        let Some(dev) = dev_of(req.dev_tag) else {
            return Err(Refusal::new(
                libc::EINVAL,
                format!("dev_tag {} unknown", req.dev_tag),
            ));
        };

        let frontend = !dev.is_uvm();

        if let Some(root) = crate::client_policy::private_client::<A>(
            dev,
            req.ioctl_nr,
            &payload[..inline_len],
            &payload[inline_len..inline_len + aux_len],
            |root| self.pool.is_private_client(root),
        ) {
            return Err(Refusal::new(
                libc::EPERM,
                format!("backend-owned RM client {root:#x}"),
            ));
        }

        // Enforce blocked controls before token lookup. The guest-side table is
        // only an optimization. In particular, SET_SUB_PROCESS_ID must not
        // replace the host-assigned label; see `xlate::blocked_ctrls`.
        if frontend && req.ioctl_nr == sys::NV_ESC_RM_CONTROL && inline_len >= 12 {
            let cmd = u32::from_le_bytes(payload[8..12].try_into().unwrap());
            if nvrm_abi::xlate::ctrl_blocked(cmd) {
                return Err(Refusal::new(
                    libc::EPERM,
                    format!("control {cmd:#x} is never forwarded"),
                ));
            }
        }

        let shape = crate::request_shape::validate::<A>(
            dev,
            req,
            &payload[..inline_len],
            &payload[inline_len..inline_len + aux_len],
        )
        .map_err(|e| Refusal::msg(e.errno, e.why))?;

        // Routing FD: the host FD behind this call's token.
        let target_fd = match self.mirror.raw(req.target_token) {
            Some(f) => f,
            None => return Err(Refusal::msg(libc::EBADF, "unknown token")),
        };

        if self.mirror.kind(req.target_token) != Some(dev) {
            return Err(Refusal::msg(
                libc::EINVAL,
                "device tag differs from opened token",
            ));
        }

        self.scratch.clear();
        self.scratch.extend_from_slice(&payload[..inline_len]);
        self.aux.clear();
        self.aux
            .extend_from_slice(&payload[inline_len..inline_len + aux_len]);

        shape.clear_unused_pointers(&mut self.scratch, &mut self.aux);

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
                // Resolved against the explicit owner session. EGLImage imports may
                // name an FD exported by another guest process.
                f
            } else if req.fd_field_proc == NONE_U32 {
                // Legacy requests omit the owner and use the caller's mirror.
                match self.mirror.raw(req.fd_field_token) {
                    Some(f) => f,
                    None => return Err(Refusal::msg(libc::EBADF, "fd_field_token")),
                }
            } else {
                return Err(Refusal::msg(libc::EBADF, "fd_field_proc/token unresolved"));
            };
            self.scratch[off..off + 4].copy_from_slice(&host_num.to_le_bytes());
        }

        // (2) The embedded pointer targets host-owned aux until the ioctl ends.
        // Recompute the driver's expected length from NVOS54.paramsSize or the
        // allocation class; trusting guest aux_len could expose the daemon heap.

        // (0x79 return channel) RM wakes the registering FD's waitqueue
        // (osapi.c/nv.c). Watch that FD and return its token to the guest.
        // A later refusal can leave an unused watch; the device deduplicates.
        if frontend && req.ioctl_nr == nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT {
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
        if frontend && req.gpa_run_count > 0 {
            // NVOS02 carries address/limit inline and page runs in aux. NVKMS
            // uses NVOS64 class 0x71 with params followed by runs in aux.
            // Older backends reject the latter shape, so no protocol bump is needed.
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
                return Err(Refusal::msg(
                    libc::EINVAL,
                    "0x71 kernel form: params too short",
                ));
            }
            let run_bytes = if kern_form {
                &self.aux[OSDESC_PARAMS..]
            } else {
                &self.aux[..]
            };
            let Some(runs) = GpaRun::decode(run_bytes, req.gpa_run_count as usize) else {
                return Err(Refusal::msg(libc::EINVAL, "0x71: runs unreadable"));
            };
            // NVOS02 limit@32 is inclusive: length = limit + 1.
            // Reject overflow here, before Arena validates the resulting range.
            let limit = if kern_form {
                u64::from_le_bytes(self.aux[OSDESC_LIMIT..OSDESC_LIMIT + 8].try_into().unwrap())
            } else {
                u64::from_le_bytes(self.scratch[32..40].try_into().unwrap())
            };
            let Some(total) = limit.checked_add(1) else {
                return Err(Refusal::msg(libc::EINVAL, "0x71: limit == u64::MAX"));
            };
            match self
                .pool
                .arena_for_osdesc(&mem, &runs, GuestLen::new(total))
            {
                Ok((arena, va)) => {
                    if kern_form {
                        self.aux[OSDESC_DESC..OSDESC_DESC + 8].copy_from_slice(&va.to_le_bytes());
                        // The replacement is a host virtual address, not a dma_buf pointer.
                        // Update descriptorType with it or RM interprets the wrong object.
                        const OSDESC_TYPE: usize = std::mem::offset_of!(
                            sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS,
                            descriptorType
                        );
                        self.aux[OSDESC_TYPE..OSDESC_TYPE + 4].copy_from_slice(
                            &sys::NVOS32_DESCRIPTOR_TYPE_VIRTUAL_ADDRESS.to_le_bytes(),
                        );
                        // RM's NVOS02 conversion uses TYPE_IMAGE for a host virtual address
                        // (rmapi_deprecated_allocmemory.c). Replace NVKMS's TYPE_PRIMARY
                        // along with its original descriptor type.
                        const OSDESC_STYPE: usize =
                            std::mem::offset_of!(sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS, type_);
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
                // Direct osmemdesc allocation rejects VIRTUAL_ADDRESS and restricts
                // DMA_BUF/SGT descriptors to kernel clients (measured status 0x56).
                // Use NVOS02's userspace conversion with TYPE_IMAGE instead.
                // Root/parent/object/class and status retain their offsets; restore
                // the guest's original middle fields in the reply.
                const NVOS02_FLAGS: usize = 16;
                const NVOS02_PMEM: usize = 24;
                const NVOS02_LIMIT: usize = 32;
                // The escape expects NVOS02 plus fd: 56 bytes. NVOS02 is padded
                // to 48 bytes by its aligned u64 fields, so fd is at 48, not 44.
                // A wrong size or fd offset causes EINVAL.
                const NVOS02_FD: usize = 48;
                const NVOS02_LEN: usize = 56;
                if self.scratch.len() < 48 {
                    return Err(Refusal::msg(
                        libc::EINVAL,
                        "0x71 kernel form: inline too short",
                    ));
                }
                osdesc_nvos64_tail = Some(self.scratch[16..40].to_vec());
                let va =
                    u64::from_le_bytes(self.aux[OSDESC_DESC..OSDESC_DESC + 8].try_into().unwrap());
                // The flags are not decoration: RmAllocOsDescriptor reads
                // them FIRST and answers NV_ERR_INVALID_FLAGS before it has
                // looked at a single page (escape.c:206). Zero would already
                // fail there; MAPPING_DEFAULT is not NO_MAP.
                const OSDESC_ATTR: usize =
                    std::mem::offset_of!(sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS, attr);
                const OSDESC_ATTR2: usize =
                    std::mem::offset_of!(sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS, attr2);
                let attr =
                    u32::from_le_bytes(self.aux[OSDESC_ATTR..OSDESC_ATTR + 4].try_into().unwrap());
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
                self.scratch[NVOS02_FLAGS..NVOS02_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
                self.scratch[NVOS02_PMEM..NVOS02_PMEM + 8].copy_from_slice(&va.to_le_bytes());
                self.scratch[NVOS02_LIMIT..NVOS02_LIMIT + 8].copy_from_slice(&limit.to_le_bytes());
                if self.scratch.len() < NVOS02_LEN {
                    self.scratch.resize(NVOS02_LEN, 0);
                }
                // RM_ALLOC_MEMORY requires the GPU node; RM_ALLOC class 0x71 uses
                // ctl (escape.c). The rewritten call must change its target FD too.
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

        // Rewritten NVOS02 consumed the params into an arena. Other embedded
        // pointers use the host-validated offset and buffer length.
        if req.embedded_ptr_off != NONE_U32 && rewrite_ioctl_nr.is_none() {
            let off = req.embedded_ptr_off as usize;
            let addr = self.aux.as_mut_ptr() as u64;
            self.scratch[off..off + 8].copy_from_slice(&addr.to_le_bytes());
        }

        // (1b) FD field INSIDE the aux buffer (NvP64, 8 bytes): guest token
        //      -> host FD. The counterpart to (1) for fds that sit in the
        //      alloc params (NV0005.data for NV01_EVENT_OS_EVENT).
        if req.aux_fd_field_off != NONE_U32 {
            let off = req.aux_fd_field_off as usize;
            // Derive FD width from the host descriptor: NvP64 for allocations,
            // NvS32 for controls. An eight-byte control write would clobber flags.
            let ctrl_fd = if frontend && req.ioctl_nr == sys::NV_ESC_RM_CONTROL && inline_len >= 32
            {
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
                // Use the resolved owner's mirror only. Session-local token values
                // can alias an unrelated FD in the caller's mirror.
                let host_num = match self.aux_fd_host {
                    Some(f) => f,
                    None => return Err(Refusal::msg(libc::EBADF, "aux_fd_field_token")),
                };
                if width == 4 {
                    self.aux[off..off + 4].copy_from_slice(&host_num.to_le_bytes());
                } else {
                    self.aux[off..off + 8].copy_from_slice(&(host_num as i64).to_le_bytes());
                    // (0x79 alloc form) RM wakes the FD in NV0005.data, which may
                    // belong to another guest session. Watch its resolved owner/token.
                    // The device deduplicates; a refused alloc can leave an unused watch.
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

        // (2b) Sparse nested descriptors were validated against the host table.
        // NULL and zero-length targets have no descriptor on the wire.
        let mut fb_info_list: Option<(usize, usize)> = None;
        for d in &req.nested[..req.nested_count as usize] {
            let po = d.ptr_off as usize;
            let ao = d.aux_off as usize;
            // SAFETY: request_shape checked every target range within aux.
            let addr = unsafe { self.aux.as_mut_ptr().add(ao) } as u64;
            self.aux[po..po + 8].copy_from_slice(&addr.to_le_bytes());
            if frontend
                && self.vram.enabled()
                && req.ioctl_nr == sys::NV_ESC_RM_CONTROL
                && u32::from_le_bytes(self.scratch[8..12].try_into().unwrap())
                    == crate::vram::CMD_FB_GET_INFO
            {
                let asked = u32::from_le_bytes(self.aux[..4].try_into().unwrap()) as usize;
                fb_info_list = Some((ao, asked));
            }
        }

        // Preserve UVM_INITIALIZE flags. The pool path needs no cross-process
        // UVM mmap; forcing MULTI_PROCESS_SHARING disables pageable access.

        // (3) The real ioctl. Frontend: rebuild the _IOC encoding.
        //     UVM: raw number, no _IOC encoding.
        let request: libc::c_ulong = if dev.is_uvm() {
            req.ioctl_nr as libc::c_ulong
        } else if inline_len > ((1 << 14) - 1) {
            // Larger payloads need nv_ioctl_xfer_t because _IOC_SIZE has 14 bits.
            // That encoding is not implemented.
            return Err(Refusal::msg(libc::EMSGSIZE, "XFER repack not built yet"));
        } else {
            // Re-encode both number and size when converting NVOS64 to NVOS02.
            iowr_raw(rewrite_ioctl_nr.unwrap_or(req.ioctl_nr), inline_len as u32) as libc::c_ulong
        };

        // Do NOT forward ALLOC_SEMAPHORE_POOL; otherwise the real UVM
        // would create the pool at GPU VA `base` and collide with the
        // external mapping the guest requests moments later via
        // UvmPoolBack. Fake success, rmStatus = NV_OK.
        let fake_semaphore_pool = dev.is_uvm() && req.ioctl_nr == UVM_ALLOC_SEMAPHORE_POOL;

        // Opt-in managed compatibility: guest pages remain an EXTERNAL_RANGE
        // in sysmem, with no migration or read duplication. Complete supported
        // operations as no-ops; normally UVM rejects them with status 0x1e.
        // MIGRATE returns NV_OK and zero userSpaceStart/Length so userspace
        // will not try move_pages itself (uvm_ioctl.h).
        let fake_managed = dev.is_uvm()
            && matches!(req.ioctl_nr, 42 | 43 | 44 | 45 | 46 | 47 | 51)
            && managed_compat();
        // All guest-buffer validation is complete before acquiring OS events.
        // (1a') Replace privileged guest callbacks with host OS events.
        // Rewrite both classes: RM checks the outer allocation for privilege
        // and the parameter class for the event type.
        let mut event_reg: Option<EventReg> = None;
        if frontend
            && req.ioctl_nr == sys::NV_ESC_RM_ALLOC
            && inline_len >= 16
            && self.aux.len() >= std::mem::size_of::<sys::NV0005_ALLOC_PARAMETERS>()
        {
            let hclass = u32::from_le_bytes(self.scratch[12..16].try_into().unwrap());
            if hclass == sys::NV01_EVENT_KERNEL_CALLBACK
                || hclass == sys::NV01_EVENT_KERNEL_CALLBACK_EX
            {
                let h_client = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                // NV0005 fields: parent@0, source@4, class@8, notifyIndex@12,
                // data@16. Save the callback pointer and full notifyIndex before
                // substitution; the guest needs both when the event fires.
                let guest_data =
                    u64::from_le_bytes(self.aux[NV0005_DATA..NV0005_DATA + 8].try_into().unwrap());
                let notify_index = u32::from_le_bytes(
                    self.aux[NV0005_NOTIFYINDEX..NV0005_NOTIFYINDEX + 4]
                        .try_into()
                        .unwrap(),
                );
                let id = self.alloc_os_event_id(h_client)?;
                self.aux[NV0005_HCLASS..NV0005_HCLASS + 4]
                    .copy_from_slice(&sys::NV01_EVENT_OS_EVENT.to_le_bytes());
                self.aux[NV0005_DATA..NV0005_DATA + 8].copy_from_slice(&(id as u64).to_le_bytes());
                self.scratch[12..16].copy_from_slice(&sys::NV01_EVENT_OS_EVENT.to_le_bytes());
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

        // (1a''') Replace kernel callback pointers in semaphore waiters.
        // For userspace RM clients, notificationHandle is a u32 OS-event ID;
        // a guest kernel pointer exceeds that range and cannot be forwarded.
        // Each waiter needs a private FD because its firing carries no data
        // (sem_surf.c). Without it, NVKMS fences have no wakeup source.
        // Pass ordinary userspace registrations through.
        let mut waiter_reg: Option<PendingWaiter> = None;
        let mut waiter_unreg: Option<u32> = None;
        if frontend && req.ioctl_nr == sys::NV_ESC_RM_CONTROL && inline_len >= 32 {
            let cmd = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
            if cmd == CTRL_SEMSURF_REGISTER_WAITER && self.aux.len() >= 32 {
                let kc = u64::from_le_bytes(self.aux[24..32].try_into().unwrap());
                if kc > u32::MAX as u64 {
                    let h_client = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                    let generation = self.take_waiter_generation()?;
                    let slot = self.waiter_slot_get(h_client)?;
                    self.aux[24..32].copy_from_slice(&(slot.id as u64).to_le_bytes());
                    // Per waiter registration, i.e. per frame; hence the
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
                        generation,
                        target: WaiterTarget::from_params(&self.scratch, &self.aux),
                        h_client,
                        guest_kc: kc,
                        token: req.target_token,
                    });
                }
            } else if cmd == CTRL_SEMSURF_UNREGISTER_WAITER && self.aux.len() >= 24 {
                let kc = u64::from_le_bytes(self.aux[16..24].try_into().unwrap());
                if kc > u32::MAX as u64 {
                    let h_client = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                    // Unregister must use the substituted registration handle. If the
                    // waiter already fired, keep the original pointer so RM returns the
                    // OBJECT_NOT_FOUND result nvidia-drm expects.
                    let target = WaiterTarget::from_params(&self.scratch, &self.aux);
                    let id = self
                        .waiters
                        .iter()
                        .find(|(_, w)| {
                            w.h_client == h_client && w.guest_kc == kc && w.target == target
                        })
                        .map(|(&id, _)| id);
                    if let Some(id) = id {
                        self.aux[16..24].copy_from_slice(&(id as u64).to_le_bytes());
                        waiter_unreg = Some(id);
                    }
                }
            }
        }

        // (2e) Reserve before allocation and settle after RM's result.
        // Accounting also runs without a cap to supply the VM process list.
        // The limit is read once at startup.
        let mut vram_reserved = 0u64;
        let mut vram_full = false;
        let mut vram_status_off = 0usize;
        let mut vram_ask = None;
        if frontend && req.ioctl_nr == sys::NV_ESC_RM_ALLOC {
            // Both alloc forms carry hClass @12; only `status` moves.
            if let Some(st_off) = alloc_status_off(inline_len) {
                let hclass = u32::from_le_bytes(self.scratch[12..16].try_into().unwrap());
                // The params are the aux buffer, whose length the
                // embedded-pointer check above already held against what
                // the driver reads for this class.
                if let Some(bytes) = crate::vram::request_bytes(hclass, &self.aux) {
                    vram_status_off = st_off;
                    let door = if st_off == 40 {
                        crate::vram::Door::Nvos64
                    } else {
                        crate::vram::Door::Nvos21
                    };
                    vram_ask = crate::vram::Ask::of_alloc(door, hclass, &self.aux);
                    if self.vram.reserve(bytes) {
                        vram_reserved = bytes;
                    } else {
                        vram_full = true;
                    }
                }
            }
        }

        // (2e') The SAME cap, through the other door. NVOS32 is entirely
        // inline; no embedded pointer; so the parameter block is
        // `scratch`, and the reserve/settle mechanics below are the
        // NVOS64 branch's, unchanged.
        let mut vram_vidheap = false;
        if frontend && req.ioctl_nr == sys::NV_ESC_RM_VID_HEAP_CONTROL {
            if let Some(bytes) = crate::vram::vidheap_request_bytes(&self.scratch[..inline_len]) {
                vram_vidheap = true;
                vram_status_off = crate::vram::V_STATUS_OFF;
                vram_ask = crate::vram::Ask::of_vidheap(&self.scratch[..inline_len]);
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
            vram_ask,
            fb_info_list,
        })
    }

    /// This VM's UUID on the guest's side of a call, the card's on the
    /// driver's (`grid::Card`), under every policy: in every UVM parameter
    /// block, and in the params of the controls that name the GPU by UUID.
    fn uuid_swap(&mut self, plan: &Plan, to_host: bool) {
        let Some(card) = crate::grid::card().filter(|_| crate::grid::mediate_uuid()) else {
            return;
        };
        let n = plan.inline_len.min(self.scratch.len());
        let buf = if dev_of(plan.dev_tag).is_some_and(|d| d.is_uvm()) {
            &mut self.scratch[..n]
        } else if plan.ioctl_nr == sys::NV_ESC_RM_CONTROL
            && n >= 32
            && crate::grid::UUID_CONTROLS
                .contains(&u32::from_le_bytes(self.scratch[8..12].try_into().unwrap()))
        {
            &mut self.aux[..]
        } else {
            return;
        };
        if to_host {
            card.to_host(buf)
        } else {
            card.to_guest(buf)
        }
    }

    /// Execute the prepared request, apply its result, and reply.
    /// Buffers remain owned by the session. Emulated responses validate their
    /// field lengths here, where their writes are made.
    fn execute(&mut self, mut plan: Plan) -> Result<()> {
        let inline_len = plan.inline_len;
        let aux_len = plan.aux_len;
        let frontend = dev_of(plan.dev_tag).is_some_and(|dev| !dev.is_uvm());

        let ret = if matches!(plan.action, Action::FakeVramFull) {
            // Match native OOM: ioctl succeeds and RM status is NV_ERR_NO_MEMORY.
            // An errno produces the wrong CUDA/PyTorch error. `vram_status_off`
            // was validated for NVOS64 or NVOS21 during preparation.
            let o = plan.vram_status_off;
            self.scratch[o..o + 4].copy_from_slice(&sys::NV_ERR_NO_MEMORY.to_le_bytes());
            // Rate-limit per request kind, including its first refusal. Keep the
            // "VRAM cap reached (" prefix used by measurement readers; log the
            // original request and current ledger.
            if let Some(ask) = plan.vram_ask {
                if let Some(n) = self.vram.count_refusal(&ask) {
                    eprintln!(
                        "vhost-user-nvrm: VRAM cap reached ({n}. refusal of this kind) -- {} of {} MiB \
                         in use, allocation answered with NV_ERR_NO_MEMORY ({}): proc {} {}[{}] asked \
                         {ask}; {}",
                        self.vram.used() >> 20,
                        self.vram.limit() >> 20,
                        self.vram.knob(),
                        self.sub_id,
                        self.proc.comm_str(),
                        self.proc.pid,
                        self.vram.census(),
                    );
                }
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
                // SET_PREFERRED_LOCATION: a residency hint; with the pages
                // sysmem-resident the location is fixed, so the hint is a
                // no-op. rmStatus @36.
                42 if inline_len >= 40 => {
                    self.scratch[36..40].copy_from_slice(&sys::NV_OK.to_le_bytes());
                }
                // SET/UNSET_ACCESSED_BY: access hints for migratable ranges
                //; with the pages sysmem-resident there is nothing to hint
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
                    // These coherent sysmem pages need no migration. Return NV_OK:
                    // NOTHING_TO_DO caused 2.4 million retries in a measured 120 s run.
                    self.scratch[72..76].copy_from_slice(&sys::NV_OK.to_le_bytes());
                    // Async migration must publish semaphorePayload in the backed pool.
                    // Without that write, libcuda's prefetch synchronization spins forever.
                    let sema = u64::from_le_bytes(
                        self.scratch
                            [MIGRATE_SEMAPHORE_ADDRESS_OFF..MIGRATE_SEMAPHORE_ADDRESS_OFF + 8]
                            .try_into()
                            .unwrap(),
                    );
                    let pay = u32::from_le_bytes(
                        self.scratch
                            [MIGRATE_SEMAPHORE_PAYLOAD_OFF..MIGRATE_SEMAPHORE_PAYLOAD_OFF + 4]
                            .try_into()
                            .unwrap(),
                    );
                    if sema != 0
                        && !self.pool.write_guest_u32_for(
                            plan.target_token,
                            GuestAddr::new(sema),
                            pay,
                        )
                    {
                        eprintln!(
                            "vhost-user-nvrm: MIGRATE semaphore {sema:#x} invalid or unaligned in \
                             pool -- guest will hang"
                        );
                    }
                    if debug_level() >= 2 {
                        let flags = u32::from_le_bytes(
                            self.scratch[MIGRATE_FLAGS_OFF..MIGRATE_FLAGS_OFF + 4]
                                .try_into()
                                .unwrap(),
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
                    return self.reply_err(
                        plan.seq,
                        libc::EINVAL,
                        "managed compat: params too short",
                    );
                }
            }
            0
        } else {
            self.uuid_swap(&plan, true);
            // The length is the initialized part of the scratch buffer:
            // `inline_len` bytes copied from the guest, plus whatever a
            // translation grew it to. The driver ignores it; the ledger in
            // the test build must not read past it.
            unsafe {
                let n = self.scratch.len();
                self.sys
                    .ioctl(plan.target_fd, plan.request, self.scratch.as_mut_ptr(), n)
            }
        };
        // Save errno before cleanup, logging or capture can overwrite it.
        let ioctl_errno = if ret < 0 {
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO)
        } else {
            0
        };
        self.uuid_swap(&plan, false);

        // After a successful 0x71, pin the arena to the created handle
        // (NVOS02: hObjectNew @8, status @40).
        if let Some(arena) = plan.osdesc_arena.take() {
            let st = u32::from_le_bytes(self.scratch[40..44].try_into().unwrap());
            if ret == 0 && st == sys::NV_OK {
                let hroot = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                let hnew = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
                self.pool.keep_osdesc_arena(hroot, hnew, arena);
            }
        }

        // (1a'') Record a successful event substitution by hObjectNew@8.
        // On failure, free its unused OS-event ID.
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

        // (1a''') Arm only on NV_OK. ALREADY_SIGNALLED registers no
        // notification (ctrl00da.h), so recycle its slot as with other errors.
        if let Some(p) = plan.waiter_reg.take() {
            let ok = ret == 0
                && inline_len >= 32
                && u32::from_le_bytes(self.scratch[28..32].try_into().unwrap()) == sys::NV_OK;
            if ok {
                self.pending_pollables.push(Pollable::Waiter {
                    registration: RegistrationId {
                        event_id: p.slot.id,
                        generation: p.generation,
                    },
                    fd: p.slot.ctl.clone(),
                });
                self.waiters.insert(
                    p.slot.id,
                    ActiveWaiter {
                        generation: p.generation,
                        target: p.target,
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
            // Only NV_OK removed the listener. Quarantine its slot until the
            // poller acknowledges; a failed cancellation may still fire.
            let ok = ret == 0
                && inline_len >= 32
                && u32::from_le_bytes(self.scratch[28..32].try_into().unwrap()) == sys::NV_OK;
            if ok {
                if let Some(w) = self.waiters.remove(&id) {
                    self.pending_unwatch.push(WaiterRetirement {
                        slot: w.slot,
                        reuse_for: Some(w.h_client),
                    });
                }
            }
        }

        // The guest sent an NVOS64 block and must read one back. Only the
        // middle was rewritten into the NVOS02 shape RM accepts; `status`
        // sits at 40 in both structs, so RM's verdict survives the restore
        // and everything else is the caller's own again.
        if let Some(tail) = plan.osdesc_nvos64_tail.take() {
            // 16..40 only. `status` is at 40 and is the one field that must
            // NOT come back from the saved copy; restoring it would hand
            // the caller its own zero instead of RM's verdict.
            if self.scratch.len() >= 40 && tail.len() >= 24 {
                self.scratch[16..40].copy_from_slice(&tail[..24]);
            }
        }

        // Defer answer refusals until the executed ioctl's accounting is complete.
        let mut refusal: Option<(i32, &'static str)> = None;

        if frontend && ret == 0 && plan.ioctl_nr == sys::NV_ESC_RM_CONTROL && inline_len >= 32 {
            let cmd = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
            // Rewrite capacity only under a cap. Mark the device name as mediated
            // unless raw naming was requested; the profile supplies its size label.
            if self.vram.enabled() && cmd == crate::vram::CMD_FB_GET_INFO_V2 {
                let st = u32::from_le_bytes(self.scratch[28..32].try_into().unwrap());
                if st == sys::NV_OK {
                    crate::vram::rewrite_fb_info(
                        &mut self.aux,
                        self.vram.limit(),
                        self.vram.used(),
                    );
                }
            }
            // The same answer through the V1 door, which is the one the
            // graphics stack uses. `fb_info_list` is only Some under a cap
            // and only for this command, so the check is the plan's.
            if let Some((off, asked)) = plan.fb_info_list {
                let st = u32::from_le_bytes(self.scratch[28..32].try_into().unwrap());
                if st == sys::NV_OK && off <= self.aux.len() {
                    let (limit, used) = (self.vram.limit(), self.vram.used());
                    crate::vram::rewrite_fb_info_list(&mut self.aux[off..], asked, limit, used);
                }
            }
            // LEA_GPU_NAME_RAW=1 leaves the driver's own name in place --
            // the discriminator for whether a client branches on the
            // product string (none has been caught doing so).
            if cmd == crate::vram::CMD_GPU_GET_NAME_STRING && !gpu_name_raw() {
                let st = u32::from_le_bytes(self.scratch[28..32].try_into().unwrap());
                if st == sys::NV_OK {
                    crate::vram::rewrite_gpu_name::<A>(&mut self.aux, self.vram.profile());
                }
            }
            // (3c) Replace host PID/usage answers with this VM's roster.
            // GET_PIDS and GET_PID_INFO would otherwise expose host processes.
            // Both control payloads are flat; no wire-layout change is needed.
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

            // (3d) Rewrite successful encoder/mode answers for the VM profile.
            // Capacity follows framebuffer share; zero leaves the encoder unrestricted.
            // UUID translation follows below.
            let profile = self.vram.profile();
            if u32::from_le_bytes(self.scratch[28..32].try_into().unwrap()) == sys::NV_OK {
                if cmd == crate::grid::CMD_GPU_GET_VIRTUALIZATION_MODE
                    && profile.policy == crate::vram::Policy::Grid
                    && crate::grid::mediate_mode()
                {
                    crate::grid::rewrite_virtualization_mode(&mut self.aux);
                } else if cmd == crate::grid::CMD_GPU_GET_ENCODER_CAPACITY
                    && crate::grid::mediate_enc()
                {
                    crate::grid::rewrite_encoder_capacity(&mut self.aux, profile.encoder_capacity);
                }
            }
        }

        // NVOS32 INFO queries FB_GET_INFO internally, bypassing the control
        // rewrite. Mediate that capacity too; status is inline at byte 20.
        if frontend
            && ret == 0
            && self.vram.enabled()
            && plan.ioctl_nr == sys::NV_ESC_RM_VID_HEAP_CONTROL
            && inline_len >= crate::vram::V_STATUS_OFF + 4
            && u32::from_le_bytes(
                self.scratch[crate::vram::V_STATUS_OFF..crate::vram::V_STATUS_OFF + 4]
                    .try_into()
                    .unwrap(),
            ) == sys::NV_OK
        {
            let (limit, used) = (self.vram.limit(), self.vram.used());
            crate::vram::rewrite_vidheap_info(&mut self.scratch[..inline_len], limit, used);
        }

        // Keep a VRAM reservation only after a successful device allocation.
        // Read the returned attr: RM may change placement (observed VIDMEM
        // request 0x18000000 returned 0x11800000). Failed allocations leave
        // attr untouched.
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
                (
                    crate::vram::vidheap_attr_out(p),
                    crate::vram::vidheap_handle(p),
                )
            } else {
                let attr = if self.aux.len() >= 28 {
                    u32::from_le_bytes(self.aux[24..28].try_into().unwrap())
                } else {
                    0
                };
                (
                    attr,
                    u32::from_le_bytes(self.scratch[8..12].try_into().unwrap()),
                )
            };
            if let Some(ask) = plan.vram_ask {
                if let Some(line) = self.vram.placement(&ask, ok, st, attr_out) {
                    eprintln!(
                        "vhost-user-nvrm: VRAM ledger, proc {} {}[{}]: {line} ({} of {} MiB in use)",
                        self.sub_id,
                        self.proc.comm_str(),
                        self.proc.pid,
                        self.vram.used() >> 20,
                        self.vram.limit() >> 20,
                    );
                }
            }
            self.vram.settle(
                ok,
                attr_out,
                crate::vram::Charge {
                    token: plan.target_token,
                    root: u32::from_le_bytes(self.scratch[0..4].try_into().unwrap()),
                    parent: u32::from_le_bytes(self.scratch[4..8].try_into().unwrap()),
                    handle,
                    bytes: plan.vram_reserved,
                },
            );
        }

        // Only a successful UVM_FREE proves this external range is gone.
        if ret == 0
            && dev_of(plan.dev_tag).is_some_and(|d| d.is_uvm())
            && plan.ioctl_nr == nvrm_abi::xlate::uvm::FREE
            && inline_len >= std::mem::size_of::<A::UvmFreeParams>()
        {
            let status = A::UVM_FREE_PARAMS_OFF_rmStatus;
            if u32::from_le_bytes(self.scratch[status..status + 4].try_into().unwrap())
                == sys::NV_OK
            {
                let offset = A::UVM_FREE_PARAMS_OFF_base;
                let base = u64::from_le_bytes(self.scratch[offset..offset + 8].try_into().unwrap());
                if let Err(error) = self
                    .pool
                    .release_uvm_range(plan.target_token, GuestAddr::new(base))
                {
                    eprintln!("vhost-user-nvrm: pool cleanup retained after UVM_FREE: {error:#}");
                }
            }
        }

        // A rejected free leaves the allocation and its charge alive.
        if frontend
            && ret == 0
            && plan.ioctl_nr == sys::NV_ESC_RM_VID_HEAP_CONTROL
            && inline_len >= crate::vram::V_STATUS_OFF + 4
            && u32::from_le_bytes(
                self.scratch[crate::vram::V_STATUS_OFF..crate::vram::V_STATUS_OFF + 4]
                    .try_into()
                    .unwrap(),
            ) == sys::NV_OK
        {
            let p = &self.scratch[..inline_len];
            if crate::vram::vidheap_function(p) == Some(sys::NVOS32_FUNCTION_FREE) {
                let hroot = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                self.vram.free_object(hroot, crate::vram::vidheap_handle(p));
            }
        }

        // Pair this RM alloc/free log with the guest's kapi_ledger to diagnose
        // handle reuse or missing cleanup.
        if frontend {
            obj_ledger(plan.ioctl_nr, &self.scratch[..inline_len], ret);
        }

        // A root has hObjectParent=0 in both alloc forms. Record its token:
        // closing that FD can end the client without an explicit RM_FREE.
        if frontend && ret == 0 && plan.ioctl_nr == sys::NV_ESC_RM_ALLOC && inline_len >= 12 {
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

        // Keep backing memory and registrations until RM confirms the free.
        // NVOS00: hRoot @0, hObjectOld @8, status @12.
        if frontend
            && ret == 0
            && plan.ioctl_nr == sys::NV_ESC_RM_FREE
            && inline_len >= 16
            && u32::from_le_bytes(self.scratch[12..16].try_into().unwrap()) == sys::NV_OK
        {
            let hroot = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
            let hold = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
            self.pool.drop_osdesc_arena(hroot, hold);
            self.vram.free_object(hroot, hold);
            // Free an event's OS registration explicitly. Root teardown frees
            // all its OS events in RM; only host bookkeeping remains.
            if hold == hroot {
                self.ctl_freed_by_rmfree += self.release_client(hroot) as u64;
            } else if let Some(r) = self.events.remove(&(hroot, hold)) {
                self.free_os_event(r.h_client, r.id);
            }
        }

        // (3a') Assign the guest label immediately after root allocation,
        // before RM uses subProcessID to group channel USERD pages
        // (kernel_fifo.c). Log assignment failure; the client stays at zero.
        if frontend
            && ret == 0
            && self.sub_id != 0
            && plan.ioctl_nr == sys::NV_ESC_RM_ALLOC
            && inline_len >= 44
        {
            let hclass = u32::from_le_bytes(self.scratch[12..16].try_into().unwrap());
            let st = u32::from_le_bytes(self.scratch[40..44].try_into().unwrap());
            if hclass == sys::NV01_ROOT_CLIENT as u32 && st == sys::NV_OK {
                let hclient = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
                let name = self.sub_name();
                let (r, s) =
                    self.sys
                        .set_sub_process_id(plan.target_fd, hclient, self.sub_id, &name);
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

        // (3b) Allow UVM copies within this backend process. A same-user
        // grant would also admit other VMs running under the same host UID.
        if frontend && ret == 0 && plan.ioctl_nr == sys::NV_ESC_RM_ALLOC && inline_len >= 48 {
            // NVOS64: hRoot @0, hObjectNew @8, hClass @12, status @40.
            let st = u32::from_le_bytes(self.scratch[40..44].try_into().unwrap());
            let hclass = u32::from_le_bytes(self.scratch[12..16].try_into().unwrap());
            if st == 0 && share::uvm_dupes_class(hclass) {
                let hclient = u32::from_le_bytes(self.scratch[0..4].try_into().unwrap());
                let hobject = u32::from_le_bytes(self.scratch[8..12].try_into().unwrap());
                let (r, s) = self
                    .sys
                    .grant_dup_same_process(plan.target_fd, hclient, hobject);
                if r != 0 || s != 0 {
                    eprintln!(
                        "vhost-user-nvrm: DUP_OBJECT grant for {hobject:#x} \
                               (hClass {hclass:#x}) failed: ret {r} status {s:#x}"
                    );
                } else if debug_level() >= 2 {
                    eprintln!(
                        "vhost-user-nvrm: DUP_OBJECT granted: {hobject:#x} (hClass {hclass:#x})"
                    );
                }
            }
        }

        {
            // Always log syscall/RM failures; level 2 also logs successful calls
            // for trace comparison. The environment lookup is cached.
            let verbose = debug_level() >= 2;
            // RM_CONTROL: cmd @ 8, status @ 28 in NVOS54.
            // RM_ALLOC: hClass @ 12 in NVOS64 (status left 0 here).
            let sub = if frontend && plan.ioctl_nr == sys::NV_ESC_RM_CONTROL && inline_len >= 32 {
                u32::from_le_bytes(self.scratch[8..12].try_into().unwrap())
            } else if frontend && plan.ioctl_nr == sys::NV_ESC_RM_ALLOC && inline_len >= 16 {
                u32::from_le_bytes(self.scratch[12..16].try_into().unwrap())
            } else {
                0
            };
            let st = if frontend && plan.ioctl_nr == sys::NV_ESC_RM_CONTROL && inline_len >= 32 {
                u32::from_le_bytes(self.scratch[28..32].try_into().unwrap())
            } else if frontend && plan.ioctl_nr == sys::NV_ESC_RM_ALLOC && inline_len >= 44 {
                // NVOS64: status @40 (nvos.h:480-490)
                u32::from_le_bytes(self.scratch[40..44].try_into().unwrap())
            } else if frontend && plan.ioctl_nr == sys::NV_ESC_RM_ALLOC_MEMORY && inline_len >= 44 {
                // NVOS02: hRoot/hParent/hNew/hClass @0..16, flags @16,
                // pMemory @24, limit @32, status @40 (nvos.h:285-295)
                u32::from_le_bytes(self.scratch[40..44].try_into().unwrap())
            } else if frontend && plan.ioctl_nr == sys::NV_ESC_RM_MAP_MEMORY && inline_len >= 44 {
                // NVOS33: hClient/hDevice/hMemory @0/4/8, offset @16,
                // length @24, pLinearAddress @32, status @40 (nvos.h:1845-1856)
                u32::from_le_bytes(self.scratch[40..44].try_into().unwrap())
            } else if dev_of(plan.dev_tag).is_some_and(|d| d.is_uvm()) {
                // rmStatus offset per UVM command, from the bindgen structs.
                match uvm_status_off::<A>(plan.ioctl_nr) {
                    Some(o) if o + 4 <= inline_len => {
                        u32::from_le_bytes(self.scratch[o..o + 4].try_into().unwrap())
                    }
                    _ => 0,
                }
            } else {
                0
            };
            // LEA_CTRL_DUMP selects control IDs in hex. Dump the first two answers
            // per command/session: success status alone cannot expose wrong fields
            // (questions 26, 32 and 35).
            if frontend
                && plan.ioctl_nr == sys::NV_ESC_RM_CONTROL
                && st == 0
                && ctrl_dump_wanted(sub)
            {
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
                // every frame cannot bury the first one; which is the
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
                // Include the target FD on syscall failure. ATTACH_GPUS can fail
                // because that OFD already has GPUs, distinct from an invalid GPU ID.
                if ret != 0 {
                    eprintln!(
                        "vhost-user-nvrm:   ^ hard ioctl failure rode host fd {} (token {:#x})",
                        plan.target_fd, plan.target_token
                    );
                }
                // Log successful attaches with their FD at debug level 2 too, so a
                // later failure can be checked against the earlier OFD (question 45).
                if frontend && plan.ioctl_nr == nvrm_abi::nvgpu::NV_ESC_ATTACH_GPUS_TO_FD {
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

        // Refuse an unsafe answer only after accounting for the executed call.
        // Returning EINVAL must not undo RM's successful allocations or hide
        // host-side cleanup. Do not forward an unrewritten host PID payload.
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
        self.reply(
            Rsp {
                seq,
                ret: -errno,
                ..Rsp::default()
            },
            &[],
            &[],
        )
    }
}

/// Log RM allocation/free outcomes by (client, handle) with LEA_OBJLOG=1.
/// Compare with guest logs to diagnose premature handle reuse (status 0x19).
/// Cache the environment lookup; this runs on every forwarded ioctl.
fn obj_ledger(ioctl_nr: u32, inline: &[u8], ret: i32) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    // Any non-empty value switches it on. NOT `is_some()`: lea_backend_start passes
    // the variable through as `LEA_OBJLOG="${LEA_OBJLOG:-}"`, so an unset
    // variable arrives here SET AND EMPTY.
    if !*ON.get_or_init(|| std::env::var_os("LEA_OBJLOG").is_some_and(|v| !v.is_empty())) {
        return;
    }
    let u32_at = |o: usize| -> u32 {
        inline
            .get(o..o + 4)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
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

/// Capture real requests when LEA_CAPTURE_DIR is set.
/// Cache the environment lookup to avoid per-ioctl locking overhead.
/// Content-hash filenames deduplicate messages across capture runs.
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

/// NVOS02 flags for guest-RAM OS descriptors.
///
/// RmAllocOsDescriptor requires PCI location and NO_MAP (escape.c).
/// osCreateMemFromOsDescriptor requires CACHED/WRITE_BACK and disallows
/// CONTIGUOUS for anonymous page arrays (osmemdesc.c).
///
/// Therefore WRITE_COMBINE requests become write-back mappings; return the
/// requested name for logging. RM converts these flags to NVOS32 attr and
/// back, so both sets of constraints apply.
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
// Tests: malformed guest requests must be refused before the forwarded ioctl.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscalls::{FakeCall, FakeSyscalls};
    use std::sync::Arc;

    /// A session with a ledger instead of a driver, plus a token pointing
    /// at a harmless FD (a memfd; an ioctl on it would give ENOTTY, but
    /// the fake never lets it get that far).
    fn session() -> (Session<sys::DefaultAbi>, Arc<FakeSyscalls>, u64) {
        session_with_kind(Dev::Ctl)
    }

    #[test]
    fn an_ioctl_cannot_change_its_opened_device_type() {
        for (kind, tag, nr, size) in [
            (Dev::Ctl, DevTag::Uvm, nvrm_abi::xlate::uvm::INITIALIZE, 16),
            (Dev::Uvm, DevTag::Ctl, sys::NV_ESC_RM_FREE, 16),
            (Dev::Gpu, DevTag::Ctl, sys::NV_ESC_RM_FREE, 16),
        ] {
            let (mut s, fake, token) = session_with_kind(kind);
            let mut req = ioctl_req(token, nr, size, 0);
            req.dev_tag = tag as u32;
            let reply = s.handle_msg(&msg(&req, &vec![0; size], &[])).unwrap();
            assert_eq!(errno_of(&reply), Some(libc::EINVAL));
            assert_eq!(fake.ioctl_count(), 0);
        }
    }

    #[test]
    fn forwarded_dup_cannot_use_a_private_pool_client() {
        let (mut s, fake, token) = session();
        let budget = PinBudget::new(1024 << 20).unwrap();
        let private = 0x12345678_u32;
        let owner = budget.private_client(private);
        s.pool = PoolState::with_budget(budget);
        let mut inline = vec![0; std::mem::size_of::<sys::NVOS55_PARAMETERS>()];
        let source = std::mem::offset_of!(sys::NVOS55_PARAMETERS, hClientSrc);
        inline[source..source + 4].copy_from_slice(&private.to_le_bytes());
        let req = ioctl_req(token, sys::NV_ESC_RM_DUP_OBJECT, inline.len(), 0);
        let reply = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&reply), Some(libc::EPERM));
        assert_eq!(fake.ioctl_count(), 0);

        drop(owner);
        let reply = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&reply), None);
        assert_eq!(fake.ioctl_count(), 1);
    }

    fn session_with_kind(kind: Dev) -> (Session<sys::DefaultAbi>, Arc<FakeSyscalls>, u64) {
        let fake = Arc::new(FakeSyscalls::default());
        let mut s = Session::<sys::DefaultAbi>::detached_proc(
            7,
            crate::vram::Ledger::off(),
            PinBudget::new(1024 << 20).unwrap(),
        )
        .unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        assert!(fd >= 0);
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let token = s.mirror.insert(owned, kind);
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
        for kind in [
            proto::KIND_GET_TABLES,
            proto::KIND_MAP_RELEASE,
            proto::KIND_PROC_GONE,
            99,
        ] {
            let req = Req {
                seq: 1,
                kind,
                ..Req::default()
            };
            let r = s.handle_msg(&msg(&req, &[], &[])).unwrap();
            assert_eq!(errno_of(&r), Some(libc::EPROTO), "Kind {kind}");
        }
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn hello_checks_the_protocol_version() {
        let (mut s, _, _) = session();
        let bad = Req {
            seq: 1,
            kind: Kind::Hello as u32,
            ioctl_nr: proto::PROTO_VERSION + 1,
            ..Req::default()
        };
        assert_eq!(
            errno_of(&s.handle_msg(&msg(&bad, &[], &[])).unwrap()),
            Some(libc::EPROTO)
        );
        let good = Req {
            ioctl_nr: proto::PROTO_VERSION,
            ..bad
        };
        assert_eq!(
            errno_of(&s.handle_msg(&msg(&good, &[], &[])).unwrap()),
            None
        );
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

    /// Reject pointer offsets that differ from the descriptor.
    /// The separate bounds guard is not isolated by this test: current
    /// commands all use offset 16. It protects future descriptor shapes.
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
        let mut req = ioctl_req(tok, nvrm_abi::nvgpu::NV_ESC_REGISTER_FD, 4, 0);
        req.fd_field_off = 0;
        req.fd_field_token = 0xdead;
        let r = s.handle_msg(&msg(&req, &[0u8; 4], &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EBADF));
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn an_unresolved_explicit_fd_owner_cannot_alias_a_local_token() {
        let (mut s, fake, tok) = session();
        let mut req = ioctl_req(tok, nvrm_abi::nvgpu::NV_ESC_REGISTER_FD, 4, 0);
        req.fd_field_off = 0;
        req.fd_field_token = tok;
        for owner in [s.sub_id, s.sub_id + 1] {
            req.fd_field_proc = owner;
            let reply = s.handle_msg(&msg(&req, &[0; 4], &[])).unwrap();
            assert_eq!(errno_of(&reply), Some(libc::EBADF));
        }
        assert_eq!(fake.ioctl_count(), 0);

        // Device resolution is required even when the explicit owner is this session.
        req.fd_field_proc = s.sub_id;
        let fd = s.mirror.raw(tok).unwrap();
        let reply = s
            .handle_msg_with(&msg(&req, &[0; 4], &[]), None, Some(fd))
            .unwrap();
        assert_eq!(Rsp::from_bytes(&reply.bytes).unwrap().ret, 0);
        assert_eq!(fake.ioctl_count(), 1);
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
        // MAX_NESTED protects the fixed request array. Current descriptor
        // validation also rejects this input; this test cannot isolate the
        // guard. `nested_ptrs_stay_within_max_nested` pins that invariant.
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 64);
        req.nested_count = proto::MAX_NESTED as u32 + 1;
        let r = s.handle_msg(&msg(&req, &[0u8; 32], &[0u8; 64])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));

        // Matching the number, but the command has no second-level pointers
        // at all; that is 0 annotated against 1 claimed.
        let mut inline = vec![0u8; 32];
        inline[8..12].copy_from_slice(&0x2080_0110u32.to_le_bytes());
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 64);
        req.nested_count = 1;
        let r = s.handle_msg(&msg(&req, &inline, &[0u8; 64])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::EINVAL));
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn omitted_embedded_metadata_is_rejected_before_forwarding() {
        let (mut s, fake, tok) = session();
        let mut inline = vec![0u8; 32];
        inline[8..12].copy_from_slice(&0x2080_0101u32.to_le_bytes());
        inline[16..24].copy_from_slice(&0x1234_5678_9000u64.to_le_bytes());
        inline[24..28].copy_from_slice(&16u32.to_le_bytes());
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        let reply = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&reply), Some(libc::EINVAL));
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn omitted_nested_and_fd_metadata_never_reach_the_driver() {
        let (mut s, fake, tok) = session();
        let mut inline = vec![0; 32];
        inline[8..12].copy_from_slice(&0x2080_1802u32.to_le_bytes());
        inline[16..24].copy_from_slice(&1u64.to_le_bytes());
        inline[24..28].copy_from_slice(&16u32.to_le_bytes());
        let mut aux = vec![0; 16];
        aux[..4].copy_from_slice(&1u32.to_le_bytes());
        aux[8..16].copy_from_slice(&0x1234_0000u64.to_le_bytes());
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 16);
        req.embedded_ptr_off = 16;
        let reply = s.handle_msg(&msg(&req, &inline, &aux)).unwrap();
        assert_eq!(errno_of(&reply), Some(libc::EINVAL));

        let req = ioctl_req(tok, nvrm_abi::nvgpu::NV_ESC_REGISTER_FD, 4, 0);
        let reply = s
            .handle_msg(&msg(&req, &123i32.to_le_bytes(), &[]))
            .unwrap();
        assert_eq!(errno_of(&reply), Some(libc::EINVAL));
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn a_short_uvm_envelope_never_reaches_the_driver() {
        let (mut s, fake, tok) = session_with_kind(Dev::Uvm);
        let mut req = ioctl_req(tok, UVM_INITIALIZE, 1, 0);
        req.dev_tag = DevTag::Uvm as u32;
        let reply = s.handle_msg(&msg(&req, &[0], &[])).unwrap();
        assert_eq!(errno_of(&reply), Some(libc::EINVAL));
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn serialized_requests_are_refused_before_the_driver() {
        let (mut s, fake, tok) = session();
        let mut inline = vec![0; 32];
        inline[8..12].copy_from_slice(&0x2080_0101u32.to_le_bytes());
        inline[12..16].copy_from_slice(&sys::NVOS54_FLAGS_FINN_SERIALIZED.to_le_bytes());
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        assert_eq!(
            errno_of(&s.handle_msg(&msg(&req, &inline, &[])).unwrap()),
            Some(libc::ENOTSUP)
        );
        let mut inline = vec![0; 48];
        inline[36..40].copy_from_slice(&sys::NVOS64_FLAGS_FINN_SERIALIZED.to_le_bytes());
        let req = ioctl_req(tok, sys::NV_ESC_RM_ALLOC, 48, 0);
        assert_eq!(
            errno_of(&s.handle_msg(&msg(&req, &inline, &[])).unwrap()),
            Some(libc::ENOTSUP)
        );
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn a_sparse_nested_control_translates_its_nonnull_target() {
        let (mut s, fake, tok) = session();
        let mut inline = vec![0; 32];
        inline[8..12].copy_from_slice(&0x0080_170du32.to_le_bytes());
        inline[16..24].copy_from_slice(&1u64.to_le_bytes());
        inline[24..28].copy_from_slice(&24u32.to_le_bytes());
        let mut aux = vec![0; 28];
        aux[..4].copy_from_slice(&1u32.to_le_bytes());
        aux[16..24].copy_from_slice(&0x1234_0000u64.to_le_bytes());
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, aux.len());
        req.embedded_ptr_off = 16;
        req.nested_count = 1;
        req.nested[0] = proto::NestedDesc {
            ptr_off: 16,
            aux_off: 24,
            len: 4,
            ..proto::NestedDesc::default()
        };
        let plan = s
            .prepare(&req, &msg(&req, &inline, &aux)[Req::WIRE_LEN..])
            .unwrap();
        assert_eq!(u64::from_le_bytes(s.aux[8..16].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_le_bytes(s.aux[16..24].try_into().unwrap()),
            s.aux.as_ptr() as u64 + 24
        );
        s.execute(plan).unwrap();
        assert_eq!(fake.ioctl_count(), 1);
    }

    #[test]
    fn xfer_repack_is_refused_loudly() {
        let (mut s, fake, tok) = session();
        // inline_len > 14 bits: no longer fits into the _IOC encoding.
        let n = (1 << 14) as usize;
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, n, 0);
        let r = s.handle_msg(&msg(&req, &vec![0u8; n], &[])).unwrap();
        assert_eq!(
            errno_of(&r),
            Some(libc::EMSGSIZE),
            "refuse loudly rather than guess"
        );
        assert_eq!(fake.ioctl_count(), 0);
    }

    // ---- deny list and routing --------------------------------------------

    /// The denial must fire BEFORE the token lookup; otherwise a denied
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
            assert_eq!(
                errno_of(&r),
                Some(libc::EPERM),
                "{cmd:#x} must give EPERM, not EBADF"
            );
        }
        assert_eq!(fake.ioctl_count(), 0);
    }

    #[test]
    fn untranslatable_capability_fd_classes_never_reach_the_driver() {
        let (mut s, fake, tok) = session();
        for class in [0xc637u32, 0xc638, 0xc639, 0xc640, 0xb0cd, 0xb0ce, 0xcdcd] {
            for len in [32, 48] {
                for with_params in [false, true] {
                    let mut inline = vec![0; len];
                    inline[12..16].copy_from_slice(&class.to_le_bytes());
                    let params_len = if with_params {
                        inline[16..24].copy_from_slice(&1u64.to_le_bytes());
                        nvrm_abi::xlate::alloc_param_size::<sys::DefaultAbi>(class).unwrap()
                            as usize
                    } else {
                        0
                    };
                    let mut req = ioctl_req(tok, sys::NV_ESC_RM_ALLOC, len, params_len);
                    if with_params {
                        req.embedded_ptr_off = 16;
                    }
                    let r = s
                        .handle_msg(&msg(&req, &inline, &vec![0; params_len]))
                        .unwrap();
                    assert_eq!(errno_of(&r), Some(libc::ENOTSUP), "class {class:#x}");
                }
            }
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

    /// The one that matters: zero flags; what this path sent until now --
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
            assert_eq!(
                f02::COHERENCY.get(flags),
                sys::NVOS02_FLAGS_COHERENCY_WRITE_BACK
            );
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
        for asked in [
            sys::NVOS32_ATTR_COHERENCY_CACHED,
            sys::NVOS32_ATTR_COHERENCY_WRITE_BACK,
        ] {
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
            assert!(
                lost.is_some(),
                "asked {asked} must be reported as substituted"
            );
        }
    }

    /// And the guest's claim about physicality changes nothing; what RM
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
        let (yes, _) = nvos02_flags_for_osdesc(
            0,
            a2::GPU_CACHEABLE.set(sys::NVOS32_ATTR2_GPU_CACHEABLE_YES),
        );
        assert_eq!(
            f02::GPU_CACHEABLE.get(yes),
            sys::NVOS02_FLAGS_GPU_CACHEABLE_YES
        );
        let (no, _) =
            nvos02_flags_for_osdesc(0, a2::GPU_CACHEABLE.set(sys::NVOS32_ATTR2_GPU_CACHEABLE_NO));
        assert_eq!(
            f02::GPU_CACHEABLE.get(no),
            sys::NVOS02_FLAGS_GPU_CACHEABLE_NO
        );
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
            FakeCall::Ioctl {
                request,
                inline: got,
                ..
            } => {
                assert_eq!(
                    *request as u32,
                    nvrm_abi::iowr_raw(sys::NV_ESC_RM_CONTROL, 32)
                );
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
        let mut req = ioctl_req(tok, nvrm_abi::nvgpu::NV_ESC_REGISTER_FD, 4, 0);
        req.fd_field_off = 0;
        req.fd_field_token = tok;
        let mut inline = vec![0u8; 4];
        inline[0..4].copy_from_slice(&0x4141_4141u32.to_le_bytes()); // guest garbage
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&r), None);
        let got = fake.last_inline().unwrap();
        assert_eq!(i32::from_le_bytes(got[0..4].try_into().unwrap()), host_fd);
    }

    #[test]
    /// The guest's UVM_INITIALIZE flags cross UNCHANGED. The multi-process
    /// flag used to be forced here; it cost pageable memory access and with
    /// it the Vulkan raytracing device (OPEN-QUESTIONS nr 11), and its
    /// reason; a foreign process mmapping this daemon's UVM fd; is gone.
    fn uvm_initialize_flags_pass_through() {
        let (mut s, fake, tok) = session_with_kind(Dev::Uvm);
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
        assert_eq!(
            flags & UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE,
            0,
            "the multi-process flag must not be forced onto the guest's UVM init"
        );
        // UVM crosses the boundary as a RAW number, not _IOC-encoded.
        match &fake.calls()[0] {
            FakeCall::Ioctl { request, .. } => assert_eq!(*request as u32, UVM_INITIALIZE),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn uvm_length_words_cannot_trigger_rm_control_policy_or_mediation() {
        use nvrm_abi::xlate;
        let nr = xlate::uvm::SET_PREFERRED_LOCATION;
        assert_eq!(nr, sys::NV_ESC_RM_CONTROL);
        let size = xlate::uvm_param_size_for::<sys::DefaultAbi>(nr).unwrap() as usize;
        let mediated = [
            crate::vram::CMD_GPU_GET_PIDS,
            crate::vram::CMD_GPU_GET_PID_INFO,
            crate::vram::CMD_FB_GET_INFO,
            crate::vram::CMD_FB_GET_INFO_V2,
            crate::vram::CMD_GPU_GET_NAME_STRING,
            CTRL_SEMSURF_REGISTER_WAITER,
            CTRL_SEMSURF_UNREGISTER_WAITER,
        ];
        for &word in xlate::blocked_ctrls().iter().chain(&mediated) {
            let (mut s, fake, token) = session_with_kind(Dev::Uvm);
            let mut inline = vec![0; size];
            // UVM length occupies the same offset as NVOS54.cmd.
            inline[8..12].copy_from_slice(&word.to_le_bytes());
            let mut req = ioctl_req(token, nr, size, 0);
            req.dev_tag = DevTag::Uvm as u32;
            let mut plan = s.prepare(&req, &inline).expect("UVM shape is valid");
            assert!(plan.event_reg.is_none());
            assert!(plan.waiter_reg.is_none());
            assert!(plan.waiter_unreg.is_none());
            assert_eq!(plan.vram_reserved, 0);
            // Exercise forwarding even when managed compatibility is enabled.
            plan.action = Action::Forward;
            s.execute(plan).unwrap();

            assert_eq!(errno_of(&s.out), None, "length word {word:#x}");
            assert_eq!(&s.out.bytes[Rsp::WIRE_LEN..], inline);
            assert_eq!(
                fake.calls(),
                vec![FakeCall::Ioctl {
                    fd: s.mirror.raw(token).unwrap(),
                    request: nr as libc::c_ulong,
                    inline,
                }]
            );
            assert!(s.client_token.is_empty());
            assert!(s.events.is_empty());
            assert!(s.waiters.is_empty());
            assert!(s.pending_pollables.is_empty());
        }
    }

    #[test]
    fn numa_info_uses_its_flat_native_envelope() {
        use nvrm_abi::nvgpu::{IoctlNumaInfo, NV_ESC_NUMA_INFO};
        let (mut s, fake, token) = session_with_kind(Dev::Gpu);
        let size = std::mem::size_of::<IoctlNumaInfo>();
        let mut inline = vec![0; size];
        inline[8..16].copy_from_slice(&0x1234_0000u64.to_le_bytes());
        let mut req = ioctl_req(token, NV_ESC_NUMA_INFO, size, 0);
        req.dev_tag = DevTag::Gpu as u32;
        let reply = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&reply), None);
        assert_eq!(&reply.bytes[Rsp::WIRE_LEN..], inline);
        assert_eq!(
            fake.calls(),
            vec![FakeCall::Ioctl {
                fd: s.mirror.raw(token).unwrap(),
                request: iowr_raw(NV_ESC_NUMA_INFO, size as u32) as libc::c_ulong,
                inline: inline.clone(),
            }]
        );

        req.inline_len -= 1;
        let reply = s.handle_msg(&msg(&req, &inline[..size - 1], &[])).unwrap();
        assert_eq!(errno_of(&reply), Some(libc::EINVAL));
        // NV_ESC_SET_NUMA_STATUS changes host state and remains unsupported.
        req.ioctl_nr = NV_ESC_NUMA_INFO + 1;
        req.inline_len = 4;
        let reply = s.handle_msg(&msg(&req, &[0; 4], &[])).unwrap();
        assert_eq!(errno_of(&reply), Some(libc::ENOTSUP));
        assert_eq!(fake.ioctl_count(), 1);
    }

    /// The GPU index at open time is a guest word. Without a bound the host
    /// pastes it into a file name and tries to open that.
    #[test]
    fn open_rejects_a_gpu_index_beyond_the_driver_limit() {
        let (mut s, fake, _) = session();
        for nr in [32u32, 1_048_577, u32::MAX] {
            let req = Req {
                seq: 1,
                kind: Kind::Open as u32,
                dev_tag: DevTag::Gpu as u32,
                ioctl_nr: nr,
                ..Req::default()
            };
            let r = s.handle_msg(&msg(&req, &[], &[])).unwrap();
            assert_eq!(errno_of(&r), Some(libc::EINVAL), "GPU index {nr} accepted");
        }
        assert_eq!(fake.ioctl_count(), 0);
    }

    /// Replay captured and minimized guest requests without a GPU or nightly.
    /// The committed corpus derives from 9287 gate messages plus fuzz cases;
    /// LEA_CAPTURE_DIR and cargo fuzz cmin can extend it. Missing corpus files
    /// must remain visible in the test output.
    #[test]
    fn the_fuzz_corpus_still_goes_through() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/handle_msg");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!(
                "fuzz corpus: {} absent -- nothing replayed (not a failure)",
                dir.display()
            );
            return;
        };
        let mut n = 0;
        for e in entries.flatten() {
            let Ok(bytes) = std::fs::read(e.path()) else {
                continue;
            };
            let (mut s, _fake, _tok) = session();
            // A second token, as the fuzz target provides: otherwise the
            // translation paths end at EBADF already.
            let fd = unsafe { libc::memfd_create(c"leandro-corpus".as_ptr(), 0) };
            if fd >= 0 {
                s.mirror.insert(
                    unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) },
                    Dev::Ctl,
                );
            }
            // A panic = test failure. Err is allowed (transport error).
            let _ = s.handle_msg(&bytes);
            n += 1;
        }
        assert!(n > 0, "corpus directory {} is empty", dir.display());
        eprintln!("fuzz corpus: {n} messages replayed");
    }

    /// A fresh RM client receives the guest process's identity; and does
    /// so IMMEDIATELY, because RM hangs USERD separation off subProcessID
    /// (kernel_fifo.c:508-511).
    #[test]
    fn a_fresh_root_client_gets_its_sub_process_id() {
        let fake = Arc::new(FakeSyscalls {
            // NVOS64: hObjectNew @8, status @40. Fake success.
            writes_back: vec![
                (8, 0xabcd_1234u32.to_le_bytes().to_vec()),
                (40, sys::NV_OK.to_le_bytes().to_vec()),
            ],
            ..Default::default()
        });
        let mut s = Session::<sys::DefaultAbi>::detached_proc(
            7,
            crate::vram::Ledger::off(),
            PinBudget::new(1024 << 20).unwrap(),
        )
        .unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned, Dev::Ctl);

        let mut inline = vec![0u8; 48];
        inline[12..16].copy_from_slice(&(sys::NV01_ROOT_CLIENT as u32).to_le_bytes());
        let req = ioctl_req(tok, sys::NV_ESC_RM_ALLOC, 48, 0);
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&r), None);

        let set = fake.calls().into_iter().find_map(|c| match c {
            FakeCall::SetSubProcessId {
                hclient,
                sub_id,
                name,
                ..
            } => Some((hclient, sub_id, name)),
            _ => None,
        });
        assert_eq!(set, Some((0xabcd_1234, 7, "guest-7".to_string())));
    }

    // ---- the cut itself (the prepare/execute seam, OPEN-QUESTIONS nr 5) ----

    /// `prepare` returns a DECISION, not a finished answer: a refusal is
    /// directly inspectable as a Refusal (errno and reason), without
    /// parsing the response bytes; that is what the cut buys over going
    /// through handle_msg.
    #[test]
    fn prepare_hands_back_the_refusal_itself() {
        let (mut s, fake, tok) = session();
        let cmd = nvrm_abi::xlate::blocked_ctrls()[0];
        let mut inline = vec![0u8; 32];
        inline[8..12].copy_from_slice(&cmd.to_le_bytes());
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        let payload = inline.clone();
        let refusal = s
            .prepare(&req, &payload)
            .err()
            .expect("a denied control must refuse");
        assert_eq!(refusal.errno, libc::EPERM);
        assert!(refusal.why.contains("never forwarded"), "{}", refusal.why);
        assert_eq!(fake.ioctl_count(), 0, "prepare issues no syscall");
    }

    /// A clean call yields a forward plan with a finished _IOC request
    /// number; inspectable BEFORE anything is executed. (That execute
    /// then issues the plan unchanged is covered by the counter-proofs
    /// above, which go through handle_msg.)
    #[test]
    fn prepare_builds_a_forward_plan_with_the_ioc_request() {
        let (mut s, fake, tok) = session();
        let inline = vec![0u8; 32];
        let req = ioctl_req(tok, 0x2a, 32, 0);
        let plan = s.prepare(&req, &inline).expect("a clean call must pass");
        assert!(matches!(plan.action, Action::Forward));
        assert_eq!(plan.request, iowr_raw(0x2a, 32) as libc::c_ulong);
        assert_eq!((plan.inline_len, plan.aux_len), (32, 0));
        assert_eq!(fake.ioctl_count(), 0, "prepare itself executes nothing");
    }

    // ---- the VRAM cap ------------------------------------------------------

    /// A session on a ledger the test keeps a handle to, so it can read
    /// the VM's counter from outside.
    fn capped_session(
        limit: u64,
    ) -> (
        Session<sys::DefaultAbi>,
        Arc<FakeSyscalls>,
        u64,
        Arc<crate::vram::Ledger>,
    ) {
        let led = crate::vram::Ledger::for_test(limit);
        let fake = Arc::new(FakeSyscalls::default());
        let mut s = Session::<sys::DefaultAbi>::detached_proc(
            7,
            led.clone(),
            PinBudget::new(1024 << 20).unwrap(),
        )
        .unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-vram-test".as_ptr(), 0) };
        assert!(fd >= 0);
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let token = s.mirror.insert(owned, Dev::Ctl);
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
    /// card gives; ioctl 0 plus NV_ERR_NO_MEMORY; and the driver is
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

        // Full. The third one must be refused; without an ioctl.
        let r = s.handle_msg(&vram_alloc_msg(tok, 0xcc, 4 << 20)).unwrap();
        let rsp = Rsp::from_bytes(&r.bytes).unwrap();
        assert_eq!(rsp.ret, 0, "the ioctl itself succeeds, as it does natively");
        assert_eq!(rm_status(&r), sys::NV_ERR_NO_MEMORY);
        assert_eq!(
            fake.ioctl_count(),
            2,
            "a refused allocation reaches no driver"
        );
        assert_eq!(led.used(), 8 << 20, "a refusal charges nothing");

        // Free one, and there is room again.
        s.handle_msg(&free_msg(tok, 0xaa)).unwrap();
        assert_eq!(led.used(), 4 << 20);
        let r = s.handle_msg(&vram_alloc_msg(tok, 0xdd, 4 << 20)).unwrap();
        assert_eq!(rm_status(&r), sys::NV_OK);
        assert_eq!(led.used(), 8 << 20);
    }

    /// Without a cap, ordinary allocations are counted but not limited.
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

    /// Process identity is announced even if opening the device later fails.
    /// An invalid GPU index tests that path without a GPU.
    #[test]
    fn opening_announces_the_guest_process_before_touching_a_device() {
        let (mut s, fake, _tok, _led) = capped_session(64 << 20);
        let mut info = proto::ProcInfo {
            pid: 4711,
            _pad: 0,
            comm: [0; 16],
        };
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
        assert_eq!(
            errno_of(&r),
            Some(libc::EINVAL),
            "the open itself is refused"
        );
        assert_eq!(fake.ioctl_count(), 0);

        let roster = s.vram.roster();
        assert_eq!(roster.len(), 1, "and the process is on the list regardless");
        assert_eq!(roster[0].guest_pid, 4711);
    }

    /// Closing the FD frees the clients on it; no RM_FREE crosses the
    /// boundary for those, so the release has to hang off the close.
    #[test]
    fn a_rejected_free_keeps_the_charge_and_client_resources() {
        let (mut s, _, tok, ledger) = capped_session(64 << 20);
        let client = 0xc1d8_3a38;
        s.handle_msg(&vram_alloc_msg(tok, 0xaa, 4 << 20)).unwrap();
        s.handle_msg(&kernel_callback_alloc(tok, client, 7, 0x10))
            .unwrap();
        s.client_token.insert(client, tok);
        let slot = s.waiter_slot_get(client).unwrap();
        s.waiter_pool.entry(client).or_default().push(slot);
        s.take_pollables();

        let failed = Arc::new(FakeSyscalls {
            writes_back: vec![(12, sys::NV_ERR_INVALID_OBJECT_HANDLE.to_le_bytes().to_vec())],
            ..Default::default()
        });
        s.sys = Box::new(failed.clone());
        for handle in [0, 0xaa, client] {
            let reply = s.handle_msg(&free_msg(tok, handle)).unwrap();
            assert_eq!(Rsp::from_bytes(&reply.bytes).unwrap().ret, 0);
            assert_eq!(ledger.used(), 4 << 20);
            assert!(s.events.contains_key(&(client, 0)));
            assert!(s.event_ctls.contains_key(&client));
            assert_eq!(s.client_token.get(&client), Some(&tok));
            assert_eq!(s.waiter_pool[&client].len(), 1);
            assert!(s.take_unwatch().is_empty());
            assert!(s.take_ctl_unwatch().is_empty());
        }
        assert_eq!(
            failed.ioctl_count(),
            3,
            "no FREE_OS_EVENT after a rejected free"
        );

        s.sys = Box::<FakeSyscalls>::default();
        s.handle_msg(&free_msg(tok, client)).unwrap();
        assert_eq!(ledger.used(), 0);
        assert!(s.events.is_empty());
        assert_eq!(s.take_unwatch().len(), 1);
        assert_eq!(s.take_ctl_unwatch().len(), 1);
    }

    #[test]
    fn a_rejected_vidheap_free_keeps_its_charge() {
        let (mut s, _, tok, ledger) = capped_session(64 << 20);
        s.handle_msg(&vram_alloc_msg(tok, 0xaa, 4 << 20)).unwrap();
        let mut inline = vec![0; std::mem::size_of::<sys::NVOS32_PARAMETERS>()];
        inline[0..4].copy_from_slice(&0xc1d8_3a38u32.to_le_bytes());
        inline[8..12].copy_from_slice(&sys::NVOS32_FUNCTION_FREE.to_le_bytes());
        inline[44..48].copy_from_slice(&0xaau32.to_le_bytes());
        let request = msg(
            &ioctl_req(tok, sys::NV_ESC_RM_VID_HEAP_CONTROL, inline.len(), 0),
            &inline,
            &[],
        );
        s.sys = Box::new(FakeSyscalls {
            writes_back: vec![(20, sys::NV_ERR_INVALID_OBJECT_HANDLE.to_le_bytes().to_vec())],
            ..Default::default()
        });
        s.handle_msg(&request).unwrap();
        assert_eq!(ledger.used(), 4 << 20);

        s.sys = Box::<FakeSyscalls>::default();
        s.handle_msg(&request).unwrap();
        assert_eq!(ledger.used(), 0);
    }

    #[test]
    fn closing_the_fd_returns_the_charges() {
        let (mut s, _fake, tok, led) = capped_session(64 << 20);
        for h in 0..4u32 {
            s.handle_msg(&vram_alloc_msg(tok, h, 4 << 20)).unwrap();
        }
        assert_eq!(led.used(), 16 << 20);

        let req = Req {
            seq: 1,
            kind: Kind::Close as u32,
            target_token: tok,
            ..Req::default()
        };
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
            let mut s = Session::<sys::DefaultAbi>::detached_proc(
                7,
                led.clone(),
                PinBudget::new(1024 << 20).unwrap(),
            )
            .unwrap();
            s.sys = Box::new(fake);
            let fd = unsafe { libc::memfd_create(c"leandro-vram-drop".as_ptr(), 0) };
            let owned =
                unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
            let tok = s.mirror.insert(owned, Dev::Ctl);
            for h in 0..4u32 {
                s.handle_msg(&vram_alloc_msg(tok, h, 4 << 20)).unwrap();
            }
            assert_eq!(led.used(), 16 << 20);
        }
        assert_eq!(led.used(), 0, "the session's debt dies with it");
    }

    /// An allocation RM itself refuses must not stay charged; otherwise
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

    /// NVOS21 uses status@28 in 32 bytes. It must honor the cap without
    /// reading NVOS64's status@40.
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
        assert_eq!(
            status28(&r),
            sys::NV_ERR_NO_MEMORY,
            "the short form is capped too"
        );
        assert_eq!(fake.ioctl_count(), 1, "and refused without an ioctl");
        // The 48-byte status field must NOT have been touched: it does not
        // exist in this message.
        assert_eq!(r.bytes.len(), Rsp::WIRE_LEN + 32 + 128);
    }

    /// At a FULL ledger, what RM puts in system memory still reaches RM:
    /// a 0x3e with LOCATION_ANY through RM_ALLOC and an NVOS32 ALLOC_SIZE
    /// with LOCATION_ANY (RM allocates that as 0x3e). Until 2026-09-17 both
    /// were answered NV_ERR_NO_MEMORY without an ioctl.
    #[test]
    fn a_full_ledger_does_not_refuse_system_memory() {
        let (mut s, fake, tok, led) = capped_session(4 << 20);
        let r = s.handle_msg(&vram_alloc_msg(tok, 0xaa, 4 << 20)).unwrap();
        assert_eq!(rm_status(&r), sys::NV_OK);
        assert_eq!(led.used(), 4 << 20, "full");
        let full = s.handle_msg(&vram_alloc_msg(tok, 0xab, 1 << 20)).unwrap();
        assert_eq!(
            rm_status(&full),
            sys::NV_ERR_NO_MEMORY,
            "VIDMEM is refused at a full ledger"
        );
        let before = fake.ioctl_count();

        let any = nvrm_abi::nvgpu::nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_ANY);
        let mut m = vram_alloc_msg(tok, 0xbb, 64 << 20);
        let hclass_off = Req::WIRE_LEN + 12;
        m[hclass_off..hclass_off + 4].copy_from_slice(&0x3eu32.to_le_bytes());
        let attr_off = Req::WIRE_LEN + 48 + 24;
        m[attr_off..attr_off + 4].copy_from_slice(&any.to_le_bytes());
        let r = s.handle_msg(&m).unwrap();
        assert_eq!(
            rm_status(&r),
            sys::NV_OK,
            "0x3e ANY is system memory, RM decides"
        );
        assert_eq!(fake.ioctl_count(), before + 1, "and it reached RM");

        let mut inline = vec![0u8; 184];
        inline[8..12].copy_from_slice(&sys::NVOS32_FUNCTION_ALLOC_SIZE.to_le_bytes());
        inline[40 + 16..40 + 20].copy_from_slice(&any.to_le_bytes());
        inline[40 + 48..40 + 56].copy_from_slice(&(64u64 << 20).to_le_bytes());
        let req = ioctl_req(tok, sys::NV_ESC_RM_VID_HEAP_CONTROL, 184, 0);
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        let o = Rsp::WIRE_LEN + crate::vram::V_STATUS_OFF;
        assert_eq!(
            u32::from_le_bytes(r.bytes[o..o + 4].try_into().unwrap()),
            sys::NV_OK
        );
        assert_eq!(
            fake.ioctl_count(),
            before + 2,
            "ALLOC_SIZE ANY reached RM too"
        );
        assert_eq!(led.used(), 4 << 20, "and neither was charged");
    }

    /// One RM control with `aux` as its params, the way the guest module
    /// sends it: NVOS54 inline, the params pointer @16 named as embedded.
    fn ctrl_msg(tok: u64, cmd: u32, aux: &[u8]) -> Vec<u8> {
        let mut inline = vec![0u8; 32];
        inline[8..12].copy_from_slice(&cmd.to_le_bytes());
        inline[16..24].copy_from_slice(&0xdead_beefu64.to_le_bytes());
        inline[24..28].copy_from_slice(&(aux.len() as u32).to_le_bytes());
        let mut req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, aux.len());
        req.embedded_ptr_off = 16;
        msg(&req, &inline, aux)
    }

    /// A session of this backend under `profile`, for the answers that
    /// depend on the profile and not on the ledger's counter.
    fn profiled_session(
        profile: crate::vram::Profile,
    ) -> (Session<sys::DefaultAbi>, Arc<FakeSyscalls>, u64) {
        let mut s = Session::<sys::DefaultAbi>::detached_proc(
            7,
            crate::vram::Ledger::for_test_profile(profile),
            PinBudget::new(1024 << 20).unwrap(),
        )
        .unwrap();
        let fake = Arc::new(FakeSyscalls::default());
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-profile-test".as_ptr(), 0) };
        assert!(fd >= 0);
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let token = s.mirror.insert(owned, Dev::Ctl);
        (s, fake, token)
    }

    /// The encoder share the guest reads is the framebuffer's, whichever
    /// policy set the framebuffer; without a cap RM's answer stands.
    #[test]
    fn the_encoder_answer_is_the_same_under_every_policy() {
        use crate::vram::{Policy, Profile};
        let fb = 3072u64 << 20;
        let share = nvrm_abi::vgpu::encoder_share(fb, 8192 << 20);
        let answer = |profile: Profile| {
            let (mut s, _fake, tok) = profiled_session(profile);
            let mut aux = vec![0u8; nvrm_abi::mediate::ENCCAP_LEN];
            aux[nvrm_abi::mediate::ENCCAP_OFF..nvrm_abi::mediate::ENCCAP_OFF + 4]
                .copy_from_slice(&100u32.to_le_bytes());
            let r = s
                .handle_msg(&ctrl_msg(
                    tok,
                    crate::grid::CMD_GPU_GET_ENCODER_CAPACITY,
                    &aux,
                ))
                .unwrap();
            let o = Rsp::WIRE_LEN + 32 + nvrm_abi::mediate::ENCCAP_OFF;
            u32::from_le_bytes(r.bytes[o..o + 4].try_into().unwrap())
        };
        let capped = |policy, size| Profile {
            policy,
            size,
            reservation: size - fb,
            fb_length: fb,
            vgpu_type: "",
            encoder_capacity: share,
        };
        for p in [
            capped(Policy::Accounting, fb),
            capped(Policy::Reserved, fb + (256 << 20)),
            capped(Policy::Grid, 3968 << 20),
        ] {
            assert_eq!(answer(p), 37, "{:?}", p.policy);
        }
        assert_eq!(answer(Profile::OFF), 100, "no cap: RM's whole encoder");
    }

    /// Every VM tells its guest its own UUID and hands the driver the
    /// card's, whatever set its limit: the answer of a UUID control carries
    /// the VM's UUID, and a UVM call made with the VM's UUID reaches the
    /// driver with the card's and comes back with the VM's.
    #[test]
    fn the_uuid_is_the_vms_own_under_every_policy() {
        use crate::vram::{Policy, Profile};
        crate::grid::set_card(
            "vm/uuid-test/nvrm.sock",
            Ok((*b"\x9e\x37\x79\xb9uuid-test-4Q", 8192 << 20)),
        );
        let card = crate::grid::card().expect("set above");
        let grid = Profile {
            policy: Policy::Grid,
            size: 3968 << 20,
            reservation: 896 << 20,
            ..Profile::accounting(3072 << 20)
        };
        for profile in [Profile::OFF, Profile::accounting(3072 << 20), grid] {
            let (mut s, fake, tok) = profiled_session(profile);
            // GID_INFO, binary: RM wrote the card's 16 bytes @12.
            let mut gid = vec![0u8; 268];
            gid[4] = 2;
            gid[12..28].copy_from_slice(&card.host);
            let r = s
                .handle_msg(&ctrl_msg(tok, crate::grid::CMD_GPU_GET_GID_INFO, &gid))
                .unwrap();
            assert_eq!(
                &r.bytes[Rsp::WIRE_LEN + 32 + 12..Rsp::WIRE_LEN + 32 + 28],
                &card.guest,
                "{:?}",
                profile.policy
            );
            let tok = s
                .mirror
                .insert(std::fs::File::open("/dev/null").unwrap().into(), Dev::Uvm);
            // UVM_PAGEABLE_MEM_ACCESS_ON_GPU { uuid 16; bool; rmStatus }.
            let mut inline = vec![0u8; 24];
            inline[..16].copy_from_slice(&card.guest);
            let mut req = ioctl_req(tok, 70, 24, 0);
            req.dev_tag = DevTag::Uvm as u32;
            let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
            assert_eq!(
                &fake.last_inline().unwrap()[..16],
                &card.host,
                "the driver sees the card"
            );
            assert_eq!(
                &r.bytes[Rsp::WIRE_LEN..Rsp::WIRE_LEN + 16],
                &card.guest,
                "the guest sees its own"
            );
        }
    }

    /// NVOS32_FUNCTION_INFO under a cap tells the capped card: the reply the
    /// guest gets back carries total = limit and free = limit - used, and
    /// the request still reached RM. Without a cap RM's answer stands.
    #[test]
    fn nvos32_info_answers_the_capped_card() {
        let info = |tok| {
            let mut inline = vec![0u8; 184];
            inline[8..12].copy_from_slice(&sys::NVOS32_FUNCTION_INFO.to_le_bytes());
            msg(
                &ioctl_req(tok, sys::NV_ESC_RM_VID_HEAP_CONTROL, 184, 0),
                &inline,
                &[],
            )
        };
        let at = |r: &Vec<u8>, o: usize| {
            u64::from_le_bytes(
                r[Rsp::WIRE_LEN + o..Rsp::WIRE_LEN + o + 8]
                    .try_into()
                    .unwrap(),
            )
        };

        let (mut s, fake, tok, led) = capped_session(64 << 20);
        let r = s.handle_msg(&vram_alloc_msg(tok, 0xaa, 4 << 20)).unwrap();
        assert_eq!(rm_status(&r), sys::NV_OK);
        let before = fake.ioctl_count();
        let r = s.handle_msg(&info(tok)).unwrap();
        assert_eq!(fake.ioctl_count(), before + 1, "INFO reaches RM");
        assert_eq!(at(&r.bytes, 24), 64 << 20, "total = limit");
        assert_eq!(
            at(&r.bytes, 32),
            (64 << 20) - led.used(),
            "free = limit - used"
        );

        let (mut s, _fake, tok) = session();
        let r = s.handle_msg(&info(tok)).unwrap();
        assert_eq!(
            at(&r.bytes, 24),
            0,
            "no cap: RM's answer (the fake's zero) stands"
        );
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
        // pAllocParms @16 (NVOS64); the embedded pointer the table names
        // for class 0x7e; the fake never dereferences it, but prepare's
        // check must find the offset it expects.
        req.embedded_ptr_off = 16;
        msg(&req, &inline, &aux)
    }

    /// The (1a') substitution keeps books now: the guest's pointer and
    /// notifyIndex are saved BEFORE the id overwrites `data`, RM's
    /// hObjectNew is filed after the call, and the ctl the OS event hangs
    /// off is reported for polling; once per client.
    #[test]
    fn event_id_exhaustion_refuses_without_wrapping_or_opening_another_ctl() {
        let (mut s, fake, _) = session();
        s.next_event_id = u32::MAX as u64;
        assert_eq!(s.alloc_os_event_id(1).unwrap(), u32::MAX);
        assert_eq!(s.alloc_os_event_id(2).unwrap_err().errno, libc::ENOSPC);
        assert_eq!(s.waiter_slot_get(2).err().unwrap().errno, libc::ENOSPC);
        assert_eq!(s.event_ctls.len(), 1);
        assert_eq!(fake.ioctl_count(), 1);
        assert_eq!(s.next_event_id, u32::MAX as u64 + 1);
    }

    #[test]
    fn a_substituted_event_is_registered_and_its_ctl_reported() {
        // NVOS64: hObjectNew @8; and NOTHING at 40: `status` stays 0 ==
        // NV_OK, and offset 8 is inside every 16-byte struct the fake also
        // writes into (the ALLOC_OS_EVENT block).
        let fake = Arc::new(FakeSyscalls {
            writes_back: vec![(8, 0x5c00_00e1u32.to_le_bytes().to_vec())],
            ..Default::default()
        });
        let mut s = Session::<sys::DefaultAbi>::detached_proc(
            7,
            crate::vram::Ledger::off(),
            PinBudget::new(1024 << 20).unwrap(),
        )
        .unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned, Dev::Ctl);

        let h_client = 0xc1d8_0001u32;
        let r = s
            .handle_msg(&kernel_callback_alloc(
                tok,
                h_client,
                0x1000_0007,
                0xffff_8881_2345_6780,
            ))
            .unwrap();
        assert_eq!(errno_of(&r), None);

        // The ledger: ALLOC_OS_EVENT with fd:1 on a fd that is NOT the
        // guest's token, then the alloc itself with hClass 0x79.
        let calls: Vec<_> = fake
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                FakeCall::Ioctl {
                    fd,
                    request,
                    inline,
                } => Some((fd, nvrm_abi::ioc_nr(request as u32), inline)),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].1, nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT);
        assert_ne!(
            calls[0].0, fd,
            "the OS event hangs off the session's own ctl, not the guest's fd"
        );
        assert_eq!(&calls[0].2[0..4], &h_client.to_le_bytes());
        assert_eq!(&calls[0].2[8..12], &1u32.to_le_bytes(), "first id is 1");
        assert_eq!(calls[1].1, sys::NV_ESC_RM_ALLOC);
        assert_eq!(&calls[1].2[12..16], &sys::NV01_EVENT_OS_EVENT.to_le_bytes());

        // The guest reads the id back in `data` (this is what
        // vdisp_event_on_missing_parent keys on).
        let aux = &r.bytes[Rsp::WIRE_LEN + 48..];
        assert_eq!(&aux[NV0005_DATA..NV0005_DATA + 8], &1u64.to_le_bytes());
        assert_eq!(
            &aux[NV0005_HCLASS..NV0005_HCLASS + 4],
            &sys::NV01_EVENT_OS_EVENT.to_le_bytes()
        );

        // The books.
        let reg = s
            .events
            .get(&(h_client, 0x5c00_00e1))
            .copied()
            .expect("registered under RM's handle");
        assert_eq!(
            reg,
            EventReg {
                h_client,
                h_event: 0x5c00_00e1,
                class: sys::NV01_EVENT_KERNEL_CALLBACK_EX,
                notify_index: 0x1000_0007,
                guest_data: 0xffff_8881_2345_6780,
                token: tok,
                id: 1,
            }
        );
        assert_eq!(s.event_stats(), (1, 0, 0, 0));
        let ctl_fd = calls[0].0;
        assert!(matches!(s.take_pollables().as_slice(),
            [Pollable::EventCtl { h_client: c, fd }] if *c == h_client && *fd == ctl_fd));
        assert!(s.take_pollables().is_empty(), "reported once, then taken");

        // A second event on the SAME client reuses the ctl (id 2, no new
        // pollable); a different client gets a ctl of its own.
        let r = s
            .handle_msg(&kernel_callback_alloc(tok, h_client, 3, 0x1))
            .unwrap();
        assert_eq!(errno_of(&r), None);
        assert!(s.take_pollables().is_empty());
        let r = s
            .handle_msg(&kernel_callback_alloc(tok, h_client + 1, 3, 0x2))
            .unwrap();
        assert_eq!(errno_of(&r), None);
        let p = s.take_pollables();
        assert_eq!(p.len(), 1);
        assert!(
            matches!(p[0], Pollable::EventCtl { h_client: c, fd } if c == h_client + 1 && fd != ctl_fd)
        );
        assert_eq!(s.event_ctls.len(), 2);
        assert_eq!(s.event_stats().0, 3);
    }

    /// RM refused the substituted alloc: nothing is filed, and the id goes
    /// back with FREE_OS_EVENT.
    #[test]
    fn a_refused_substituted_event_gives_its_id_back() {
        let fake = Arc::new(FakeSyscalls {
            ioctl_ret: -1,
            ..Default::default()
        });
        let mut s = Session::<sys::DefaultAbi>::detached_proc(
            7,
            crate::vram::Ledger::off(),
            PinBudget::new(1024 << 20).unwrap(),
        )
        .unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned, Dev::Ctl);
        // ret -1 also fails ALLOC_OS_EVENT itself -> refusal, no alloc sent.
        let r = s
            .handle_msg(&kernel_callback_alloc(tok, 0xc100_0001, 7, 0x10))
            .unwrap();
        assert_eq!(errno_of(&r), Some(libc::EIO));
        assert_eq!(fake.ioctl_count(), 1);
        assert!(s.events.is_empty());

        // The fake cannot return different statuses by ioctl shape, and its
        // writes_back offsets are unchecked. Exercise this refusal through a
        // hand-built Plan instead of overwriting the smaller OS-event buffer.
        let fake = Arc::new(FakeSyscalls::default());
        s.sys = Box::new(fake.clone());
        let mut m = kernel_callback_alloc(tok, 0xc100_0002, 7, 0x10);
        let req = Req::from_bytes(&m).unwrap();
        let payload = m.split_off(Req::WIRE_LEN);
        let plan = s.prepare(&req, &payload).expect("prepare accepts");
        assert!(plan.event_reg.is_some());
        // Pretend RM refused: force a status that is not NV_OK into scratch
        // before execute reads it back (the fake writes nothing at 40).
        s.scratch[40..44].copy_from_slice(&sys::NV_ERR_INVALID_CLASS.to_le_bytes());
        s.execute(plan).unwrap();
        assert!(s.events.is_empty(), "a refused alloc registers nothing");
        let frees: Vec<_> = fake
            .calls()
            .into_iter()
            .filter(|c| {
                matches!(c,
            FakeCall::Ioctl { request, .. }
                if nvrm_abi::ioc_nr(*request as u32) == nvrm_abi::nvgpu::NV_ESC_FREE_OS_EVENT)
            })
            .collect();
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
        let mut s = Session::<sys::DefaultAbi>::detached_proc(
            7,
            crate::vram::Ledger::off(),
            PinBudget::new(1024 << 20).unwrap(),
        )
        .unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned, Dev::Ctl);
        let c1 = 0xc1d8_0001u32;
        s.handle_msg(&kernel_callback_alloc(tok, c1, 7, 0x10))
            .unwrap();
        assert_eq!(s.events.len(), 1);

        // Free the event object: NVOS00 hRoot @0, hObjectOld @8.
        let mut inline = vec![0u8; 16];
        inline[0..4].copy_from_slice(&c1.to_le_bytes());
        inline[8..12].copy_from_slice(&0x5c00_00e1u32.to_le_bytes());
        let n_before = fake.ioctl_count();
        s.handle_msg(&msg(
            &ioctl_req(tok, sys::NV_ESC_RM_FREE, 16, 0),
            &inline,
            &[],
        ))
        .unwrap();
        assert!(s.events.is_empty());
        let after: Vec<_> = fake.calls()[n_before..]
            .iter()
            .filter_map(|c| match c {
                FakeCall::Ioctl { request, .. } => Some(nvrm_abi::ioc_nr(*request as u32)),
                _ => None,
            })
            .collect();
        assert_eq!(
            after,
            vec![sys::NV_ESC_RM_FREE, nvrm_abi::nvgpu::NV_ESC_FREE_OS_EVENT]
        );

        // Free one client while retaining the other's events. Use a fake with
        // no writes_back so it cannot overwrite RM_FREE's hObjectOld@8.
        let fake = Arc::new(FakeSyscalls::default());
        s.sys = Box::new(fake.clone());
        s.handle_msg(&kernel_callback_alloc(tok, c1, 7, 0x10))
            .unwrap();
        s.handle_msg(&kernel_callback_alloc(tok, c1 + 1, 7, 0x11))
            .unwrap();
        assert!(s.events.contains_key(&(c1, 0)) && s.events.contains_key(&(c1 + 1, 0)));
        let mut inline = vec![0u8; 16];
        inline[0..4].copy_from_slice(&c1.to_le_bytes());
        inline[8..12].copy_from_slice(&c1.to_le_bytes());
        let n_before = fake.ioctl_count();
        assert_eq!(s.event_ctls.len(), 2, "one ctl per client, before the free");
        s.handle_msg(&msg(
            &ioctl_req(tok, sys::NV_ESC_RM_FREE, 16, 0),
            &inline,
            &[],
        ))
        .unwrap();
        assert_eq!(s.events.len(), 1, "the other client's event stays");
        assert!(s.events.contains_key(&(c1 + 1, 0)));
        assert_eq!(
            fake.ioctl_count(),
            n_before + 1,
            "RM frees the client's os events itself"
        );

        // And the ctl fd of the freed client leaves too. Until 2026-08-17
        // it did not, and RM therefore kept the client alive on the host
        // for as long as the session ran (`pending_ctl_unwatch`).
        assert_eq!(s.event_ctls.len(), 1, "the freed client's ctl is gone");
        assert!(
            s.event_ctls.contains_key(&(c1 + 1)),
            "the other client keeps its own"
        );
        let out = s.take_ctl_unwatch();
        assert_eq!(out.len(), 1, "handed to the device, not dropped here");
        assert_eq!(out[0].0, c1, "and it names the client whose ctl it is");
        assert!(s.take_ctl_unwatch().is_empty(), "reported once, then taken");
    }

    /// Closing the client FD releases RM objects without an RM_FREE.
    /// Event holdings must follow that token, not wait for process teardown;
    /// question 31 recorded 2003 control FDs held by one live guest process.
    #[test]
    fn closing_the_fd_a_client_was_allocated_on_gives_its_ctl_back() {
        let fake = Arc::new(FakeSyscalls::default());
        let mut s = Session::<sys::DefaultAbi>::detached_proc(
            7,
            crate::vram::Ledger::off(),
            PinBudget::new(1024 << 20).unwrap(),
        )
        .unwrap();
        s.sys = Box::new(fake.clone());
        let mktok = |s: &mut Session<sys::DefaultAbi>| {
            let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
            let owned =
                unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
            s.mirror.insert(owned, Dev::Ctl)
        };
        let tok_a = mktok(&mut s);
        let tok_b = mktok(&mut s);
        let (c_a, c_b) = (0xc1d8_0001u32, 0xc1d8_0002u32);

        // Each client is ALLOCATED on its own FD: NVOS21 hRoot @0,
        // hObjectParent @4 (0 = a root), hObjectNew @8, status @28.
        let mut root = |tok: u64, h: u32| {
            let mut inline = vec![0u8; 32];
            inline[8..12].copy_from_slice(&h.to_le_bytes());
            s.handle_msg(&msg(
                &ioctl_req(tok, sys::NV_ESC_RM_ALLOC, 32, 0),
                &inline,
                &[],
            ))
            .unwrap();
        };
        root(tok_a, c_a);
        root(tok_b, c_b);
        assert_eq!(s.client_token.get(&c_a), Some(&tok_a), "noted at the alloc");
        assert_eq!(s.client_token.get(&c_b), Some(&tok_b));

        // Give each of them an event ctl, the FD that used to leak.
        s.handle_msg(&kernel_callback_alloc(tok_a, c_a, 7, 0x10))
            .unwrap();
        s.handle_msg(&kernel_callback_alloc(tok_b, c_b, 7, 0x11))
            .unwrap();
        assert_eq!(s.event_ctls.len(), 2, "one ctl per client");
        let _ = s.take_ctl_unwatch();

        // Close ONLY the first FD. No RM_FREE crosses; this is the point.
        s.handle_msg(&msg(&close_req(tok_a), &[], &[])).unwrap();
        assert_eq!(
            s.event_ctls.len(),
            1,
            "the closed FD's client gives its ctl back"
        );
        assert!(
            s.event_ctls.contains_key(&c_b),
            "the other FD's client keeps its own"
        );
        assert!(
            !s.client_token.contains_key(&c_a),
            "and its note goes with it"
        );
        let out = s.take_ctl_unwatch();
        assert_eq!(out.len(), 1, "handed to the device, not dropped here");
        assert_eq!(out[0].0, c_a, "and it names the right client");
        assert_eq!(s.ctl_freed_by_close, 1, "counted on the close door");
        assert_eq!(s.ctl_freed_by_rmfree, 0, "and not on the other one");

        // The second FD too, so a session that closes everything keeps
        // nothing: that is the property the leak violated.
        s.handle_msg(&msg(&close_req(tok_b), &[], &[])).unwrap();
        assert!(
            s.event_ctls.is_empty(),
            "nothing held after both FDs are closed"
        );
        assert!(s.client_token.is_empty());
    }

    /// Drain each event by (hClient, hObject). The fake leaves NvUnixEvent
    /// zeroed; it matches only a registration with hObject=0.
    #[test]
    fn drain_asks_get_event_data_and_matches_by_client_and_handle() {
        let fake = Arc::new(FakeSyscalls::default());
        let mut s = Session::<sys::DefaultAbi>::detached_proc(
            7,
            crate::vram::Ledger::off(),
            PinBudget::new(1024 << 20).unwrap(),
        )
        .unwrap();
        s.sys = Box::new(fake.clone());
        let fd = unsafe { libc::memfd_create(c"leandro-session-test".as_ptr(), 0) };
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned, Dev::Ctl);
        let c1 = 0xc1d8_0001u32;
        // hObjectNew stays 0 (no writes_back): registered under (c1, 0).
        s.handle_msg(&kernel_callback_alloc(tok, c1, 0x1000_0007, 0x10))
            .unwrap();
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
            FakeCall::Ioctl {
                fd: on,
                request,
                inline,
            } => {
                assert_eq!(
                    nvrm_abi::ioc_nr(request as u32),
                    sys::NV_ESC_RM_GET_EVENT_DATA
                );
                assert_eq!(
                    nvrm_abi::ioc_size(request as u32) as usize,
                    std::mem::size_of::<sys::NVOS41_PARAMETERS>()
                );
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
    /// names the fd it rode on as its wake channel; reported as
    /// `Pollable::Client` with the token, whether or not RM accepts.
    #[test]
    fn a_guest_os_event_alloc_reports_its_fd_for_polling() {
        let (mut s, fake, tok) = session_with_kind(Dev::Gpu);
        let fd = s.mirror.raw(tok).unwrap();
        let mut inline = vec![0u8; 16];
        inline[8..12].copy_from_slice(&23u32.to_le_bytes());
        let mut req = ioctl_req(tok, nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT, 16, 0);
        req.dev_tag = DevTag::Gpu as u32;
        req.fd_field_off = 8;
        req.fd_field_token = tok;
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&r), None);
        assert_eq!(fake.ioctl_count(), 1);
        assert!(matches!(s.take_pollables().as_slice(),
            [Pollable::Client { token, fd: actual, owner: None }] if *token == tok && *actual == fd));
        // A UVM ioctl with the same raw number is not an OS event.
        let mut req = ioctl_req(tok, nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT, 16, 0);
        req.dev_tag = DevTag::Uvm as u32;
        s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert!(s.take_pollables().is_empty());
    }

    // ---- semaphore-surface waiters (the Wayland fence path) ---------------

    /// One semsurf control as the guest kernel sends it: NVOS54 (32 B) with
    /// the cmd, params in aux with the notification handle at `off`.
    fn semsurf_control(
        tok: u64,
        h_client: u32,
        cmd: u32,
        aux_len: usize,
        off: usize,
        handle: u64,
    ) -> Vec<u8> {
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
    /// is reported for the poller; NOT for the epoll set.
    #[test]
    fn a_semsurf_waiter_is_substituted_and_armed() {
        let (mut s, fake, tok) = session();
        let h_client = 0xc1d8_0002u32;
        let kc = 0xffff_8881_dead_be00u64;

        let r = s
            .handle_msg(&semsurf_control(
                tok,
                h_client,
                CTRL_SEMSURF_REGISTER_WAITER,
                32,
                24,
                kc,
            ))
            .unwrap();
        assert_eq!(errno_of(&r), None);

        // The ledger: ALLOC_OS_EVENT of (h_client, id 1) on a PRIVATE fd,
        // then the control itself with the id where the pointer was.
        let calls: Vec<_> = fake
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                FakeCall::Ioctl {
                    fd,
                    request,
                    inline,
                } => Some((fd, nvrm_abi::ioc_nr(request as u32), inline)),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].1, nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT);
        assert_eq!(&calls[0].2[0..4], &h_client.to_le_bytes());
        assert_eq!(&calls[0].2[8..12], &1u32.to_le_bytes());
        assert_eq!(calls[1].1, sys::NV_ESC_RM_CONTROL);

        // The guest's params went out with the id, and came back with it.
        let aux = &r.bytes[Rsp::WIRE_LEN + 32..];
        assert_eq!(&aux[24..32], &1u64.to_le_bytes());

        // Armed, and reported for the POLLER (the waiter fd must never
        // enter the epoll set; waiters.rs says why).
        let w = s.waiters.get(&1).expect("armed under its id");
        assert_eq!((w.h_client, w.guest_kc), (h_client, kc));
        use std::os::fd::AsRawFd;
        let slot_fd = w.slot.ctl.as_raw_fd();
        let registration = RegistrationId {
            event_id: 1,
            generation: 1,
        };
        assert!(matches!(s.take_pollables().as_slice(),
            [Pollable::Waiter { registration: r, fd }] if *r == registration && fd.as_raw_fd() == slot_fd));

        // The wake retires it and recycles the slot.
        assert_eq!(s.semsurf_wake(registration), Some((h_client, kc, tok)));
        assert!(s.waiters.is_empty());
        assert_eq!(s.waiter_pool[&h_client].len(), 1);
        assert_eq!(
            s.semsurf_wake(registration),
            None,
            "one-shot: the second wake finds nothing"
        );
    }

    /// A user-space registration carries an OS-event id (NvU32; RM casts,
    /// os.c:1744); it passes through byte for byte, no slot, no pollable.
    #[test]
    fn a_user_space_waiter_handle_passes_through() {
        let (mut s, fake, tok) = session();
        let r = s
            .handle_msg(&semsurf_control(
                tok,
                0xc1d8_0003,
                CTRL_SEMSURF_REGISTER_WAITER,
                32,
                24,
                23,
            ))
            .unwrap();
        assert_eq!(errno_of(&r), None);
        assert_eq!(fake.ioctl_count(), 1, "no ALLOC_OS_EVENT for a user handle");
        let aux = &r.bytes[Rsp::WIRE_LEN + 32..];
        assert_eq!(&aux[24..32], &23u64.to_le_bytes(), "untouched");
        assert!(s.waiters.is_empty());
        assert!(s.take_pollables().is_empty());
    }

    /// Successful cancellation quarantines the slot until poller acknowledgement.
    #[test]
    fn an_unregistered_waiter_recycles_its_slot() {
        let (mut s, fake, tok) = session();
        let h_client = 0xc1d8_0004u32;
        let kc = 0xffff_8881_dead_bf00u64;
        s.handle_msg(&semsurf_control(
            tok,
            h_client,
            CTRL_SEMSURF_REGISTER_WAITER,
            32,
            24,
            kc,
        ))
        .unwrap();
        s.take_pollables();

        let r = s
            .handle_msg(&semsurf_control(
                tok,
                h_client,
                CTRL_SEMSURF_UNREGISTER_WAITER,
                24,
                16,
                kc,
            ))
            .unwrap();
        assert_eq!(errno_of(&r), None);
        let aux = &r.bytes[Rsp::WIRE_LEN + 32..];
        assert_eq!(
            &aux[16..24],
            &1u64.to_le_bytes(),
            "translated to the same id"
        );
        assert!(s.waiters.is_empty(), "cancelled");
        assert!(
            s.waiter_pool.get(&h_client).is_none_or(Vec::is_empty),
            "not reusable before poller acknowledgement"
        );
        let retired = s.take_unwatch();
        assert_eq!(retired.len(), 1);
        s.finish_unwatch(retired);
        assert_eq!(s.waiter_pool[&h_client].len(), 1);

        // Re-register: the pooled slot serves again; no new ALLOC_OS_EVENT.
        let before = fake.calls().iter().filter(|c| matches!(c,
            FakeCall::Ioctl { request, .. }
                if nvrm_abi::ioc_nr(*request as u32) == nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT
        )).count();
        s.handle_msg(&semsurf_control(
            tok,
            h_client,
            CTRL_SEMSURF_REGISTER_WAITER,
            32,
            24,
            kc,
        ))
        .unwrap();
        let after = fake.calls().iter().filter(|c| matches!(c,
            FakeCall::Ioctl { request, .. }
                if nvrm_abi::ioc_nr(*request as u32) == nvrm_abi::nvgpu::NV_ESC_ALLOC_OS_EVENT
        )).count();
        assert_eq!(before, after, "the slot was reused, not re-allocated");
        assert!(s.waiters.contains_key(&1));

        // An unregister for a handle nobody armed leaves the pointer alone:
        // RM answers OBJECT_NOT_FOUND, which is the truthful "too late".
        let r = s
            .handle_msg(&semsurf_control(
                tok,
                h_client,
                CTRL_SEMSURF_UNREGISTER_WAITER,
                24,
                16,
                0xffff_8881_0000_0100,
            ))
            .unwrap();
        let aux = &r.bytes[Rsp::WIRE_LEN + 32..];
        assert_eq!(&aux[16..24], &0xffff_8881_0000_0100u64.to_le_bytes());
    }

    #[test]
    fn a_stale_completion_cannot_retire_a_rearmed_slot() {
        let (mut s, _, tok) = session();
        let client = 0xc100_0001;
        let callback = 0xffff_8881_dead_be00;
        let register = semsurf_control(tok, client, CTRL_SEMSURF_REGISTER_WAITER, 32, 24, callback);
        s.handle_msg(&register).unwrap();
        let old = RegistrationId {
            event_id: 1,
            generation: s.waiters[&1].generation,
        };
        assert_eq!(s.semsurf_wake(old), Some((client, callback, tok)));
        s.take_pollables();

        s.handle_msg(&register).unwrap();
        let new = RegistrationId {
            event_id: 1,
            generation: s.waiters[&1].generation,
        };
        assert_ne!(old, new);
        assert_eq!(s.semsurf_wake(old), None);
        assert!(s.waiters.contains_key(&1));
        assert!(s.waiter_pool[&client].is_empty());
        assert_eq!(s.semsurf_wake(new), Some((client, callback, tok)));
        assert_eq!(s.waiter_pool[&client].len(), 1);
    }

    #[test]
    fn unregister_matches_surface_index_and_wait_value() {
        let callback = 0xffff_8881_dead_be00;
        for different in [
            WaiterTarget {
                surface: 2,
                index: 3,
                wait_value: 4,
            },
            WaiterTarget {
                surface: 1,
                index: 5,
                wait_value: 4,
            },
            WaiterTarget {
                surface: 1,
                index: 3,
                wait_value: 6,
            },
        ] {
            let (mut s, _, tok) = session();
            let client = 0xc100_0001;
            let control = |target: WaiterTarget, unregister| {
                let (cmd, len, off) = if unregister {
                    (CTRL_SEMSURF_UNREGISTER_WAITER, 24, 16)
                } else {
                    (CTRL_SEMSURF_REGISTER_WAITER, 32, 24)
                };
                let mut bytes = semsurf_control(tok, client, cmd, len, off, callback);
                bytes[Req::WIRE_LEN + 4..Req::WIRE_LEN + 8]
                    .copy_from_slice(&target.surface.to_le_bytes());
                bytes[Req::WIRE_LEN + 32..Req::WIRE_LEN + 40]
                    .copy_from_slice(&target.index.to_le_bytes());
                bytes[Req::WIRE_LEN + 40..Req::WIRE_LEN + 48]
                    .copy_from_slice(&target.wait_value.to_le_bytes());
                bytes
            };
            let first = WaiterTarget {
                surface: 1,
                index: 3,
                wait_value: 4,
            };
            s.handle_msg(&control(first, false)).unwrap();
            s.handle_msg(&control(different, false)).unwrap();
            let response = s.handle_msg(&control(different, true)).unwrap();
            assert_eq!(
                &response.bytes[Rsp::WIRE_LEN + 48..Rsp::WIRE_LEN + 56],
                &2u64.to_le_bytes()
            );
            assert_eq!(s.waiters.len(), 1);
            assert_eq!(s.waiters[&1].target, first);
            let retired = s.take_unwatch();
            assert_eq!(retired.len(), 1);
            assert_eq!(retired[0].event_id(), 2);
        }
    }

    #[test]
    fn a_failed_unregister_keeps_the_original_arm() {
        for syscall_failed in [false, true] {
            let (mut s, _, tok) = session();
            let client = 0xc100_0001;
            let callback = 0xffff_8881_dead_be00;
            s.handle_msg(&semsurf_control(
                tok,
                client,
                CTRL_SEMSURF_REGISTER_WAITER,
                32,
                24,
                callback,
            ))
            .unwrap();
            s.take_pollables();
            s.sys = Box::new(FakeSyscalls {
                ioctl_ret: if syscall_failed { -1 } else { 0 },
                writes_back: vec![(28, sys::NV_ERR_OBJECT_NOT_FOUND.to_le_bytes().to_vec())],
                ..FakeSyscalls::default()
            });
            s.handle_msg(&semsurf_control(
                tok,
                client,
                CTRL_SEMSURF_UNREGISTER_WAITER,
                24,
                16,
                callback,
            ))
            .unwrap();
            assert!(s.take_unwatch().is_empty());
            assert!(s.waiter_pool.get(&client).is_none_or(Vec::is_empty));
            assert_eq!(
                s.semsurf_wake(RegistrationId {
                    event_id: 1,
                    generation: 1
                }),
                Some((client, callback, tok))
            );
        }
    }

    #[test]
    fn malformed_event_requests_acquire_no_os_events() {
        let (mut s, fake, tok) = session();
        let callback = kernel_callback_alloc(tok, 0xc100_0001, 7, 0x10);
        let waiter = semsurf_control(
            tok,
            0xc100_0001,
            CTRL_SEMSURF_REGISTER_WAITER,
            32,
            24,
            0xffff_8881_dead_be00,
        );
        for mut bytes in [callback, waiter] {
            let mut req = Req::from_bytes(&bytes).unwrap();
            req.nested_count = 1;
            bytes[..Req::WIRE_LEN].copy_from_slice(req.as_bytes());
            let reply = s.handle_msg(&bytes).unwrap();
            assert_eq!(errno_of(&reply), Some(libc::EINVAL));
        }
        assert_eq!(fake.ioctl_count(), 0);
        assert!(s.take_pollables().is_empty());
        assert!(s.event_ctls.is_empty());
        assert!(s.waiters.is_empty());
        assert!(s.waiter_pool.is_empty());
        assert_eq!(s.next_waiter_generation, 1);
    }

    #[test]
    fn exhausted_waiter_generations_refuse_before_allocating() {
        let (mut s, fake, tok) = session();
        s.next_waiter_generation = u64::MAX;
        let reply = s
            .handle_msg(&semsurf_control(
                tok,
                0xc100_0001,
                CTRL_SEMSURF_REGISTER_WAITER,
                32,
                24,
                0xffff_8881_dead_be00,
            ))
            .unwrap();
        assert_eq!(errno_of(&reply), Some(libc::ENOSPC));
        assert_eq!(fake.ioctl_count(), 0);
        assert!(s.take_pollables().is_empty());
        assert!(s.waiter_pool.is_empty());
        assert_eq!(s.next_waiter_generation, u64::MAX);
    }

    // ---- errno: the guest gets the errno of ITS call ---------------------

    /// A failed forwarded ioctl answers `-errno`; the plain case, which
    /// establishes that the fake's errno reaches the wire at all.
    #[test]
    fn a_failed_ioctl_answers_its_own_errno() {
        let (mut s, fake, tok) = session();
        fake.ioctl_rets
            .lock()
            .unwrap()
            .push_back((-1, libc::ENOSPC));
        let inline = vec![0u8; 32];
        let req = ioctl_req(tok, sys::NV_ESC_RM_CONTROL, 32, 0);
        let r = s.handle_msg(&msg(&req, &inline, &[])).unwrap();
        assert_eq!(errno_of(&r), Some(libc::ENOSPC));
        assert_eq!(fake.ioctl_count(), 1);
    }

    /// Preserve the forwarded call's ENOSPC across a cleanup ioctl that
    /// sets EPERM. The guest must receive the original failure.
    #[test]
    fn a_later_syscall_does_not_replace_the_errno_of_the_forwarded_call() {
        let (mut s, fake, tok) = session();
        {
            let mut q = fake.ioctl_rets.lock().unwrap();
            q.push_back((0, 0)); // ALLOC_OS_EVENT in prepare
            q.push_back((-1, libc::ENOSPC)); // the forwarded RM_ALLOC
            q.push_back((0, libc::EPERM)); // FREE_OS_EVENT on the failure path
        }
        let r = s
            .handle_msg(&kernel_callback_alloc(tok, 0xc100_0001, 7, 0x10))
            .unwrap();
        assert_eq!(
            fake.ioctl_count(),
            3,
            "alloc-os-event, the alloc, free-os-event: {:?}",
            fake.calls()
        );
        assert_eq!(
            errno_of(&r),
            Some(libc::ENOSPC),
            "the guest must see the alloc's errno"
        );
        assert!(s.events.is_empty());
    }

    // ---- UVM parameter layouts: hand-quoted numbers vs the vendor structs --

    /// Each UVM status offset must fit its descriptor size, and both tables
    /// must describe the same command set.
    #[test]
    fn uvm_status_offsets_lie_inside_the_sizes_xlate_sends() {
        use nvrm_abi::xlate::{uvm, uvm_param_size};
        let known = [
            uvm::INITIALIZE,
            uvm::PAGEABLE_MEM_ACCESS,
            uvm::MM_INITIALIZE,
            uvm::REGISTER_GPU_VASPACE,
            uvm::UNREGISTER_GPU_VASPACE,
            uvm::REGISTER_CHANNEL,
            uvm::UNREGISTER_CHANNEL,
            uvm::MAP_EXTERNAL_ALLOCATION,
            uvm::FREE,
            uvm::REGISTER_GPU,
            uvm::MAP_DYNAMIC_PARALLELISM_REGION,
            uvm::ALLOC_SEMAPHORE_POOL,
            uvm::PAGEABLE_MEM_ACCESS_ON_GPU,
            uvm::SET_PREFERRED_LOCATION,
            uvm::UNSET_PREFERRED_LOCATION,
            uvm::ENABLE_READ_DUPLICATION,
            uvm::DISABLE_READ_DUPLICATION,
            uvm::SET_ACCESSED_BY,
            uvm::UNSET_ACCESSED_BY,
            uvm::MIGRATE,
            uvm::VALIDATE_VA_RANGE,
            uvm::CREATE_EXTERNAL_RANGE,
        ];
        for nr in known {
            let off = uvm_status_off::<sys::DefaultAbi>(nr)
                .unwrap_or_else(|| panic!("{nr:#x} has no status offset"));
            let size = uvm_param_size(nr).unwrap_or_else(|| panic!("{nr:#x} has no size")) as usize;
            assert!(
                off + 4 <= size,
                "{nr:#x}: rmStatus @{off} outside the {size}-byte block"
            );
        }
        // DEINITIALIZE has no parameter block, hence no status; a number
        // nobody knows answers None rather than a guess.
        assert_eq!(uvm_status_off::<sys::DefaultAbi>(uvm::DEINITIALIZE), None);
        assert_eq!(uvm_status_off::<sys::DefaultAbi>(0x7fff), None);
        // And the two the fake answers write to, as literals the way the
        // execute branch quotes them.
        assert_eq!(uvm_status_off::<sys::DefaultAbi>(uvm::MIGRATE), Some(72));
        assert_eq!(
            uvm_status_off::<sys::DefaultAbi>(uvm::ALLOC_SEMAPHORE_POOL),
            Some(9240)
        );
    }

    /// An undersized process-list answer returns EINVAL with no payload.
    /// Forwarding the unrewritten answer would expose host PIDs.
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
        assert_eq!(
            errno_of(&r),
            Some(libc::EINVAL),
            "short process list refused"
        );
        // The ioctl DID run; the refusal is about RM's answer, not about
        // the guest's request, and that is why the bookkeeping after it
        // must not be skipped.
        assert_eq!(fake.ioctl_count(), 1);
        // A refusal carries no payload: nothing of RM's answer reaches the
        // guest.
        let rsp = Rsp::from_bytes(&r.bytes).unwrap();
        assert_eq!((rsp.inline_len, rsp.aux_len), (0, 0));
        assert_eq!(r.bytes.len(), Rsp::WIRE_LEN);
    }

    /// Skip blob ID 0 when the counter wraps, including session sub_id=0.
    /// The guest treats zero as registration failure. Start near wrap to
    /// exercise the boundary without billions of mappings.
    #[test]
    fn a_wrapping_blob_id_never_hands_out_zero() {
        let mut s = Session::<sys::DefaultAbi>::detached_proc(
            0,
            crate::vram::Ledger::off(),
            PinBudget::new(1024 << 20).unwrap(),
        )
        .unwrap();
        s.sys = Box::new(Arc::new(FakeSyscalls::default()));
        let fd = unsafe { libc::memfd_create(c"leandro-blobid-test".as_ptr(), 0) };
        assert!(fd >= 0);
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let tok = s.mirror.insert(owned, Dev::Ctl);

        let prepare = |s: &mut Session<sys::DefaultAbi>, seq: u32| -> u64 {
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
        assert!(
            !s.pending_maps.contains_key(&0),
            "a mapping registered under 0"
        );
        // And the session id in the high half is untouched by the wrap.
        assert_eq!(wrapped >> 32, 0);
    }

    /// Reject unknown device tags with EINVAL before issuing an ioctl.
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
