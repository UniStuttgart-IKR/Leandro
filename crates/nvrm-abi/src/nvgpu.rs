// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! NVIDIA RM definitions missing from the generated SDK bindings, plus
//! independent compile-time checks of constants and layouts.
//!
//! # Provenance
//!
//! Layouts, escape numbers, and bitfield positions were transcribed from:
//!
//!   github.com/google/gvisor @ 3355a32
//!   pkg/abi/nvgpu/{frontend,classes,nvgpu,status}.go
//!   Copyright 2023 The gVisor Authors
//!   License: Apache License 2.0
//!
//! This Rust transcription is MIT-licensed; gVisor's source is Apache-2.0.
//! See LICENSES.md, including its "Third-party material" attribution.
//!
//! Handwritten definitions cover Unix wrapper structs
//! (`nv-unix-nvos-params-wrappers.h`), DRF bit positions such as `25:23`,
//! and `NV_IOCTL_BASE + n` escapes from `nv-ioctl-numbers.h`.
//! The remaining declarations check the generated bindings.
//!
//! Investigate ABI changes before updating a failed guard; do not remove it.
//! Add missing generated symbols through `nvrm-sys/abi.toml` and wrapper.h,
//! then regenerate with `cargo xtask abi`.

use crate::sys;

// DRF - bitfields

/// A bitfield within a 32-bit word.
/// Accepts both nvos.h's `hi:lo` and gVisor's SHIFT/MASK notation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drf {
    pub shift: u32,
    pub mask: u32,
}

impl Drf {
    /// Header notation `hi:lo`.
    pub const fn hi_lo(hi: u32, lo: u32) -> Self {
        assert!(hi >= lo && hi < 32);
        let width = hi - lo + 1;
        let mask = if width >= 32 {
            u32::MAX
        } else {
            (1u32 << width) - 1
        };
        Self { shift: lo, mask }
    }

    /// gVisor notation SHIFT/MASK.
    pub const fn shift_mask(shift: u32, mask: u32) -> Self {
        Self { shift, mask }
    }

    /// Shift a value into its position (ready to OR in).
    pub const fn set(self, v: u32) -> u32 {
        (v & self.mask) << self.shift
    }

    /// Read the field out of a word.
    pub const fn get(self, word: u32) -> u32 {
        (word >> self.shift) & self.mask
    }

    /// Replace the field inside a word.
    pub const fn put(self, word: u32, v: u32) -> u32 {
        (word & !(self.mask << self.shift)) | self.set(v)
    }
}

// Escape numbers
// Sources: gVisor frontend.go, kernel-open/common/inc/nv-ioctl-numbers.h,
// and src/nvidia/arch/nvalloc/unix/include/nv_escape.h.
// Bindgen supplies the nv_escape.h group. The NV_IOCTL_BASE group is
// transcribed here because its expressions require a separate allowlist entry.

pub const NV_IOCTL_BASE: u32 = 200;

pub const NV_ESC_CARD_INFO: u32 = NV_IOCTL_BASE;
/// Bind a `/dev/nvidia<N>` FD to the client's `/dev/nvidiactl` FD.
/// Parameters: [`IoctlRegisterFd`]. Errors are reported through ioctl errno.
pub const NV_ESC_REGISTER_FD: u32 = NV_IOCTL_BASE + 1;
pub const NV_ESC_ALLOC_OS_EVENT: u32 = NV_IOCTL_BASE + 6;
pub const NV_ESC_FREE_OS_EVENT: u32 = NV_IOCTL_BASE + 7;
pub const NV_ESC_CHECK_VERSION_STR: u32 = NV_IOCTL_BASE + 10;
pub const NV_ESC_ATTACH_GPUS_TO_FD: u32 = NV_IOCTL_BASE + 12;
pub const NV_ESC_SYS_PARAMS: u32 = NV_IOCTL_BASE + 14;
/// from nv-ioctl-numa.h
pub const NV_ESC_NUMA_INFO: u32 = NV_IOCTL_BASE + 15;
pub const NV_ESC_WAIT_OPEN_COMPLETE: u32 = NV_IOCTL_BASE + 18;

/// `NV_ADDRESS_SPACE`, used by `NV_MEMORY_DESC_PARAMS::addressSpace` and
/// `memMap_IMPL`'s `effectiveAddrSpace`.
///
/// Source: src/nvidia/generated/g_mem_desc_nvoc.h:96 ff., outside the SDK.
/// The matching `NV_ADDR_*` definitions are in
/// src/nvidia/inc/kernel/vgpu/rm_plugin_shared_code.h:65.
pub mod addr_space {
    pub const UNKNOWN: u32 = 0;
    pub const SYSMEM: u32 = 1;
    pub const FBMEM: u32 = 2;
    pub const REGMEM: u32 = 3;
    pub const VIRTUAL: u32 = 4;
    // 5 is deprecated ADDR_FABRIC; 6 and 7 are Fabric V2 and Multicast.
}

// Structs that are not in an SDK header

/// `nv_ioctl_register_fd_t`, kernel-open/common/inc/nv-ioctl.h.
///
/// gVisor: `IoctlRegisterFD`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoctlRegisterFd {
    pub ctl_fd: i32,
}

/// `nv_offline_addresses_t`, kernel-open/common/inc/nv-ioctl-numa.h.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug)]
pub struct IoctlNumaOfflineAddresses {
    pub addresses: [u64; 64],
    pub num_entries: u32,
}

