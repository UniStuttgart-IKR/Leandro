// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Map guest RAM into contiguous host arenas for NVIDIA Resource Manager (RM).
//!
//! Semaphore pools register an arena as an OS descriptor and attach it at the
//! guest's GPU VA through UVM. Forwarded RM_ALLOC_MEMORY calls use the arena's
//! host VA as NVOS02.pMemory. Both paths require file-backed guest RAM
//! (`--memory shared=on`) and retain the arena while RM uses its pages.

use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use nvrm_abi::{share, sys, NvDevice};

use crate::guest_words::{GuestAddr, GuestLen};

type Mem = vm_memory::GuestMemoryAtomic<vm_memory::GuestMemoryMmap<()>>;

/// Linux UVM command numbers (UVM_IOCTL_BASE(i) == i).
/// The guest has already registered its GPU and VASpace.
const UVM_MAP_EXTERNAL_ALLOCATION: u64 = 33;
const UVM_CREATE_EXTERNAL_RANGE: u64 = 73;

/// VM-wide admission budget for registered guest-page arenas, including
/// failed cleanup and forwarded allocations whose remaining aliases are unknown.
pub struct PinBudget {
    limit: u64,
    used: AtomicU64,
    retained_bytes: AtomicU64,
    retained: Mutex<Vec<Arena>>,
    quarantine: Mutex<Vec<PoolMap>>,
    private_clients: Mutex<HashSet<u32>>,
}

impl PinBudget {
    pub fn new(limit: u64) -> Result<Arc<Self>> {
        if limit == 0 {
            bail!("aggregate pin budget must be nonzero");
        }
        Ok(Arc::new(Self {
            limit,
            used: AtomicU64::new(0),
            retained_bytes: AtomicU64::new(0),
            retained: Mutex::new(Vec::new()),
            quarantine: Mutex::new(Vec::new()),
            private_clients: Mutex::new(HashSet::new()),
        }))
    }

    /// Provisional default: 1 GiB per VM. Invalid overrides fail startup.
    pub fn from_env() -> Result<Arc<Self>> {
        let limit = match std::env::var("LEA_MAX_PIN_TOTAL_MIB") {
            Ok(raw) => parse_pin_bytes(&raw)
                .context("LEA_MAX_PIN_TOTAL_MIB must be a positive MiB count that fits u64")?,
            Err(std::env::VarError::NotPresent) => 1024 << 20,
            Err(error) => return Err(error).context("LEA_MAX_PIN_TOTAL_MIB"),
        };
        Self::new(limit)
    }

    pub fn used_bytes(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    pub fn retained_bytes(&self) -> u64 {
        self.retained_bytes.load(Ordering::Relaxed)
    }

    pub fn quarantined_bytes(&self) -> u64 {
        self.quarantine
            .lock()
            .unwrap()
            .iter()
            .map(|pool| pool.quarantined_charge)
            .sum()
    }

    fn reserve(self: &Arc<Self>, bytes: u64) -> Result<PinLease> {
        if bytes == 0 {
            bail!("cannot reserve an empty arena");
        }
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|sum| *sum <= self.limit)
            })
            .map_err(|used| {
                anyhow::anyhow!(
                    "guest-page registration budget exceeded: {used}/{} bytes, \
                 {} retained after source free, requested {bytes} (LEA_MAX_PIN_TOTAL_MIB)",
                    self.limit,
                    self.retained_bytes()
                )
            })?;
        Ok(PinLease {
            budget: self.clone(),
            bytes,
        })
    }

    fn retain(&self, mut arena: Arena) {
        let bytes = arena.detach_charge();
        if bytes != 0 && self.retained_bytes.fetch_add(bytes, Ordering::Relaxed) == 0 {
            eprintln!(
                "vhost-user-nvrm: retaining forwarded guest-page registrations: \
                source RM_FREE does not prove duplicate or exported references are gone"
            );
        }
        self.retained.lock().unwrap().push(arena);
    }

    fn quarantine(&self, mut pool: PoolMap) {
        pool.quarantined_charge = pool.arena.as_mut().unwrap().detach_charge();
        self.quarantine.lock().unwrap().push(pool);
    }

    pub(crate) fn private_client(self: &Arc<Self>, root: u32) -> PrivateClient {
        self.private_clients.lock().unwrap().insert(root);
        PrivateClient {
            root,
            budget: Arc::downgrade(self),
        }
    }

    /// Retry owners transferred from closed sessions before admitting more memory.
    pub fn retry_cleanup(&self) {
        self.quarantine.lock().unwrap().retain_mut(|pool| {
            if pool.cleanup().is_err() {
                return true;
            }
            self.used
                .fetch_sub(pool.quarantined_charge, Ordering::Relaxed);
            pool.quarantined_charge = 0;
            false
        });
    }
}

struct PinLease {
    budget: Arc<PinBudget>,
    bytes: u64,
}

pub(crate) struct PrivateClient {
    root: u32,
    budget: Weak<PinBudget>,
}

impl Drop for PrivateClient {
    fn drop(&mut self) {
        if let Some(budget) = self.budget.upgrade() {
            budget.private_clients.lock().unwrap().remove(&self.root);
        }
    }
}

impl Drop for PinLease {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// nvos.h:192-279, DRF hi:lo as a shift: PHYSICALITY 7:4 (NONCONTIGUOUS=1),
/// LOCATION 11:8 (PCI=0), COHERENCY 15:12 (CACHED=1), MAPPING 31:30
/// (NO_MAP=1). RmAllocOsDescriptor (escape.c:206-225) demands exactly this.
#[allow(clippy::identity_op)] // (0 << 8) documents the DRF field LOCATION=PCI
const OSDESC_FLAGS: u32 = (1 << 4) | (0 << 8) | (1 << 12) | (1 << 30);

fn parse_pin_bytes(raw: &str) -> Option<u64> {
    raw.trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1 << 20)
        .filter(|bytes| *bytes != 0)
}

fn host_page_size() -> Result<u64> {
    let bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if bytes <= 0 {
        bail!("cannot determine host page size");
    }
    Ok(bytes as u64)
}

fn registration_bytes(bytes: u64) -> Result<u64> {
    if bytes == 0 {
        bail!("cannot reserve an empty arena");
    }
    let page = host_page_size()?;
    let rounded = bytes
        .checked_add(page - 1)
        .context("page-rounded arena length overflows")?;
    Ok(rounded / page * page)
}

/// Per-arena pin limit, read once. Invalid LEA_MAX_PIN_MIB values use 256 MiB.
fn max_pin_bytes() -> u64 {
    use std::sync::OnceLock;
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let default_mib = 256u64;
        match std::env::var("LEA_MAX_PIN_MIB") {
            Err(_) => default_mib << 20,
            Ok(s) => match parse_pin_bytes(&s) {
                Some(bytes) => bytes,
                None => {
                    eprintln!(
                        "vhost-user-nvrm: LEA_MAX_PIN_MIB={s:?} unusable, \
                         using default {default_mib} MiB"
                    );
                    default_mib << 20
                }
            },
        }
    })
}

/// One guest-physical range, derived from the guest's /proc/self/pagemap.
#[derive(Copy, Clone, Debug)]
pub struct GpaRun {
    pub gpa: GuestAddr,
    pub len: GuestLen,
}

impl GpaRun {
    pub const WIRE: usize = 16;

    pub fn decode(aux: &[u8], count: usize) -> Option<Vec<GpaRun>> {
        // Reject overflow before indexing the guest's auxiliary buffer.
        let need = count.checked_mul(Self::WIRE)?;
        if aux.len() < need {
            return None;
        }
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let o = i * Self::WIRE;
            out.push(GpaRun {
                gpa: GuestAddr::new(u64::from_le_bytes(aux[o..o + 8].try_into().unwrap())),
                len: GuestLen::new(u64::from_le_bytes(aux[o + 8..o + 16].try_into().unwrap())),
            });
        }
        Some(out)
    }
}

/// One contiguous host VA that aliases exactly the guest pages of the runs.
/// Reserved as PROT_NONE, then covered run by run from the guest RAM file
/// (MAP_SHARED|MAP_FIXED). While it lives, the pages are addressable for
/// RM; Drop releases them.
pub struct Arena {
    base: *mut u8,
    len: usize,
    lease: Option<PinLease>,
}

