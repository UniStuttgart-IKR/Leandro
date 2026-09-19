// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! RM system-memory allocation and CPU/GPU mappings for diagnostics.
//!
//! Each CPU mapping needs a fresh FD because RM permits one mmap context per
//! FD. NVOS33 selects the allocation offset; mmap itself requires offset zero.
//! GPU mappings reserve their own offsets within an NV50_MEMORY_VIRTUAL range.

use nvrm_abi::nvgpu::{nvos32_attr, nvos33_flags, nvos46_flags, Nvos33WithFd};
use nvrm_abi::{check_status, sys, NvDevice, Result};

use crate::RmClient;

/// Heap attribution tag identifying this project in RM allocation reports.
const OWNER_TAG: u32 = u32::from_le_bytes(*b"lean");

// Attributes
/// PCI system-memory attributes with explicit 4 KiB pages. `coherency` is an
/// NVOS32_ATTR_COHERENCY value. libcuda instead uses PAGE_SIZE_DEFAULT and
/// PHYSICALITY_ALLOW_NONCONTIGUOUS (attr 0x3a000000).
pub fn sysmem_attr(coherency: u32) -> u32 {
    nvos32_attr::LOCATION.set(sys::NVOS32_ATTR_LOCATION_PCI as u32)
        | nvos32_attr::PHYSICALITY.set(sys::NVOS32_ATTR_PHYSICALITY_NONCONTIGUOUS as u32)
        | nvos32_attr::COHERENCY.set(coherency)
        | nvos32_attr::PAGE_SIZE.set(sys::NVOS32_ATTR_PAGE_SIZE_4KB as u32)
}
// Allocation
/// Allocate NV01_MEMORY_SYSTEM under `device`. Alignment and flags stay zero.
/// RM adds MAP_NOT_REQUIRED (standard_mem.c); libcuda also requests
/// IGNORE_BANK_PLACEMENT and MEMORY_HANDLE_PROVIDED.
pub fn alloc_sysmem(rm: &mut RmClient, device: u32, size: usize, coherency: u32) -> Result<u32> {
    let handle = rm.next_handle();

    let mut p = sys::NV_MEMORY_ALLOCATION_PARAMS::default();
    p.owner = OWNER_TAG;
    p.type_ = sys::NVOS32_TYPE_IMAGE as u32;
    p.flags = 0;
    p.attr = sysmem_attr(coherency);
    p.attr2 = 0;
    p.size = size as u64;
    p.alignment = 0;

    // SAFETY: NV_MEMORY_ALLOCATION_PARAMS matches this memory class; no host pointers.
    unsafe { rm.alloc(device, handle, sys::NV01_MEMORY_SYSTEM, Some(&mut p)) }
}

/// A GPU VA reservation with a bump allocator. RM requires callers to choose
/// each mapping offset within NV50_MEMORY_VIRTUAL.
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
        // Validate alignment and arithmetic before advancing the reservation.
        if align == 0 || !align.is_power_of_two() {
            return Err(full);
        }
        let Some(start) = self.next.checked_add(align - 1).map(|v| v & !(align - 1)) else {
            return Err(full);
        };
        let Some(end) = start.checked_add(len) else {
            return Err(full);
        };
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

/// Reserve NV50_MEMORY_VIRTUAL within `vaspace`. MAP_MEMORY_DMA uses this
/// object as hDma; a VASpace handle alone is invalid.
pub fn alloc_virtual(rm: &mut RmClient, device: u32, vaspace: u32, size: u64) -> Result<VaRange> {
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

    // SAFETY: NV_MEMORY_ALLOCATION_PARAMS matches this memory class; no host pointers.
    unsafe { rm.alloc(device, handle, sys::NV50_MEMORY_VIRTUAL, Some(&mut p)) }?;

    eprintln!("vamem h={handle:#x} base={:#x} size={size:#x}", p.offset);
    Ok(VaRange {
        handle,
        base: p.offset,
        size,
        next: p.offset,
    })
}
// GPU address space
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
            p.hClient, p.hDevice, p.hDma, p.hMemory, p.offset, p.length, p.flags, p.dmaOffset,
        );
    }
    check_status(sys::NV_ESC_RM_MAP_MEMORY_DMA, p.status as u32)?;
    Ok(p.dmaOffset)
}
// CPU address space
/// A CPU mapping and the FD holding its RM mmap context.
pub struct CpuMap {
    _fd: NvDevice,
    ptr: *mut u8,
    len: usize,
    readable: bool,
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