/// `nv_ioctl_numa_info_t`, same header. Addresses are values, not user pointers.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug)]
pub struct IoctlNumaInfo {
    pub nid: i32,
    pub status: i32,
    pub memblock_size: u64,
    pub numa_mem_addr: u64,
    pub numa_mem_size: u64,
    pub use_auto_online: u8,
    pub offline_addresses: IoctlNumaOfflineAddresses,
}

const _: () = {
    assert!(core::mem::size_of::<IoctlNumaOfflineAddresses>() == 520);
    assert!(core::mem::offset_of!(IoctlNumaOfflineAddresses, num_entries) == 512);
    assert!(core::mem::size_of::<IoctlNumaInfo>() == 560);
    assert!(core::mem::offset_of!(IoctlNumaInfo, memblock_size) == 8);
    assert!(core::mem::offset_of!(IoctlNumaInfo, use_auto_online) == 32);
    assert!(core::mem::offset_of!(IoctlNumaInfo, offline_addresses) == 40);
};

/// `nv_ioctl_alloc_os_event_t`, kernel-open/common/inc/nv-ioctl.h:72.
///
/// `allocate_os_event` (osapi.c:597) uses `fd` as an event key, not a file
/// descriptor. RM notifies through the file receiving the ioctl. Use the same
/// key in `NV0005_ALLOC_PARAMETERS.data`; duplicate `(hClient, fd)` pairs
/// return `NV_ERR_INVALID_ARGUMENT`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoctlAllocOsEvent {
    pub h_client: u32,
    pub h_device: u32,
    pub fd: u32,
    pub status: u32,
}

/// `nv_ioctl_free_os_event_t`, same header, same layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoctlFreeOsEvent {
    pub h_client: u32,
    pub h_device: u32,
    pub fd: u32,
    pub status: u32,
}

const _: () = {
    assert!(core::mem::size_of::<IoctlAllocOsEvent>() == 16);
    assert!(core::mem::size_of::<IoctlFreeOsEvent>() == 16);
};

/// `nv_ioctl_nvos33_parameters_with_fd`,
/// src/nvidia/arch/nvalloc/unix/include/nv-unix-nvos-params-wrappers.h:44.
///
/// gVisor's `IoctlNVOS33ParametersWithFD` implements `HasFrontendFD`:
/// the process-local `fd` needs translation when forwarded; RM handles do not.
/// Explicit padding preserves the ioctl-encoded size and is initialized by `new`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Nvos33WithFd {
    pub params: sys::NVOS33_PARAMETERS,
    pub fd: i32,
    _pad: u32,
}

impl Nvos33WithFd {
    pub const fn new(params: sys::NVOS33_PARAMETERS, fd: i32) -> Self {
        Self {
            params,
            fd,
            _pad: 0,
        }
    }
}

/// `nv_ioctl_nvos02_parameters_with_fd`, from the same Unix wrapper header.
/// Used by `NV_ESC_RM_ALLOC_MEMORY` (0x27), including OS-descriptor allocations
/// in `host_pool.rs` and `e1-extmap`. The FD requires translation.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Nvos02WithFd {
    pub params: sys::NVOS02_PARAMETERS,
    pub fd: i32,
    _pad: u32,
}

/// Escapes represented by gVisor's `nvgpu.HasFrontendFD` interface:
/// `IoctlAllocOSEvent`, `IoctlFreeOSEvent`, `IoctlNVOS02ParametersWithFD`,
/// and `IoctlNVOS33ParametersWithFD`.
/// `NV_ESC_REGISTER_FD` also carries an FD and is handled separately.
pub const ESCAPES_WITH_FD: [u32; 4] = [
    NV_ESC_ALLOC_OS_EVENT,
    NV_ESC_FREE_OS_EVENT,
    sys::NV_ESC_RM_ALLOC_MEMORY,
    sys::NV_ESC_RM_MAP_MEMORY,
];

// Bitfields

/// `NVOS33_PARAMETERS::flags` for `NV_ESC_RM_MAP_MEMORY`.
/// Positions: nvos.h:1724 ff. (610.43.03). gVisor also defines CACHING_TYPE.
pub mod nvos33_flags {
    use super::Drf;

    pub const ACCESS: Drf = Drf::hi_lo(1, 0);
    pub const ACCESS_READ_WRITE: u32 = 0;
    pub const ACCESS_READ_ONLY: u32 = 1;
    pub const ACCESS_WRITE_ONLY: u32 = 2;

    pub const PERSISTENT: Drf = Drf::hi_lo(4, 4);
    pub const PERSISTENT_DISABLE: u32 = 0;
    pub const PERSISTENT_ENABLE: u32 = 1;

    /// Bit 8; FIFO_MAPPING occupies bit 17.
    pub const SKIP_SIZE_CHECK: Drf = Drf::hi_lo(8, 8);
    pub const SKIP_SIZE_CHECK_DISABLE: u32 = 0;
    pub const SKIP_SIZE_CHECK_ENABLE: u32 = 1;

    pub const MEM_SPACE: Drf = Drf::hi_lo(14, 14);
    pub const MEM_SPACE_CLIENT: u32 = 0;
    pub const MEM_SPACE_USER: u32 = 1;

    pub const MAPPING: Drf = Drf::hi_lo(16, 15);
    pub const MAPPING_DEFAULT: u32 = 0;
    pub const MAPPING_DIRECT: u32 = 1;
    pub const MAPPING_REFLECTED: u32 = 2;

    pub const FIFO_MAPPING: Drf = Drf::hi_lo(17, 17);
    pub const FIFO_MAPPING_DEFAULT: u32 = 0;
    pub const FIFO_MAPPING_ENABLE: u32 = 1;