// SAFETY: the arena is a pure address-space reservation of this process;
// access (and the Drop/munmap) is serialized by the device RwLock that
// carries the whole session.
unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
    /// Build from GPA runs and the guest RAM file. `total` is the expected
    /// overall length (the host does not blindly trust the sum of the runs).
    pub fn build(mem: &Mem, runs: &[GpaRun], total: GuestLen) -> Result<Arena> {
        use vm_memory::{GuestAddress, GuestAddressSpace, GuestMemoryBackend, GuestMemoryRegion};
        // Guest lengths must fit without wrapping: MAP_FIXED outside this
        // reservation would overwrite unrelated host mappings.
        let mut sum = GuestLen::new(0);
        let page_size = host_page_size()?;
        for r in runs {
            if r.len.is_zero() || r.gpa.get() % page_size != 0 || r.len.get() % page_size != 0 {
                bail!("guest page runs must be nonempty and host-page aligned");
            }
            sum = sum
                .plus(r.len)
                .ok_or_else(|| anyhow::anyhow!("sum of run lengths overflows"))?;
        }
        if sum != total {
            bail!("runs sum to {sum}, expected {total}");
        }
        // Limit each allocation's pinned host RAM.
        if total.is_zero() || total.get() > max_pin_bytes() {
            bail!(
                "arena length {total} over the pin limit {} MiB (LEA_MAX_PIN_MIB)",
                max_pin_bytes() >> 20
            );
        }

        let len =
            usize::try_from(total.get()).context("arena length exceeds host address space")?;
        // Reserve the address space in one piece.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("reserve arena");
        }
        let arena = Arena {
            base: base as *mut u8,
            len,
            lease: None,
        };

        let guard = mem.memory();
        let mut off = 0u64;
        for r in runs {
            // Map the run onto the guest RAM region and that region's file offset.
            let region = guard
                .find_region(GuestAddress(r.gpa.get()))
                .with_context(|| format!("GPA {:#x} in no region", r.gpa))?;
            let fo = region.file_offset().context(
                "guest RAM without a file -- is the VM running with --memory shared=on?",
            )?;
            let region_base = GuestAddr::new(region.start_addr().0);
            // A wrapping end must not pass the region bounds check.
            let run_end = r
                .gpa
                .end(r.len)
                .ok_or_else(|| anyhow::anyhow!("run {:#x}+{:#x} overflows", r.gpa, r.len))?;
            let region_end = region_base
                .end(GuestLen::new(region.len()))
                .ok_or_else(|| anyhow::anyhow!("region end overflows"))?;
            if run_end > region_end {
                bail!("run {:#x}+{:#x} exceeds the region", r.gpa, r.len);
            }
            // Check the MAP_FIXED destination locally, even though the
            // checked sum above already guarantees this bound.
            let end_in_arena = off.checked_add(r.len.get()).filter(|e| *e <= total.get());
            if end_in_arena.is_none() {
                bail!(
                    "run {:#x} no longer fits into the arena (offset {off:#x})",
                    r.len
                );
            }
            let in_region = r
                .gpa
                .offset_from(region_base)
                .ok_or_else(|| anyhow::anyhow!("GPA {:#x} before the region", r.gpa))?;
            let file_off = fo
                .start()
                .checked_add(in_region)
                .and_then(|offset| libc::off_t::try_from(offset).ok())
                .context("guest RAM file offset exceeds mmap range")?;
            let dst = unsafe { arena.base.add(off as usize) };
            let p = unsafe {
                libc::mmap(
                    dst as *mut libc::c_void,
                    r.len.get() as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    fo.file().as_raw_fd(),
                    file_off,
                )
            };
            if p == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("map run @arena+{off:#x}"));
            }
            off += r.len.get();
        }
        Ok(arena)
    }

    pub fn base(&self) -> *mut u8 {
        self.base
    }

    fn detach_charge(&mut self) -> u64 {
        self.lease.take().map_or(0, |mut lease| {
            let bytes = lease.bytes;
            lease.bytes = 0;
            bytes
        })
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.len) };
    }
}

/// The host's own RM client, used to describe guest pages and attach them
/// to the GPU VA. One per guest session, created lazily.
struct Backing {
    ctl: NvDevice,
    gpu_reg: NvDevice,
    root: u32,
    device: u32,
    uuid: sys::NvProcessorUuid,
    next_handle: u32,
}

impl Backing {
    fn new() -> Result<Backing> {
        // Client (an RM_ALLOC, NVOS64 block, of NV01_ROOT_CLIENT).
        let ctl = NvDevice::open_ctl().context("open ctl (backing client)")?;
        let mut p = sys::NVOS64_PARAMETERS::default();
        p.hClass = sys::NV01_ROOT_CLIENT;
        unsafe { ctl.ioctl_raw(sys::NV_ESC_RM_ALLOC, &mut p)? };
        nvrm_abi::check_status(sys::NV_ESC_RM_ALLOC, p.status as u32)?;
        let root = p.hObjectNew;

        let gpu = NvDevice::open_gpu(0).context("open gpu (backing client)")?;
        // Registered GPU FD: 0x27 only runs on a GPU node
        // (NV_ACTUAL_DEVICE_ONLY), and the FD needs REGISTER_FD, else 0x23.
        let gpu_reg = gpu
            .open_for_mapping(&ctl)
            .context("REGISTER_FD (backing client)")?;

        let mut b = Backing {
            ctl,
            gpu_reg,
            root,
            device: 0,
            uuid: Default::default(),
            next_handle: root.checked_add(1).context("backing RM handle overflow")?,
        };

        // Device + subdevice, to fetch the GPU UUID.
        let device = b.handle()?;
        let mut dp = sys::NV0080_ALLOC_PARAMETERS::default();
        dp.deviceId = 0;
        b.alloc(root, device, sys::NV01_DEVICE_0, Some(&mut dp))?;
        b.device = device;

        let subdevice = b.handle()?;
        let mut sp = sys::NV2080_ALLOC_PARAMETERS::default();
        sp.subDeviceId = 0;
        b.alloc(device, subdevice, sys::NV20_SUBDEVICE_0, Some(&mut sp))?;

        let mut gid = sys::NV2080_CTRL_GPU_GET_GID_INFO_PARAMS::default();
        gid.index = 0;
        gid.flags = 2; // FORMAT_BINARY|TYPE_SHA1 -> 16 bytes
        b.control(subdevice, sys::NV2080_CTRL_CMD_GPU_GET_GID_INFO, &mut gid)?;
        if gid.length != 16 {
            bail!("GID length {} instead of 16", gid.length);
        }
        b.uuid.uuid.copy_from_slice(&gid.data[..16]);

        Ok(b)
    }

    fn handle(&mut self) -> Result<u32> {
        let h = self.next_handle;
        self.next_handle = h.checked_add(1).context("backing RM handles exhausted")?;
        Ok(h)
    }

    fn alloc<P>(&self, parent: u32, handle: u32, class: u32, params: Option<&mut P>) -> Result<()> {
        let mut p = sys::NVOS64_PARAMETERS::default();
        p.hRoot = self.root;
        p.hObjectParent = parent;
        p.hObjectNew = handle;
        p.hClass = class;
        p.pAllocParms = params.map_or(0usize as sys::NvP64, |r| r as *mut P as usize as sys::NvP64);
        unsafe { self.ctl.ioctl_raw(sys::NV_ESC_RM_ALLOC, &mut p)? };
        nvrm_abi::check_status(sys::NV_ESC_RM_ALLOC, p.status as u32)?;
        Ok(())
    }

    fn control<P>(&self, object: u32, cmd: u32, params: &mut P) -> Result<()> {
        let mut p = sys::NVOS54_PARAMETERS::default();
        p.hClient = self.root;
        p.hObject = object;
        p.cmd = cmd;
        p.params = params as *mut P as *mut libc::c_void;
        p.paramsSize = std::mem::size_of::<P>() as u32;
        unsafe { self.ctl.ioctl_raw(sys::NV_ESC_RM_CONTROL, &mut p)? };
        nvrm_abi::check_status(sys::NV_ESC_RM_CONTROL, p.status as u32)?;
        Ok(())
    }

