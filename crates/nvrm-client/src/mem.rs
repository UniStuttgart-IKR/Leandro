// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Memory: allocate NV01_MEMORY_SYSTEM (system memory the GPU can reach),
//! map it into a GPU virtual address space, map it into our own address
//! space -- what the diagnostic binaries of this crate need to talk to the
//! card directly.
//!
//! The direction is deliberate: *RM* allocates, not us. The other
//! direction -- describing memory we already own to RM as an
//! NV01_MEMORY_SYSTEM_OS_DESCRIPTOR -- is what application buffers use
//! (cudaHostRegister, pin_memory) and what the host backend does for guest
//! pages (`crates/vhost-user-nvrm/src/host_pool.rs`); this crate stays on
//! the allocating side.
//!
//! NVOS33 and NVOS46 below are the parameter blocks of
//! `NV_ESC_RM_MAP_MEMORY` and `NV_ESC_RM_MAP_MEMORY_DMA` (nvos.h); USERD
//! and GPFIFO are a channel's user-space doorbell page and its push-buffer
//! ring.
//!
//! Two driver rules that dictate the structure here:
//!
//! 1. **Exactly one mmap context per fd.** NV_ESC_RM_MAP_MEMORY installs
//!    the context on the fd it points at; a second time on the same fd
//!    yields "attempted to reuse FD". Hence every [`CpuMap`] holds its
//!    own fd.
//! 2. **mmap demands vm_pgoff == 0.** The work happens in the ioctl, mmap
//!    only consumes - the offset belongs in NVOS33.offset, not in mmap.
//!
//! DRF positions (NVIDIA's hi:lo bit-field notation) live in one place
//! only, `nvrm_abi::nvgpu`, where they are checked against gVisor.

use nvrm_abi::nvgpu::{nvos32_attr, nvos33_flags, nvos46_flags, Nvos33WithFd};
use nvrm_abi::{check_status, sys, NvDevice, Result};

use crate::RmClient;

/// Arbitrary tag in the `owner` field. RM only uses it for heap
/// attribution, the value is free -- "lean" (the project name, truncated
/// to four ASCII bytes) makes this crate's allocations findable in nvidia-smi
/// dumps. It read "nvsh" until 2026-08-18, a leftover of the project's
/// former name (docs/NAMING.md).
const OWNER_TAG: u32 = u32::from_le_bytes(*b"lean");

/// Where the fd that receives the mmap context comes from.
///
/// Traces show a fresh `open` before every NV_ESC_RM_MAP_MEMORY libcuda
/// issues, and the payload fd number is exactly that fd. The tracer
/// labels most of them `ctl`. Until that is confirmed, the switch stays
/// here in code rather than in a comment.
const MAPPING_FD_IS_CTL: bool = true;

// ---------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------

/// Attributes for a sysmem allocation the GPU is supposed to see.
///
/// The *values* come from the bindings, the *positions* from
/// `nvgpu::nvos32_attr`.
///
/// Deliberate deviation from libcuda, left in place: libcuda sends
/// `PAGE_SIZE_DEFAULT` and `PHYSICALITY_ALLOW_NONCONTIGUOUS`
/// (attr = 0x3a000000). That is inconsequential as long as `memdescMap`
/// goes through - and it does, otherwise `MAPPING_DIRECT` would not be
/// set in the returned flags.
///
/// `coherency` is one of the `NVOS32_ATTR_COHERENCY_*` constants.
pub fn sysmem_attr(coherency: u32) -> u32 {
    nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_PCI as u32)
        | nvos32_attr::PHYSICALITY.set(sys::NVOS32_ATTR_PHYSICALITY_NONCONTIGUOUS as u32)
        | nvos32_attr::COHERENCY.set(coherency)
        | nvos32_attr::PAGE_SIZE.set(sys::NVOS32_ATTR_PAGE_SIZE_4KB as u32)
}

// ---------------------------------------------------------------------------
// Allocation
// ---------------------------------------------------------------------------

/// NV01_MEMORY_SYSTEM under `device`.
///
/// `alignment` stays 0, and so does `flags`: without
/// NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE, RM ignores the field, and sysmem
/// allocations are page-granular anyway - enough for GPFIFO and USERD.
///
/// libcuda sends `flags = 0xc001` here. Of those, RM sets
/// MAP_NOT_REQUIRED (0x8000) itself in standard_mem.c:68;
/// IGNORE_BANK_PLACEMENT (0x1) and MEMORY_HANDLE_PROVIDED (0x4000) are
/// not replicated yet.
pub fn alloc_sysmem(
    rm: &mut RmClient,
    device: u32,
    size: usize,
    coherency: u32,
) -> Result<u32> {
    let handle = rm.next_handle();

    let mut p = sys::NV_MEMORY_ALLOCATION_PARAMS::default();
    p.owner = OWNER_TAG;
    p.type_ = sys::NVOS32_TYPE_IMAGE as u32;
    p.flags = 0;
    p.attr = sysmem_attr(coherency);
    p.attr2 = 0;
    p.size = size as u64;
    p.alignment = 0;

    rm.alloc(device, handle, sys::NV01_MEMORY_SYSTEM, Some(&mut p))
}