    /// Request the supplied address rather than a driver-chosen address.
    pub const MAP_FIXED: Drf = Drf::hi_lo(18, 18);
    pub const MAP_FIXED_DISABLE: u32 = 0;
    pub const MAP_FIXED_ENABLE: u32 = 1;

    /// Preserve the address reservation when unmapping.
    pub const RESERVE_ON_UNMAP: Drf = Drf::hi_lo(19, 19);
    pub const RESERVE_ON_UNMAP_DISABLE: u32 = 0;
    pub const RESERVE_ON_UNMAP_ENABLE: u32 = 1;

    pub const BUS: Drf = Drf::hi_lo(21, 20);
    pub const BUS_DEFAULT: u32 = 0;
    pub const BUS_COHERENT_LINK: u32 = 1;
    pub const BUS_PCIE: u32 = 2;

    pub const OS_DESCRIPTOR: Drf = Drf::hi_lo(22, 22);
    pub const OS_DESCRIPTOR_DISABLE: u32 = 0;
    pub const OS_DESCRIPTOR_ENABLE: u32 = 1;

    /// Output caching type; gVisor uses SHIFT 23, MASK 0x7.
    /// nvproxy reads the driver's result to choose its mmap memory type.
    pub const CACHING_TYPE: Drf = Drf::hi_lo(25, 23);
    pub const CACHING_TYPE_CACHED: u32 = 0;
    pub const CACHING_TYPE_UNCACHED: u32 = 1;
    pub const CACHING_TYPE_WRITECOMBINED: u32 = 2;
    pub const CACHING_TYPE_WRITEBACK: u32 = 5;
    pub const CACHING_TYPE_DEFAULT: u32 = 6;
    pub const CACHING_TYPE_UNCACHED_WEAK: u32 = 7;

    pub const ALLOW_MAPPING_ON_HCC: Drf = Drf::hi_lo(26, 26);
    pub const ALLOW_MAPPING_ON_HCC_NO: u32 = 0;
    pub const ALLOW_MAPPING_ON_HCC_YES: u32 = 1;
}

/// `NVOS46_PARAMETERS::flags` for `NV_ESC_RM_MAP_MEMORY_DMA`.
/// Positions: nvos.h:1975 ff. (610.43.03); absent from gVisor.
pub mod nvos46_flags {
    use super::Drf;

    pub const ACCESS: Drf = Drf::hi_lo(1, 0);
    pub const ACCESS_READ_WRITE: u32 = 0;
    pub const ACCESS_READ_ONLY: u32 = 1;
    pub const ACCESS_WRITE_ONLY: u32 = 2;

    pub const PAGE_KIND: Drf = Drf::hi_lo(3, 3);
    pub const PAGE_KIND_PHYSICAL: u32 = 0;
    pub const PAGE_KIND_VIRTUAL: u32 = 1;

    /// Snoop CPU caches for coherent CPU/GPU access to sysmem.
    /// `map_gpu` enables this on every sysmem mapping; RM exposes no flush call.
    pub const CACHE_SNOOP: Drf = Drf::hi_lo(4, 4);
    pub const CACHE_SNOOP_DISABLE: u32 = 0;
    pub const CACHE_SNOOP_ENABLE: u32 = 1;

    pub const KERNEL_MAPPING: Drf = Drf::hi_lo(5, 5);
    pub const KERNEL_MAPPING_NONE: u32 = 0;

    pub const SHADER_ACCESS: Drf = Drf::hi_lo(7, 6);
    pub const SHADER_ACCESS_DEFAULT: u32 = 0;
    pub const SHADER_ACCESS_READ_ONLY: u32 = 1;
    pub const SHADER_ACCESS_WRITE_ONLY: u32 = 2;
    pub const SHADER_ACCESS_READ_WRITE: u32 = 3;

    /// Four-bit mapping page size. It must match the allocation's
    /// NVOS32_ATTR_PAGE_SIZE or RM returns NV_ERR_INVALID_ARGUMENT.
    pub const PAGE_SIZE: Drf = Drf::hi_lo(11, 8);
    pub const PAGE_SIZE_DEFAULT: u32 = 0;
    pub const PAGE_SIZE_4KB: u32 = 1;
    pub const PAGE_SIZE_BIG: u32 = 2;
    pub const PAGE_SIZE_BOTH: u32 = 3;
    pub const PAGE_SIZE_HUGE: u32 = 4;
    pub const PAGE_SIZE_512M: u32 = 5;

    pub const DMA_OFFSET_GROWS: Drf = Drf::hi_lo(14, 14);
    pub const DMA_OFFSET_GROWS_UP: u32 = 0;
    pub const DMA_OFFSET_GROWS_DOWN: u32 = 1;

    /// FALSE = `dmaOffset` is an output, RM picks the VA itself.
    pub const DMA_OFFSET_FIXED: Drf = Drf::hi_lo(15, 15);
    pub const DMA_OFFSET_FIXED_FALSE: u32 = 0;
    pub const DMA_OFFSET_FIXED_TRUE: u32 = 1;

    pub const GPU_CACHEABLE: Drf = Drf::hi_lo(18, 17);
    pub const GPU_CACHEABLE_DEFAULT: u32 = 0;
    pub const GPU_CACHEABLE_YES: u32 = 1;
    pub const GPU_CACHEABLE_NO: u32 = 2;