    /// Allocate the source object; its owner must be recorded before granting DUP.
    fn os_descriptor(&mut self, host_va: *mut u8, len: u64) -> Result<u32> {
        let osdesc = self.handle()?;
        let mut wfd = nvrm_abi::nvgpu::Nvos02WithFd::default();
        wfd.params.hRoot = self.root;
        wfd.params.hObjectParent = self.device;
        wfd.params.hObjectNew = osdesc;
        wfd.params.hClass = sys::NV01_MEMORY_SYSTEM_OS_DESCRIPTOR;
        wfd.params.flags = OSDESC_FLAGS;
        wfd.params.pMemory = host_va as usize as sys::NvP64;
        wfd.params.limit = len - 1;
        wfd.fd = -1;
        unsafe {
            self.gpu_reg
                .ioctl_raw(sys::NV_ESC_RM_ALLOC_MEMORY, &mut wfd)?
        };
        nvrm_abi::check_status(sys::NV_ESC_RM_ALLOC_MEMORY, wfd.params.status as u32)?;

        Ok(osdesc)
    }

    fn grant(&self, osdesc: u32) -> Result<()> {
        // Allow UVM duplication from this backend PID only.
        let (r, s) =
            unsafe { share::grant_dup_same_process(self.ctl.as_raw_fd(), self.root, osdesc) };
        if r != 0 || s != 0 {
            bail!("DUP grant for OS descriptor {osdesc:#x}: ret {r} status {s:#x}");
        }
        Ok(())
    }

    fn free(&self, osdesc: u32) -> Result<()> {
        let mut params = sys::NVOS00_PARAMETERS::default();
        params.hRoot = self.root;
        params.hObjectParent = self.device;
        params.hObjectOld = osdesc;
        unsafe { self.ctl.ioctl_raw(sys::NV_ESC_RM_FREE, &mut params)? };
        nvrm_abi::check_status(sys::NV_ESC_RM_FREE, params.status as u32)?;
        Ok(())
    }
}

/// The card this backend serves: its UUID and `TOTAL_RAM_SIZE` in bytes,
/// asked once at start-up through a backing client that closes again.
pub fn card() -> Result<([u8; 16], u64)> {
    let b = Backing::new()?;
    let mut fb = sys::NV2080_CTRL_FB_GET_INFO_V2_PARAMS::default();
    fb.fbInfoListSize = 1;
    fb.fbInfoList[0].index = nvrm_abi::mediate::FB_INFO_INDEX_TOTAL_RAM_SIZE;
    // The subdevice is the handle Backing::new allocated after the device.
    b.control(b.device + 1, nvrm_abi::mediate::CMD_FB_GET_INFO_V2, &mut fb)?;
    Ok((b.uuid.uuid, fb.fbInfoList[0].data as u64 * 1024))
}

/// Operations whose ordering determines the lifetime of a registered arena.
trait PoolDriver: Send + Sync {
    fn allocate(&self, arena: &Arena, len: GuestLen) -> Result<u32>;
    fn grant(&self, handle: u32) -> Result<()>;
    fn create_range(&self, fd: RawFd, addr: GuestAddr, len: GuestLen) -> Result<()>;
    fn map(&self, fd: RawFd, addr: GuestAddr, len: GuestLen, handle: u32) -> Result<()>;
    fn free_range(&self, fd: RawFd, addr: GuestAddr, len: GuestLen) -> Result<()>;
    fn free_object(&self, handle: u32) -> Result<()>;
}

struct RealPoolDriver {
    backing: Mutex<Backing>,
    free_range: fn(RawFd, GuestAddr, GuestLen) -> Result<()>,
    _private_client: PrivateClient,
}

impl PoolDriver for RealPoolDriver {
    fn allocate(&self, arena: &Arena, len: GuestLen) -> Result<u32> {
        self.backing
            .lock()
            .unwrap()
            .os_descriptor(arena.base(), len.get())
    }

    fn grant(&self, handle: u32) -> Result<()> {
        self.backing.lock().unwrap().grant(handle)
    }

    fn create_range(&self, fd: RawFd, addr: GuestAddr, len: GuestLen) -> Result<()> {
        let mut params = sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS {
            base: addr.get(),
            length: len.get(),
            ..Default::default()
        };
        uvm_call(
            fd,
            UVM_CREATE_EXTERNAL_RANGE,
            &mut params,
            CREATE_RANGE_STATUS_OFF,
        )
        .context("CREATE_EXTERNAL_RANGE")
    }

    fn map(&self, fd: RawFd, addr: GuestAddr, len: GuestLen, handle: u32) -> Result<()> {
        let backing = self.backing.lock().unwrap();
        let mut params: Box<sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS> =
            unsafe { Box::new(std::mem::zeroed()) };
        params.base = addr.get();
        params.length = len.get();
        params.perGpuAttributes[0].gpuUuid = backing.uuid;
        params.perGpuAttributes[0].gpuMappingType = sys::UvmGpuMappingTypeReadWriteAtomic as u32;
        params.perGpuAttributes[0].gpuCachingType = sys::UvmGpuCachingTypeDefault as u32;
        params.gpuAttributesCount = 1;
        params.rmCtrlFd = backing.ctl.as_raw_fd();
        params.hClient = backing.root;
        params.hMemory = handle;
        uvm_call(
            fd,
            UVM_MAP_EXTERNAL_ALLOCATION,
            params.as_mut(),
            MAP_EXTERNAL_STATUS_OFF,
        )
        .context("MAP_EXTERNAL_ALLOCATION")
    }

    fn free_range(&self, fd: RawFd, addr: GuestAddr, len: GuestLen) -> Result<()> {
        (self.free_range)(fd, addr, len)
    }

    fn free_object(&self, handle: u32) -> Result<()> {
        self.backing.lock().unwrap().free(handle)
    }
}

fn free_params<A: nvrm_sys::RmAbi>(addr: GuestAddr, len: GuestLen) -> A::UvmFreeParams {
    let mut params: A::UvmFreeParams = unsafe { std::mem::zeroed() };
    // SAFETY: RmAbi supplies offsets for this exact generated parameter type.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            &mut params as *mut A::UvmFreeParams as *mut u8,
            std::mem::size_of::<A::UvmFreeParams>(),
        )
    };
    let offset = A::UVM_FREE_PARAMS_OFF_base;
    bytes[offset..offset + 8].copy_from_slice(&addr.get().to_le_bytes());
    let offset = A::UVM_FREE_PARAMS_OFF_length;
    // R580 carries a length; newer layouts use an out-of-bounds sentinel.
    if offset + 8 <= bytes.len() {
        bytes[offset..offset + 8].copy_from_slice(&len.get().to_le_bytes());
    }
    params
}

fn free_range<A: nvrm_sys::RmAbi>(fd: RawFd, addr: GuestAddr, len: GuestLen) -> Result<()> {
    uvm_call(
        fd,
        nvrm_abi::xlate::uvm::FREE as u64,
        &mut free_params::<A>(addr, len),
        A::UVM_FREE_PARAMS_OFF_rmStatus,
    )
    .context("UVM_FREE")
}

/// A transaction owns each resource as soon as its allocating call succeeds.
struct PoolMap {
    token: u64,
    addr: GuestAddr,
    len: GuestLen,
    arena: Option<Arena>,
    uvm_fd: Option<OwnedFd>,
    driver: Arc<dyn PoolDriver>,
    osdesc: Option<u32>,
    range_live: bool,
    active: bool,
    quarantined_charge: u64,
}

impl PoolMap {
    fn cleanup(&mut self) -> Result<()> {
        self.active = false;
        // UVM_FREE releases its duplicate reference before the source RM_FREE.
        if self.range_live {
            self.driver.free_range(
                self.uvm_fd.as_ref().unwrap().as_raw_fd(),
                self.addr,
                self.len,
            )?;
            self.range_live = false;
        }
        if let Some(handle) = self.osdesc {
            self.driver.free_object(handle)?;
            self.osdesc = None;
        }
        Ok(())
    }
}