/// A reserved VA range plus a pointer to the next free spot inside it.
///
/// RM does not place allocations inside an NV50_MEMORY_VIRTUAL by itself:
/// the second NV_ESC_RM_MAP_MEMORY_DMA with DMA_OFFSET_FIXED_FALSE yields
/// NV_ERR_INVALID_ARGUMENT, even at identical length. Whoever requests a
/// reservation also manages it.
pub struct VaRange {
    pub handle: u32,
    /// Base GPU VA, from NV_MEMORY_ALLOCATION_PARAMS.offset after the alloc.
    pub base: u64,
    pub size: u64,
    next: u64,
}

impl VaRange {
    /// Next free address, aligned to `align`.
    fn bump(&mut self, len: u64, align: u64) -> Result<u64> {
        let full = nvrm_abi::Error::Rm {
            nr: sys::NV_ESC_RM_MAP_MEMORY_DMA,
            status: sys::NV_ERR_NO_MEMORY as u32,
        };
        // Checked throughout: `align` must be a power of two, and neither
        // the rounding nor `start + len` may wrap -- a wrapped sum would
        // pass the exhaustion check below and hand out an address outside
        // the reservation. The only caller passes 4096 and host-chosen
        // sizes, so this is a guard, not a measured failure.
        if align == 0 || !align.is_power_of_two() {
            return Err(full);
        }
        let Some(start) = self.next.checked_add(align - 1).map(|v| v & !(align - 1)) else {
            return Err(full);
        };
        let Some(end) = start.checked_add(len) else {
            return Err(full);
        };
        // checked, like the rest of bump(): base and size are RM's, not a
        // guest word, so a wrap is not reachable -- but the comment above
        // says this arithmetic is checked throughout, and this was the one
        // add that was not.
        let Some(limit) = self.base.checked_add(self.size) else {
            return Err(full);
        };
        if end > limit {
            return Err(full);
        }
        self.next = end;
        Ok(start)
    }
}

/// NV50_MEMORY_VIRTUAL - a reserved virtual range *inside* a VASpace.
///
/// This is the object NV_ESC_RM_MAP_MEMORY_DMA expects in `hDma`. Putting
/// a VASpace handle there directly yields NV_ERR_INVALID_OBJECT_HANDLE.
///
/// `size` is the size of the reserved VA range, not of the memory.
/// Everything this client maps must fit into it together.
pub fn alloc_virtual(
    rm: &mut RmClient,
    device: u32,
    vaspace: u32,
    size: u64,
) -> Result<VaRange> {
    let handle = rm.next_handle();

    let mut p = sys::NV_MEMORY_ALLOCATION_PARAMS::default();
    p.owner = OWNER_TAG;
    p.type_ = sys::NVOS32_TYPE_IMAGE as u32;
    p.flags = sys::NVOS32_ALLOC_FLAGS_VIRTUAL as u32;
    p.attr = nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_PCI as u32)
        | nvos32_attr::PAGE_SIZE.set(sys::NVOS32_ATTR_PAGE_SIZE_4KB as u32);
    p.attr2 = 0;
    p.size = size;
    p.alignment = 0;
    p.offset = 0; // [IN/OUT] - RM writes the base VA back here
    p.hVASpace = vaspace;

    rm.alloc(device, handle, sys::NV50_MEMORY_VIRTUAL, Some(&mut p))?;

    eprintln!("vamem h={handle:#x} base={:#x} size={size:#x}", p.offset);
    Ok(VaRange { handle, base: p.offset, size, next: p.offset })
}

// ---------------------------------------------------------------------------
// GPU address space
// ---------------------------------------------------------------------------