    pub const PAGE_KIND_OVERRIDE: Drf = Drf::hi_lo(19, 19);
    pub const TLB_LOCK: Drf = Drf::hi_lo(28, 28);
    pub const DEFER_TLB_INVALIDATION: Drf = Drf::hi_lo(31, 31);
}

/// `NV_MEMORY_ALLOCATION_PARAMS::attr` positions from nvos.h.
/// Values come from the bindings; gVisor also defines LOCATION (SHIFT 25, MASK 0x3).
pub mod nvos32_attr {
    use super::Drf;

    /// Position from nvos.h.
    pub const PAGE_SIZE: Drf = Drf::hi_lo(24, 23);
    /// gVisor: SHIFT 25, MASK 0x3
    pub const LOCATION: Drf = Drf::shift_mask(25, 0x3);
    /// Position from nvos.h.
    pub const PHYSICALITY: Drf = Drf::hi_lo(28, 27);
    /// Position from nvos.h.
    pub const COHERENCY: Drf = Drf::hi_lo(31, 29);
}

/// `NV_MEMORY_ALLOCATION_PARAMS::attr2`.
pub mod nvos32_attr2 {
    use super::Drf;

    /// Position from nvos.h:1124.
    pub const GPU_CACHEABLE: Drf = Drf::hi_lo(3, 2);

    /// nvos.h:1238. NVKMS sets this for SCANOUT (nvkms-kapi.c:852);
    /// the guest VRAM balloon uses it to identify display memory.
    pub const ISO: Drf = Drf::hi_lo(18, 18);

    /// gVisor: SHIFT 24, MASK 0x1
    pub const USE_EGM: Drf = Drf::shift_mask(24, 0x1);
    pub const USE_EGM_FALSE: u32 = 0;
    pub const USE_EGM_TRUE: u32 = 1;
}

/// `NVOS02_PARAMETERS::flags` for `NV_ESC_RM_ALLOC_MEMORY`.
/// ALLOC and MAPPING use gVisor's positions; the rest use nvos.h:192-204,281.
/// `RmAllocOsDescriptor` (escape.c:206-238) converts these into NVOS32 flags.
pub mod nvos02_flags {
    use super::Drf;

    /// Position from nvos.h.
    pub const PHYSICALITY: Drf = Drf::hi_lo(7, 4);
    /// Position from nvos.h. OS-descriptor allocations require PCI (escape.c:206).
    pub const LOCATION: Drf = Drf::hi_lo(11, 8);
    /// OS-descriptor path: CACHED and WRITE_BACK become OS32 WRITE_BACK;
    /// UNCACHED stays unchanged. Other values fail (escape.c:215-225).
    pub const COHERENCY: Drf = Drf::hi_lo(15, 12);

    pub const ALLOC: Drf = Drf::shift_mask(16, 0x3);
    pub const ALLOC_NONE: u32 = 1;

    /// Position from nvos.h.
    pub const GPU_CACHEABLE: Drf = Drf::hi_lo(18, 18);

    pub const MAPPING: Drf = Drf::shift_mask(30, 0x3);
    pub const MAPPING_NO_MAP: u32 = 1;
}

/// `NV_CHANNEL_ALLOC_PARAMS::flags` positions from
/// src/common/sdk/nvidia/inc/alloc/alloc_channel.h:65 ff.
/// Values come from the bindings. Recheck NVOS04_FLAGS when updating headers.
pub mod nvos04_flags {
    use super::Drf;

    pub const CHANNEL_TYPE: Drf = Drf::hi_lo(1, 0);
    pub const VPR: Drf = Drf::hi_lo(2, 2);
    /// Confidential Computing interpretation of VPR's bit 2.
    pub const CC_SECURE: Drf = Drf::hi_lo(2, 2);
    pub const CHANNEL_SKIP_MAP_REFCOUNTING: Drf = Drf::hi_lo(3, 3);
    pub const GROUP_CHANNEL_RUNQUEUE: Drf = Drf::hi_lo(4, 4);
    pub const PRIVILEGED_CHANNEL: Drf = Drf::hi_lo(5, 5);
    pub const DELAY_CHANNEL_SCHEDULING: Drf = Drf::hi_lo(6, 6);
    pub const CHANNEL_DENY_PHYSICAL_MODE_CE: Drf = Drf::hi_lo(7, 7);
    /// Only relevant when RM provides the USERD itself (`hUserdMemory[0] == 0`).
    pub const CHANNEL_USERD_INDEX_VALUE: Drf = Drf::hi_lo(10, 8);
    pub const CHANNEL_USERD_INDEX_FIXED: Drf = Drf::hi_lo(11, 11);
    pub const CHANNEL_USERD_INDEX_PAGE_VALUE: Drf = Drf::hi_lo(20, 12);
    pub const CHANNEL_USERD_INDEX_PAGE_FIXED: Drf = Drf::hi_lo(21, 21);
    pub const CHANNEL_DENY_AUTH_LEVEL_PRIV: Drf = Drf::hi_lo(22, 22);
    pub const CHANNEL_SKIP_SCRUBBER: Drf = Drf::hi_lo(23, 23);
    pub const CHANNEL_CLIENT_MAP_FIFO: Drf = Drf::hi_lo(24, 24);
    pub const SET_EVICT_LAST_CE_PREFETCH_CHANNEL: Drf = Drf::hi_lo(25, 25);
    pub const CHANNEL_VGPU_PLUGIN_CONTEXT: Drf = Drf::hi_lo(26, 26);
    pub const CHANNEL_PBDMA_ACQUIRE_TIMEOUT: Drf = Drf::hi_lo(27, 27);
    pub const GROUP_CHANNEL_THREAD: Drf = Drf::hi_lo(29, 28);
    pub const MAP_CHANNEL: Drf = Drf::hi_lo(30, 30);
    pub const SKIP_CTXBUFFER_ALLOC: Drf = Drf::hi_lo(31, 31);
}