    fn access_ptr<T>(&self, off: usize) -> *mut T {
        assert!(
            off % std::mem::align_of::<T>() == 0
                && off
                    .checked_add(std::mem::size_of::<T>())
                    .is_some_and(|end| end <= self.len),
            "CPU mapping access is unaligned or out of range"
        );
        // SAFETY: mmap provides page alignment; the full access is within the mapping.
        unsafe { self.ptr.add(off).cast() }
    }

    /// Volatile write to GPU-visible memory.
    ///
    /// # Panics
    /// If `off` is not 4-byte-aligned or out of range.
    pub fn write_u32(&self, off: usize, v: u32) {
        let ptr = self.access_ptr::<u32>(off);
        // SAFETY: access_ptr checked the range and alignment; all CpuMaps are writable.
        unsafe { std::ptr::write_volatile(ptr, v) };
    }

    /// Volatile read from GPU-visible memory.
    ///
    /// # Panics
    /// Panics for write-only mappings, unaligned offsets, or out-of-range accesses.
    pub fn read_u32(&self, off: usize) -> u32 {
        assert!(self.readable, "CPU mapping is write-only");
        let ptr = self.access_ptr::<u32>(off);
        // SAFETY: access_ptr checked the range and alignment; this mapping is readable.
        unsafe { std::ptr::read_volatile(ptr) }
    }

    /// Volatile write to GPU-visible memory.
    ///
    /// # Panics
    /// Panics if `off` is not 8-byte-aligned or is out of range.
    pub fn write_u64(&self, off: usize, v: u64) {
        let ptr = self.access_ptr::<u64>(off);
        // SAFETY: access_ptr checked the range and alignment; all CpuMaps are writable.
        unsafe { std::ptr::write_volatile(ptr, v) };
    }
}