pub fn map_gpu(
    rm: &RmClient,
    device: u32,
    range: &mut VaRange,
    handle: u32,
    size: usize,
) -> Result<u64> {
    let va = range.bump(size as u64, 4096)?;

    let mut p = sys::NVOS46_PARAMETERS::default();
    p.hClient = rm.root();
    p.hDevice = device;
    p.hDma = range.handle;
    p.hMemory = handle;
    p.offset = 0;
    p.length = size as u64;
    p.flags = nvos46_flags::ACCESS.set(nvos46_flags::ACCESS_READ_WRITE)
        | nvos46_flags::PAGE_SIZE.set(nvos46_flags::PAGE_SIZE_4KB)
        | nvos46_flags::CACHE_SNOOP.set(nvos46_flags::CACHE_SNOOP_ENABLE)
        | nvos46_flags::DMA_OFFSET_FIXED.set(nvos46_flags::DMA_OFFSET_FIXED_TRUE);
    p.flags2 = 0;
    p.kindOverride = 0;
    p.dmaOffset = va; // [IN], because DMA_OFFSET_FIXED_TRUE

    unsafe { rm.ctl().ioctl_raw(sys::NV_ESC_RM_MAP_MEMORY_DMA, &mut p)? };

    if p.status as u32 != sys::NV_OK {
        eprintln!(
            "MAP_MEMORY_DMA failed: hClient={:#x} hDevice={:#x} hDma={:#x} \
             hMemory={:#x} offset={:#x} length={:#x} flags={:#x} dmaOffset={:#x}",
            p.hClient, p.hDevice, p.hDma, p.hMemory,
            p.offset, p.length, p.flags, p.dmaOffset,
        );
    }
    check_status(sys::NV_ESC_RM_MAP_MEMORY_DMA, p.status as u32)?;
    Ok(p.dmaOffset)
}

// ---------------------------------------------------------------------------
// CPU address space
// ---------------------------------------------------------------------------

/// An mmap window onto an RM allocation, together with the fd that
/// carries the mmap context.
///
/// The fd *must* stay alive as long as the mapping does - which is why it
/// lives in here and not with the caller. (libcuda closes it right after
/// the mmap; that is allowed because mmap holds a reference to the file.
/// Here it stays open, the more conservative variant.)
pub struct CpuMap {
    _fd: NvDevice,
    ptr: *mut u8,
    len: usize,
}

impl CpuMap {
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Volatile, because the GPU sits on the other side. The compiler may
    /// coalesce or optimize away a normal write - exactly the bug here
    /// that takes three evenings to find.
    ///
    /// # Panics
    /// If `off` is not 4-byte-aligned or out of range.
    pub fn write_u32(&self, off: usize, v: u32) {
        assert!(off % 4 == 0 && off + 4 <= self.len, "write_u32 out of range");
        unsafe { std::ptr::write_volatile(self.ptr.add(off) as *mut u32, v) };
    }

    pub fn read_u32(&self, off: usize) -> u32 {
        assert!(off % 4 == 0 && off + 4 <= self.len, "read_u32 out of range");
        unsafe { std::ptr::read_volatile(self.ptr.add(off) as *const u32) }
    }

    pub fn write_u64(&self, off: usize, v: u64) {
        assert!(off % 8 == 0 && off + 8 <= self.len, "write_u64 out of range");
        unsafe { std::ptr::write_volatile(self.ptr.add(off) as *mut u64, v) };
    }
}