/// TURING_CHANNEL_GPFIFO_A pushbuffer, semaphore, and USERD definitions.
///
/// Source: class/clc46f.h; see also tinygrad's ops_nv.py, `nvmethod()`.
/// Values come from the bindings; DRF positions and volatile `Nvc46fControl`
/// offsets are transcribed here. These fields live in GPU-visible memory
/// and cannot be inferred from ioctl traces.
pub mod clc46f {
    use super::Drf;

    // GP_ENTRY - two dwords per GPFIFO entry (GP_ENTRY__SIZE == 8).
    /// Bits 31:2 of the pushbuffer VA (the entry must be 4-byte aligned).
    pub const GP_ENTRY0_GET: Drf = Drf::hi_lo(31, 2);
    /// Bits 39:32 of the pushbuffer VA.
    pub const GP_ENTRY1_GET_HI: Drf = Drf::hi_lo(7, 0);
    /// Length of the segment in DWORDS, not bytes.
    pub const GP_ENTRY1_LENGTH: Drf = Drf::hi_lo(30, 10);

    // DMA method header (one dword ahead of the data words).
    /// Method address >> 2.
    pub const DMA_METHOD_ADDRESS: Drf = Drf::hi_lo(11, 0);
    pub const DMA_METHOD_SUBCHANNEL: Drf = Drf::hi_lo(15, 13);
    /// Number of data words.
    pub const DMA_METHOD_COUNT: Drf = Drf::hi_lo(28, 16);
    /// SEC_OP_INC_METHOD (1) = the address increments per data word.
    pub const DMA_SEC_OP: Drf = Drf::hi_lo(31, 29);

    // SEM_EXECUTE fields.
    pub const SEM_EXECUTE_OPERATION: Drf = Drf::hi_lo(2, 0);
    pub const SEM_EXECUTE_RELEASE_WFI: Drf = Drf::hi_lo(20, 20);
    pub const SEM_EXECUTE_PAYLOAD_SIZE: Drf = Drf::hi_lo(24, 24);
    pub const SEM_EXECUTE_RELEASE_TIMESTAMP: Drf = Drf::hi_lo(25, 25);

    // Volatile Nvc46fControl offsets from clc46f.h:48 ff.
    /// Read-only GP FIFO get. Advancement means the scheduler consumed an entry.
    pub const USERD_GP_GET: usize = 0x88;
    /// GP FIFO put; write before ringing the doorbell.
    pub const USERD_GP_PUT: usize = 0x8c;
}

pub mod status {
    pub const NV_OK: u32 = 0x00;
    pub const NV_ERR_INVALID_ADDRESS: u32 = 0x1e;
    pub const NV_ERR_INVALID_ARGUMENT: u32 = 0x1f;
    pub const NV_ERR_INVALID_CLASS: u32 = 0x22;
    pub const NV_ERR_INVALID_CLIENT: u32 = 0x23;
    pub const NV_ERR_INVALID_LIMIT: u32 = 0x2e;
    pub const NV_ERR_NOT_SUPPORTED: u32 = 0x56;

    // Encountered in practice, not taken from gVisor:
    pub const NV_ERR_INSUFFICIENT_PERMISSIONS: u32 = 0x1b;
    pub const NV_ERR_INVALID_OBJECT_HANDLE: u32 = 0x33;
    pub const NV_ERR_STATE_IN_USE: u32 = 0x24;
}

// Guards
// Compare the independent transcription with generated vendor bindings.
// Investigate disagreements before updating guards; retain them when adapting
// bindgen field names such as `type_` and `internalflags`.

macro_rules! assert_layout {
    ($t:ty, size = $size:expr, align = $align:expr $(, $field:ident @ $off:expr)* $(,)?) => {
        const _: () = {
            assert!(::core::mem::size_of::<$t>() == $size);
            assert!(::core::mem::align_of::<$t>() == $align);
            $( assert!(::core::mem::offset_of!($t, $field) == $off); )*
        };
    };
}

// Escape numbers
// gVisor's nv_escape.h group against the bindings.
const _: () = {
    assert!(sys::NV_ESC_RM_ALLOC_MEMORY as u32 == 0x27);
    assert!(sys::NV_ESC_RM_FREE as u32 == 0x29);
    assert!(sys::NV_ESC_RM_CONTROL as u32 == 0x2a);
    assert!(sys::NV_ESC_RM_ALLOC as u32 == 0x2b);
    assert!(sys::NV_ESC_RM_DUP_OBJECT as u32 == 0x34);
    assert!(sys::NV_ESC_RM_SHARE as u32 == 0x35);
    assert!(sys::NV_ESC_RM_VID_HEAP_CONTROL as u32 == 0x4a);
    assert!(sys::NV_ESC_RM_MAP_MEMORY as u32 == 0x4e);
    assert!(sys::NV_ESC_RM_UNMAP_MEMORY as u32 == 0x4f);
    assert!(sys::NV_ESC_RM_MAP_MEMORY_DMA as u32 == 0x57);
    assert!(sys::NV_ESC_RM_UNMAP_MEMORY_DMA as u32 == 0x58);
};

