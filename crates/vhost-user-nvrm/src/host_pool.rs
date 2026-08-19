// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Attach guest pages to a GPU VA without an mmap on the host's uvm FD.
//!
//! Terms, once: RM is NVIDIA's Resource Manager, the kernel driver behind
//! /dev/nvidiactl and /dev/nvidiaN; UVM is its unified-memory driver
//! (/dev/nvidia-uvm); an OS descriptor is an
//! NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, memory RM pins rather than copies;
//! NVOS02 is the parameter block of RM_ALLOC_MEMORY (nvos.h); DRF is
//! NVIDIA's `hi:lo` bitfield notation, which `OSDESC_FLAGS` below is
//! written in.
//!
//! Two users, one core primitive:
//!  A) The semaphore pool. The guest places writable guest pages at
//!     `addr == GPU VA` and names their GPA runs. The host assembles them
//!     into one contiguous host VA (guest RAM is a file, because the VM
//!     runs with `--memory shared=on`), registers that VA with RM as
//!     NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, and attaches it with
//!     UVM_CREATE_EXTERNAL_RANGE + UVM_MAP_EXTERNAL_ALLOCATION at GPU VA
//!     `addr`.
//!  B) Every forwarded 0x71 alloc (RM_ALLOC_MEMORY) of the guest. Same
//!     arena, but the host VA goes into `NVOS02.pMemory`; the session runs
//!     that alloc anyway, and would otherwise pass RM a guest-side VA,
//!     which means nothing in the host address space.
//!
//! `crates/nvrm-client/src/bin/e1-extmap.rs` demonstrates the chain on its
//! own: it carries at freely chosen GPU VAs. The same chain stands here,
//! only with guest pages instead of a memfd, and with a private backing
//! client that UVM duplicates into the guest's registered VASpace (the
//! GPU's virtual-address-space object) via the
//! DUP grant (grant_dup_same_user).

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::os::fd::AsRawFd;

use nvrm_abi::{share, sys, NvDevice};

use crate::guest_words::{GuestAddr, GuestLen};

type Mem = vm_memory::GuestMemoryAtomic<vm_memory::GuestMemoryMmap<()>>;

/// uvm_ioctl.h -- raw numbers (UVM_IOCTL_BASE(i) == i on Linux). GPU and
/// VASpace are registered by the guest itself (forwarded); only the two
/// external commands are needed here.
const UVM_MAP_EXTERNAL_ALLOCATION: u64 = 33;
const UVM_CREATE_EXTERNAL_RANGE: u64 = 73;

/// nvos.h:192-279, DRF hi:lo as a shift: PHYSICALITY 7:4 (NONCONTIGUOUS=1),
/// LOCATION 11:8 (PCI=0), COHERENCY 15:12 (CACHED=1), MAPPING 31:30
/// (NO_MAP=1). RmAllocOsDescriptor (escape.c:206-225) demands exactly this.
#[allow(clippy::identity_op)] // (0 << 8) documents the DRF field LOCATION=PCI
const OSDESC_FLAGS: u32 = (1 << 4) | (0 << 8) | (1 << 12) | (1 << 30);

/// Pin limit per arena, for both users above: LEA_MAX_PIN_MIB, default 256.
/// Read once on first use; nonsense (0, unparsable) falls back to the
/// default loudly, because a silent limit of 0 would mean "every
/// allocation fails".
fn max_pin_bytes() -> u64 {
    use std::sync::OnceLock;
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let default_mib = 256u64;
        let mib = match std::env::var("LEA_MAX_PIN_MIB") {
            Err(_) => default_mib,
            Ok(s) => match s.trim().parse::<u64>() {
                Ok(v) if v > 0 => v,
                _ => {
                    eprintln!(
                        "vhost-user-nvrm: LEA_MAX_PIN_MIB={s:?} unusable, \
                         using default {default_mib} MiB"
                    );
                    default_mib
                }
            },
        };
        mib << 20
    })
}

/// One (GPA, length) run, as the guest derives it from /proc/self/pagemap.
/// Both fields are guest words -- hence [`GuestAddr`]/[`GuestLen`], whose
/// arithmetic exists only checked (see `guest_words.rs`).
#[derive(Copy, Clone, Debug)]
pub struct GpaRun {
    pub gpa: GuestAddr,
    pub len: GuestLen,
}

impl GpaRun {
    pub const WIRE: usize = 16;