impl Drop for PoolMap {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("vhost-user-nvrm: final pool cleanup failed at {:#x}: {error:#};                 retaining arena, UVM fd and backing client until process exit", self.addr);
            // Exceptional fallback: releasing these owners would invalidate live DMA.
            std::mem::forget((self.arena.take(), self.uvm_fd.take(), self.driver.clone()));
        }
    }
}

/// Guest-page registrations for one session; the admission budget is VM-wide.
pub struct PoolState {
    driver: Option<Arc<dyn PoolDriver>>,
    budget: Arc<PinBudget>,
    pools: Vec<PoolMap>,
    osdesc_arenas: HashMap<(u32, u32), Arena>,
}

impl Default for PoolState {
    fn default() -> Self {
        Self::with_budget(PinBudget::new(1024 << 20).unwrap())
    }
}

impl PoolState {
    pub fn with_budget(budget: Arc<PinBudget>) -> Self {
        Self {
            driver: None,
            budget,
            pools: Vec::new(),
            osdesc_arenas: HashMap::new(),
        }
    }

    fn driver<A: nvrm_sys::RmAbi>(&mut self) -> Result<Arc<dyn PoolDriver>> {
        if self.driver.is_none() {
            let backing = Backing::new().context("create backing client")?;
            let private_client = self.budget.private_client(backing.root);
            self.driver = Some(Arc::new(RealPoolDriver {
                backing: Mutex::new(backing),
                free_range: free_range::<A>,
                _private_client: private_client,
            }));
        }
        Ok(self.driver.as_ref().unwrap().clone())
    }

    /// Reject guest DUP and UVM imports from these backend-only RM clients.
    /// Their source objects must have no references beyond our tracked mapping.
    pub fn is_private_client(&self, root: u32) -> bool {
        self.budget.private_clients.lock().unwrap().contains(&root)
    }

    fn arena(&self, mem: &Mem, runs: &[GpaRun], total: GuestLen) -> Result<Arena> {
        self.budget.retry_cleanup();
        let lease = self.budget.reserve(registration_bytes(total.get())?)?;
        let mut arena = Arena::build(mem, runs, total)?;
        arena.lease = Some(lease);
        Ok(arena)
    }

    /// The request plan owns this reservation until RM allocation succeeds.
    pub fn arena_for_osdesc(
        &self,
        mem: &Mem,
        runs: &[GpaRun],
        total: GuestLen,
    ) -> Result<(Arena, u64)> {
        let arena = self.arena(mem, runs, total)?;
        let va = arena.base() as u64;
        Ok((arena, va))
    }

    pub fn keep_osdesc_arena(&mut self, root: u32, hmemory: u32, arena: Arena) {
        if let Some(previous) = self.osdesc_arenas.insert((root, hmemory), arena) {
            self.budget.retain(previous);
        }
    }

    /// Source handles may have surviving DUP or export references.
    pub fn drop_osdesc_arena(&mut self, root: u32, hmemory: u32) {
        if let Some(arena) = self.osdesc_arenas.remove(&(root, hmemory)) {
            self.budget.retain(arena);
        }
    }

    pub fn drop_osdesc_client(&mut self, root: u32) {
        let keys: Vec<_> = self
            .osdesc_arenas
            .keys()
            .copied()
            .filter(|(client, _)| *client == root)
            .collect();
        for (_, handle) in keys {
            self.drop_osdesc_arena(root, handle);
        }
    }

    /// Signal a semaphore only in the request's UVM address space.
    pub fn write_guest_u32_for(&self, token: u64, va: GuestAddr, val: u32) -> bool {
        let Some(end) = va.end(GuestLen::new(4)) else {
            return false;
        };
        for pool in &self.pools {
            if !pool.active || pool.token != token {
                continue;
            }
            let Some(pool_end) = pool.addr.end(pool.len) else {
                continue;
            };
            if va >= pool.addr && end <= pool_end {
                let off = va.offset_from(pool.addr).unwrap();
                if off % std::mem::align_of::<u32>() as u64 != 0 {
                    return false;
                }
                // SAFETY: mmap aligns the base; off is aligned and off+4 <= len.
                unsafe {
                    std::ptr::write_volatile(
                        pool.arena.as_ref().unwrap().base().add(off as usize) as *mut u32,
                        val,
                    );
                }
                return true;
            }
        }
        false
    }