// Class numbers
// gVisor classes.go. NV01_ROOT_CLIENT is 0x41; NV01_ROOT is 0x0.
const _: () = {
    assert!(sys::NV01_ROOT_CLIENT as u32 == 0x41);
    assert!(sys::NV01_MEMORY_SYSTEM as u32 == 0x3e);
    assert!(sys::NV01_DEVICE_0 as u32 == 0x80);
    assert!(sys::NV20_SUBDEVICE_0 as u32 == 0x2080);
    assert!(sys::NV50_MEMORY_VIRTUAL as u32 == 0x50a0);
    assert!(sys::FERMI_CONTEXT_SHARE_A as u32 == 0x9067);
    assert!(sys::FERMI_VASPACE_A as u32 == 0x90f1);
    assert!(sys::KEPLER_CHANNEL_GROUP_A as u32 == 0xa06c);
    assert!(sys::TURING_CHANNEL_GPFIFO_A as u32 == 0xc46f);
    assert!(sys::TURING_USERMODE_A as u32 == 0xc461);
};

// Status codes
const _: () = {
    assert!(sys::NV_OK as u32 == status::NV_OK);
    assert!(sys::NV_ERR_INVALID_ARGUMENT as u32 == status::NV_ERR_INVALID_ARGUMENT);
    assert!(sys::NV_ERR_NOT_SUPPORTED as u32 == status::NV_ERR_NOT_SUPPORTED);
};

// Layouts
// Layouts computed independently from gVisor structs.

// NVOS64_PARAMETERS: _IOC_SIZE 48.
assert_layout!(sys::NVOS64_PARAMETERS, size = 48, align = 8,
    hRoot @ 0, hObjectParent @ 4, hObjectNew @ 8, hClass @ 12,
    pAllocParms @ 16, pRightsRequested @ 24, paramsSize @ 32,
    flags @ 36, status @ 40);

// NVOS00_PARAMETERS - NV_ESC_RM_FREE
assert_layout!(sys::NVOS00_PARAMETERS, size = 16, align = 4,
    hRoot @ 0, hObjectParent @ 4, hObjectOld @ 8, status @ 12);

// NVOS02_PARAMETERS: NV_ESC_RM_ALLOC_MEMORY (nvos.h:285-295).
// table.rs exports these offsets as NVOS02_{HOBJECTNEW,HCLASS,PMEMORY,LIMIT,STATUS}_OFF.
// The guest writes OS-descriptor fields through them, so layout drift must fail here.
assert_layout!(sys::NVOS02_PARAMETERS, size = 48, align = 8,
    hRoot @ 0, hObjectParent @ 4, hObjectNew @ 8, hClass @ 12,
    flags @ 16, pMemory @ 24, limit @ 32, status @ 40);

// NVOS54_PARAMETERS - NV_ESC_RM_CONTROL
assert_layout!(sys::NVOS54_PARAMETERS, size = 32, align = 8,
    hClient @ 0, hObject @ 4, cmd @ 8, flags @ 12,
    params @ 16, paramsSize @ 24, status @ 28);

// NVOS33_PARAMETERS: NV_ESC_RM_MAP_MEMORY.
// gVisor's explicit Pad0[4] after hMemory matches bindgen's implicit padding.
assert_layout!(sys::NVOS33_PARAMETERS, size = 48, align = 8,
    hClient @ 0, hDevice @ 4, hMemory @ 8,
    offset @ 16, length @ 24, pLinearAddress @ 32,
    status @ 40, flags @ 44);

// Wrapper size encoded in the ioctl: 48-byte params, 4-byte FD, 4-byte padding.
assert_layout!(Nvos33WithFd, size = 56, align = 8, params @ 0, fd @ 48);

// NVOS46_PARAMETERS: NV_ESC_RM_MAP_MEMORY_DMA.
// flags2 and kindOverride were added in 580.65.06 (gVisor's
// NVOS46_PARAMETERS_V580); this guard requires that layout.
assert_layout!(sys::NVOS46_PARAMETERS, size = 64, align = 8,
    hClient @ 0, hDevice @ 4, hDma @ 8, hMemory @ 12,
    offset @ 16, length @ 24, flags @ 32, flags2 @ 36,
    kindOverride @ 40, dmaOffset @ 48, status @ 56);

// NV_MEMORY_ALLOCATION_PARAMS_V545 added numaNode in 545.23.06,
// increasing the struct from 120 to 128 bytes.
assert_layout!(sys::NV_MEMORY_ALLOCATION_PARAMS, size = 128, align = 8,
    owner @ 0, type_ @ 4, flags @ 8, width @ 12, height @ 16,
    pitch @ 20, attr @ 24, attr2 @ 28, format @ 32,
    comprCovg @ 36, zcullCovg @ 40,
    rangeLo @ 48, rangeHi @ 56, size @ 64, alignment @ 72,
    offset @ 80, limit @ 88, address @ 96,
    ctagOffset @ 104, hVASpace @ 108, internalflags @ 112,
    tag @ 116, numaNode @ 120);

// NVOS32_PARAMETERS: NV_ESC_RM_VID_HEAP_CONTROL.
// The function selects the union after `free`; the ledger uses AllocSize and Free.
// NvS16 ivcHeapNumber is followed by two padding bytes.
assert_layout!(sys::NVOS32_PARAMETERS, size = 184, align = 8,
    hRoot @ 0, hObjectParent @ 4, function @ 8, hVASpace @ 12,
    ivcHeapNumber @ 16, status @ 20, total @ 24, free @ 32, data @ 40);