    pub fn decode(aux: &[u8], count: usize) -> Option<Vec<GpaRun>> {
        // checked_mul, not `count * WIRE`: today `count` comes from a u32
        // (`Req.gpa_run_count`) and cannot overflow on 64 bit -- but the
        // length check must not depend on who calls it. On overflow it
        // would answer "fits" and the loop would run past the aux buffer.
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
        use vm_memory::{GuestAddress, GuestAddressSpace, GuestMemory, GuestMemoryRegion};
        // WARNING: every value here comes from the guest -- hence
        // GuestLen/GuestAddr, whose arithmetic exists only checked. The sum
        // check is the guarantee that the runs fit into the arena.
        //
        // Unchecked, two crafted runs pass both checks -- 2 MiB and
        // (0x1000 - 2 MiB) mod 2^64 sum to exactly `total`, and the second
        // run's `gpa + len` wraps small enough to look in-region. The
        // `MAP_FIXED` below then writes 2 MiB over a 4 KiB reservation, into
        // unrelated host address space, and `Drop` only takes the first page
        // back. Hence checked arithmetic throughout. Reachable from the
        // guest via `UvmPoolBack` (`map_len` and the runs are all guest
        // words) and via the forwarded 0x71 alloc.
        let mut sum = GuestLen::new(0);
        for r in runs {
            sum = sum
                .plus(r.len)
                .ok_or_else(|| anyhow::anyhow!("sum of run lengths overflows"))?;
        }
        if sum != total {
            bail!("runs sum to {sum}, expected {total}");
        }
        // The upper bound is a plausibility barrier against a lying guest,
        // not a technical one: 768 MiB arenas carry fine (pagemap, run
        // encoding, RM pinning). The host pins real RAM per allocation, so
        // some limit stays -- but it is selectable per deployment:
        // LEA_MAX_PIN_MIB (default 256).
        if total.is_zero() || total.get() > max_pin_bytes() {
            bail!(
                "arena length {total} over the pin limit {} MiB (LEA_MAX_PIN_MIB)",
                max_pin_bytes() >> 20
            );
        }

        let len = total.get() as usize;
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
        let arena = Arena { base: base as *mut u8, len };

        let guard = mem.memory();
        let mut off = 0u64;
        for r in runs {
            // Map the run onto the guest RAM region and that region's file offset.
            let region = guard
                .find_region(GuestAddress(r.gpa.get()))
                .with_context(|| format!("GPA {:#x} in no region", r.gpa))?;
            let fo = region
                .file_offset()
                .context("guest RAM without a file -- is the VM running with --memory shared=on?")?;
            let region_base = GuestAddr::new(region.start_addr().0);
            // Computed checked: a `gpa + len` that wraps would otherwise be
            // "below the region end" and the run would count as inside it.
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
            // Check the target offset explicitly instead of deriving it from
            // `sum == total`: otherwise the MAP_FIXED below writes past the
            // end of the reservation into unrelated host address space, and
            // Drop only takes `len` back.
            //
            // WARNING: honesty note on test coverage: as long as the checked
            // sum above stands, `off + r.len <= total` holds by construction
            // -- so this bound is unreachable, and it is the only line of
            // this block for which NO failing test exists (demonstrated:
            // remove it alone and everything stays green; remove both and
            // the test dies with SIGSEGV). It stays regardless: it holds the
            // moment `total` comes from a source other than the sum of the
            // runs -- and that is exactly what a further restructuring could
            // do.
            let end_in_arena = off.checked_add(r.len.get()).filter(|e| *e <= total.get());
            if end_in_arena.is_none() {
                bail!("run {:#x} no longer fits into the arena (offset {off:#x})", r.len);
            }
            // `gpa - region_base` is never negative after find_region -- the
            // subtraction stays checked anyway, because it computes with a
            // guest word.
            let in_region = r
                .gpa
                .offset_from(region_base)
                .ok_or_else(|| anyhow::anyhow!("GPA {:#x} before the region", r.gpa))?;
            let file_off = fo.start() + in_region;
            let dst = unsafe { arena.base.add(off as usize) };
            let p = unsafe {
                libc::mmap(
                    dst as *mut libc::c_void,
                    r.len.get() as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    fo.file().as_raw_fd(),
                    file_off as i64,
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
        let gpu_reg = gpu.open_for_mapping(&ctl).context("REGISTER_FD (backing client)")?;

        let mut b = Backing { ctl, gpu_reg, root, device: 0, uuid: Default::default(), next_handle: root + 1 };

        // Device + subdevice, to fetch the GPU UUID.
        let device = b.handle();
        let mut dp = sys::NV0080_ALLOC_PARAMETERS::default();
        dp.deviceId = 0;
        b.alloc(root, device, sys::NV01_DEVICE_0, Some(&mut dp))?;
        b.device = device;

        let subdevice = b.handle();
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

    fn handle(&mut self) -> u32 {
        let h = self.next_handle;
        self.next_handle += 1;
        h
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

    /// OS descriptor onto a host VA, plus the DUP grant for UVM.
    fn os_descriptor(&mut self, host_va: *mut u8, len: u64) -> Result<u32> {
        let osdesc = self.handle();
        let mut wfd = nvrm_abi::nvgpu::Nvos02WithFd::default();
        wfd.params.hRoot = self.root;
        wfd.params.hObjectParent = self.device;
        wfd.params.hObjectNew = osdesc;
        wfd.params.hClass = sys::NV01_MEMORY_SYSTEM_OS_DESCRIPTOR;
        wfd.params.flags = OSDESC_FLAGS;
        wfd.params.pMemory = host_va as usize as sys::NvP64;
        wfd.params.limit = len - 1;
        wfd.fd = -1;
        unsafe { self.gpu_reg.ioctl_raw(sys::NV_ESC_RM_ALLOC_MEMORY, &mut wfd)? };
        nvrm_abi::check_status(sys::NV_ESC_RM_ALLOC_MEMORY, wfd.params.status as u32)?;

        // UVM later duplicates this object into its kernel client. Without
        // the grant, MAP_EXTERNAL fails with 0x1b
        // (INSUFFICIENT_PERMISSIONS).
        let (r, s) =
            unsafe { share::grant_dup_same_user(self.ctl.as_raw_fd(), self.root, osdesc) };
        if r != 0 || s != 0 {
            bail!("DUP grant for OS descriptor {osdesc:#x}: ret {r} status {s:#x}");
        }
        Ok(osdesc)
    }
}

/// An active external mapping of a pool -- held so that the arena and the
/// OS descriptor stay alive as long as the guest uses the pool.
/// `addr`/`len`/`osdesc` are what a re-map path would need (re-attaching
/// after a REGISTER_GPU_VASPACE); today the entry only keeps the arena
/// alive.
#[allow(dead_code)]
struct PoolMap {
    addr: GuestAddr,
    len: GuestLen,
    osdesc: u32,
    arena: Arena,
}

/// Everything a session needs to attach guest pages to GPU VAs. Hooked
/// into `Session`.
#[derive(Default)]
pub struct PoolState {
    backing: Option<Backing>,
    /// Semaphore pools, one per base VA.
    pools: Vec<PoolMap>,
    /// Arenas of forwarded 0x71 allocs, one per created hMemory handle --
    /// held until the RM_FREE.
    osdesc_arenas: HashMap<u32, Arena>,
}

impl PoolState {
    fn backing(&mut self) -> Result<&mut Backing> {
        if self.backing.is_none() {
            self.backing = Some(Backing::new().context("create backing client")?);
        }
        Ok(self.backing.as_mut().unwrap())
    }

    /// Build an arena for a forwarded 0x71 alloc and return the host VA that
    /// belongs in NVOS02.pMemory. The arena is only kept after a successful
    /// alloc, via [`PoolState::keep_osdesc_arena`].
    pub fn arena_for_osdesc(
        &self,
        mem: &Mem,
        runs: &[GpaRun],
        total: GuestLen,
    ) -> Result<(Arena, u64)> {
        let arena = Arena::build(mem, runs, total)?;
        let va = arena.base() as u64;
        Ok((arena, va))
    }

    pub fn keep_osdesc_arena(&mut self, hmemory: u32, arena: Arena) {
        self.osdesc_arenas.insert(hmemory, arena);
    }

    /// Write a u32 to a guest VA, provided it lies inside one of the
    /// registered pools (the arena aliases exactly those guest pages). For
    /// the managed-compat path: the MIGRATE semaphore lies in the semaphore
    /// pool (measured: 0x204a03ff8). `false` if the VA is in no pool -- the
    /// caller then reports loudly.
    ///
    /// WARNING: `va` is a guest word (the `semaphoreAddress` from
    /// UVM_MIGRATE) -- hence [`GuestAddr`], and the range check computes
    /// only checked: with an unchecked `va + 4`, `va = u64::MAX - 3` passed
    /// the check (the sum wraps to 0), and the write offset `va - p.addr`
    /// became an arbitrary, guest-chosen distance from the arena base -- a
    /// directed writer into unrelated host memory.
    pub fn write_guest_u32(&self, va: GuestAddr, val: u32) -> bool {
        let Some(end) = va.end(GuestLen::new(4)) else { return false };
        for p in &self.pools {
            let Some(pool_end) = p.addr.end(p.len) else { continue };
            if va >= p.addr && end <= pool_end {
                let Some(off) = va.offset_from(p.addr) else { continue };
                // SAFETY: the arena is at least p.len big and lives as long
                // as the pool is registered; off+4 <= len has been checked.
                unsafe {
                    std::ptr::write_volatile(p.arena.base().add(off as usize) as *mut u32, val);
                }
                return true;
            }
        }
        false
    }

    pub fn drop_osdesc_arena(&mut self, hmemory: u32) {
        self.osdesc_arenas.remove(&hmemory);
    }

    /// Back the semaphore pool with guest pages and attach it at GPU VA
    /// `addr`. `uvm_fd` is this session's host uvm FD (the guest's GPU and
    /// VASpace are already registered on it).
    pub fn back_pool(
        &mut self,
        mem: &Mem,
        uvm_fd: i32,
        addr: GuestAddr,
        len: GuestLen,
        runs: &[GpaRun],
    ) -> Result<()> {
        let arena = Arena::build(mem, runs, len).context("pool arena")?;
        let backing = self.backing()?;
        let uuid = backing.uuid;
        let root = backing.root;
        let ctl_fd = backing.ctl.as_raw_fd();
        let osdesc =
            backing.os_descriptor(arena.base(), len.get()).context("OS descriptor for pool")?;

        // CREATE_EXTERNAL_RANGE(addr, len) on the guest's uvm FD.
        let mut cr: sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS = unsafe { std::mem::zeroed() };
        cr.base = addr.get();
        cr.length = len.get();
        uvm_call(uvm_fd, UVM_CREATE_EXTERNAL_RANGE, &mut cr, CREATE_RANGE_STATUS_OFF)
            .context("CREATE_EXTERNAL_RANGE")?;

        // MAP_EXTERNAL_ALLOCATION(addr, len, osdesc).
        let mut mp: Box<sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS> =
            unsafe { Box::new(std::mem::zeroed()) };
        mp.base = addr.get();
        mp.length = len.get();
        mp.offset = 0;
        mp.perGpuAttributes[0].gpuUuid = uuid;
        mp.perGpuAttributes[0].gpuMappingType = sys::UvmGpuMappingTypeReadWriteAtomic as u32;
        mp.perGpuAttributes[0].gpuCachingType = sys::UvmGpuCachingTypeDefault as u32;
        mp.gpuAttributesCount = 1;
        mp.rmCtrlFd = ctl_fd;
        mp.hClient = root;
        mp.hMemory = osdesc;
        uvm_call(uvm_fd, UVM_MAP_EXTERNAL_ALLOCATION, mp.as_mut(), MAP_EXTERNAL_STATUS_OFF)
            .context("MAP_EXTERNAL_ALLOCATION")?;

        // An older entry for the same GPU VA is dropped here -- otherwise it
        // would win the search in `write_guest_u32` forever, and its arena
        // would leak for as long as the session lives.
        //
        // Why the same GPU VA recurs at all: libcuda places the semaphore
        // pool of EVERY process at the same address (measured: 0x204a00000).
        // As long as one session served a whole VM, that made the host write
        // the MIGRATE semaphore of the SECOND process into the dead arena of
        // the first, and that process spun forever in a userspace wait loop
        // -- managedprobe stage 2 green on the first run, hanging on every
        // later one.
        //
        // Sessions are keyed per guest process now (docs/OPEN-QUESTIONS.md
        // nr 4): every process gets its own `PoolState`, the VA collision
        // is structurally gone, and this line only fires when a process
        // re-registers its own pool. It still matters for the one case that
        // shares a session: a guest that sends `guest_proc == 0` for all of
        // its processes. There the last registration wins, and the losing
        // process gets a loud "not in any pool" on its semaphore write
        // instead of a silent write into the wrong arena.
        //
        // GPU-side the processes do NOT collide: each opens its own uvm FD,
        // so the host holds a separate va_space per process.
        self.pools.retain(|p| p.addr != addr);
        self.pools.push(PoolMap { addr, len, osdesc, arena });
        Ok(())
    }
}

/// Where `rmStatus` sits in the two UVM parameter blocks this file sends.
///
/// From the bindgen structs, not from "the last word": until 2026-08-18
/// `uvm_call` read `size_of::<T>() - 4`, and for
/// `UVM_CREATE_EXTERNAL_RANGE_PARAMS` (24 bytes, `rmStatus @16`, four bytes
/// of tail padding) that is the padding -- always zero, so a refused
/// CREATE_EXTERNAL_RANGE was never noticed here and only the following
/// MAP_EXTERNAL_ALLOCATION failed, blaming the wrong call.
const CREATE_RANGE_STATUS_OFF: usize =
    std::mem::offset_of!(sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS, rmStatus);
const MAP_EXTERNAL_STATUS_OFF: usize =
    std::mem::offset_of!(sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS, rmStatus);
const _: () = {
    assert!(CREATE_RANGE_STATUS_OFF + 4 <= std::mem::size_of::<sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS>());
    assert!(MAP_EXTERNAL_STATUS_OFF + 4 <= std::mem::size_of::<sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS>());
    // The numbers xlate.rs cites for these two structs (uvm_ioctl.h).
    assert!(CREATE_RANGE_STATUS_OFF == 16);
    assert!(MAP_EXTERNAL_STATUS_OFF == 9260);
};

/// Run a UVM ioctl on a raw FD and check the `rmStatus` at `status_off`
/// (an `offset_of!` of the struct behind `p`, never a guess -- see the two
/// constants above).
fn uvm_call<T>(fd: i32, cmd: u64, p: &mut T, status_off: usize) -> Result<()> {
    debug_assert!(status_off + 4 <= std::mem::size_of::<T>());
    let r = unsafe { libc::ioctl(fd, cmd as libc::c_ulong, p as *mut T as *mut libc::c_void) };
    if r != 0 {
        return Err(std::io::Error::last_os_error()).context("UVM ioctl");
    }
    let status = unsafe {
        let base = p as *const T as *const u8;
        u32::from_le_bytes(
            std::slice::from_raw_parts(base.add(status_off), 4).try_into().unwrap(),
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

    /// This process's mappings that sit on the test memfd, as (start, end).
    /// Only those are of interest: a leftover MAP_FIXED cover is
    /// memfd-backed by construction, whereas ordinary heap growth
    /// (anonymous) would otherwise add noise -- in the debug profile,
    /// formatting the error message alone costs a fresh 12 KiB anonymous
    /// mapping.
    fn maps() -> Vec<(u64, u64)> {
        std::fs::read_to_string("/proc/self/maps")
            .unwrap()
            .lines()
            .filter(|l| l.contains(MEMFD_NAME))
            .filter_map(|l| {
                let (a, b) = l.split_whitespace().next()?.split_once('-')?;
                Some((u64::from_str_radix(a, 16).ok()?, u64::from_str_radix(b, 16).ok()?))
            })
            .collect()
    }

    /// `/proc/self/maps` is process-wide -- while a leak measurement runs,
    /// no other test may map anything, or its arenas get counted too. Hence
    /// EVERY test in this module takes this lock.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// GpaRun from raw numbers -- the tests deliberately prepare wrapping
    /// values too, hence the direct route through the constructors.
    fn run(gpa: u64, len: u64) -> GpaRun {
        GpaRun { gpa: GuestAddr::new(gpa), len: GuestLen::new(len) }
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

    /// The arena really does alias the named guest pages -- in run order,
    /// not in GPA order.
    #[test]
    fn arena_aliases_the_named_guest_pages() {
        let _x = exclusive();
        let mem = guest_mem();
        let write_guest = |gpa: u64, val: u8| {
            use vm_memory::{Bytes, GuestAddressSpace};
            mem.memory().write_slice(&[val; 0x1000], GuestAddress(gpa)).unwrap();
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
        assert!(Arena::build(&guest_mem(), &[], glen(0)).is_err(), "length 0");

        // The region must be LARGER than the pin limit, otherwise the region
        // check would already refuse and the test would prove nothing about
        // the limit (only this way does it go red when the limit is removed).
        // The memfd is sparsely populated -- the 512 MiB cost nothing as long
        // as nobody writes into them.
        let mem = guest_mem_at(REGION_BASE, 512 << 20);
        let huge = (256 << 20) + 0x1000; // default pin limit + 1 page
        let runs = [run(REGION_BASE, huge)];
        assert!(Arena::build(&mem, &runs, glen(huge)).is_err(), "over the pin limit");
        // Exactly at the limit it must carry -- the boundary sits where it should.
        let ok_len = 256 << 20;
        let runs = [run(REGION_BASE, ok_len)];
        assert!(Arena::build(&mem, &runs, glen(ok_len)).is_ok(), "exactly at the limit must carry");
    }

    #[test]
    fn rejects_sum_mismatch_and_runs_outside_the_region() {
        let _x = exclusive();
        let mem = guest_mem();
        let runs = [run(REGION_BASE, 0x1000)];
        assert!(Arena::build(&mem, &runs, glen(0x2000)).is_err(), "sum != total");

        // Run starts inside the region but extends past its end.
        let runs = [run(REGION_BASE + REGION_LEN as u64 - 0x1000, 0x2000)];
        assert!(Arena::build(&mem, &runs, glen(0x2000)).is_err(), "run past the region end");

        // Run outside every region.
        let runs = [run(REGION_BASE + (64 << 20), 0x1000)];
        assert!(Arena::build(&mem, &runs, glen(0x1000)).is_err(), "GPA in no region");
    }

    /// WARNING: the case this is all about: two runs whose sum **wraps**.
    ///
    /// Run 1 carries 2 MiB, run 2 carries `(0x1000 - 2 MiB) mod 2^64`. The
    /// wrapping sum is exactly `total = 0x1000`, so it passes the length
    /// check; run 2 additionally passes the region check, because its
    /// `gpa + len` wraps too and becomes small in the process. Computed
    /// unchecked, run 1 then lays a MAP_FIXED of 2 MiB over an arena that
    /// reserves only 0x1000 -- 2 MiB of guest memory land in unrelated host
    /// address space, and `Drop` takes only the first page back.
    ///
    /// The test fails in the **release** profile if the arithmetic is
    /// unchecked (in the debug profile Rust panics on its own first -- also
    /// not an acceptable outcome, but a different one).
    #[test]
    fn rejects_wrapping_sum_without_leaking_address_space() {
        let _x = exclusive();
        let mem = guest_mem();
        const TOTAL: u64 = 0x1000;
        const BIG: u64 = 2 << 20;
        let len2 = TOTAL.wrapping_sub(BIG);
        let gpa2 = REGION_BASE + BIG;
        assert_eq!(BIG.wrapping_add(len2), TOTAL, "setup: sum wraps onto total");
        assert!(gpa2.wrapping_add(len2) < REGION_BASE + REGION_LEN as u64,
                "setup: the region check wraps too");

        let runs = [run(REGION_BASE, BIG), run(gpa2, len2)];
        let leaked = leaked_bytes(|| {
            assert!(Arena::build(&mem, &runs, glen(TOTAL)).is_err(), "wrapping sum must be caught");
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

    /// A PoolState with one registered pool, without a GPU: `osdesc` is
    /// bookkeeping only here, the arena is the only thing touched.
    fn pool_state_with(mem: &Mem, addr: u64, gpa: u64, len: u64) -> PoolState {
        let arena = Arena::build(mem, &[run(gpa, len)], glen(len)).unwrap();
        PoolState {
            backing: None,
            pools: vec![PoolMap {
                addr: GuestAddr::new(addr),
                len: GuestLen::new(len),
                osdesc: 0,
                arena,
            }],
            osdesc_arenas: HashMap::new(),
        }
    }

    #[test]
    fn write_guest_u32_hits_the_pool_and_stops_at_its_edges() {
        let _x = exclusive();
        let mem = guest_mem();
        let (addr, len) = (0x2000_0000u64, 0x2000u64);
        let ps = pool_state_with(&mem, addr, REGION_BASE, len);

        assert!(ps.write_guest_u32(GuestAddr::new(addr), 0xdead_beef), "start of the pool");
        assert!(ps.write_guest_u32(GuestAddr::new(addr + len - 4), 1), "last complete word");
        // SAFETY: the arena lives in the PoolState and is len big.
        unsafe {
            assert_eq!(*(ps.pools[0].arena.base() as *const u32), 0xdead_beef);
        }

        assert!(!ps.write_guest_u32(GuestAddr::new(addr - 4), 1), "before the pool");
        assert!(!ps.write_guest_u32(GuestAddr::new(addr + len - 3), 1), "extends past the end");
        assert!(!ps.write_guest_u32(GuestAddr::new(addr + len), 1), "behind the pool");
    }

    /// WARNING: `va` arrives as the `semaphoreAddress` from UVM_MIGRATE, so
    /// it comes from the guest. Computed unchecked, `va + 4` wraps to 0 at
    /// `u64::MAX - 3`, the check `va + 4 <= p.addr + p.len` then counts as
    /// satisfied, and the write offset `va - p.addr` becomes a guest-chosen
    /// distance from the arena base. The test fails in the release profile
    /// if the check computes unchecked -- usually as a SIGSEGV, not as an
    /// assertion.
    #[test]
    fn write_guest_u32_rejects_wrapping_addresses() {
        let _x = exclusive();
        let mem = guest_mem();
        let ps = pool_state_with(&mem, 0x2000_0000, REGION_BASE, 0x2000);
        for va in [u64::MAX, u64::MAX - 3, u64::MAX - 4] {
            assert!(!ps.write_guest_u32(GuestAddr::new(va), 0x4141_4141), "va {va:#x} accepted");
        }
    }

    // ---- GpaRun::decode ---------------------------------------------------

    #[test]
    fn decode_reads_little_endian_and_checks_length() {
        let mut aux = Vec::new();
        aux.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        aux.extend_from_slice(&0x1000u64.to_le_bytes());
        let r = GpaRun::decode(&aux, 1).unwrap();
        assert_eq!((r[0].gpa.get(), r[0].len.get()), (0x1122_3344_5566_7788, 0x1000));

        assert!(GpaRun::decode(&aux, 2).is_none(), "too short for 2 runs");
        assert!(GpaRun::decode(&aux[..15], 1).is_none(), "one byte too short");
        assert_eq!(GpaRun::decode(&[], 0).unwrap().len(), 0);
        // An aux buffer may be longer than the runs -- the rest is not read.
        aux.push(0xff);
        assert_eq!(GpaRun::decode(&aux, 1).unwrap().len(), 1);
    }

    /// A lied-about run count must not make the size computation overflow
    /// (`count * WIRE`): otherwise the length check would answer "fits" and
    /// `decode` would run past the aux buffer.
    #[test]
    fn decode_survives_an_absurd_count() {
        let aux = [0u8; 64];
        for count in [usize::MAX, usize::MAX / 16, (u32::MAX as usize) + 1, 1 << 60] {
            assert!(GpaRun::decode(&aux, count).is_none(), "count {count} accepted");
        }
    }

    /// `GpaRun::WIRE` is the wire size of one run, and the guest module
    /// packs the aux buffer to exactly this stride: two little-endian u64,
    /// no padding, no header.
    ///
    /// It is the multiplier in every bounds check `decode` makes, so it is
    /// not a free-floating number: were it smaller than the real stride,
    /// the length check would pass for a buffer that is too short.
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

    /// `OSDESC_FLAGS` is written as four raw shifts, and RM's
    /// `RmAllocOsDescriptor` (escape.c:206-225) refuses anything else. This
    /// pins those shifts to the DRF field definitions in `nvrm-abi`, which
    /// are themselves guarded against nvos.h -- so the hand-typed word here
    /// cannot drift away from the header it was copied from.
    ///
    /// Not a `const _` guard only because it reads better as a test next to
    /// the other flag arithmetic; the composition is `const fn` throughout.
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
        assert_eq!(nvos02_flags::LOCATION.get(OSDESC_FLAGS), sys::NVOS02_FLAGS_LOCATION_PCI);
        assert_eq!(
            nvos02_flags::COHERENCY.get(OSDESC_FLAGS),
            sys::NVOS02_FLAGS_COHERENCY_CACHED
        );
        assert_eq!(nvos02_flags::MAPPING.get(OSDESC_FLAGS), sys::NVOS02_FLAGS_MAPPING_NO_MAP);
    }
}