    /// Reject overlapping registrations before touching UVM: its map operation
    /// unmaps an existing mapping before validating the replacement.
    pub fn back_pool_for<A: nvrm_sys::RmAbi>(
        &mut self,
        mem: &Mem,
        token: u64,
        uvm_fd: RawFd,
        addr: GuestAddr,
        len: GuestLen,
        runs: &[GpaRun],
    ) -> Result<()> {
        let end = addr.end(len).context("pool address range overflows")?;
        if len.is_zero() || addr.get() % 4096 != 0 || len.get() % 4096 != 0 {
            bail!("pool address and length must describe whole 4 KiB pages");
        }
        self.retry_cleanup();
        if self.pools.iter().any(|pool| {
            pool.token == token && addr < pool.addr.end(pool.len).unwrap() && pool.addr < end
        }) {
            bail!("pool overlaps a registered or cleanup-pending range");
        }
        let arena = self.arena(mem, runs, len).context("pool arena")?;
        // Keep the same open file description alive during cleanup retries.
        let fd = unsafe { libc::fcntl(uvm_fd, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("duplicate pool UVM fd");
        }
        // SAFETY: fcntl returned a new descriptor owned by this call.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let driver = self.driver::<A>()?;
        let mut pool = PoolMap {
            token,
            addr,
            len,
            arena: Some(arena),
            uvm_fd: Some(fd),
            driver,
            osdesc: None,
            range_live: false,
            active: false,
            quarantined_charge: 0,
        };
        let setup = (|| {
            let handle = pool.driver.allocate(pool.arena.as_ref().unwrap(), len)?;
            pool.osdesc = Some(handle);
            pool.driver.grant(handle)?;
            let fd = pool.uvm_fd.as_ref().unwrap().as_raw_fd();
            pool.driver.create_range(fd, addr, len)?;
            pool.range_live = true;
            pool.driver.map(fd, addr, len, handle)?;
            pool.active = true;
            Ok(())
        })();
        match setup {
            Ok(()) => {
                self.pools.push(pool);
                Ok(())
            }
            Err(error) => {
                if let Err(cleanup) = pool.cleanup() {
                    eprintln!("vhost-user-nvrm: pool rollback retained at {addr:#x}: {cleanup:#}");
                    self.pools.push(pool);
                }
                Err(error)
            }
        }
    }

    fn retry_cleanup(&mut self) {
        self.budget.retry_cleanup();
        self.pools
            .retain_mut(|pool| pool.active || pool.cleanup().is_err());
    }

    /// Call only after the guest's UVM_FREE returned both transport success and NV_OK.
    pub fn release_uvm_range(&mut self, token: u64, addr: GuestAddr) -> Result<()> {
        for pool in &mut self.pools {
            if pool.token == token && pool.addr == addr {
                pool.range_live = false;
                pool.active = false;
            }
        }
        self.cleanup_matching(token, Some(addr))
    }

    /// Call before closing the mirrored UVM token. Failed owners stay retryable.
    pub fn close_uvm(&mut self, token: u64) -> Result<()> {
        self.cleanup_matching(token, None)
    }

    fn cleanup_matching(&mut self, token: u64, addr: Option<GuestAddr>) -> Result<()> {
        let mut failure = None;
        self.pools.retain_mut(|pool| {
            if pool.token != token || addr.is_some_and(|addr| pool.addr != addr) {
                return true;
            }
            match pool.cleanup() {
                Ok(()) => false,
                Err(error) => {
                    failure = Some(error);
                    true
                }
            }
        });
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for PoolState {
    fn drop(&mut self) {
        for mut pool in std::mem::take(&mut self.pools) {
            if let Err(error) = pool.cleanup() {
                eprintln!(
                    "vhost-user-nvrm: quarantining pool {:#x} after session close: {error:#}",
                    pool.addr
                );
                self.budget.quarantine(pool);
            }
        }
        for (_, arena) in self.osdesc_arenas.drain() {
            self.budget.retain(arena);
        }
    }
}

/// Read the declared rmStatus field. CREATE_EXTERNAL_RANGE has tail padding,
/// so the final word of the struct is not its status.
const CREATE_RANGE_STATUS_OFF: usize =
    std::mem::offset_of!(sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS, rmStatus);
const MAP_EXTERNAL_STATUS_OFF: usize =
    std::mem::offset_of!(sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS, rmStatus);
const _: () = {
    assert!(
        CREATE_RANGE_STATUS_OFF + 4 <= std::mem::size_of::<sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS>()
    );
    assert!(
        MAP_EXTERNAL_STATUS_OFF + 4
            <= std::mem::size_of::<sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS>()
    );
    // The numbers xlate.rs cites for these two structs (uvm_ioctl.h).
    assert!(CREATE_RANGE_STATUS_OFF == 16);
    assert!(MAP_EXTERNAL_STATUS_OFF == 9260);
};

/// Run a UVM ioctl and check rmStatus at the struct's declared field offset.
fn uvm_call<T>(fd: i32, cmd: u64, p: &mut T, status_off: usize) -> Result<()> {
    debug_assert!(status_off + 4 <= std::mem::size_of::<T>());
    let r = unsafe { libc::ioctl(fd, cmd as libc::Ioctl, p as *mut T as *mut libc::c_void) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("UVM ioctl");
    }
    let status = unsafe {
        let base = p as *const T as *const u8;
        u32::from_le_bytes(
            std::slice::from_raw_parts(base.add(status_off), 4)
                .try_into()
                .unwrap(),
        )
    };
    if status != sys::NV_OK {
        bail!("rmStatus {status:#x}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::fd::FromRawFd;
    use vm_memory::{FileOffset, GuestAddress};

    const REGION_BASE: u64 = 0x1000_0000;
    const REGION_LEN: usize = 8 << 20;

    /// Guest RAM the way `--memory shared=on` delivers it: one region over
    /// a file (memfd). Without a file offset `Arena::build` bails out
    /// earlier, and it is precisely the arithmetic behind that which is
    /// under test.
    fn guest_mem_at(base: u64, len: usize) -> Mem {
        let fd = unsafe { libc::memfd_create(c"leandro-arena-test".as_ptr(), 0) };
        assert!(fd >= 0, "memfd_create: {}", std::io::Error::last_os_error());
        let f = unsafe { File::from_raw_fd(fd) };
        f.set_len(len as u64).unwrap();
        let m = vm_memory::GuestMemoryMmap::<()>::from_ranges_with_files([(
            GuestAddress(base),
            len,
            Some(FileOffset::new(f, 0)),
        )])
        .unwrap();
        vm_memory::GuestMemoryAtomic::new(m)
    }

    fn guest_mem() -> Mem {
        guest_mem_at(REGION_BASE, REGION_LEN)
    }

    const MEMFD_NAME: &str = "leandro-arena-test";

    /// Test-memfd mappings only, excluding unrelated anonymous heap growth.
    fn maps() -> Vec<(u64, u64)> {
        std::fs::read_to_string("/proc/self/maps")
            .unwrap()
            .lines()
            .filter(|l| l.contains(MEMFD_NAME))
            .filter_map(|l| {
                let (a, b) = l.split_whitespace().next()?.split_once('-')?;
                Some((
                    u64::from_str_radix(a, 16).ok()?,
                    u64::from_str_radix(b, 16).ok()?,
                ))
            })
            .collect()
    }

    /// Serialize mmap tests because /proc/self/maps is process-wide.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Construct raw test ranges, including deliberately overflowing values.
    fn run(gpa: u64, len: u64) -> GpaRun {
        GpaRun {
            gpa: GuestAddr::new(gpa),
            len: GuestLen::new(len),
        }
    }

    fn glen(v: u64) -> GuestLen {
        GuestLen::new(v)
    }

    /// How much address space did `f` leave behind that was not there
    /// before and did not disappear again on Drop? The control question for
    /// every path that uses MAP_FIXED.
    fn leaked_bytes(f: impl FnOnce()) -> u64 {
        let before = maps();
        f();
        maps()
            .iter()
            .filter(|(s, e)| !before.iter().any(|(bs, be)| bs == s && be == e) && e > s)
            .map(|(s, e)| e - s)
            .sum()
    }

    // ---- the good case ----------------------------------------------------

    #[test]
    fn contiguous_and_fragmented_runs_build() {
        let _x = exclusive();
        let mem = guest_mem();
        let total = 3 * 0x1000;
        for runs in [
            vec![run(REGION_BASE, total)],
            vec![
                run(REGION_BASE + 0x5000, 0x1000),
                run(REGION_BASE + 0x2000, 0x2000),
            ],
            vec![
                run(REGION_BASE + 0x7000, 0x1000),
                run(REGION_BASE, 0x1000),
                run(REGION_BASE + 0x4000, 0x1000),
            ],
        ] {
            let a = Arena::build(&mem, &runs, glen(total)).expect("valid runs must carry");
            assert!(!a.base().is_null());
        }
    }

    /// Aliases follow run order, which may differ from GPA order.
    #[test]
    fn arena_aliases_the_named_guest_pages() {
        let _x = exclusive();
        let mem = guest_mem();
        let write_guest = |gpa: u64, val: u8| {
            use vm_memory::{Bytes, GuestAddressSpace};
            mem.memory()
                .write_slice(&[val; 0x1000], GuestAddress(gpa))
                .unwrap();
        };
        write_guest(REGION_BASE + 0x3000, 0xaa);
        write_guest(REGION_BASE + 0x1000, 0xbb);

        let runs = [
            run(REGION_BASE + 0x3000, 0x1000),
            run(REGION_BASE + 0x1000, 0x1000),
        ];
        let a = Arena::build(&mem, &runs, glen(0x2000)).unwrap();
        // SAFETY: the arena is 0x2000 big and lives until the test ends.
        unsafe {
            assert_eq!(*a.base(), 0xaa, "first run sits at arena+0");
            assert_eq!(*a.base().add(0x1000), 0xbb, "second run at arena+0x1000");
        }
    }

    // ---- the refusals -----------------------------------------------------

    #[test]
    fn rejects_length_zero_and_over_the_pin_limit() {
        let _x = exclusive();
        assert!(
            Arena::build(&guest_mem(), &[], glen(0)).is_err(),
            "length 0"
        );

        // Use sparse RAM larger than the pin cap to isolate that check
        // from region bounds validation.
        let mem = guest_mem_at(REGION_BASE, 512 << 20);
        let huge = (256 << 20) + 0x1000; // default pin limit + 1 page
        let runs = [run(REGION_BASE, huge)];
        assert!(
            Arena::build(&mem, &runs, glen(huge)).is_err(),
            "over the pin limit"
        );
        // The pin cap itself is inclusive.
        let ok_len = 256 << 20;
        let runs = [run(REGION_BASE, ok_len)];
        assert!(
            Arena::build(&mem, &runs, glen(ok_len)).is_ok(),
            "exactly at the limit must carry"
        );
    }

    #[test]
    fn rejects_sum_mismatch_and_runs_outside_the_region() {
        let _x = exclusive();
        let mem = guest_mem();
        let runs = [run(REGION_BASE, 0x1000)];
        assert!(
            Arena::build(&mem, &runs, glen(0x2000)).is_err(),
            "sum != total"
        );

        // Run starts inside the region but extends past its end.
        let runs = [run(REGION_BASE + REGION_LEN as u64 - 0x1000, 0x2000)];
        assert!(
            Arena::build(&mem, &runs, glen(0x2000)).is_err(),
            "run past the region end"
        );

        // Run outside every region.
        let runs = [run(REGION_BASE + (64 << 20), 0x1000)];
        assert!(
            Arena::build(&mem, &runs, glen(0x1000)).is_err(),
            "GPA in no region"
        );
    }

    /// A wrapped run sum must not let MAP_FIXED exceed its reservation.
    /// Runs of 2 MiB and (0x1000 - 2 MiB) mod 2^64 would falsely fit 4 KiB.
    /// Keep release coverage: debug overflow panics can hide a missing check.
    #[test]
    fn rejects_wrapping_sum_without_leaking_address_space() {
        let _x = exclusive();
        let mem = guest_mem();
        const TOTAL: u64 = 0x1000;
        const BIG: u64 = 2 << 20;
        let len2 = TOTAL.wrapping_sub(BIG);
        let gpa2 = REGION_BASE + BIG;
        assert_eq!(BIG.wrapping_add(len2), TOTAL, "setup: sum wraps onto total");
        assert!(
            gpa2.wrapping_add(len2) < REGION_BASE + REGION_LEN as u64,
            "setup: the region check wraps too"
        );

        let runs = [run(REGION_BASE, BIG), run(gpa2, len2)];
        let leaked = leaked_bytes(|| {
            assert!(
                Arena::build(&mem, &runs, glen(TOTAL)).is_err(),
                "wrapping sum must be caught"
            );
        });
        assert_eq!(leaked, 0, "{leaked} bytes of address space left behind");
    }

    /// The same arithmetic from the other side: a single run whose
    /// `gpa + len` wraps. Unchecked, `gpa+len > region_end` is then false
    /// and the run counts as "inside the region".
    #[test]
    fn rejects_wrapping_region_check() {
        let _x = exclusive();
        let mem = guest_mem();
        let len = u64::MAX - REGION_BASE + 1; // gpa + len == 0
        assert_eq!(REGION_BASE.wrapping_add(len), 0);
        let runs = [run(REGION_BASE, len)];
        let leaked = leaked_bytes(|| {
            assert!(Arena::build(&mem, &runs, glen(len)).is_err());
        });
        assert_eq!(leaked, 0);
    }

    /// `limit + 1` in session.rs: a guest that sends `NVOS02.limit =
    /// u64::MAX` makes the length wrap to 0. The zero-length check is the
    /// last barrier in front of that and must hold.
    #[test]
    fn total_zero_is_rejected() {
        let _x = exclusive();
        let mem = guest_mem();
        assert!(Arena::build(&mem, &[run(REGION_BASE, 0)], glen(0)).is_err());
        assert!(Arena::build(&mem, &[], glen(0)).is_err());
    }

    // ---- write_guest_u32 --------------------------------------------------

    #[derive(Default)]
    struct FakeDriver {
        calls: Mutex<Vec<&'static str>>,
        failures: Mutex<Vec<&'static str>>,
        _private_client: Option<PrivateClient>,
    }

    impl FakeDriver {
        fn call(&self, name: &'static str) -> Result<()> {
            self.calls.lock().unwrap().push(name);
            let mut failures = self.failures.lock().unwrap();
            if let Some(index) = failures.iter().position(|failure| *failure == name) {
                failures.remove(index);
                bail!("injected {name} failure");
            }
            Ok(())
        }

        fn fail(&self, names: &[&'static str]) {
            self.failures.lock().unwrap().extend_from_slice(names);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PoolDriver for FakeDriver {
        fn allocate(&self, _: &Arena, _: GuestLen) -> Result<u32> {
            self.call("allocate")?;
            Ok(7)
        }
        fn grant(&self, _: u32) -> Result<()> {
            self.call("grant")
        }
        fn create_range(&self, _: RawFd, _: GuestAddr, _: GuestLen) -> Result<()> {
            self.call("create")
        }
        fn map(&self, _: RawFd, _: GuestAddr, _: GuestLen, _: u32) -> Result<()> {
            self.call("map")
        }
        fn free_range(&self, _: RawFd, _: GuestAddr, _: GuestLen) -> Result<()> {
            self.call("free_range")
        }
        fn free_object(&self, _: u32) -> Result<()> {
            self.call("free_object")
        }
    }

    /// A pool backed by test RAM, without an RM client or GPU mapping.
    fn pool_state_with(mem: &Mem, addr: u64, gpa: u64, len: u64) -> PoolState {
        let arena = Arena::build(mem, &[run(gpa, len)], glen(len)).unwrap();
        let mut state = PoolState::default();
        state.pools.push(PoolMap {
            token: 1,
            addr: GuestAddr::new(addr),
            len: GuestLen::new(len),
            arena: Some(arena),
            uvm_fd: None,
            driver: Arc::new(FakeDriver::default()),
            osdesc: None,
            range_live: false,
            active: true,
            quarantined_charge: 0,
        });
        state
    }

    fn fake_pool(budget: Arc<PinBudget>) -> (PoolState, Arc<FakeDriver>, File) {
        let driver = Arc::new(FakeDriver::default());
        let mut state = PoolState::with_budget(budget);
        state.driver = Some(driver.clone());
        (state, driver, File::open("/dev/null").unwrap())
    }

    fn register(
        state: &mut PoolState,
        mem: &Mem,
        fd: &File,
        token: u64,
        va: u64,
        gpa: u64,
    ) -> Result<()> {
        state.back_pool_for::<sys::DefaultAbi>(
            mem,
            token,
            fd.as_raw_fd(),
            GuestAddr::new(va),
            glen(0x1000),
            &[run(gpa, 0x1000)],
        )
    }

    #[test]
    fn setup_failure_releases_only_resources_it_acquired() {
        let _x = exclusive();
        let mem = guest_mem();
        for (failed, expected) in [
            ("allocate", vec!["allocate"]),
            ("grant", vec!["allocate", "grant", "free_object"]),
            ("create", vec!["allocate", "grant", "create", "free_object"]),
            (
                "map",
                vec![
                    "allocate",
                    "grant",
                    "create",
                    "map",
                    "free_range",
                    "free_object",
                ],
            ),
        ] {
            let budget = PinBudget::new(0x1000).unwrap();
            let (mut state, driver, fd) = fake_pool(budget.clone());
            driver.fail(&[failed]);
            let leaked = leaked_bytes(|| {
                assert!(register(&mut state, &mem, &fd, 1, 0x2000, REGION_BASE).is_err());
            });
            assert_eq!(driver.calls(), expected, "{failed}");
            assert_eq!(leaked, 0, "{failed}");
            assert_eq!(budget.used_bytes(), 0, "{failed}");
            assert!(state.pools.is_empty(), "{failed}");
        }
    }

    #[test]
    fn failed_uvm_rollback_keeps_the_arena_fd_and_charge_until_retry() {
        let _x = exclusive();
        let mem = guest_mem();
        let budget = PinBudget::new(0x1000).unwrap();
        let (mut state, driver, fd) = fake_pool(budget.clone());
        driver.fail(&["map", "free_range"]);
        assert!(register(&mut state, &mem, &fd, 1, 0x2000, REGION_BASE).is_err());
        let held_fd = state.pools[0].uvm_fd.as_ref().unwrap().as_raw_fd();
        drop(fd);
        assert!(unsafe { libc::fcntl(held_fd, libc::F_GETFD) } >= 0);
        assert_eq!(budget.used_bytes(), 0x1000);
        assert!(state.pools[0].range_live);
        assert!(!state.write_guest_u32_for(1, GuestAddr::new(0x2000), 5));
        assert!(!driver.calls().contains(&"free_object"));
        state.retry_cleanup();
        assert_eq!(budget.used_bytes(), 0);
        assert!(state.pools.is_empty());
        assert_eq!(&driver.calls()[5..], &["free_range", "free_object"]);
    }

    #[test]
    fn failed_rm_free_does_not_repeat_successful_uvm_free() {
        let _x = exclusive();
        let mem = guest_mem();
        let budget = PinBudget::new(0x1000).unwrap();
        let (mut state, driver, fd) = fake_pool(budget.clone());
        register(&mut state, &mem, &fd, 1, 0x2000, REGION_BASE).unwrap();
        driver.fail(&["free_object"]);
        assert!(state.close_uvm(1).is_err());
        assert_eq!(budget.used_bytes(), 0x1000);
        assert!(!state.pools[0].range_live);
        state.close_uvm(1).unwrap();
        assert_eq!(budget.used_bytes(), 0);
        assert_eq!(
            &driver.calls()[4..],
            &["free_range", "free_object", "free_object"]
        );
    }

    #[test]
    fn session_cleanup_failure_moves_to_the_shared_retryable_quarantine() {
        let _x = exclusive();
        let mem = guest_mem();
        let budget = PinBudget::new(0x1000).unwrap();
        let (mut state, driver, fd) = fake_pool(budget.clone());
        register(&mut state, &mem, &fd, 1, 0x2000, REGION_BASE).unwrap();
        driver.fail(&["free_range", "free_range"]);
        drop(state);
        assert_eq!(budget.used_bytes(), 0x1000);
        assert_eq!(budget.quarantined_bytes(), 0x1000);
        budget.retry_cleanup();
        assert_eq!(budget.quarantined_bytes(), 0x1000);
        assert!(budget.reserve(0x1000).is_err());
        budget.retry_cleanup();
        assert_eq!(budget.used_bytes(), 0);
        assert_eq!(budget.quarantined_bytes(), 0);
        assert_eq!(
            &driver.calls()[4..],
            &["free_range", "free_range", "free_range", "free_object"]
        );
    }

    #[test]
    fn private_client_guard_survives_session_close_and_failed_cleanup() {
        let _x = exclusive();
        let mem = guest_mem();
        let budget = PinBudget::new(0x1000).unwrap();
        let observer = PoolState::with_budget(budget.clone());
        let driver = Arc::new(FakeDriver {
            _private_client: Some(budget.private_client(42)),
            ..Default::default()
        });
        let mut state = PoolState::with_budget(budget.clone());
        state.driver = Some(driver.clone());
        let fd = File::open("/dev/null").unwrap();
        register(&mut state, &mem, &fd, 1, 0x2000, REGION_BASE).unwrap();
        driver.fail(&["free_range", "free_range"]);
        drop(driver);
        assert!(observer.is_private_client(42));
        assert!(!observer.is_private_client(43));
        drop(state);
        assert!(observer.is_private_client(42));
        budget.retry_cleanup();
        assert!(observer.is_private_client(42));
        budget.retry_cleanup();
        assert!(!observer.is_private_client(42));
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn registration_charge_rounds_to_host_pages_without_overflow() {
        let page = host_page_size().unwrap();
        for (bytes, expected) in [
            (1, page),
            (page - 1, page),
            (page, page),
            (page + 1, page * 2),
        ] {
            assert_eq!(registration_bytes(bytes).unwrap(), expected);
        }
        assert!(registration_bytes(0).is_err());
        assert!(registration_bytes(u64::MAX).is_err());
        let largest = u64::MAX - u64::MAX % page;
        assert_eq!(registration_bytes(largest).unwrap(), largest);
    }

    #[test]
    fn overlapping_pools_are_rejected_before_driver_calls() {
        let _x = exclusive();
        let mem = guest_mem();
        let budget = PinBudget::new(0x3000).unwrap();
        let (mut state, driver, fd) = fake_pool(budget.clone());
        state
            .back_pool_for::<sys::DefaultAbi>(
                &mem,
                1,
                fd.as_raw_fd(),
                GuestAddr::new(0x2000),
                glen(0x2000),
                &[run(REGION_BASE, 0x2000)],
            )
            .unwrap();
        let calls = driver.calls();
        for va in [0x2000, 0x3000] {
            assert!(register(&mut state, &mem, &fd, 1, va, REGION_BASE).is_err());
            assert_eq!(driver.calls(), calls);
            assert_eq!(budget.used_bytes(), 0x2000);
        }
        assert!(state.write_guest_u32_for(1, GuestAddr::new(0x2000), 7));
    }

    #[test]
    fn equal_vas_on_distinct_uvm_tokens_keep_independent_owners() {
        let _x = exclusive();
        let mem = guest_mem();
        let budget = PinBudget::new(0x2000).unwrap();
        let (mut state, driver, fd) = fake_pool(budget.clone());
        register(&mut state, &mem, &fd, 1, 0x2000, REGION_BASE).unwrap();
        register(&mut state, &mem, &fd, 2, 0x2000, REGION_BASE + 0x1000).unwrap();
        assert!(state.write_guest_u32_for(1, GuestAddr::new(0x2000), 11));
        assert!(state.write_guest_u32_for(2, GuestAddr::new(0x2000), 22));
        assert!(!state.write_guest_u32_for(3, GuestAddr::new(0x2000), 33));
        use vm_memory::{Bytes, GuestAddressSpace};
        assert_eq!(
            mem.memory()
                .read_obj::<u32>(GuestAddress(REGION_BASE))
                .unwrap(),
            11
        );
        assert_eq!(
            mem.memory()
                .read_obj::<u32>(GuestAddress(REGION_BASE + 0x1000))
                .unwrap(),
            22
        );
        state.release_uvm_range(1, GuestAddr::new(0x2000)).unwrap();
        assert_eq!(budget.used_bytes(), 0x1000);
        assert_eq!(&driver.calls()[8..], &["free_object"]);
        assert!(!state.write_guest_u32_for(1, GuestAddr::new(0x2000), 44));
        assert!(state.write_guest_u32_for(2, GuestAddr::new(0x2000), 55));
    }

    #[test]
    fn forwarded_source_free_retains_its_shared_budget_reservation() {
        let _x = exclusive();
        let mem = guest_mem();
        let budget = PinBudget::new(0x1000).unwrap();
        let mut first = PoolState::with_budget(budget.clone());
        let second = PoolState::with_budget(budget.clone());
        let (arena, _) = first
            .arena_for_osdesc(&mem, &[run(REGION_BASE, 0x1000)], glen(0x1000))
            .unwrap();
        first.keep_osdesc_arena(1, 10, arena);
        first.drop_osdesc_arena(1, 10);
        drop(first);
        assert_eq!(budget.used_bytes(), 0x1000);
        assert_eq!(budget.retained_bytes(), 0x1000);
        assert!(second
            .arena_for_osdesc(&mem, &[run(REGION_BASE, 0x1000)], glen(0x1000))
            .is_err());
    }

    #[test]
    fn arena_build_failure_refunds_the_shared_reservation() {
        let _x = exclusive();
        let mem = guest_mem();
        let budget = PinBudget::new(0x1000).unwrap();
        let state = PoolState::with_budget(budget.clone());
        for (gpa, len) in [
            (REGION_BASE + 1, 0x1000),
            (REGION_BASE, 1),
            (REGION_BASE, 0),
        ] {
            assert!(state
                .arena_for_osdesc(&mem, &[run(gpa, len)], glen(len))
                .is_err());
            assert_eq!(budget.used_bytes(), 0);
        }
    }

    #[test]
    fn shared_budget_reservations_are_atomic_and_checked() {
        let budget = PinBudget::new(0x1000).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let budget = budget.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let lease = budget.reserve(0x1000);
                    barrier.wait();
                    lease.is_ok()
                })
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| usize::from(worker.join().unwrap()))
                .sum::<usize>(),
            1
        );
        assert_eq!(budget.used_bytes(), 0);
        let largest = PinBudget::new(u64::MAX).unwrap();
        let _lease = largest.reserve(u64::MAX).unwrap();
        assert!(largest.reserve(1).is_err());
        assert!(largest.reserve(0).is_err());
        assert_eq!(largest.used_bytes(), u64::MAX);
    }

    #[test]
    fn uvm_free_uses_the_selected_driver_layout() {
        fn check<A: nvrm_sys::RmAbi>() {
            let params = free_params::<A>(GuestAddr::new(0x1000), glen(0x2000));
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &params as *const A::UvmFreeParams as *const u8,
                    std::mem::size_of::<A::UvmFreeParams>(),
                )
            };
            assert_eq!(bytes.len(), A::UVM_FREE_PARAMS_SIZE as usize);
            assert_eq!(
                &bytes[A::UVM_FREE_PARAMS_OFF_base..][..8],
                &0x1000u64.to_le_bytes()
            );
            assert_eq!(&bytes[A::UVM_FREE_PARAMS_OFF_rmStatus..][..4], &[0; 4]);
            if A::UVM_FREE_PARAMS_OFF_length + 8 <= bytes.len() {
                assert_eq!(
                    &bytes[A::UVM_FREE_PARAMS_OFF_length..][..8],
                    &0x2000u64.to_le_bytes()
                );
            }
        }
        #[cfg(feature = "v580")]
        check::<nvrm_sys::V580>();
        #[cfg(feature = "v595")]
        check::<nvrm_sys::V595>();
        #[cfg(feature = "v610")]
        check::<nvrm_sys::V610>();
        #[cfg(feature = "v615")]
        check::<nvrm_sys::V615>();
    }

    #[test]
    fn write_guest_u32_hits_the_pool_and_stops_at_its_edges() {
        let _x = exclusive();
        let mem = guest_mem();
        let (addr, len) = (0x2000_0000u64, 0x2000u64);
        let ps = pool_state_with(&mem, addr, REGION_BASE, len);

        assert!(
            ps.write_guest_u32_for(1, GuestAddr::new(addr), 0xdead_beef),
            "start of the pool"
        );
        assert!(
            ps.write_guest_u32_for(1, GuestAddr::new(addr + len - 4), 1),
            "last complete word"
        );
        // SAFETY: the arena lives in the PoolState and is len big.
        unsafe {
            assert_eq!(
                *(ps.pools[0].arena.as_ref().unwrap().base() as *const u32),
                0xdead_beef
            );
        }

        assert!(
            !ps.write_guest_u32_for(1, GuestAddr::new(addr - 4), 1),
            "before the pool"
        );
        assert!(
            !ps.write_guest_u32_for(1, GuestAddr::new(addr + len - 3), 1),
            "extends past the end"
        );
        assert!(
            !ps.write_guest_u32_for(1, GuestAddr::new(addr + len), 1),
            "behind the pool"
        );
    }

    #[test]
    fn write_guest_u32_rejects_unaligned_offsets() {
        let _x = exclusive();
        let mem = guest_mem();
        let addr = 0x2000_0000;
        let ps = pool_state_with(&mem, addr, REGION_BASE, 0x2000);
        for offset in [1, 2, 3, 5, 0x1ffb] {
            assert!(!ps.write_guest_u32_for(1, GuestAddr::new(addr + offset), 0xdead_beef));
        }
        // SAFETY: the pool owns an initialized, readable mapping of 0x2000 bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts(ps.pools[0].arena.as_ref().unwrap().base(), 0x2000)
        };
        assert!(bytes.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn osdesc_arenas_are_scoped_to_the_rm_client() {
        let _x = exclusive();
        let mem = guest_mem();
        let arena = || Arena::build(&mem, &[run(REGION_BASE, 0x1000)], glen(0x1000)).unwrap();
        let mut ps = PoolState::default();
        ps.keep_osdesc_arena(1, 10, arena());
        ps.keep_osdesc_arena(2, 10, arena());
        ps.keep_osdesc_arena(2, 11, arena());
        assert_eq!(ps.osdesc_arenas.len(), 3);

        ps.drop_osdesc_arena(1, 10);
        assert!(!ps.osdesc_arenas.contains_key(&(1, 10)));
        assert!(ps.osdesc_arenas.contains_key(&(2, 10)));
        assert!(ps.osdesc_arenas.contains_key(&(2, 11)));

        ps.keep_osdesc_arena(1, 10, arena());
        ps.drop_osdesc_client(2);
        assert_eq!(ps.osdesc_arenas.len(), 1);
        assert!(ps.osdesc_arenas.contains_key(&(1, 10)));
    }

    #[test]
    fn pin_limit_rejects_values_that_do_not_fit_in_bytes() {
        assert_eq!(parse_pin_bytes(" 256 "), Some(256 << 20));
        assert_eq!(
            parse_pin_bytes(&(u64::MAX >> 20).to_string()),
            Some(u64::MAX & !((1 << 20) - 1))
        );
        for raw in ["0", "-1", "abc", "17592186044416", "18446744073709551615"] {
            assert_eq!(parse_pin_bytes(raw), None, "{raw}");
        }
    }

    /// A wrapping semaphore end must not bypass pool bounds validation.
    #[test]
    fn write_guest_u32_rejects_wrapping_addresses() {
        let _x = exclusive();
        let mem = guest_mem();
        let ps = pool_state_with(&mem, 0x2000_0000, REGION_BASE, 0x2000);
        for va in [u64::MAX, u64::MAX - 3, u64::MAX - 4] {
            assert!(
                !ps.write_guest_u32_for(1, GuestAddr::new(va), 0x4141_4141),
                "va {va:#x} accepted"
            );
        }
    }

    // ---- GpaRun::decode ---------------------------------------------------

    #[test]
    fn decode_reads_little_endian_and_checks_length() {
        let mut aux = Vec::new();
        aux.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        aux.extend_from_slice(&0x1000u64.to_le_bytes());
        let r = GpaRun::decode(&aux, 1).unwrap();
        assert_eq!(
            (r[0].gpa.get(), r[0].len.get()),
            (0x1122_3344_5566_7788, 0x1000)
        );

        assert!(GpaRun::decode(&aux, 2).is_none(), "too short for 2 runs");
        assert!(
            GpaRun::decode(&aux[..15], 1).is_none(),
            "one byte too short"
        );
        assert_eq!(GpaRun::decode(&[], 0).unwrap().len(), 0);
        // Trailing auxiliary bytes are not decoded as runs.
        aux.push(0xff);
        assert_eq!(GpaRun::decode(&aux, 1).unwrap().len(), 1);
    }

    /// A lied-about run count must not make the size computation overflow
    /// (`count * WIRE`): otherwise the length check would answer "fits" and
    /// `decode` would run past the aux buffer.
    #[test]
    fn decode_survives_an_absurd_count() {
        let aux = [0u8; 64];
        for count in [
            usize::MAX,
            usize::MAX / 16,
            (u32::MAX as usize) + 1,
            1 << 60,
        ] {
            assert!(
                GpaRun::decode(&aux, count).is_none(),
                "count {count} accepted"
            );
        }
    }

    /// Wire stride is two little-endian u64 values without padding.
    /// decode uses this same stride for both bounds checks and indexing.
    #[test]
    fn gpa_run_wire_is_two_little_endian_u64() {
        assert_eq!(GpaRun::WIRE, 16);
        assert_eq!(GpaRun::WIRE, 2 * std::mem::size_of::<u64>());
        // ... and `decode` really does read at that stride.
        let mut aux = Vec::new();
        aux.extend_from_slice(&1u64.to_le_bytes());
        aux.extend_from_slice(&2u64.to_le_bytes());
        aux.extend_from_slice(&3u64.to_le_bytes());
        aux.extend_from_slice(&4u64.to_le_bytes());
        assert_eq!(aux.len(), 2 * GpaRun::WIRE);
        let r = GpaRun::decode(&aux, 2).unwrap();
        assert_eq!((r[0].gpa.get(), r[0].len.get()), (1, 2));
        assert_eq!((r[1].gpa.get(), r[1].len.get()), (3, 4));
    }

    /// Match OSDESC_FLAGS to the DRF definitions required by RmAllocOsDescriptor
    /// (escape.c:206-225), using the independently checked nvrm-abi fields.
    #[test]
    fn osdesc_flags_is_the_drf_composition_rm_demands() {
        use nvrm_abi::nvgpu::nvos02_flags;
        let want = nvos02_flags::PHYSICALITY.set(sys::NVOS02_FLAGS_PHYSICALITY_NONCONTIGUOUS)
            | nvos02_flags::LOCATION.set(sys::NVOS02_FLAGS_LOCATION_PCI)
            | nvos02_flags::COHERENCY.set(sys::NVOS02_FLAGS_COHERENCY_CACHED)
            | nvos02_flags::MAPPING.set(sys::NVOS02_FLAGS_MAPPING_NO_MAP);
        assert_eq!(
            OSDESC_FLAGS, want,
            "OSDESC_FLAGS {OSDESC_FLAGS:#x} is not the DRF composition {want:#x}"
        );
        // And the fields really are the ones named, read back out.
        assert_eq!(
            nvos02_flags::PHYSICALITY.get(OSDESC_FLAGS),
            sys::NVOS02_FLAGS_PHYSICALITY_NONCONTIGUOUS
        );
        assert_eq!(
            nvos02_flags::LOCATION.get(OSDESC_FLAGS),
            sys::NVOS02_FLAGS_LOCATION_PCI
        );
        assert_eq!(
            nvos02_flags::COHERENCY.get(OSDESC_FLAGS),
            sys::NVOS02_FLAGS_COHERENCY_CACHED
        );
        assert_eq!(
            nvos02_flags::MAPPING.get(OSDESC_FLAGS),
            sys::NVOS02_FLAGS_MAPPING_NO_MAP
        );
    }
}