// NVOS32_FUNCTION_ALLOC_SIZE offsets are relative to the union.
// size and attr are IN/OUT: the ledger settles the returned allocation size
// and location, which may differ from the request.
assert_layout!(sys::NVOS32_PARAMETERS__bindgen_ty_1__bindgen_ty_1, size = 120, align = 8,
    owner @ 0, hMemory @ 4, type_ @ 8, flags @ 12, attr @ 16,
    format @ 20, comprCovg @ 24, zcullCovg @ 28, partitionStride @ 32,
    width @ 36, height @ 40, size @ 48, alignment @ 56, offset @ 64,
    limit @ 72, address @ 80, rangeBegin @ 88, rangeEnd @ 96,
    attr2 @ 104, ctagOffset @ 108, numaNode @ 112);

// NVOS32_FUNCTION_FREE shares hMemory@4 with AllocSize.
// The ledger uses that common offset for charges and releases.
assert_layout!(sys::NVOS32_PARAMETERS__bindgen_ty_1__bindgen_ty_3, size = 12, align = 4,
    owner @ 0, hMemory @ 4, flags @ 8);

const _: () = {
    assert!(
        core::mem::offset_of!(sys::NVOS32_PARAMETERS__bindgen_ty_1__bindgen_ty_1, hMemory)
            == core::mem::offset_of!(sys::NVOS32_PARAMETERS__bindgen_ty_1__bindgen_ty_3, hMemory),
        "AllocSize and Free must name the memory handle at the same offset"
    );
};

// NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS: gVisor's 610 layout.
// 615.71.09 adds reserved and internalFlags, growing the struct to 28 bytes.
// Other layouts are checked in their generated modules; callers use A::TsgParams.
#[cfg(feature = "v610")]
assert_layout!(sys::v610::NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS, size = 20, align = 4,
    hObjectError @ 0, hObjectEccError @ 4, hVASpace @ 8,
    engineType @ 12, bIsCallingContextVgpuPlugin @ 16);

// DRF against gVisor
// Both notations mapped onto the same type, then compared.
const _: () = {
    use nvos32_attr::*;
    assert!(LOCATION.shift == 25 && LOCATION.mask == 0x3);
    assert!(PAGE_SIZE.shift == 23 && PAGE_SIZE.mask == 0x3);
    assert!(PHYSICALITY.shift == 27 && PHYSICALITY.mask == 0x3);
    assert!(COHERENCY.shift == 29 && COHERENCY.mask == 0x7);
    assert!(nvos33_flags::CACHING_TYPE.shift == 23);
    assert!(nvos33_flags::CACHING_TYPE.mask == 0x7);
    assert!(nvos32_attr2::GPU_CACHEABLE.shift == 2 && nvos32_attr2::GPU_CACHEABLE.mask == 0x3);
};

// NVOS02 flags: gVisor SHIFT/MASK must agree with nvos.h hi:lo positions.
const _: () = {
    use nvos02_flags::*;
    assert!(PHYSICALITY.shift == 4 && PHYSICALITY.mask == 0xf);
    assert!(LOCATION.shift == 8 && LOCATION.mask == 0xf);
    assert!(COHERENCY.shift == 12 && COHERENCY.mask == 0xf);
    assert!(ALLOC.shift == 16 && ALLOC.mask == 0x3);
    assert!(GPU_CACHEABLE.shift == 18 && GPU_CACHEABLE.mask == 0x1);
    assert!(MAPPING.shift == 30 && MAPPING.mask == 0x3);
    // No field may overlap the next one.
    assert!(PHYSICALITY.shift + 4 == LOCATION.shift);
    assert!(LOCATION.shift + 4 == COHERENCY.shift);
    assert!(COHERENCY.shift + 4 == ALLOC.shift);
};

// NV_MEMORY_DESC_PARAMS - the four descriptors inside the channel alloc.
// On the client side they are output/RPC fields: kernel RM fills them in
// only when it issues the GSP RPC (kernel_channel.c, memdescGetPhysAddr
// block). They stay zero in the RM_ALLOC request.
assert_layout!(sys::NV_MEMORY_DESC_PARAMS, size = 24, align = 8,
    base @ 0, size @ 8, addressSpace @ 16, cacheAttrib @ 20);

// NV_CHANNEL_ALLOC_PARAMS_V610: hHandleVASpace@32 was added in 610.43.02.
// The four-byte gap before userdOffset@72 aligns its u64 array.
#[cfg(feature = "v610")]
assert_layout!(sys::v610::NV_CHANNEL_ALLOC_PARAMS, size = 376, align = 8,
    hObjectError @ 0, hObjectBuffer @ 4, gpFifoOffset @ 8,
    gpFifoEntries @ 16, flags @ 20, hContextShare @ 24,
    hVASpace @ 28, hHandleVASpace @ 32,
    hUserdMemory @ 36, userdOffset @ 72,
    engineType @ 136, cid @ 140, subDeviceId @ 144, hObjectEccError @ 148,
    instanceMem @ 152, userdMem @ 176, ramfcMem @ 200, mthdbufMem @ 224,
    hPhysChannelGroup @ 248, internalFlags @ 252,
    errorNotifierMem @ 256, eccErrorNotifierMem @ 280,
    ProcessID @ 304, SubProcessID @ 308,
    encryptIv @ 312, decryptIv @ 324, hmacNonce @ 336, tpcConfigID @ 368);