impl Drop for CpuMap {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

/// NV_ESC_RM_MAP_MEMORY + mmap.
///
/// The mapping fd is opened fresh here - the driver permits only one
/// mmap context per fd. Whether it has to be a `/dev/nvidiactl` or a
/// `/dev/nvidia<N>` is decided by the `MAPPING_FD_IS_CTL` switch above.
///
/// In libcuda traces a fresh `open` precedes every call and its number
/// shows up in the payload; here the fresh fd rides in the params
/// (`Nvos33WithFd`) while the ioctl itself is issued on the client's ctl
/// fd -- and the mapping works. Whether the driver also accepts the
/// libcuda arrangement exclusively is untested (pre-release bug list).
pub fn map_cpu(
    rm: &RmClient,
    gpu: &NvDevice,
    device: u32,
    handle: u32,
    size: usize,
) -> Result<CpuMap> {
    use std::os::fd::AsRawFd;

    let fd = if MAPPING_FD_IS_CTL {
        NvDevice::open_ctl()?
    } else {
        gpu.open_for_mapping(rm.ctl())?
    };

    let mut params = sys::NVOS33_PARAMETERS::default();
    params.hClient = rm.root();
    params.hDevice = device;
    params.hMemory = handle;
    params.offset = 0;
    params.length = size as u64;
    params.pLinearAddress = 0usize as sys::NvP64;
    // Explicit rather than 0, even though it comes out numerically almost
    // the same: at the next 0x1f one wants to see which fields are set
    // this way on purpose. CACHING_TYPE stays out - the driver overwrites
    // it. libcuda does carry MAP_FIXED along but sends pLinear == 0; on
    // Linux that is only a statement of intent for the later mmap.
    params.flags = nvos33_flags::ACCESS.set(nvos33_flags::ACCESS_READ_WRITE)
        | nvos33_flags::MEM_SPACE.set(nvos33_flags::MEM_SPACE_CLIENT)
        | nvos33_flags::MAPPING.set(nvos33_flags::MAPPING_DEFAULT)
        | nvos33_flags::RESERVE_ON_UNMAP.set(nvos33_flags::RESERVE_ON_UNMAP_ENABLE);

    let mut p = Nvos33WithFd::new(params, fd.as_raw_fd());

    debug_assert_eq!(std::mem::size_of::<Nvos33WithFd>(), 56);
    unsafe { rm.ctl().ioctl_raw(sys::NV_ESC_RM_MAP_MEMORY, &mut p)? };
    // On failure, show the 56 bytes that went out. Only then can this be
    // compared against an nvos33in line from a trace without repeating
    // the run.
    if p.params.status as u32 != sys::NV_OK {
        let raw = unsafe {
            std::slice::from_raw_parts(
                &p as *const Nvos33WithFd as *const u8,
                std::mem::size_of::<Nvos33WithFd>(),
            )
        };
        eprintln!(
            "MAP_MEMORY failed ({}): hClient={:#x} hDevice={:#x} hMemory={:#x} \
             offset={:#x} length={:#x} flags={:#x} pLinear={:#x} fd={}",
            fd.path(),
            p.params.hClient, p.params.hDevice, p.params.hMemory,
            p.params.offset, p.params.length, p.params.flags,
            p.params.pLinearAddress as u64, p.fd,
        );
        eprint!("  raw:");
        for (i, b) in raw.iter().enumerate() {
            if i % 8 == 0 {
                eprint!("\n  {i:02x}:");
            }
            eprint!(" {b:02x}");
        }
        eprintln!();
    }
    check_status(sys::NV_ESC_RM_MAP_MEMORY, p.params.status as u32)?;

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0, // vm_pgoff == 0, otherwise the driver rejects it
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(nvrm_abi::Error::Open {
            path: format!("mmap {} ({size} B)", fd.path()),
            source: std::io::Error::last_os_error(),
        });
    }

    Ok(CpuMap { _fd: fd, ptr: ptr as *mut u8, len: size })
}

// ---------------------------------------------------------------------------
// Convenience
// ---------------------------------------------------------------------------

/// An allocation with both views.
pub struct Buffer {
    pub handle: u32,
    pub size: usize,
    /// 0 = not entered into the VASpace (e.g. USERD).
    pub gpu_va: u64,
    pub cpu: CpuMap,
}

/// Allocate, optionally into the VA range, always into our address space.
pub fn buffer(
    rm: &mut RmClient,
    gpu: &NvDevice,
    device: u32,
    range: Option<&mut VaRange>,
    size: usize,
    coherency: u32,
) -> Result<Buffer> {
    let handle = alloc_sysmem(rm, device, size, coherency)?;
    let gpu_va = match range {
        Some(r) => {
            let va = map_gpu(rm, device, r, handle, size)?;
            // The buffer is mapped into the NV50_MEMORY_VIRTUAL and must
            // be torn down before it - otherwise free() of the vamem rips
            // the mappings out from under the allocations.
            rm.depends_on(r.handle, handle);
            va
        }
        None => 0,
    };
    let cpu = map_cpu(rm, gpu, device, handle, size)?;
    Ok(Buffer { handle, size, gpu_va, cpu })
}

// ---------------------------------------------------------------------------
// Doorbell page
// ---------------------------------------------------------------------------