impl Drop for CpuMap {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

/// Install an RM mapping context on a fresh control FD, then mmap offset zero.
pub fn map_cpu(rm: &RmClient, device: u32, handle: u32, size: usize) -> Result<CpuMap> {
    use std::os::fd::AsRawFd;

    let fd = NvDevice::open_ctl()?;

    let mut params = sys::NVOS33_PARAMETERS::default();
    params.hClient = rm.root();
    params.hDevice = device;
    params.hMemory = handle;
    params.offset = 0;
    params.length = size as u64;
    params.pLinearAddress = 0usize as sys::NvP64;
    // RM supplies cacheability. pLinearAddress zero lets mmap choose the VA.
    params.flags = nvos33_flags::ACCESS.set(nvos33_flags::ACCESS_READ_WRITE)
        | nvos33_flags::MEM_SPACE.set(nvos33_flags::MEM_SPACE_CLIENT)
        | nvos33_flags::MAPPING.set(nvos33_flags::MAPPING_DEFAULT)
        | nvos33_flags::RESERVE_ON_UNMAP.set(nvos33_flags::RESERVE_ON_UNMAP_ENABLE);

    let mut p = Nvos33WithFd::new(params, fd.as_raw_fd());

    debug_assert_eq!(std::mem::size_of::<Nvos33WithFd>(), 56);
    unsafe { rm.ctl().ioctl_raw(sys::NV_ESC_RM_MAP_MEMORY, &mut p)? };
    // Print named fields; reading repr(C) padding as bytes is not sound Rust.
    if p.params.status as u32 != sys::NV_OK {
        eprintln!(
            "MAP_MEMORY failed ({}): hClient={:#x} hDevice={:#x} hMemory={:#x} \
             offset={:#x} length={:#x} flags={:#x} pLinear={:#x} fd={}",
            fd.path(),
            p.params.hClient,
            p.params.hDevice,
            p.params.hMemory,
            p.params.offset,
            p.params.length,
            p.params.flags,
            p.params.pLinearAddress as u64,
            p.fd,
        );
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

    Ok(CpuMap {
        _fd: fd,
        ptr: ptr as *mut u8,
        len: size,
        readable: true,
    })
}
// Convenience
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
    device: u32,
    range: Option<&mut VaRange>,
    size: usize,
    coherency: u32,
) -> Result<Buffer> {
    let handle = alloc_sysmem(rm, device, size, coherency)?;
    let gpu_va = match range {
        Some(r) => {
            let va = map_gpu(rm, device, r, handle, size)?;
            // Release this buffer before its GPU VA reservation.
            rm.depends_on(r.handle, handle);
            va
        }
        None => 0,
    };
    let cpu = map_cpu(rm, device, handle, size)?;
    Ok(Buffer {
        handle,
        size,
        gpu_va,
        cpu,
    })
}
// Doorbell page
/// Map TURING_USERMODE_A through a fresh registered GPU FD. hDevice is the
/// subdevice; both RM and mmap use write-only access. CpuMap rejects reads.
pub fn map_doorbell(
    rm: &RmClient,
    gpu: &NvDevice,
    subdevice: u32,
    usermode: u32,
) -> Result<CpuMap> {
    use nvrm_abi::doorbell;
    use std::os::fd::AsRawFd;

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
            p.params.hClient,
            p.params.hDevice,
            p.params.hMemory,
            p.params.length,
            p.params.flags,
            p.fd,
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

    Ok(CpuMap {
        _fd: fd,
        ptr: ptr as *mut u8,
        len: doorbell::USERMODE_SIZE,
        readable: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_map() -> CpuMap {
        let len = 4096;
        // SAFETY: a fresh anonymous mapping, released by CpuMap::drop.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        CpuMap {
            _fd: NvDevice::open("/dev/null").unwrap(),
            ptr: ptr.cast(),
            len,
            readable: true,
        }
    }

    #[test]
    fn cpu_access_checks_alignment_bounds_and_overflow() {
        let map = cpu_map();
        map.write_u32(4092, 42);
        assert_eq!(map.read_u32(4092), 42);
        map.write_u64(4088, 17);
        for off in [1, 4096, usize::MAX - 3] {
            assert!(std::panic::catch_unwind(|| map.write_u32(off, 0)).is_err());
            assert!(std::panic::catch_unwind(|| map.read_u32(off)).is_err());
        }
        for off in [4, 4096, usize::MAX - 7] {
            assert!(std::panic::catch_unwind(|| map.write_u64(off, 0)).is_err());
        }
    }

    #[test]
    fn write_only_mapping_rejects_reads() {
        let mut map = cpu_map();
        map.readable = false;
        map.write_u32(0, 42);
        assert!(std::panic::catch_unwind(|| map.read_u32(0)).is_err());
    }

    // Construct a reservation without requiring a GPU.
    fn range(base: u64, size: u64, next: u64) -> VaRange {
        VaRange {
            handle: 0xdead_beef,
            base,
            size,
            next,
        }
    }

    #[test]
    fn bump_rounds_the_next_address_up_to_the_alignment() {
        let base = 0x7000_0000;
        let mut r = range(base, 0x10_0000, base + 1);
        assert_eq!(r.bump(0x1000, 4096).unwrap(), base + 4096);
        // The next reservation starts after the previous one.
        assert_eq!(r.bump(0x1000, 4096).unwrap(), base + 2 * 4096);
    }

    #[test]
    fn bump_accepts_an_allocation_that_exactly_fills_the_range() {
        let base = 0x7000_0000;
        let mut r = range(base, 0x2000, base);
        assert_eq!(r.bump(0x2000, 4096).unwrap(), base);
        assert_eq!(r.next, base + 0x2000);
        // Full: not even a single byte more.
        assert!(r.bump(1, 4096).is_err());
    }

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

        // Alignment padding also consumes reserved VA.
        let mut r = range(base, 0x2000, base + 1);
        assert!(r.bump(0x2000, 4096).is_err());
        assert_eq!(r.next, base + 1);
    }
}