// Channel scheduling and submit-token controls.
// NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS aliases the three-NvBool NVA06F struct
// (ctrla06fgpfifo.h:69), so paramsSize must be 3.
const _: () = {
    assert!(sys::NVA06C_CTRL_CMD_GPFIFO_SCHEDULE as u32 == 0xa06c0101);
    assert!(sys::NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN as u32 == 0xc36f0108);
    // Notifier index for cross-checking the submit token (nvos.h:2855)
    assert!(sys::NV_CHANNELGPFIFO_NOTIFICATION_TYPE_WORK_SUBMIT_TOKEN as u32 == 1);
};
assert_layout!(sys::NVA06C_CTRL_GPFIFO_SCHEDULE_PARAMS, size = 3, align = 1,
    bEnable @ 0, bSkipSubmit @ 1, bSkipEnable @ 2);
assert_layout!(sys::NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN_PARAMS,
    size = 4, align = 4, workSubmitToken @ 0);

// Pushbuffer, semaphore and doorbell constants against the bindings.
const _: () = {
    assert!(sys::NVC46F_SEM_ADDR_LO as u32 == 0x5c);
    assert!(sys::NVC46F_SEM_ADDR_HI as u32 == 0x60);
    assert!(sys::NVC46F_SEM_PAYLOAD_LO as u32 == 0x64);
    assert!(sys::NVC46F_SEM_PAYLOAD_HI as u32 == 0x68);
    assert!(sys::NVC46F_SEM_EXECUTE as u32 == 0x6c);
    assert!(sys::NVC46F_SEM_EXECUTE_OPERATION_RELEASE as u32 == 1);
    assert!(sys::NVC46F_DMA_SEC_OP_INC_METHOD as u32 == 1);
    assert!(sys::NVC46F_GP_ENTRY__SIZE as u32 == 8);
    // Doorbell class is c461; register offsets come from c361.
    assert!(sys::TURING_USERMODE_A as u32 == 0xc461);
    assert!(sys::NVC361_NOTIFY_CHANNEL_PENDING as u32 == 0x90);
};

// Tests

#[cfg(test)]
mod drf_tests {
    use super::Drf;

    /// Header notation `hi:lo` must agree with gVisor's SHIFT/MASK pair.
    #[test]
    fn hi_lo_takes_the_shift_from_lo_and_the_width_from_the_span() {
        let f = Drf::hi_lo(25, 23); // NVOS33_FLAGS_CACHING_TYPE
        assert_eq!(f.shift, 23);
        assert_eq!(f.mask, 0x7);
        assert_eq!(f, Drf::shift_mask(23, 0x7), "both notations, one field");

        assert_eq!(Drf::hi_lo(11, 8).mask, 0xf); // NVOS02_FLAGS_LOCATION
        assert_eq!(Drf::hi_lo(11, 8).shift, 8);
    }

    /// Equal endpoints describe exactly one bit.
    #[test]
    fn a_single_bit_field_has_mask_one() {
        for bit in 0..32u32 {
            let f = Drf::hi_lo(bit, bit);
            assert_eq!(f.shift, bit);
            assert_eq!(f.mask, 1, "bit {bit}");
            assert_eq!(f.set(1), 1u32 << bit);
            assert_eq!(f.get(1u32 << bit), 1);
        }
    }

    /// A 32-bit width must avoid overflowing `1 << width`.
    #[test]
    fn a_full_width_field_masks_the_whole_word() {
        let f = Drf::hi_lo(31, 0);
        assert_eq!(f.shift, 0);
        assert_eq!(f.mask, u32::MAX);
        assert_eq!(f.set(0xdead_beef), 0xdead_beef);
        assert_eq!(f.get(0xdead_beef), 0xdead_beef);
        assert_eq!(f.put(0x0, 0xdead_beef), 0xdead_beef);
    }

    /// Mask wide values before shifting to preserve neighboring fields.
    #[test]
    fn set_masks_the_value_before_shifting_it() {
        let f = Drf::hi_lo(3, 2); // two bits
        assert_eq!(f.set(0b11), 0b1100);
        assert_eq!(
            f.set(0xffff_ffff),
            0b1100,
            "the value must be clipped, not shifted whole"
        );
        assert_eq!(
            f.set(0b100),
            0,
            "a value that is all overflow leaves nothing"
        );
    }

    /// Reading a written value returns its masked bits.
    #[test]
    fn get_of_set_returns_the_value_masked() {
        let fields = [
            Drf::hi_lo(1, 0),
            Drf::hi_lo(25, 23),
            Drf::hi_lo(31, 29),
            Drf::hi_lo(18, 18),
        ];
        for f in fields {
            for v in [0u32, 1, 2, 3, 7, 0x55, 0xffff_ffff] {
                assert_eq!(
                    f.get(f.set(v)),
                    v & f.mask,
                    "shift {} mask {:#x} v {v:#x}",
                    f.shift,
                    f.mask
                );
            }
        }
    }

    /// Replacing a field clears its old value and preserves other bits.
    #[test]
    fn put_replaces_only_its_own_field() {
        let f = Drf::hi_lo(25, 23); // CACHING_TYPE, 3 bits
        let word = 0xffff_ffffu32;
        let out = f.put(word, 0b010);
        assert_eq!(f.get(out), 0b010);
        // Every bit outside the field is untouched.
        let outside = !(f.mask << f.shift);
        assert_eq!(out & outside, word & outside);

        // Replacing a value must clear its old bits.
        let word = f.set(0b111);
        assert_eq!(f.get(f.put(word, 0b001)), 0b001);
        assert_eq!(f.put(word, 0), 0);
    }

    /// Reject inverted spans in handwritten field definitions.
    #[test]
    #[should_panic]
    fn hi_lo_rejects_an_inverted_span() {
        let _ = Drf::hi_lo(23, 25);
    }
}