/// Map the TURING_USERMODE_A page. Three deviations from [`map_cpu`],
/// all from libcuda traces and all deliberate:
///
///   1. The mapping fd is a fresh `/dev/nvidia<N>`, not nvidiactl -
///      the page hangs off the subdevice. (The fresh GPU fd needs
///      REGISTER_FD, which `open_for_mapping` takes care of.)
///   2. `hDevice` is the *subdevice* handle, not the device.
///   3. ACCESS_WRITE_ONLY, and mmap accordingly PROT_WRITE without
///      PROT_READ - the driver rejects read protection on a write-only
///      context. Consequence: read_u32 must never be called on the
///      returned map.
pub fn map_doorbell(
    rm: &RmClient,
    gpu: &NvDevice,
    subdevice: u32,
    usermode: u32,
) -> Result<CpuMap> {
    use std::os::fd::AsRawFd;
    use nvrm_abi::doorbell;

    let fd = gpu.open_for_mapping(rm.ctl())?;

    let mut params = sys::NVOS33_PARAMETERS::default();
    params.hClient = rm.root();
    params.hDevice = subdevice;
    params.hMemory = usermode;
    params.offset = 0;
    params.length = doorbell::USERMODE_SIZE as u64;
    params.pLinearAddress = 0usize as sys::NvP64;
    params.flags = nvos33_flags::ACCESS.set(sys::NVOS33_FLAGS_ACCESS_WRITE_ONLY as u32)
        | nvos33_flags::MEM_SPACE.set(nvos33_flags::MEM_SPACE_CLIENT);

    let mut p = Nvos33WithFd::new(params, fd.as_raw_fd());
    unsafe { rm.ctl().ioctl_raw(sys::NV_ESC_RM_MAP_MEMORY, &mut p)? };
    if p.params.status as u32 != sys::NV_OK {
        eprintln!(
            "MAP_MEMORY (doorbell) failed: hClient={:#x} hDevice={:#x} \
             hMemory={:#x} length={:#x} flags={:#x} fd={}",
            p.params.hClient, p.params.hDevice, p.params.hMemory,
            p.params.length, p.params.flags, p.fd,
        );
    }
    check_status(sys::NV_ESC_RM_MAP_MEMORY, p.params.status as u32)?;

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            doorbell::USERMODE_SIZE,
            libc::PROT_WRITE, // no PROT_READ, see above
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(nvrm_abi::Error::Open {
            path: format!("mmap doorbell {}", fd.path()),
            source: std::io::Error::last_os_error(),
        });
    }

    Ok(CpuMap { _fd: fd, ptr: ptr as *mut u8, len: doorbell::USERMODE_SIZE })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A range as `alloc_virtual` hands it back: `next` starts at `base`,
    /// and nothing but [`VaRange::bump`] ever moves it. Built here by hand
    /// because the real constructor needs a GPU.
    fn range(base: u64, size: u64, next: u64) -> VaRange {
        VaRange { handle: 0xdead_beef, base, size, next }
    }

    /// The GPU VA has to satisfy the page size the mapping is made with:
    /// `map_gpu` asks for PAGE_SIZE_4KB and passes DMA_OFFSET_FIXED_TRUE,
    /// so RM takes `dmaOffset` as given and an unaligned one comes back as
    /// NV_ERR_INVALID_ARGUMENT rather than being rounded for us.
    #[test]
    fn bump_rounds_the_next_address_up_to_the_alignment() {
        let base = 0x7000_0000;
        let mut r = range(base, 0x10_0000, base + 1);
        assert_eq!(r.bump(0x1000, 4096).unwrap(), base + 4096);
        // And it continues from the END of what it just handed out, so two
        // mappings can never overlap.
        assert_eq!(r.bump(0x1000, 4096).unwrap(), base + 2 * 4096);
    }

    /// The last address of the range is usable: the reservation is
    /// [base, base + size), and refusing an allocation that ends exactly on
    /// the limit would waste a whole page of a range whose size the caller
    /// picked deliberately.
    #[test]
    fn bump_accepts_an_allocation_that_exactly_fills_the_range() {
        let base = 0x7000_0000;
        let mut r = range(base, 0x2000, base);
        assert_eq!(r.bump(0x2000, 4096).unwrap(), base);
        assert_eq!(r.next, base + 0x2000);
        // Full: not even a single byte more.
        assert!(r.bump(1, 4096).is_err());
    }

    /// Running out of VA range is reported as the RM status RM itself would
    /// have returned, and it leaves the range untouched. `next` moving on a
    /// refused request would silently burn address space -- and because
    /// only `bump` knows where the free spot is, nothing else could ever
    /// give it back.
    #[test]
    fn an_exhausted_range_refuses_without_consuming_anything() {
        let base = 0x7000_0000;
        let mut r = range(base, 0x2000, base);
        let err = r.bump(0x2001, 4096).unwrap_err();
        assert!(
            matches!(err, nvrm_abi::Error::Rm { nr, status }
                     if nr == sys::NV_ESC_RM_MAP_MEMORY_DMA
                        && status == sys::NV_ERR_NO_MEMORY as u32),
            "unexpected error: {err:?}",
        );
        assert_eq!(r.next, base, "a refused request costs no address space");

        // The rounding counts against the range as well: 0x2000 bytes fit,
        // but not from an address that had to be rounded up first.
        let mut r = range(base, 0x2000, base + 1);
        assert!(r.bump(0x2000, 4096).is_err());
        assert_eq!(r.next, base + 1);
    }
}
