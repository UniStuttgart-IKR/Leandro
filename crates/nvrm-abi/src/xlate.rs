// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Forwarding descriptors shared by the host and guest table generator.
//!
//! RM frontend escapes use `_IOC` encoding; UVM uses raw request numbers.
//! `nvos.h` defines the RM parameter blocks. Layouts use bindgen types where
//! available; handwritten sizes retain their vendor-header derivations.
//! Unknown descriptors require refusal or a separate reviewed handling path.

use crate::sys;
use core::mem::{offset_of, size_of};
use sys::RmAbi;

/// Which class of device the call goes to. Decides the dispatch level:
/// the frontend (ctl/gpu) uses _IOC encoding, UVM does not.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Dev {
    Ctl,
    Gpu,
    Uvm,
    UvmTools,
}

impl Dev {
    pub fn is_uvm(self) -> bool {
        matches!(self, Dev::Uvm | Dev::UvmTools)
    }
}

// UVM command numbers: kernel-open/nvidia-uvm/{uvm_ioctl,uvm_linux_ioctl}.h.
// Raw numbers carry no encoded payload size.

pub mod uvm {
    pub const INITIALIZE: u32 = 0x3000_0001;
    pub const DEINITIALIZE: u32 = 0x3000_0002;
    pub const PAGEABLE_MEM_ACCESS: u32 = 39; // 0x27  (NO collision with
                                             // frontend 0x27; other device)
    pub const MM_INITIALIZE: u32 = 75; // 0x4b, carries UvmFD @ 0

    // The libcuda path (from the nvprobe lvl4 trace). Names and numbers from
    // kernel-open/nvidia-uvm/uvm_ioctl.h.
    pub const REGISTER_GPU_VASPACE: u32 = 25; // rmCtrlFd @ 16
    pub const UNREGISTER_GPU_VASPACE: u32 = 26;
    pub const REGISTER_CHANNEL: u32 = 27; // rmCtrlFd @ 16
    pub const UNREGISTER_CHANNEL: u32 = 28;
    pub const MAP_EXTERNAL_ALLOCATION: u32 = 33; // rmCtrlFd @ 9248 (after per-GPU array)
    pub const FREE: u32 = 34;
    pub const REGISTER_GPU: u32 = 37; // rmCtrlFd @ 24
    pub const MAP_DYNAMIC_PARALLELISM_REGION: u32 = 65;
    pub const ALLOC_SEMAPHORE_POOL: u32 = 68;
    // The managed-memory surface (managedprobe trace): libcuda calls these
    // from the first cuMemAllocManaged onwards.
    pub const SET_PREFERRED_LOCATION: u32 = 42;
    pub const UNSET_PREFERRED_LOCATION: u32 = 43;
    pub const ENABLE_READ_DUPLICATION: u32 = 44;
    pub const DISABLE_READ_DUPLICATION: u32 = 45;
    pub const SET_ACCESSED_BY: u32 = 46;
    pub const UNSET_ACCESSED_BY: u32 = 47;
    pub const MIGRATE: u32 = 51;
    pub const PAGEABLE_MEM_ACCESS_ON_GPU: u32 = 70;
    pub const VALIDATE_VA_RANGE: u32 = 72;
    pub const CREATE_EXTERNAL_RANGE: u32 = 73;
}

/// Reference UVM payload sizes, derived from the headers below.
/// Unknown commands return `None`; forwarding must not guess their size.
/// Use [`uvm_param_size_for`] for a selected driver ABI.
pub fn uvm_param_size(cmd: u32) -> Option<u32> {
    Some(match cmd {
        // UVM_INITIALIZE_PARAMS { Flags u64; RMStatus u32; Pad0[4] } = 16
        uvm::INITIALIZE => 16,
        // UVM_DEINITIALIZE takes no parameter struct.
        uvm::DEINITIALIZE => 0,
        // UVM_PAGEABLE_MEM_ACCESS_PARAMS { u8; Pad[3]; RMStatus u32 } = 8
        uvm::PAGEABLE_MEM_ACCESS => 8,
        // UVM_MM_INITIALIZE_PARAMS { UvmFD i32; Status u32 } = 8
        uvm::MM_INITIALIZE => 8,

        // --- libcuda path (nvprobe lvl4 trace). All sizes computed from uvm_ioctl.h; the
        // shared building blocks are:
        //   NvProcessorUuid = NvUuid { NvU8 uuid[16] } = 16   (nvCpuUuid.h:27-31)
        //   UvmGpuMappingAttributes = uuid(16) + 5*NvU32 = 36 (uvm_types.h:87-96)
        //   UVM_MAX_GPUS = NV_MAX_DEVICES(32, nvlimits.h:37)
        //                * UVM_PARENT_ID_MAX_SUB_PROCESSORS(8, uvm_types.h:50)
        //                = 256                                (uvm_types.h:57)
        //   NvBool = NvU8 (nvtypes.h:276), NV_STATUS = NvU32 (nvstatus.h:33)

        // UVM_REGISTER_GPU_VASPACE_PARAMS (uvm_ioctl.h:290-297)
        // { uuid 16; rmCtrlFd i32 @16; hClient @20; hVaSpace @24; rmStatus @28 } = 32
        uvm::REGISTER_GPU_VASPACE => 32,
        // UVM_UNREGISTER_GPU_VASPACE_PARAMS (uvm_ioctl.h:303-308)
        // { uuid 16; rmStatus @16 } = 20
        uvm::UNREGISTER_GPU_VASPACE => 20,
        // UVM_REGISTER_CHANNEL_PARAMS (uvm_ioctl.h:315-324)
        // { uuid 16; rmCtrlFd @16; hClient @20; hChannel @24; pad 4;
        //   base u64 @32 (NV_ALIGN_BYTES(8)); length u64 @40; rmStatus @48;
        //   tail pad 4 } = 56
        uvm::REGISTER_CHANNEL => 56,
        // UVM_UNREGISTER_CHANNEL_PARAMS (uvm_ioctl.h:331-336)
        // { hClient; hChannel; rmStatus } = 12
        uvm::UNREGISTER_CHANNEL => 12,
        // UVM_MAP_EXTERNAL_ALLOCATION_PARAMS (uvm_ioctl.h:365-378)
        // { base u64; length u64; offset u64 @16;
        //   perGpuAttributes[256] @24 = 9216 -> ends 9240;
        //   gpuAttributesCount u64 @9240; rmCtrlFd @9248; hClient @9252;
        //   hMemory @9256; rmStatus @9260 } = 9264
        uvm::MAP_EXTERNAL_ALLOCATION => 9264,
        // UVM_FREE_PARAMS (uvm_ioctl.h:384-388)
        // { base u64; rmStatus @8; tail pad 4 } = 16
        uvm::FREE => 16,
        // UVM_REGISTER_GPU_PARAMS (uvm_ioctl.h:405-416)
        // { uuid 16; numaEnabled NvBool @16; pad 3; numaNodeId i32 @20;
        //   rmCtrlFd @24; hClient @28; hSmcPartRef @32; rmStatus @36 } = 40
        uvm::REGISTER_GPU => 40,
        // UVM_MAP_DYNAMIC_PARALLELISM_REGION_PARAMS (uvm_ioctl.h:724-730)
        // { base u64; length u64; uuid 16 @16; rmStatus @32; tail pad 4 } = 40
        uvm::MAP_DYNAMIC_PARALLELISM_REGION => 40,
        // UVM_ALLOC_SEMAPHORE_POOL_PARAMS (uvm_ioctl.h:758-765)
        // { base u64; length u64; perGpuAttributes[256] @16 = 9216 -> ends
        //   9232; gpuAttributesCount u64 @9232; rmStatus @9240; tail pad 4 }
        //   = 9248
        uvm::ALLOC_SEMAPHORE_POOL => 9248,
        // UVM_PAGEABLE_MEM_ACCESS_ON_GPU_PARAMS (uvm_ioctl.h:782-786)
        // { uuid 16; pageableMemAccess NvBool @16; pad 3; rmStatus @20 } = 24
        uvm::PAGEABLE_MEM_ACCESS_ON_GPU => 24,
        // UVM_SET_PREFERRED_LOCATION_PARAMS (uvm_ioctl.h:440-449)
        // { requestedBase u64; length u64; preferredLocation uuid 16 @16;
        //   preferredCpuNumaNode i32 @32; rmStatus @36 } = 40
        uvm::SET_PREFERRED_LOCATION => 40,
        // UVM_UNSET_PREFERRED_LOCATION_PARAMS (uvm_ioctl.h:454-461)
        // { requestedBase u64; length u64; rmStatus @16; tail pad 4 } = 24
        uvm::UNSET_PREFERRED_LOCATION => 24,
        // UVM_ENABLE/DISABLE_READ_DUPLICATION_PARAMS (uvm_ioctl.h:466-485)
        // { requestedBase u64; length u64; rmStatus @16; tail pad 4 } = 24
        uvm::ENABLE_READ_DUPLICATION => 24,
        uvm::DISABLE_READ_DUPLICATION => 24,
        // UVM_SET/UNSET_ACCESSED_BY_PARAMS (uvm_ioctl.h:490-509)
        // { requestedBase u64; length u64; accessedByUuid 16 @16;
        //   rmStatus @32; tail pad 4 } = 40
        uvm::SET_ACCESSED_BY => 40,
        uvm::UNSET_ACCESSED_BY => 40,
        // UVM_MIGRATE_PARAMS (uvm_ioctl.h:602-615)
        // { base u64; length u64; uuid 16 @16; flags u32 @32; pad 4;
        //   semaphoreAddress u64 @40; semaphorePayload u32 @48;
        //   cpuNumaNode i32 @52; userSpaceStart u64 @56;
        //   userSpaceLength u64 @64; rmStatus @72; tail pad 4 } = 80
        uvm::MIGRATE => 80,
        // UVM_VALIDATE_VA_RANGE_PARAMS (uvm_ioctl.h:835-840)
        // { base u64; length u64; rmStatus @16; tail pad 4 } = 24
        uvm::VALIDATE_VA_RANGE => 24,
        // UVM_CREATE_EXTERNAL_RANGE_PARAMS (uvm_ioctl.h:843-848)
        // { base u64; length u64; rmStatus @16; tail pad 4 } = 24
        uvm::CREATE_EXTERNAL_RANGE => 24,
        _ => return None,
    })
}

/// UVM payload sizes from the selected ABI's bindgen types.
/// Compared with the handwritten reference table in tests and used by the
/// tracer. DEINITIALIZE has no struct; [`uvm_param_size_for`] returns its
/// zero-length payload explicitly.
pub fn uvm_param_size_compiled<A: RmAbi>(cmd: u32) -> Option<usize> {
    Some(match cmd {
        uvm::INITIALIZE => size_of::<sys::UVM_INITIALIZE_PARAMS>(),
        uvm::PAGEABLE_MEM_ACCESS => size_of::<sys::UVM_PAGEABLE_MEM_ACCESS_PARAMS>(),
        uvm::MM_INITIALIZE => size_of::<sys::UVM_MM_INITIALIZE_PARAMS>(),
        uvm::REGISTER_GPU_VASPACE => size_of::<sys::UVM_REGISTER_GPU_VASPACE_PARAMS>(),
        uvm::UNREGISTER_GPU_VASPACE => size_of::<sys::UVM_UNREGISTER_GPU_VASPACE_PARAMS>(),
        uvm::REGISTER_CHANNEL => size_of::<sys::UVM_REGISTER_CHANNEL_PARAMS>(),
        uvm::UNREGISTER_CHANNEL => size_of::<A::UvmUnregisterChannelParams>(),
        uvm::MAP_EXTERNAL_ALLOCATION => size_of::<sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS>(),
        uvm::FREE => size_of::<A::UvmFreeParams>(),
        uvm::REGISTER_GPU => size_of::<sys::UVM_REGISTER_GPU_PARAMS>(),
        uvm::MAP_DYNAMIC_PARALLELISM_REGION => {
            size_of::<sys::UVM_MAP_DYNAMIC_PARALLELISM_REGION_PARAMS>()
        }
        uvm::ALLOC_SEMAPHORE_POOL => size_of::<sys::UVM_ALLOC_SEMAPHORE_POOL_PARAMS>(),
        uvm::SET_PREFERRED_LOCATION => size_of::<sys::UVM_SET_PREFERRED_LOCATION_PARAMS>(),
        uvm::UNSET_PREFERRED_LOCATION => size_of::<sys::UVM_UNSET_PREFERRED_LOCATION_PARAMS>(),
        uvm::ENABLE_READ_DUPLICATION => size_of::<sys::UVM_ENABLE_READ_DUPLICATION_PARAMS>(),
        uvm::DISABLE_READ_DUPLICATION => size_of::<sys::UVM_DISABLE_READ_DUPLICATION_PARAMS>(),
        uvm::SET_ACCESSED_BY => size_of::<sys::UVM_SET_ACCESSED_BY_PARAMS>(),
        uvm::UNSET_ACCESSED_BY => size_of::<sys::UVM_UNSET_ACCESSED_BY_PARAMS>(),
        uvm::MIGRATE => size_of::<sys::UVM_MIGRATE_PARAMS>(),
        uvm::PAGEABLE_MEM_ACCESS_ON_GPU => size_of::<sys::UVM_PAGEABLE_MEM_ACCESS_ON_GPU_PARAMS>(),
        uvm::VALIDATE_VA_RANGE => size_of::<sys::UVM_VALIDATE_VA_RANGE_PARAMS>(),
        uvm::CREATE_EXTERNAL_RANGE => size_of::<sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS>(),
        _ => return None,
    })
}

/// Size selected for the host driver's ABI, including parameterless teardown.
pub fn uvm_param_size_for<A: RmAbi>(cmd: u32) -> Option<u32> {
    if cmd == uvm::DEINITIALIZE {
        Some(0)
    } else {
        uvm_param_size_compiled::<A>(cmd).and_then(|n| u32::try_from(n).ok())
    }
}

/// Reviewed frontend request envelopes. Nested controls/classes need separate rules.
#[derive(Clone, Copy, Debug)]
pub enum FrontendSize {
    Fixed(u32),
    Array(u32),
    Allocation,
}

impl FrontendSize {
    pub fn accepts(self, size: u32) -> bool {
        match self {
            Self::Fixed(n) => size == n,
            Self::Array(n) => size.is_multiple_of(n),
            Self::Allocation => {
                size == sz::<sys::NVOS21_PARAMETERS>() || size == sz::<sys::NVOS64_PARAMETERS>()
            }
        }
    }
}

/// Frontend envelopes supported by forwarding. Sources: escape.c and nv.c's
/// ioctl validation tables. Pointer-bearing exceptions are translated separately;
/// unhandled escapes, including raw XFER wrappers, are refused by the host.
pub fn frontend_size(nr: u32) -> Option<FrontendSize> {
    use crate::nvgpu;
    use FrontendSize::{Allocation, Array, Fixed};
    Some(match nr {
        sys::NV_ESC_RM_ALLOC => Allocation,
        sys::NV_ESC_RM_ALLOC_OBJECT => Fixed(sz::<sys::NVOS05_PARAMETERS>()),
        sys::NV_ESC_RM_FREE => Fixed(sz::<sys::NVOS00_PARAMETERS>()),
        sys::NV_ESC_RM_CONTROL => Fixed(sz::<sys::NVOS54_PARAMETERS>()),
        sys::NV_ESC_RM_ALLOC_MEMORY => Fixed(sz::<nvgpu::Nvos02WithFd>()),
        sys::NV_ESC_RM_MAP_MEMORY => Fixed(sz::<nvgpu::Nvos33WithFd>()),
        sys::NV_ESC_RM_UNMAP_MEMORY => Fixed(sz::<sys::NVOS34_PARAMETERS>()),
        sys::NV_ESC_RM_VID_HEAP_CONTROL => Fixed(sz::<sys::NVOS32_PARAMETERS>()),
        sys::NV_ESC_RM_ALLOC_CONTEXT_DMA2 => Fixed(sz::<sys::NVOS39_PARAMETERS>()),
        sys::NV_ESC_RM_BIND_CONTEXT_DMA => Fixed(sz::<sys::NVOS49_PARAMETERS>()),
        sys::NV_ESC_RM_MAP_MEMORY_DMA => Fixed(sz::<sys::NVOS46_PARAMETERS>()),
        sys::NV_ESC_RM_UNMAP_MEMORY_DMA => Fixed(sz::<sys::NVOS47_PARAMETERS>()),
        sys::NV_ESC_RM_DUP_OBJECT => Fixed(sz::<sys::NVOS55_PARAMETERS>()),
        sys::NV_ESC_RM_SHARE => Fixed(sz::<sys::NVOS57_PARAMETERS>()),
        sys::NV_ESC_RM_GET_EVENT_DATA => Fixed(sz::<sys::NVOS41_PARAMETERS>()),
        sys::NV_ESC_STATUS_CODE => Fixed(sz::<sys::nv_ioctl_status_code_t>()),
        nvgpu::NV_ESC_CARD_INFO => Array(sz::<sys::nv_ioctl_card_info_t>()),
        nvgpu::NV_ESC_ATTACH_GPUS_TO_FD => Array(sz::<u32>()),
        nvgpu::NV_ESC_REGISTER_FD => Fixed(sz::<nvgpu::IoctlRegisterFd>()),
        nvgpu::NV_ESC_ALLOC_OS_EVENT => Fixed(sz::<nvgpu::IoctlAllocOsEvent>()),
        nvgpu::NV_ESC_FREE_OS_EVENT => Fixed(sz::<nvgpu::IoctlFreeOsEvent>()),
        nvgpu::NV_ESC_CHECK_VERSION_STR => Fixed(sz::<sys::nv_ioctl_rm_api_version_t>()),
        nvgpu::NV_ESC_SYS_PARAMS => Fixed(sz::<sys::nv_ioctl_sys_params_t>()),
        nvgpu::NV_ESC_NUMA_INFO => Fixed(sz::<nvgpu::IoctlNumaInfo>()),
        nvgpu::NV_ESC_WAIT_OPEN_COMPLETE => Fixed(sz::<sys::nv_ioctl_wait_open_complete_t>()),
        // NV_ESC_QUERY_DEVICE_INTR, nv-ioctl-numbers.h.
        213 => Fixed(sz::<sys::nv_ioctl_query_device_intr>()),
        _ => return None,
    })
}

/// Allocation payload sizes from bindgen, or `None` for unbound classes.
/// The tracer uses these lengths independently of the forwarding size table;
/// a class without a compiled layout gets no parameter dump.
pub fn alloc_param_size_compiled<A: RmAbi>(hclass: u32) -> Option<usize> {
    Some(match hclass {
        0x003e | 0x0040 | 0x50a0 | 0x90ce => size_of::<sys::NV_MEMORY_ALLOCATION_PARAMS>(),
        0x0071 => size_of::<sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS>(),
        0x0080 => size_of::<sys::NV0080_ALLOC_PARAMETERS>(),
        0x90f1 => size_of::<sys::NV_VASPACE_ALLOCATION_PARAMETERS>(),
        0xa06c => size_of::<A::TsgParams>(),
        0x9067 => size_of::<sys::NV_CTXSHARE_ALLOCATION_PARAMETERS>(),
        0x906f | 0xa06f | 0xa16f | 0xb06f | 0xc06f | 0xc36f | 0xc46f | 0xc56f | 0xc86f | 0xc96f
        | 0xca6f => size_of::<A::AllocChannelParams>(),
        0x902d | 0xa140 | 0xc597 | 0xc5c0 | 0xc697 | 0xc6c0 | 0xc797 | 0xc7c0 | 0xc997 | 0xc9c0
        | 0xcb97 | 0xcbc0 | 0xcd40 | 0xcd97 | 0xcdc0 | 0xce97 | 0xcec0 => {
            size_of::<sys::NV_GR_ALLOCATION_PARAMETERS>()
        }
        0xb8b0 | 0xc4b0 | 0xc6b0 | 0xc7b0 | 0xc9b0 | 0xcdb0 | 0xceb0 | 0xcfb0 | 0xd1b0 | 0xd2b0 => {
            size_of::<sys::NV_NVDEC_ALLOCATION_PARAMETERS>()
        }
        0xb4b7 | 0xc4b7 | 0xc7b7 | 0xc9b7 | 0xceb7 | 0xcfb7 | 0xd1b7 => {
            size_of::<sys::NV_NVENC_ALLOCATION_PARAMETERS>()
        }
        0xb8fa | 0xc6fa | 0xc7fa | 0xc9fa | 0xcdfa | 0xcefa | 0xcffa | 0xd1fa | 0xd2fa => {
            size_of::<sys::NV_OFA_ALLOCATION_PARAMETERS>()
        }
        0xb8d1 | 0xc4d1 | 0xc9d1 | 0xcdd1 | 0xced0 | 0xcfd1 | 0xd2d1 => {
            size_of::<sys::NV_NVJPG_ALLOCATION_PARAMETERS>()
        }
        0x0002 => size_of::<sys::NV_CONTEXT_DMA_ALLOCATION_PARAMS>(),
        0x0005 | 0x0078 | 0x0079 | 0x007e => size_of::<sys::NV0005_ALLOC_PARAMETERS>(),
        0x2080 => size_of::<sys::NV2080_ALLOC_PARAMETERS>(),
        0xc661 | 0xc761 => size_of::<sys::NV_HOPPER_USERMODE_A_PARAMS>(),
        0xc763 | 0xc863 => size_of::<sys::NV_VIDMEM_ACCESS_BIT_ALLOCATION_PARAMS>(),
        0xb0b5 | 0xc0b5 | 0xc5b5 | 0xc6b5 | 0xc7b5 | 0xc8b5 | 0xc9b5 | 0xcab5 => {
            size_of::<sys::NVB0B5_ALLOCATION_PARAMETERS>()
        }
        0x0070 => size_of::<sys::NV_MEMORY_VIRTUAL_ALLOCATION_PARAMS>(),
        0x9072 => size_of::<sys::NV9072_ALLOCATION_PARAMETERS>(),
        0x2081 => size_of::<sys::NV2081_ALLOC_PARAMETERS>(),
        0x00fe => size_of::<sys::NV_MEMORY_MAPPER_ALLOCATION_PARAMS>(),
        0x00de => size_of::<sys::NV00DE_ALLOC_PARAMETERS>(),
        0xcb33 => size_of::<sys::NV_CONFIDENTIAL_COMPUTE_ALLOC_PARAMS>(),
        0x83de => size_of::<sys::NV83DE_ALLOC_PARAMETERS>(),
        0x00da => size_of::<sys::NV_SEMAPHORE_SURFACE_ALLOC_PARAMETERS>(),
        0xc640 => size_of::<sys::NVC640_ALLOCATION_PARAMETERS>(),
        0xa0bc => size_of::<sys::NVA0BC_ALLOC_PARAMETERS>(),
        0x00c2 => size_of::<sys::NV_PHYSICAL_MEMORY_ALLOCATION_PARAMS>(),
        0x00c3 => size_of::<sys::NV_MEMORY_SYNCPOINT_ALLOCATION_PARAMS>(),
        _ => return None,
    })
}

/// Nested pointer/count offsets from bindgen for trace capture.
/// The tracer must follow these pointers to compare answer data, not just
/// its enclosing count/pointer fields. Tests compare the compiled offsets
/// with the forwarding descriptors.
///
/// Element types use sizeof where available, but count units still require
/// header review: identical layouts can count bytes (GET_CAPS) or entries
/// (GET_INFO). Matching tables cannot independently validate those semantics.
pub fn ctrl_nested_compiled(cmd: u32) -> &'static [(usize, usize, u32)] {
    /// `NVXXXX_CTRL_XXX_INFO { index; data }`, the element of every
    /// `...InfoList` below (ctrlxxxx.h:71).
    const INFO: u32 = size_of::<sys::NVXXXX_CTRL_XXX_INFO>() as u32;
    match cmd {
        // Three string buffers, one shared size, in BYTES.
        0x101 => &[
            (
                offset_of!(
                    sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS,
                    pDriverVersionBuffer
                ),
                offset_of!(
                    sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS,
                    sizeOfStrings
                ),
                1,
            ),
            (
                offset_of!(
                    sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS,
                    pVersionBuffer
                ),
                offset_of!(
                    sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS,
                    sizeOfStrings
                ),
                1,
            ),
            (
                offset_of!(
                    sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS,
                    pTitleBuffer
                ),
                offset_of!(
                    sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS,
                    sizeOfStrings
                ),
                1,
            ),
        ],
        // Entry lists: the count is a number of NVXXXX_CTRL_XXX_INFO.
        0x20800802 => &[(
            offset_of!(sys::NV2080_CTRL_BIOS_GET_INFO_PARAMS, biosInfoList),
            offset_of!(sys::NV2080_CTRL_BIOS_GET_INFO_PARAMS, biosInfoListSize),
            INFO,
        )],
        0x20801201 => &[(
            offset_of!(sys::NV2080_CTRL_GR_GET_INFO_PARAMS, grInfoList),
            offset_of!(sys::NV2080_CTRL_GR_GET_INFO_PARAMS, grInfoListSize),
            INFO,
        )],
        0x20801301 => &[(
            offset_of!(sys::NV2080_CTRL_FB_GET_INFO_PARAMS, fbInfoList),
            offset_of!(sys::NV2080_CTRL_FB_GET_INFO_PARAMS, fbInfoListSize),
            INFO,
        )],
        0x20801802 => &[(
            offset_of!(sys::NV2080_CTRL_BUS_GET_INFO_PARAMS, busInfoList),
            offset_of!(sys::NV2080_CTRL_BUS_GET_INFO_PARAMS, busInfoListSize),
            INFO,
        )],
        0x801104 => &[(
            offset_of!(sys::NV0080_CTRL_GR_GET_INFO_PARAMS, grInfoList),
            offset_of!(sys::NV0080_CTRL_GR_GET_INFO_PARAMS, grInfoListSize),
            INFO,
        )],
        // NvU32 arrays: the count is a number of 4-byte items.
        0x20800123 => &[(
            offset_of!(sys::NV2080_CTRL_GPU_GET_ENGINES_PARAMS, engineList),
            offset_of!(sys::NV2080_CTRL_GPU_GET_ENGINES_PARAMS, engineCount),
            4,
        )],
        0x800201 => &[(
            offset_of!(sys::NV0080_CTRL_GPU_GET_CLASSLIST_PARAMS, classList),
            offset_of!(sys::NV0080_CTRL_GPU_GET_CLASSLIST_PARAMS, numClasses),
            4,
        )],
        0x80170d => &[
            (
                offset_of!(
                    sys::NV0080_CTRL_FIFO_GET_CHANNELLIST_PARAMS,
                    pChannelHandleList
                ),
                offset_of!(sys::NV0080_CTRL_FIFO_GET_CHANNELLIST_PARAMS, numChannels),
                4,
            ),
            (
                offset_of!(sys::NV0080_CTRL_FIFO_GET_CHANNELLIST_PARAMS, pChannelList),
                offset_of!(sys::NV0080_CTRL_FIFO_GET_CHANNELLIST_PARAMS, numChannels),
                4,
            ),
        ],
        // Caps tables: the count is in BYTES, however identical the struct.
        0x801102 => &[(
            offset_of!(sys::NV0080_CTRL_GR_GET_CAPS_PARAMS, capsTbl),
            offset_of!(sys::NV0080_CTRL_GR_GET_CAPS_PARAMS, capsTblSize),
            1,
        )],
        0x801301 => &[(
            offset_of!(sys::NV0080_CTRL_FB_GET_CAPS_PARAMS, capsTbl),
            offset_of!(sys::NV0080_CTRL_FB_GET_CAPS_PARAMS, capsTblSize),
            1,
        )],
        0x801701 => &[(
            offset_of!(sys::NV0080_CTRL_FIFO_GET_CAPS_PARAMS, capsTbl),
            offset_of!(sys::NV0080_CTRL_FIFO_GET_CAPS_PARAMS, capsTblSize),
            1,
        )],
        0x801b01 => &[(
            offset_of!(sys::NV0080_CTRL_NVENC_GET_CAPS_PARAMS, capsTbl),
            offset_of!(sys::NV0080_CTRL_NVENC_GET_CAPS_PARAMS, capsTblSize),
            1,
        )],
        _ => &[],
    }
}

#[cfg(test)]
mod ctrl_nested_tests {
    use super::*;

    /// Compare forwarding pointer/count offsets with bindgen.
    /// Element-size agreement is checked too, but byte-vs-entry count semantics
    /// remain derived from header documentation.
    #[test]
    fn the_nested_pointer_offsets_are_what_the_compiler_measures() {
        let mut checked = 0;
        for cmd in nested_cmds() {
            let compiled = ctrl_nested_compiled(*cmd);
            if compiled.is_empty() {
                continue; // no bindgen struct for it; nothing to check
            }
            let table = nested_ptrs(*cmd);
            assert_eq!(
                compiled.len(),
                table.len(),
                "command {cmd:#x}: {} compiled pointers against {} in the table",
                compiled.len(),
                table.len()
            );
            for (i, (ptr_off, len_off, elem)) in compiled.iter().enumerate() {
                assert_eq!(
                    table[i].ptr_off as usize, *ptr_off,
                    "command {cmd:#x} pointer {i}: table says +{}, the compiler +{ptr_off}",
                    table[i].ptr_off
                );
                match table[i].len {
                    LenSource::Field { off, elem: e } => {
                        assert_eq!(
                            off as usize, *len_off,
                            "command {cmd:#x} pointer {i}: count at +{off} against +{len_off}"
                        );
                        assert_eq!(
                            e, *elem,
                            "command {cmd:#x} pointer {i}: elem {e} against {elem}"
                        );
                    }
                    _ => panic!("command {cmd:#x} pointer {i}: not a Field length"),
                }
                checked += 1;
            }
        }
        assert!(checked >= 15, "only {checked} pointers checked");
    }
}

#[cfg(test)]
mod uvm_size_tests {
    use super::*;

    /// Cross-check allocation sizes against bindgen where types are available.
    /// A wrong shared size can truncate or over-read identically on both sides,
    /// so matching native/guest traces alone cannot validate it. Remaining sizes
    /// are checked by the C `class-sizes` test against their cited headers.
    #[test]
    fn the_hand_computed_allocation_sizes_are_what_the_compiler_measures() {
        macro_rules! check {
            ($t:ty, $($hclass:expr),+) => {{
                $(
                    let hand = alloc_param_size::<sys::DefaultAbi>($hclass)
                        .expect("class is in the hand-written table");
                    assert_eq!(
                        hand as usize, size_of::<$t>(),
                        "class {:#x}: the table says {} bytes, the compiler {}",
                        $hclass, hand, size_of::<$t>()
                    );
                )+
            }};
        }
        // Graphics/compute, one struct for every architecture (nvos.h:2724).
        check!(
            sys::NV_GR_ALLOCATION_PARAMETERS,
            0xc5c0u32,
            0x902d,
            0xa140,
            0xc597,
            0xc6c0,
            0xc797,
            0xc9c0,
            0xcdc0
        );
        // Video engines. NV_BSP_/NV_MSENC_ are #defines onto these.
        check!(
            sys::NV_NVDEC_ALLOCATION_PARAMETERS,
            0xc4b0u32,
            0xb8b0,
            0xc6b0,
            0xd2b0
        );
        check!(
            sys::NV_NVENC_ALLOCATION_PARAMETERS,
            0xc4b7u32,
            0xb4b7,
            0xc7b7,
            0xd1b7
        );
        check!(sys::NV_OFA_ALLOCATION_PARAMETERS, 0xb8fau32, 0xc6fa, 0xd2fa);
        check!(
            sys::NV_NVJPG_ALLOCATION_PARAMETERS,
            0xb8d1u32,
            0xc4d1,
            0xd2d1
        );
        // NV01_CONTEXT_DMA (nvos.h:1594).
        check!(sys::NV_CONTEXT_DMA_ALLOCATION_PARAMS, 0x0002u32);
        // Hopper/Blackwell USERMODE take an optional params struct where
        // Volta/Turing/Ampere take none (nvos.h:3327).
        check!(sys::NV_HOPPER_USERMODE_A_PARAMS, 0xc661u32, 0xc761);
        // MMU access-bit buffer (nvos.h:3310).
        check!(
            sys::NV_VIDMEM_ACCESS_BIT_ALLOCATION_PARAMS,
            0xc763u32,
            0xc863
        );
        // The event classes, which share NV01_EVENT_OS_EVENT's struct.
        check!(
            sys::NV0005_ALLOC_PARAMETERS,
            0x0079u32,
            0x0005,
            0x0078,
            0x007e
        );
        // NV20_SUBDEVICE_0 (cl2080.h).
        check!(sys::NV2080_ALLOC_PARAMETERS, 0x2080u32);
        // The classes whose params live in their own class header.
        check!(
            sys::NVB0B5_ALLOCATION_PARAMETERS,
            0xc5b5u32,
            0xb0b5,
            0xc0b5,
            0xc6b5,
            0xc7b5,
            0xc8b5,
            0xc9b5,
            0xcab5
        );
        check!(sys::NV_MEMORY_VIRTUAL_ALLOCATION_PARAMS, 0x0070u32);
        check!(sys::NV9072_ALLOCATION_PARAMETERS, 0x9072u32);
        check!(sys::NV2081_ALLOC_PARAMETERS, 0x2081u32);
        check!(sys::NV_MEMORY_MAPPER_ALLOCATION_PARAMS, 0x00feu32);
        check!(sys::NV00DE_ALLOC_PARAMETERS, 0x00deu32);
        check!(sys::NV_CONFIDENTIAL_COMPUTE_ALLOC_PARAMS, 0xcb33u32);
        check!(sys::NV83DE_ALLOC_PARAMETERS, 0x83deu32);
        check!(sys::NV_SEMAPHORE_SURFACE_ALLOC_PARAMETERS, 0x00dau32);
        check!(sys::NVC640_ALLOCATION_PARAMETERS, 0xc640u32);
        check!(sys::NVA0BC_ALLOC_PARAMETERS, 0xa0bcu32);
        check!(sys::NV_PHYSICAL_MEMORY_ALLOCATION_PARAMS, 0x00c2u32);
        check!(sys::NV_MEMORY_SYNCPOINT_ALLOCATION_PARAMS, 0x00c3u32);
    }

    /// Compare reference UVM sizes with bindgen. Matching guest/host traces
    /// cannot detect a wrong size shared by both sides.
    #[test]
    fn the_hand_computed_uvm_sizes_are_what_the_compiler_measures() {
        let cmds = [
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
            uvm::SET_PREFERRED_LOCATION,
            uvm::UNSET_PREFERRED_LOCATION,
            uvm::ENABLE_READ_DUPLICATION,
            uvm::DISABLE_READ_DUPLICATION,
            uvm::SET_ACCESSED_BY,
            uvm::UNSET_ACCESSED_BY,
            uvm::MIGRATE,
            uvm::PAGEABLE_MEM_ACCESS_ON_GPU,
            uvm::VALIDATE_VA_RANGE,
            uvm::CREATE_EXTERNAL_RANGE,
        ];
        for cmd in cmds {
            let hand = uvm_param_size(cmd).expect("in the hand-written table");
            let compiled = uvm_param_size_compiled::<sys::DefaultAbi>(cmd).expect("has a struct");
            assert_eq!(
                hand as usize, compiled,
                "UVM command {cmd:#x}: the table says {hand} bytes, the compiler {compiled}"
            );
        }
        assert_eq!(
            cmds.len(),
            22,
            "every command with a compiled struct is checked"
        );
        // The one command that genuinely has no parameter block.
        assert_eq!(uvm_param_size(uvm::DEINITIALIZE), Some(0));
        assert_eq!(
            uvm_param_size_compiled::<sys::DefaultAbi>(uvm::DEINITIALIZE),
            None
        );
    }
}

// fd field offset  (the only value translation besides the aux pointer)

/// Known inline FD offsets. Frontend requests include REGISTER_FD in
/// addition to gVisor's four HasFrontendFD cases. `None` means no registered
/// FD translation; it does not establish that an unknown request is flat.
pub fn fd_field_offset(dev: Dev, nr: u32, _size: u32) -> Option<u32> {
    if dev.is_uvm() {
        return Some(match nr {
            uvm::MM_INITIALIZE => 0, // UvmFD @ 0
            // The rmCtrlFd fields below carry the process-local RM ctl FD;
            // offsets verified against the structs in uvm_ioctl.h (see the
            // size entries in uvm_param_size for the field-by-field layout).
            uvm::REGISTER_GPU_VASPACE => 16, // uvm_ioctl.h:290-297
            uvm::REGISTER_CHANNEL => 16,     // uvm_ioctl.h:315-324
            uvm::REGISTER_GPU => 24,         // uvm_ioctl.h:405-416
            // rmCtrlFd sits AFTER perGpuAttributes[UVM_MAX_GPUS]:
            // 24 + 256*36 + 8 (gpuAttributesCount) = 9248 (uvm_ioctl.h:365-378)
            uvm::MAP_EXTERNAL_ALLOCATION => 9248,
            _ => return None,
        });
    }
    // Frontend (ctl/gpu):
    Some(match nr {
        // REGISTER_FD: ctl_fd i32 @0. gVisor handles this separately from
        // HasFrontendFD; the vectorAdd trace used it after each GPU-node open.
        x if x == crate::nvgpu::NV_ESC_REGISTER_FD => 0,

        // IoctlAllocOSEvent/FreeOSEvent { HClient, HDevice, FD u32 @ 8, Status }
        x if x == nvgpu_alloc_os_event() || x == nvgpu_free_os_event() => 8,
        // nv_ioctl_nvos33_parameters_with_fd: NVOS33(48) + fd @ 48
        x if x == sys::NV_ESC_RM_MAP_MEMORY => 48,
        // nv_ioctl_nvos02_parameters_with_fd: NVOS02(48) + fd @ 48
        x if x == sys::NV_ESC_RM_ALLOC_MEMORY => 48,
        _ => return None,
    })
}

#[inline]
fn nvgpu_alloc_os_event() -> u32 {
    crate::nvgpu::NV_ESC_ALLOC_OS_EVENT
}
#[inline]
fn nvgpu_free_os_event() -> u32 {
    crate::nvgpu::NV_ESC_FREE_OS_EVENT
}

// Embedded pointer  (RM_CONTROL / RM_ALLOC)

/// Description of an embedded pointer that points at a second buffer,
/// which has to travel across the boundary as well.
pub struct Embedded {
    /// Byte offset of the P64 pointer field in the inline struct.
    pub ptr_off: u32,
    /// Length of the target buffer in bytes.
    pub len: u32,
}

/// Describe a known primary embedded pointer in the inline payload.
/// `Ok(None)` means no translation descriptor was selected; it does not
/// validate the complete request. `Err(())` means an unknown allocation
/// layout or unsupported rights pointer.
///
/// # Safety
/// `buf` must be readable and initialized for at least `size` bytes.
#[allow(clippy::result_unit_err)] // Err(()) means "not determinable"; the caller maps it to ENOTSUP
pub unsafe fn embedded_ptr<A: RmAbi>(
    dev: Dev,
    nr: u32,
    buf: *const u8,
    size: u32,
) -> Result<Option<Embedded>, ()> {
    if dev.is_uvm() {
        // All UVM params known so far are flat: the big MAP_EXTERNAL_-
        // ALLOCATION/ALLOC_SEMAPHORE_POOL attribute arrays are inline,
        // not pointed to.
        return Ok(None);
    }
    let rd32 = |off: u32| -> u32 { core::ptr::read_unaligned(buf.add(off as usize) as *const u32) };
    match nr {
        // NVOS54: params P64 @ 16, paramsSize u32 @ 24. Self-describing.
        sys::NV_ESC_RM_CONTROL if size >= 32 => {
            let plen = rd32(24);
            let pptr = core::ptr::read_unaligned(buf.add(16) as *const u64);
            if pptr == 0 || plen == 0 {
                Ok(None) // a control with no params, and many have none
            } else {
                Ok(Some(Embedded {
                    ptr_off: 16,
                    len: plen,
                }))
            }
        }
        // RM_ALLOC accepts NVOS64 (48 bytes) and NVOS21 (32), escape.c:325.
        // Both use hClass @12 and pAllocParms @16 (ct_assert at :286).
        // Byte 24 is pRightsRequested in NVOS64, paramsSize in NVOS21.
        sys::NV_ESC_RM_ALLOC if size == 48 || size == 32 => {
            let pptr = core::ptr::read_unaligned(buf.add(16) as *const u64);
            if pptr == 0 {
                return Ok(None); // class with no params (ROOT_CLIENT, USERMODE)
            }
            if size == 48 {
                // Non-null rights pointers have no translation descriptor.
                let rights = core::ptr::read_unaligned(buf.add(24) as *const u64);
                if rights != 0 {
                    return Err(());
                }
            }
            let hclass = rd32(12);
            match alloc_param_size::<A>(hclass) {
                Some(len) => Ok(Some(Embedded { ptr_off: 16, len })),
                None => Err(()), // unknown hClass -> do not guess
            }
        }
        // NVOS41: pEvent P64 @0 -> exactly one NvUnixEvent, written by RM
        // (osapi.c:504-535). A NULL pointer is RM's problem (it answers
        // with a status), not a reason to guess.
        sys::NV_ESC_RM_GET_EVENT_DATA if size as usize >= size_of::<sys::NVOS41_PARAMETERS>() => {
            let pptr = core::ptr::read_unaligned(buf as *const u64);
            if pptr == 0 {
                Ok(None)
            } else {
                Ok(Some(Embedded {
                    ptr_off: 0,
                    len: size_of::<sys::NvUnixEvent>() as u32,
                }))
            }
        }
        _ => Ok(None),
    }
}

/// `size_of` as a `u32`, for the table below.
#[inline]
const fn sz<T>() -> u32 {
    core::mem::size_of::<T>() as u32
}

/// Classes whose capability FDs have no guest translation.
pub fn alloc_class_blocked(hclass: u32) -> bool {
    matches!(
        hclass,
        0xc637 | 0xc638 | 0xc639 | 0xc640 | 0xb0cd | 0xb0ce | 0xcdcd
    )
}

/// Classes exercised by hardware gates, including historical refusals.
/// `KF_UNVERIFIED` marks other classes; this is coverage, not a safety policy.
/// The host's OSdesc pool path covers 0x71. Promotion requires a gate on the
/// relevant architecture; blocked classes remain refused.
pub fn alloc_class_verified(hclass: u32) -> bool {
    matches!(
        hclass,
        0x003e
            | 0x0040
            | 0x0071
            | 0x0079
            | 0x0080
            | 0x00de
            | 0x2080
            | 0x2081
            | 0x50a0
            | 0x83de
            | 0x9067
            | 0x90f1
            | 0xa06c
            | 0xc46f
            | 0xc5b5
            | 0xc5c0
            | 0xc640
            | 0xcb33
            // Video classes observed with NV_OK in the 2026-08-06 h264_nvenc and
            // CUDA decode runs; covered by the GPU encode gate.
            | 0x0002 // NV01_CONTEXT_DMA
            | 0x0041 // NV01_ROOT_USER
            | 0x0070 // NV01_MEMORY_SYSTEM_DYNAMIC
            | 0xa0bc // NVENC_SW_SESSION
            // Parameterless USERMODE classes have no allocation-size table row.
            | 0xc361 // VOLTA_USERMODE_A (clc361.h)
            | 0xc461 // TURING_USERMODE_A (already the doorbell path, now traced here too)
            | 0xc4b0 // NVC4B0_VIDEO_DECODER  -- NVDEC, Turing
            | 0xc4b7 // NVC4B7_VIDEO_ENCODER  -- NVENC, Turing
    )
}

/// Allocation class to parameter size (`rmapi/resource_list.h` RS_ENTRY).
///
/// Non-null parameters need a known layout. Bindgen types supply sizes where
/// available; `tools/check.sh` compares handwritten rows with C sizeof.
/// Incorrect sizes previously affected 0x90f1 (48 vs 56, missing pasid) and
/// 0x71 (128 vs 40, separate OSdesc type).
///
/// RS_NONE classes need no row when pAllocParms is NULL: 0x73, 0x90e7,
/// 0x9096 and Volta/Turing/Ampere USERMODE. Hopper/Blackwell USERMODE instead
/// have optional parameters. The 2026-08-22 coverage run observed 35
/// successful parameterless 0x9096 allocations (OPEN-QUESTIONS 59).
///
/// This table establishes copy size only. It does not authorize a class or
/// prove all pointer/FD fields are translated; the host also validates shape
/// and policy. Hardware coverage is recorded by [`alloc_class_verified`].
pub fn alloc_param_size<A: RmAbi>(hclass: u32) -> Option<u32> {
    Some(match hclass {
        // --- derived from the bindgen types (the vendor tree governs) ---
        0x0080 => sz::<sys::NV0080_ALLOC_PARAMETERS>(), // NV01_DEVICE_0
        0x90f1 => sz::<sys::NV_VASPACE_ALLOCATION_PARAMETERS>(), // FERMI_VASPACE_A
        0xa06c => sz::<A::TsgParams>(),                 // TSG
        0x9067 => sz::<sys::NV_CTXSHARE_ALLOCATION_PARAMETERS>(),
        0xc46f => sz::<A::AllocChannelParams>(), // this version's layout

        // --- by hand, because not in the bindgen allowlist ---
        0x2080 => 4,  // NV2080_ALLOC_PARAMETERS { subDeviceId } (cl2080.h)
        0x2081 => 4,  // NV2081_ALLOC_PARAMETERS { reserved } (cl2081.h:38)
        0x0079 => 24, // NV0005_ALLOC_PARAMETERS (cl0005.h:40-47):
        // hParentClient@0, hSrcResource@4, hClass@8,
        // notifyIndex@12, data P64 @16 -> 24
        // capDescriptor is a Unix FD (clc640.h); alloc_class_blocked refuses
        // this class until capability FDs can be translated.
        0xc640 => 8, // AMPERE_SMC_MONITOR_SESSION
        // NV_MEMORY_ALLOCATION_PARAMS for the ordinary memory classes
        // (resource_list.h:574, :542, :563).
        0x003e | 0x0040 | 0x50a0 => sz::<sys::NV_MEMORY_ALLOCATION_PARAMS>(),
        // NV01_MEMORY_SYSTEM_OS_DESCRIPTOR uses a struct of its OWN, much
        // smaller (resource_list.h:605); not the same as above.
        0x0071 => sz::<sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS>(),

        // --- libcuda path (nvprobe lvl4 trace). Class -> param struct
        // mapping confirmed in src/nvidia/src/kernel/rmapi/resource_list.h.

        // RM_USER_SHARED_DATA: NV00DE_ALLOC_PARAMETERS { polledDataMask u64 }
        // = 8 (cl00de.h:503-505, resource_list.h:880)
        0x00de => 8,
        // GT200_DEBUGGER: NV83DE_ALLOC_PARAMETERS { hDebuggerClient_Obsolete;
        // hAppClient; hClass3dObject } = 12 (cl83de.h:51-56, resource_list.h:192)
        0x83de => 12,
        // TURING_DMA_COPY_A: NVB0B5_ALLOCATION_PARAMETERS { version u32;
        // engineType u32 } = 8 (clb0b5sw.h:50-53, resource_list.h:1688)
        0xc5b5 => 8,
        // TURING_COMPUTE_A: NV_GR_ALLOCATION_PARAMETERS { version; flags;
        // size; caps } = 16 (nvos.h:2724-2729, resource_list.h:2299)
        0xc5c0 => 16,
        // NV_CONFIDENTIAL_COMPUTE: NV_CONFIDENTIAL_COMPUTE_ALLOC_PARAMS
        // { hClient } = 4 (clcb33.h:37-39, resource_list.h:2365)
        0xcb33 => 4,

        // Additional header-derived classes. Hardware coverage is recorded by
        // alloc_class_verified, not by row order; promoted entries keep their
        // original header citations.

        // NV_GR_ALLOCATION_PARAMETERS (nvos.h:2729) = 16. Graphics/compute class
        // of every architecture. Turing (0xc5c0) above is the verified one; these
        // are its siblings.
        // 16 classes, resource_list.h from :2121
        0x902d | 0xa140 | 0xc597 | 0xc697 | 0xc6c0 | 0xc797 | 0xc7c0 | 0xc997 | 0xc9c0 | 0xcb97
        | 0xcbc0 | 0xcd40 | 0xcd97 | 0xcdc0 | 0xce97 | 0xcec0 => 16,

        // NV_BSP_ALLOCATION_PARAMETERS (nvos.h:2945) = 12. NVDEC video decoder.
        // Alias of NV_NVDEC_ALLOCATION_PARAMETERS.
        // 10 classes, resource_list.h from :1748
        0xb8b0 | 0xc4b0 | 0xc6b0 | 0xc7b0 | 0xc9b0 | 0xcdb0 | 0xceb0 | 0xcfb0 | 0xd1b0 | 0xd2b0 => {
            12
        }

        // NV_CHANNEL_ALLOC_PARAMS (alloc/alloc_channel.h:347) = 376. GPFIFO
        // channel of every architecture; one struct for all of them, so the
        // verified Turing entry (0xc46f) fixes the size for the rest. Without
        // these the FIRST channel allocation on a non-Turing card fails.
        // 10 classes, resource_list.h from :315
        0x906f | 0xa06f | 0xa16f | 0xb06f | 0xc06f | 0xc36f | 0xc56f | 0xc86f | 0xc96f | 0xca6f => {
            sz::<A::AllocChannelParams>()
        }

        // NV_OFA_ALLOCATION_PARAMETERS (nvos.h:3014) = 12. Optical flow
        // accelerator.
        // 9 classes, resource_list.h from :1935
        0xb8fa | 0xc6fa | 0xc7fa | 0xc9fa | 0xcdfa | 0xcefa | 0xcffa | 0xd1fa | 0xd2fa => 12,

        // NVB0B5_ALLOCATION_PARAMETERS (class/clb0b5sw.h:53) = 8. Copy engine of
        // every architecture; Turing (0xc5b5) above is verified.
        // 7 classes, resource_list.h from :1659
        0xb0b5 | 0xc0b5 | 0xc6b5 | 0xc7b5 | 0xc8b5 | 0xc9b5 | 0xcab5 => 8,

        // NV_MSENC_ALLOCATION_PARAMETERS (nvos.h:2994) = 12. NVENC video encoder.
        // Alias of NV_NVENC_ALLOCATION_PARAMETERS.
        // 7 classes, resource_list.h from :2034
        0xb4b7 | 0xc4b7 | 0xc7b7 | 0xc9b7 | 0xceb7 | 0xcfb7 | 0xd1b7 => 12,

        // NV_NVJPG_ALLOCATION_PARAMETERS (nvos.h:3007) = 12. JPEG engine.
        // 7 classes, resource_list.h from :1858
        0xb8d1 | 0xc4d1 | 0xc9d1 | 0xcdd1 | 0xced0 | 0xcfd1 | 0xd2d1 => 12,

        // NvHandle (nvtypes.h:263) = 4. RS_OPTIONAL(NvHandle): a bare client
        // handle. pAllocParms is normally NULL for these and embedded_ptr bails
        // out earlier; the entry only covers the case where a client does pass
        // one.
        // 4 classes, resource_list.h from :63
        0x0000 | 0x0001 | 0x0020 | 0x0041 => 4,

        // NV0005_ALLOC_PARAMETERS (class/cl0005.h:47) = 24. Event classes sharing
        // NV01_EVENT_OS_EVENT's struct (0x0079 above, which also carries the fd
        // field).
        // 0x0005 NV01_EVENT, 0x0078 NV01_EVENT_KERNEL_CALLBACK, 0x007e NV01_EVENT_KERNEL_CALLBACK_EX
        0x0005 | 0x0078 | 0x007e => 24,

        // NV_HOPPER_USERMODE_A_PARAMS (nvos.h:3327) = 2. { NvBool bBar1Mapping;
        // NvBool bPriv } = 2. WARNING: unlike Volta/Turing/Ampere USERMODE
        // (RS_NONE, parameterless), Hopper and Blackwell take an OPTIONAL params
        // struct. Without this entry the doorbell allocation fails on those cards.
        // 0xc661 HOPPER_USERMODE_A, 0xc761 BLACKWELL_USERMODE_A
        0xc661 | 0xc761 => 2,

        // NV_VIDMEM_ACCESS_BIT_ALLOCATION_PARAMS (nvos.h:3310) = 528. MMU access-
        // bit buffer.
        // 0xc763 MMU_VIDMEM_ACCESS_BIT_BUFFER, 0xc863 HOPPER_MMU_VIDMEM_ACCESS_BIT_BUFFER
        0xc763 | 0xc863 => 528,

        // NV000F_ALLOCATION_PARAMETERS (class/cl000f.h:54) = 16.
        // 0x000f FABRIC_MANAGER_SESSION
        0x000f => 16,

        // NV0050_ALLOCATION_PARAMETERS (class/cl0050.h:43) = 24.
        // 0x0050 NV_CE_UTILS
        0x0050 => 24,

        // NV0060_ALLOC_PARAMETERS (class/cl0060.h:39) = 4.
        // 0x0060 NV0060_SYNC_GPU_BOOST
        0x0060 => 4,

        // NV00DB_ALLOCATION_PARAMETERS (class/cl00db.h:42) = 8.
        // 0x00db NV40_DEBUG_BUFFER
        0x00db => 8,

        // NV00E0_ALLOCATION_PARAMETERS (class/cl00e0.h:111) = 244.
        // 0x00e0 NV_MEMORY_EXPORT
        0x00e0 => 244,

        // NV00F8_ALLOCATION_PARAMETERS (class/cl00f8.h:132) = 48.
        // 0x00f8 NV_MEMORY_FABRIC
        0x00f8 => 48,

        // NV00FB_ALLOCATION_PARAMETERS (class/cl00fb.h:70) = 32.
        // 0x00fb NV_MEMORY_FABRIC_IMPORTED_REF
        0x00fb => 32,

        // NV2082_ALLOC_PARAMETERS (class/cl2082.h:39) = 4.
        // 0x2082 NV2082_BINAPI_PRIVILEGED
        0x2082 => 4,

        // NV30F1_ALLOC_PARAMETERS (class/cl30f1.h:55) = 4.
        // 0x30f1 NV30_GSYNC
        0x30f1 => 4,

        // NV503B_ALLOC_PARAMETERS (class/cl503b.h:81) = 88.
        // 0x503b NV50_P2P
        0x503b => 88,

        // NV503C_ALLOC_PARAMETERS (class/cl503c.h:41) = 4.
        // 0x503c NV50_THIRD_PARTY_P2P
        0x503c => 4,

        // NV5080_ALLOC_PARAMS (class/cl5080.h:43) = 1.
        // 0x5080 NV50_DEFERRED_API_CLASS
        0x5080 => 1,

        // NV9072_ALLOCATION_PARAMETERS (class/cl9072.h:46) = 12.
        // 0x9072 GF100_DISP_SW
        0x9072 => 12,

        // NVA084_ALLOC_PARAMETERS (class/cla084.h:82) = 88.
        // 0xa084 NVA084_KERNEL_HOST_VGPU_DEVICE
        0xa084 => 88,

        // NVA0BC_ALLOC_PARAMETERS (class/cla0bc.h:201) = 20.
        // 0xa0bc NVENC_SW_SESSION
        0xa0bc => 20,

        // NVA0BD_ALLOC_PARAMETERS (class/cla0bd.h:67) = 20.
        // 0xa0bd NVFBC_SW_SESSION
        0xa0bd => 20,

        // NVB0CD_ALLOC_PARAMETERS (class/clb0cd.h:48) = 8.
        // 0xb0cd PROFILER_DEVICE_EVENT
        0xb0cd => 8,

        // NVB0CE_ALLOC_PARAMETERS (class/clb0ce.h:52) = 16.
        // 0xb0ce PROFILER_CONTEXT_EVENT
        0xb0ce => 16,

        // NVB1CC_ALLOC_PARAMETERS (class/clb1cc.h:49) = 4.
        // 0xb1cc MAXWELL_PROFILER_CONTEXT
        0xb1cc => 4,

        // NVB2CC_ALLOC_PARAMETERS (class/clb2cc.h:59) = 8.
        // 0xb2cc MAXWELL_PROFILER_DEVICE
        0xb2cc => 8,

        // NVC637_ALLOCATION_PARAMETERS (class/clc637.h:61) = 16.
        // 0xc637 AMPERE_SMC_PARTITION_REF
        0xc637 => 16,

        // NVC638_ALLOCATION_PARAMETERS (class/clc638.h:49) = 16.
        // 0xc638 AMPERE_SMC_EXEC_PARTITION_REF
        0xc638 => 16,

        // NVC639_ALLOCATION_PARAMETERS (class/clc639.h:47) = 8.
        // 0xc639 AMPERE_SMC_CONFIG_SESSION
        0xc639 => 8,

        // NVCDCD_ALLOC_PARAMETERS (class/clcdcd.h:46) = 8.
        // 0xcdcd TRACE_DEVICE_EVENT
        0xcdcd => 8,

        // NV_ACCESS_COUNTER_NOTIFY_BUFFER_ALLOC_PARAMS
        // (alloc/alloc_access_counter_buffer.h:50) = 4.
        // 0xc365 ACCESS_COUNTER_NOTIFY_BUFFER
        0xc365 => 4,

        // NV_CONTEXT_DMA_ALLOCATION_PARAMS (nvos.h:1594) = 32.
        // 0x0002 NV01_CONTEXT_DMA
        0x0002 => 32,

        // NV_MEMORY_ALLOCATION_PARAMS (nvos.h:1634) = 128.
        // 0x90ce NV01_MEMORY_DEVICELESS
        0x90ce => sz::<sys::NV_MEMORY_ALLOCATION_PARAMS>(),

        // NV_MEMORY_MAPPER_ALLOCATION_PARAMS (class/cl00fe.h:48) = 24.
        // 0x00fe NV_MEMORY_MAPPER
        0x00fe => 24,

        // NV_MEMORY_SYNCPOINT_ALLOCATION_PARAMS (class/cl00c3.h:42) = 4.
        // 0x00c3 NV01_MEMORY_SYNCPOINT
        0x00c3 => 4,

        // NV_MEMORY_VIRTUAL_ALLOCATION_PARAMS (class/cl0070.h:70) = 24.
        // 0x0070 NV01_MEMORY_VIRTUAL
        0x0070 => 24,

        // NV_PHYSICAL_MEMORY_ALLOCATION_PARAMS (class/cl00c2.h:40) = 24.
        // 0x00c2 NV01_MEMORY_LOCAL_PHYSICAL
        0x00c2 => 24,

        // NV_SEMAPHORE_SURFACE_ALLOC_PARAMETERS (class/cl00da.h:84) = 16.
        // 0x00da NV_SEMAPHORE_SURFACE
        0x00da => 16,

        // NV_UVM_CHANNEL_RETAINER_ALLOC_PARAMS (class/clc574.h:40) = 8.
        // 0xc574 UVM_CHANNEL_RETAINER
        0xc574 => 8,
        _ => return None,
    })
}

/// Known FD offset within allocation parameters (NV_ESC_RM_ALLOC aux).
/// The field is an 8-byte NvP64; [`alloc_fd_guard`] determines when it is an
/// FD rather than a callback. `None` means no registered FD translation.
pub fn alloc_fd_field(hclass: u32) -> Option<u32> {
    Some(match hclass {
        // NV0005.data @16 (cl0005.h:40-47) identifies an OS-event registration.
        // User clients resolve it through osUserHandleToKernelPtr
        // (event_api.c:173; os.c:1741), so it must match the registered host ID.
        0x0079 => 16,

        // Vulkan uses outer class 0x0005 with inner class 0x79; CUDA uses 0x79
        // outside too. The 2026-08-07 pre-ioctl trace confirmed both carry an FD.
        // Inspect before the call: RM overwrites data with a kernel pointer.
        0x0005 => 16,

        _ => return None,
    })
}

/// `(offset, value)` required before translating an allocation FD.
/// NV0005.hClass @8 distinguishes OS-event data @16 from callback pointers.
/// The outer class alone is insufficient. `None` means no condition.
pub fn alloc_fd_guard(hclass: u32) -> Option<(u32, u32)> {
    match hclass {
        // hClass @8 == NV01_EVENT_OS_EVENT (cl0005.h:40-47, cl0000.h)
        0x0005 | 0x0079 => Some((8, 0x0079)),
        _ => None,
    }
}

// Nested pointers inside NVOS54.params need command-specific lengths.
// Untranslated guest addresses refer to the backend's address space;
// known unsupported pointer controls must remain blocked.

/// Where the length of the target buffer comes from.
#[derive(Copy, Clone, Debug)]
pub enum LenSource {
    /// Fixed size in bytes.
    Fixed(u32),
    /// u32 at `off` in the params buffer, times `elem` bytes per element.
    /// `elem = 1` means: the field is directly a byte length.
    Field { off: u32, elem: u32 },
}

/// A P64 field in the params buffer that points at a further buffer.
#[derive(Copy, Clone, Debug)]
pub struct NestedPtr {
    /// Offset of the P64 field IN THE PARAMS BUFFER (not in the inline struct).
    pub ptr_off: u32,
    pub len: LenSource,
}

/// Upper bound per command. Enough for everything known (maximum so far:
/// 3, at GET_BUILD_VERSION). More -> fail loudly rather than truncate
/// quietly.
pub const MAX_NESTED: usize = 4;

/// Enumerate [`nested_ptrs`] for table serialization.
/// Add commands to both lists; table construction verifies each annotation.
pub fn nested_cmds() -> &'static [u32] {
    &[
        0x101, 0x20801802, 0x20801201, 0x80170d, 0x800201, 0x801b01, 0x801301, 0x801102, 0x801701,
        0x801104, 0x20801301, 0x20800123, 0x410110, 0x20800802,
    ]
}

/// Controls the host refuses and advertises with CF_BLOCK to the guest.
///
/// The first group preserves host-owned process attribution (ctrl0000proc.h).
/// The second contains untranslated FD inputs (ctrl0000unix.h, os.c).
/// The last is the Linux driver's embeddedParamCopyIn switch minus controls
/// handled by nested_ptrs. Their user pointers would address the backend.
/// Add translation and tests before enabling one; native RM privilege checks
/// do not make an untranslated process address safe to forward.
pub fn blocked_ctrls() -> &'static [u32] {
    &[
        0x901,      // SET_SUB_PROCESS_ID
        0x902,      // DISABLE_SUB_PROCESS_USERD_ISOLATION
        0x3d08,     // GET_EXPORT_OBJECT_INFO
        0x3d0a,     // CREATE_EXPORT_OBJECT_FD
        0x3d0b,     // EXPORT_OBJECTS_TO_FD
        0x3d0c,     // IMPORT_OBJECTS_FROM_FD
        0x127,      // NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS
        0x130,      // NV0000_CTRL_CMD_SYSTEM_EXECUTE_ACPI_METHOD
        0x602,      // NV0000_CTRL_CMD_NVD_GET_DUMP
        0x730120,   // NV0073_CTRL_CMD_SYSTEM_EXECUTE_ACPI_METHOD
        0x801401,   // NV0080_CTRL_CMD_HOST_GET_CAPS
        0x80180f,   // NV0080_CTRL_CMD_DMA_UPDATE_PDE_2
        0x20800122, // NV2080_CTRL_CMD_GPU_EXEC_REG_OPS
        0x20800124, // NV2080_CTRL_CMD_GPU_GET_ENGINE_CLASSLIST
        0x2080016e, // NV2080_CTRL_GPU_GET_NVENC_SW_SESSION_INFO
        0x208001e8, // NV2080_CTRL_CMD_GPU_RPC_GSP_TEST
        0x208001f2, // NV2080_CTRL_CMD_GSP_CRYPTO_CONTROL
        0x20800610, // NV2080_CTRL_CMD_I2C_ACCESS
        0x20800803, // NV2080_CTRL_CMD_BIOS_GET_NBSI
        0x20800806, // NV2080_CTRL_CMD_BIOS_GET_NBSI_OBJ
        0x20801336, // NV2080_CTRL_CMD_FB_GET_AMAP_CONF
        0x20802204, // NV2080_CTRL_CMD_RC_READ_VIRTUAL_MEM
        0x20802402, // NV2080_CTRL_CMD_NVD_GET_DUMP
        0x20802a01, // NV2080_CTRL_CMD_CE_GET_CAPS
        0x402c0102, // NV402C_CTRL_CMD_I2C_INDEXED
        0x402c0105, // NV402C_CTRL_CMD_I2C_TRANSACTION
        0x83de0315, // NV83DE_CTRL_CMD_DEBUG_READ_MEMORY
        0x83de0316, // NV83DE_CTRL_CMD_DEBUG_WRITE_MEMORY
        0x83de0326, // NV83DE_CTRL_CMD_DEBUG_READ_BATCH_MEMORY
        0x83de0327, // NV83DE_CTRL_CMD_DEBUG_WRITE_BATCH_MEMORY
        0xa0830103, // NVA083_CTRL_CMD_VIRTUAL_DISPLAY_GET_DEFAULT_EDID
        0xa0bc0101, // NVA0BC_CTRL_CMD_NVENC_SW_SESSION_UPDATE_INFO
        0xb06f010c, // NVB06F_CTRL_CMD_GET_ENGINE_CTX_DATA
        0xb06f010d, // NVB06F_CTRL_CMD_MIGRATE_ENGINE_CTX_DATA
    ]
}

/// Is this control blocked? One place, two consumers (host + table).
pub fn ctrl_blocked(cmd: u32) -> bool {
    blocked_ctrls().contains(&cmd)
}

/// Known NvS32 FD offsets inside NVOS54 control parameters.
/// These fields are four bytes, unlike allocation NvP64 FDs.
/// `None` means no registered translation; unsupported inputs are blocked.
pub fn ctrl_fd_offset(cmd: u32) -> Option<u32> {
    match cmd {
        // EXPORT_OBJECT_TO_FD (ctrl0000unix.h:147): object @0, fd i32 @16,
        // flags @20; 24 bytes. Despite IN/OUT in the header, the measured RMAPI
        // path supplies an already-open ctl FD and preserves its number.
        0x3d05 => Some(16),

        // IMPORT_OBJECT_FROM_FD (ctrl0000unix.h:181): fd i32 @0, object @4;
        // 20 bytes. Resolve the input FD to the exporting OFD.
        0x3d06 => Some(0),

        // Other Unix FD-input controls are refused by blocked_ctrls until
        // translated. 0x3d04 declares an output FD, not a token to resolve.
        _ => None,
    }
}

/// Every command `ctrl_fd_offset` knows, so the table builder can enumerate
/// them. Same contract as `nested_cmds()`: whoever adds a match arm above
/// adds the number here, and `collect()` asserts the two agree.
pub fn ctrl_fd_cmds() -> &'static [u32] {
    &[0x3d05, 0x3d06]
}

/// Which pointers sit inside the params buffer of this RM_CONTROL command?
///
/// Empty slice = flat, nothing to do (the normal case).
pub fn nested_ptrs(cmd: u32) -> &'static [NestedPtr] {
    match cmd {
        // SYSTEM_GET_BUILD_VERSION (ctrl0000system.h): SizeOfStrings @0,
        // three pointers @8/16/24, changelist @32, official changelist @36;
        // 40 bytes. Each pointed-to buffer contains SizeOfStrings bytes.
        0x101 => &[
            NestedPtr {
                ptr_off: 8,
                len: LenSource::Field { off: 0, elem: 1 },
            },
            NestedPtr {
                ptr_off: 16,
                len: LenSource::Field { off: 0, elem: 1 },
            },
            NestedPtr {
                ptr_off: 24,
                len: LenSource::Field { off: 0, elem: 1 },
            },
        ],

        // BUS_GET_INFO (ctrl2080bus.h:583): count u32 @0, list NvP64 @8.
        // NV2080_CTRL_BUS_INFO is an 8-byte { index, data } pair.
        0x20801802 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 8 },
        }],

        // BIOS_GET_INFO (ctrl2080bios.h:71-76): count @0, pointer @8;
        // 16-byte params, 8-byte { index, data } entries (:39).
        // The 2026-08-20 sweep found this missing despite nvidia-smi succeeding
        // (OPEN-QUESTIONS 51): whole-workload status alone missed the refusal.
        0x20800802 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 8 },
        }],

        // GET_SURFACE_INFO (ctrl0041.h:279-286): count @0, pointer @8,
        // 8-byte { index, data } entries. NVKMS uses it for the display LUT;
        // a missing translation failed that allocation (2026-08-08).
        0x410110 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 8 },
        }],

        // GR_GET_INFO (ctrl2080gr.h:408-416): count @0, pointer @8,
        // route @16; 32-byte params. Entries are 8-byte NVXXXX_CTRL_XXX_INFO
        // pairs (ctrlxxxx.h:71; ctrl2080gr.h:154; ctrl0080gr.h:99).
        0x20801201 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 8 },
        }],

        // FIFO_GET_CHANNELLIST (ctrl0080fifo.h:178-185): count @0,
        // handle pointer @8, channel-ID pointer @16; 24-byte params.
        // Both arrays have count elements of four bytes.
        0x80170d => &[
            NestedPtr {
                ptr_off: 8,
                len: LenSource::Field { off: 0, elem: 4 },
            },
            NestedPtr {
                ptr_off: 16,
                len: LenSource::Field { off: 0, elem: 4 },
            },
        ],

        // GPU_GET_CLASSLIST (ctrl0080gpu.h:70-77): count @0, pointer @8;
        // 16-byte params, four bytes per class. NULL queries the count.
        // The encoder and NVKMS display HAL both consume the returned list.
        0x800201 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 4 },
        }],

        // NVENC_GET_CAPS (ctrl0080nvenc.h:59-68): byte count @0, pointer @8;
        // 16-byte params. Count is bytes (:47), not entries; table size is six.
        0x801b01 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 1 },
        }],

        // Device-class GET_CAPS/GET_INFO lengths are not interchangeable:
        // GET_CAPS uses bytes; GET_INFO uses a count of index/data pairs.

        // NV0080_CTRL_CMD_FB_GET_CAPS (ctrl0080fb.h:60, params :63-66).
        // { capsTblSize u32 @0; NV_DECLARE_ALIGNED(capsTbl NvP64, 8) @8 }
        // = 16. The header: "the size in BYTES of the caps table"; so
        // elem 1. NV0080_CTRL_FB_CAPS_TBL_SIZE is 3 (:99).
        0x801301 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 1 },
        }],

        // NV0080_CTRL_CMD_GR_GET_CAPS (ctrl0080gr.h, params right after).
        // Same shape, same byte semantics; NV0080_CTRL_GR_CAPS_TBL_SIZE is
        // 23 (:78).
        0x801102 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 1 },
        }],

        // NV0080_CTRL_CMD_FIFO_GET_CAPS (ctrl0080fifo.h).
        // NV0080_CTRL_FIFO_CAPS_TBL_SIZE is 2 (:95). Bytes again.
        0x801701 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 1 },
        }],

        // GR_GET_INFO (ctrl0080gr.h:72,99): listSize is an entry count;
        // each NVXXXX_CTRL_XXX_INFO pair occupies eight bytes.
        0x801104 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 8 },
        }],

        // FB_GET_INFO (ctrl2080fb.h:480-486): count @0, pointer @8;
        // 16-byte params, eight-byte NVXXXX_CTRL_XXX_INFO entries (:315).
        0x20801301 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 8 },
        }],

        // GPU_GET_ENGINES (ctrl2080gpu.h): count @0, pointer @8;
        // 16-byte params, four bytes per engine. NULL queries the count.
        // Vulkan device creation requires this translation.
        0x20800123 => &[NestedPtr {
            ptr_off: 8,
            len: LenSource::Field { off: 0, elem: 4 },
        }],

        // GPU_GET_ID_INFO (0x202) declares szName, but the vendored
        // gpumgrGetGpuIdInfo only copies scalar fields through its V2 form.
        // Recheck that unused pointer when updating the driver source.
        _ => &[],
    }
}

impl LenSource {
    /// Compute the length. `params` is the content of the params buffer.
    ///
    /// # Safety
    /// `params` must be long enough for the referenced field.
    pub unsafe fn resolve(self, params: *const u8, params_len: u32) -> Option<u32> {
        Some(match self {
            LenSource::Fixed(n) => n,
            LenSource::Field { off, elem } => {
                // checked_add for symmetry with checked_mul below: `off`
                // comes from the static nested_ptrs table today, but the
                // bound must not depend on that staying true.
                if off.checked_add(4).is_none_or(|end| end > params_len) {
                    return None;
                }
                let n = core::ptr::read_unaligned(params.add(off as usize) as *const u32);
                n.checked_mul(elem)?
            }
        })
    }
}

#[cfg(test)]
mod nested_tests {
    use super::*;

    /// Every command named in `nested_cmds()` must actually carry an
    /// annotation. `table::build()` checks the same thing at build time,
    /// but a unit test says which one is missing without a panic in a
    /// serializer.
    #[test]
    fn every_named_command_has_an_annotation() {
        for &cmd in nested_cmds() {
            assert!(
                !nested_ptrs(cmd).is_empty(),
                "{cmd:#x} is in nested_cmds() but nested_ptrs() has nothing for it"
            );
        }
    }

    /// BIOS list entries are eight-byte index/data pairs
    /// (ctrl2080bios.h:39,73); an incorrect stride truncates or over-reads.
    #[test]
    fn bios_get_info_points_at_offset_8_with_eight_bytes_per_entry() {
        let specs = nested_ptrs(0x20800802);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].ptr_off, 8);
        assert!(matches!(specs[0].len, LenSource::Field { off: 0, elem: 8 }));
        assert!(nested_cmds().contains(&0x20800802));

        // What nvidia-smi asked for in the run that found this: two entries,
        // i.e. 16 bytes behind the pointer.
        let mut params = [0u8; 16];
        params[0..4].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            unsafe { specs[0].len.resolve(params.as_ptr(), 16) },
            Some(16)
        );
    }

    /// Pin class count @0, pointer @8 and four-byte entries (ctrl0080gpu.h:74).
    #[test]
    fn classlist_points_at_offset_8_with_four_bytes_per_class() {
        let specs = nested_ptrs(0x800201);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].ptr_off, 8);
        assert!(matches!(specs[0].len, LenSource::Field { off: 0, elem: 4 }));

        // 95 classes on this rig -> 380 bytes.
        let params = {
            let mut p = [0u8; 16];
            p[0..4].copy_from_slice(&95u32.to_le_bytes());
            p
        };
        // SAFETY: params is 16 bytes, the field sits at offset 0.
        let len = unsafe { specs[0].len.resolve(params.as_ptr(), 16) };
        assert_eq!(len, Some(380));
    }

    /// NVENC caps use a byte count, not an entry count (ctrl0080nvenc.h:65).
    #[test]
    fn nvenc_caps_length_is_bytes_not_elements() {
        let specs = nested_ptrs(0x801b01);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].ptr_off, 8);
        assert!(matches!(specs[0].len, LenSource::Field { off: 0, elem: 1 }));

        // NV0080_CTRL_NVENC_CAPS_TBL_SIZE = 6 bytes, and 6 must stay 6.
        let params = {
            let mut p = [0u8; 16];
            p[0..4].copy_from_slice(&6u32.to_le_bytes());
            p
        };
        // SAFETY: params is 16 bytes, the field sits at offset 0.
        assert_eq!(
            unsafe { specs[0].len.resolve(params.as_ptr(), 16) },
            Some(6)
        );
    }

    /// NV0000_CTRL_CMD_GPU_GET_ID_INFO has an NvP64 and is deliberately NOT
    /// annotated: measured, that pointer is NULL in every call, and an
    /// entry with a guessed length source would turn a clean ENOTSUP into
    /// an out-of-bounds read in the driver.
    #[test]
    fn gpu_get_id_info_stays_unannotated() {
        assert!(nested_ptrs(0x202).is_empty());
        assert!(!nested_cmds().contains(&0x202));
    }

    /// A length that overflows u32 must fail, not wrap. numClasses is read
    /// from the GUEST's params buffer, so it is attacker-controlled.
    #[test]
    fn absurd_counts_fail_rather_than_wrap() {
        let specs = nested_ptrs(0x800201);
        let params = {
            let mut p = [0u8; 16];
            p[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
            p
        };
        // SAFETY: params is 16 bytes, the field sits at offset 0.
        assert_eq!(unsafe { specs[0].len.resolve(params.as_ptr(), 16) }, None);
    }

    /// A params buffer too short for the length field yields None rather
    /// than reading past it.
    #[test]
    fn short_params_buffer_yields_no_length() {
        let specs = nested_ptrs(0x801b01);
        let params = [0u8; 16];
        // SAFETY: the resolver is told the buffer is 2 bytes and must
        // refuse before touching the 4-byte field.
        assert_eq!(unsafe { specs[0].len.resolve(params.as_ptr(), 2) }, None);
    }
}

#[cfg(test)]
mod uvm_tests {
    use super::*;
    use core::mem::offset_of;

    /// Scan through UVM_IOCTL_BASE (2047, uvm_ioctl.h), matching table.rs.
    /// The two Linux 0x3000_000x commands are checked separately.
    const SCAN_MAX: u32 = 2047;
    const SPECIAL: [u32; 2] = [uvm::INITIALIZE, uvm::DEINITIALIZE];

    /// Command -> the bindgen struct whose size `uvm_param_size` claims.
    /// One row per arm of that function; `no_uvm_arm_is_untested` below
    /// proves the list is complete.
    fn size_table() -> Vec<(&'static str, u32, u32)> {
        macro_rules! row {
            ($cmd:expr, $t:ty) => {
                (stringify!($t), $cmd, core::mem::size_of::<$t>() as u32)
            };
        }
        vec![
            row!(uvm::INITIALIZE, sys::UVM_INITIALIZE_PARAMS),
            row!(
                uvm::PAGEABLE_MEM_ACCESS,
                sys::UVM_PAGEABLE_MEM_ACCESS_PARAMS
            ),
            row!(uvm::MM_INITIALIZE, sys::UVM_MM_INITIALIZE_PARAMS),
            row!(
                uvm::REGISTER_GPU_VASPACE,
                sys::UVM_REGISTER_GPU_VASPACE_PARAMS
            ),
            row!(
                uvm::UNREGISTER_GPU_VASPACE,
                sys::UVM_UNREGISTER_GPU_VASPACE_PARAMS
            ),
            row!(uvm::REGISTER_CHANNEL, sys::UVM_REGISTER_CHANNEL_PARAMS),
            row!(
                uvm::UNREGISTER_CHANNEL,
                sys::default_version::UVM_UNREGISTER_CHANNEL_PARAMS
            ),
            row!(
                uvm::MAP_EXTERNAL_ALLOCATION,
                sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS
            ),
            row!(uvm::FREE, sys::default_version::UVM_FREE_PARAMS),
            row!(uvm::REGISTER_GPU, sys::UVM_REGISTER_GPU_PARAMS),
            row!(
                uvm::MAP_DYNAMIC_PARALLELISM_REGION,
                sys::UVM_MAP_DYNAMIC_PARALLELISM_REGION_PARAMS
            ),
            row!(
                uvm::ALLOC_SEMAPHORE_POOL,
                sys::UVM_ALLOC_SEMAPHORE_POOL_PARAMS
            ),
            row!(
                uvm::PAGEABLE_MEM_ACCESS_ON_GPU,
                sys::UVM_PAGEABLE_MEM_ACCESS_ON_GPU_PARAMS
            ),
            row!(
                uvm::SET_PREFERRED_LOCATION,
                sys::UVM_SET_PREFERRED_LOCATION_PARAMS
            ),
            row!(
                uvm::UNSET_PREFERRED_LOCATION,
                sys::UVM_UNSET_PREFERRED_LOCATION_PARAMS
            ),
            row!(
                uvm::ENABLE_READ_DUPLICATION,
                sys::UVM_ENABLE_READ_DUPLICATION_PARAMS
            ),
            row!(
                uvm::DISABLE_READ_DUPLICATION,
                sys::UVM_DISABLE_READ_DUPLICATION_PARAMS
            ),
            row!(uvm::SET_ACCESSED_BY, sys::UVM_SET_ACCESSED_BY_PARAMS),
            row!(uvm::UNSET_ACCESSED_BY, sys::UVM_UNSET_ACCESSED_BY_PARAMS),
            row!(uvm::MIGRATE, sys::UVM_MIGRATE_PARAMS),
            row!(uvm::VALIDATE_VA_RANGE, sys::UVM_VALIDATE_VA_RANGE_PARAMS),
            row!(
                uvm::CREATE_EXTERNAL_RANGE,
                sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS
            ),
        ]
    }

    /// Compare each handwritten UVM size with the selected compiled ABI.
    #[test]
    fn every_uvm_size_equals_its_bindgen_struct() {
        for (name, cmd, want) in size_table() {
            assert_eq!(
                uvm_param_size(cmd),
                Some(want),
                "UVM cmd {cmd:#x}: xlate says {:?}, sizeof({name}) is {want}",
                uvm_param_size(cmd)
            );
        }
    }

    /// UVM_DEINITIALIZE takes no parameter struct at all; there is no
    /// `UVM_DEINITIALIZE_PARAMS` in the vendor header. It must answer 0,
    /// not `None`: `None` means "unknown command" and makes the host reject
    /// the call.
    #[test]
    fn deinitialize_is_a_known_command_with_a_zero_length_payload() {
        assert_eq!(uvm_param_size(uvm::DEINITIALIZE), Some(0));
    }

    /// An unknown UVM number must be `None` rather than a guess. `None` is
    /// what makes the host answer ENOTSUP instead of copying an arbitrary
    /// number of bytes.
    #[test]
    fn an_unknown_uvm_command_has_no_size() {
        assert_eq!(uvm_param_size(0), None);
        assert_eq!(uvm_param_size(1023), None);
        // The header's outlier, now inside the scan range: unimplemented
        // here, and the scan will pick it up on the day it is not.
        assert_eq!(uvm_param_size(2047), None);
        assert_eq!(uvm_param_size(0x3000_0003), None);
    }

    /// The size table above must name EVERY arm of `uvm_param_size`. A new
    /// arm added without a row here would otherwise be an unchecked
    /// hand-transcribed number again; which is the exact failure this
    /// file's tests exist to prevent.
    #[test]
    fn no_uvm_arm_is_untested() {
        let known: std::collections::BTreeSet<u32> = (0..=SCAN_MAX)
            .chain(SPECIAL)
            .filter(|&nr| uvm_param_size(nr).is_some())
            .collect();
        let mut tested: std::collections::BTreeSet<u32> =
            size_table().iter().map(|&(_, cmd, _)| cmd).collect();
        tested.insert(uvm::DEINITIALIZE); // has no struct, covered above
        assert_eq!(known, tested, "an arm of uvm_param_size has no test row");
    }

    /// Compare UVM FD offsets with bindgen to prevent adjacent-field corruption.
    #[test]
    fn every_uvm_fd_offset_is_the_structs_own_field() {
        let want = |cmd: u32, off: u32| {
            let size = uvm_param_size(cmd).expect("command must have a size");
            for dev in [Dev::Uvm, Dev::UvmTools] {
                assert_eq!(
                    fd_field_offset(dev, cmd, size),
                    Some(off),
                    "{dev:?} cmd {cmd:#x}"
                );
            }
        };
        // UVM_MM_INITIALIZE carries `uvmFd`, an fd on /dev/nvidia-uvm
        // itself; all the others carry `rmCtrlFd`, an fd on /dev/nvidiactl.
        want(
            uvm::MM_INITIALIZE,
            offset_of!(sys::UVM_MM_INITIALIZE_PARAMS, uvmFd) as u32,
        );
        want(
            uvm::REGISTER_GPU_VASPACE,
            offset_of!(sys::UVM_REGISTER_GPU_VASPACE_PARAMS, rmCtrlFd) as u32,
        );
        want(
            uvm::REGISTER_CHANNEL,
            offset_of!(sys::UVM_REGISTER_CHANNEL_PARAMS, rmCtrlFd) as u32,
        );
        want(
            uvm::REGISTER_GPU,
            offset_of!(sys::UVM_REGISTER_GPU_PARAMS, rmCtrlFd) as u32,
        );
        // The interesting one: rmCtrlFd sits BEHIND the 9216-byte
        // per-GPU attribute array, so it is the offset most likely to be
        // mis-added by hand.
        want(
            uvm::MAP_EXTERNAL_ALLOCATION,
            offset_of!(sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS, rmCtrlFd) as u32,
        );
    }

    /// A UVM command without an fd field answers `None`. Answering an
    /// offset here would make the host rewrite four bytes of an unrelated
    /// field.
    #[test]
    fn a_uvm_command_without_an_fd_field_answers_none() {
        for cmd in [
            uvm::INITIALIZE,
            uvm::DEINITIALIZE,
            uvm::FREE,
            uvm::MIGRATE,
            uvm::UNREGISTER_CHANNEL,
            uvm::VALIDATE_VA_RANGE,
        ] {
            for dev in [Dev::Uvm, Dev::UvmTools] {
                assert_eq!(fd_field_offset(dev, cmd, 0), None, "{dev:?} cmd {cmd:#x}");
            }
        }
    }

    /// Exactly five UVM commands carry an fd, and they are the five above.
    /// A sixth arm added without a test row would go unchecked; a lost arm
    /// would make the host forward a guest fd number unchanged.
    #[test]
    fn exactly_five_uvm_commands_carry_an_fd() {
        let with_fd: Vec<u32> = (0..=SCAN_MAX)
            .chain(SPECIAL)
            .filter(|&nr| fd_field_offset(Dev::Uvm, nr, 0).is_some())
            .collect();
        assert_eq!(
            with_fd,
            vec![
                uvm::REGISTER_GPU_VASPACE,
                uvm::REGISTER_CHANNEL,
                uvm::MAP_EXTERNAL_ALLOCATION,
                uvm::REGISTER_GPU,
                uvm::MM_INITIALIZE,
            ],
            "the set of UVM commands with an fd field has changed"
        );
    }
}

#[cfg(test)]
mod embedded_ptr_tests {
    use super::*;

    /// `embedded_ptr` in a shape a test can compare: `(ptr_off, len)`
    /// instead of the field-less `Embedded`.
    #[allow(clippy::type_complexity)]
    fn probe(dev: Dev, nr: u32, buf: &[u8], size: u32) -> Result<Option<(u32, u32)>, ()> {
        assert!(
            buf.len() >= size as usize,
            "the test buffer must cover `size`"
        );
        // SAFETY: the assert above guarantees `buf` is valid for `size` bytes.
        unsafe { embedded_ptr::<sys::DefaultAbi>(dev, nr, buf.as_ptr(), size) }
            .map(|o| o.map(|e| (e.ptr_off, e.len)))
    }

    /// NVOS54 = the RM_CONTROL parameter block from nvos.h: cmd @8,
    /// params (a 64-bit pointer) @16, paramsSize @24.
    fn nvos54(params: u64, params_size: u32) -> [u8; 64] {
        let mut b = [0u8; 64];
        b[16..24].copy_from_slice(&params.to_le_bytes());
        b[24..28].copy_from_slice(&params_size.to_le_bytes());
        b
    }

    /// NVOS64 = the 48-byte RM_ALLOC block: hClass @12, pAllocParms @16,
    /// pRightsRequested @24. The 32-byte NVOS21 form has the same first two
    /// fields and a paramsSize where NVOS64 has the rights pointer.
    fn nvos64(hclass: u32, params: u64, rights: u64) -> [u8; 64] {
        let mut b = [0u8; 64];
        b[12..16].copy_from_slice(&hclass.to_le_bytes());
        b[16..24].copy_from_slice(&params.to_le_bytes());
        b[24..32].copy_from_slice(&rights.to_le_bytes());
        b
    }

    /// A control call whose params pointer or length is zero carries
    /// nothing across the boundary. Reporting an embedded buffer here would
    /// make the host copy from a null guest pointer.
    #[test]
    fn rm_control_with_no_params_carries_no_embedded_pointer() {
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_CONTROL, &nvos54(0, 128), 32),
            Ok(None)
        );
        assert_eq!(
            probe(
                Dev::Ctl,
                sys::NV_ESC_RM_CONTROL,
                &nvos54(0xdead_beef, 0),
                32
            ),
            Ok(None)
        );
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_CONTROL, &nvos54(0, 0), 32),
            Ok(None)
        );
    }

    /// RM_CONTROL is self-describing: the params buffer sits behind the
    /// pointer at 16 and its length is the `paramsSize` field at 24. This
    /// is the one escape where the guest states the length itself, and the
    /// host must use exactly that number.
    #[test]
    fn rm_control_takes_the_length_from_paramssize() {
        for len in [1u32, 4, 24, 1234, 16384] {
            assert_eq!(
                probe(
                    Dev::Ctl,
                    sys::NV_ESC_RM_CONTROL,
                    &nvos54(0xdead_beef, len),
                    32
                ),
                Ok(Some((16, len))),
                "paramsSize {len}"
            );
        }
        // The same on a per-GPU node, not just on /dev/nvidiactl.
        assert_eq!(
            probe(
                Dev::Gpu,
                sys::NV_ESC_RM_CONTROL,
                &nvos54(0xdead_beef, 40),
                32
            ),
            Ok(Some((16, 40)))
        );
    }

    /// A class that allocates with `pAllocParms == NULL` (ROOT_CLIENT,
    /// USERMODE) has no second buffer; and no need for the hClass table
    /// either, which is why classes with no params need no entry there.
    #[test]
    fn rm_alloc_with_a_null_params_pointer_carries_nothing() {
        // 0xdead is not in the hClass table; the null pointer must be
        // decided BEFORE the class is looked up, otherwise every
        // parameterless alloc of an unlisted class would fail.
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(0xdead, 0, 0), 48),
            Ok(None)
        );
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(0x0041, 0, 0), 48),
            Ok(None)
        );
    }

    /// RM_ALLOC is NOT self-describing: the length comes from the hClass at
    /// 12 via `alloc_param_size`. An unknown class must fail loudly
    /// (`Err`), because any guess is either a truncated copy or an
    /// out-of-bounds read in the host driver's `copy_from_user`.
    #[test]
    fn rm_alloc_takes_the_length_from_the_hclass_table() {
        for hclass in [0x0080u32, 0x2080, 0x0079, 0x90f1, 0xc46f] {
            let want = alloc_param_size::<sys::DefaultAbi>(hclass)
                .expect("probe class missing from the table");
            assert_eq!(
                probe(
                    Dev::Ctl,
                    sys::NV_ESC_RM_ALLOC,
                    &nvos64(hclass, 0xdead_beef, 0),
                    48
                ),
                Ok(Some((16, want))),
                "hClass {hclass:#x}"
            );
        }
    }

    /// An hClass the table does not know must be `Err(())`; the caller
    /// turns that into ENOTSUP. Silently forwarding it would hand RM a
    /// guest address, or copy a guessed number of bytes.
    #[test]
    fn rm_alloc_of_an_unknown_hclass_fails_loudly() {
        assert_eq!(
            alloc_param_size::<sys::DefaultAbi>(0xdead),
            None,
            "the probe class must stay unknown"
        );
        assert_eq!(
            probe(
                Dev::Ctl,
                sys::NV_ESC_RM_ALLOC,
                &nvos64(0xdead, 0xbeef, 0),
                48
            ),
            Err(())
        );
    }

    /// `pRightsRequested` (NVOS64 @24) is not supported. It appears in no
    /// measured run, and it points at yet another buffer that nothing
    /// translates; so a non-null value must fail rather than be ignored.
    #[test]
    fn rm_alloc_refuses_a_non_null_rights_pointer() {
        let known = 0x2080u32; // a class the table knows
        assert_eq!(
            probe(
                Dev::Ctl,
                sys::NV_ESC_RM_ALLOC,
                &nvos64(known, 0xdead_beef, 1),
                48
            ),
            Err(()),
            "pRightsRequested != 0 must not be forwarded"
        );
        // ... and the check happens only in the 48-byte form (below).
    }

    /// NVOS21 uses paramsSize at byte 24, not NVOS64's rights pointer.
    /// Both forms must translate pAllocParms at byte 16.
    #[test]
    fn the_32_byte_nvos21_form_is_accepted_like_the_48_byte_one() {
        let hclass = 0x2080u32;
        let want = alloc_param_size::<sys::DefaultAbi>(hclass).unwrap();
        // Word at 24 is paramsSize here, deliberately non-zero.
        let buf = nvos64(hclass, 0xdead_beef, 4);
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &buf, 32),
            Ok(Some((16, want)))
        );
        // Null params and unknown class behave the same way in both forms.
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(hclass, 0, 4), 32),
            Ok(None)
        );
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(0xdead, 1, 4), 32),
            Err(())
        );
    }

    /// The driver accepts exactly two sizes for RM_ALLOC (escape.c:325),
    /// and this function matches exactly those two. Any other size is not
    /// an NVOS64/NVOS21 block, so its bytes must not be read as one.
    #[test]
    fn rm_alloc_of_any_other_size_is_not_read_as_a_parameter_block() {
        let buf = nvos64(0x2080, 0xdead_beef, 0);
        for size in (33..=47u32).chain(49..=64) {
            assert_eq!(
                probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &buf, size),
                Ok(None),
                "size {size} must not be decoded as NVOS64/NVOS21"
            );
        }
    }

    /// NVOS41 (`NV_ESC_RM_GET_EVENT_DATA`): `pEvent` @0 points at exactly
    /// ONE `NvUnixEvent`, which RM writes. The length is a constant; it
    /// cannot be read out of the guest's buffer; so it must be
    /// `sizeof(NvUnixEvent)` and nothing else.
    #[test]
    fn get_event_data_points_at_exactly_one_event() {
        let mut buf = [0u8; 64];
        buf[0..8].copy_from_slice(&0xdead_beef_u64.to_le_bytes());
        let want = core::mem::size_of::<sys::NvUnixEvent>() as u32;
        let size = core::mem::size_of::<sys::NVOS41_PARAMETERS>() as u32;
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_GET_EVENT_DATA, &buf, size),
            Ok(Some((0, want)))
        );
        // A null pEvent is RM's problem (it answers with a status), not a
        // reason to copy a buffer that is not there.
        let zero = [0u8; 64];
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_GET_EVENT_DATA, &zero, size),
            Ok(None)
        );
        // A payload too short to hold NVOS41 is not decoded at all; the
        // pointer field would be read past the end of the guest's buffer.
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_GET_EVENT_DATA, &buf, size - 1),
            Ok(None)
        );
    }

    /// Known UVM arrays are inline. Check the device before interpreting a
    /// number that also names an RM escape.
    #[test]
    fn uvm_calls_never_carry_an_embedded_pointer() {
        let buf = nvos64(0x2080, 0xdead_beef, 0);
        for dev in [Dev::Uvm, Dev::UvmTools] {
            for nr in [
                uvm::INITIALIZE,
                uvm::PAGEABLE_MEM_ACCESS, // == NV_ESC_RM_ALLOC_MEMORY (0x27)
                uvm::MAP_EXTERNAL_ALLOCATION,
                sys::NV_ESC_RM_CONTROL,
                sys::NV_ESC_RM_ALLOC,
            ] {
                assert_eq!(probe(dev, nr, &buf, 48), Ok(None), "{dev:?} nr {nr:#x}");
            }
        }
    }

    /// An escape with no embedded pointer at all is `Ok(None)`, not an
    /// error: the vast majority of escapes are flat and are forwarded
    /// unchanged.
    #[test]
    fn a_flat_escape_carries_nothing() {
        let buf = [0xffu8; 64];
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_FREE, &buf, 16), Ok(None));
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_MAP_MEMORY, &buf, 56),
            Ok(None)
        );
        assert_eq!(
            probe(Dev::Ctl, crate::nvgpu::NV_ESC_REGISTER_FD, &buf, 4),
            Ok(None)
        );
    }
}

#[cfg(test)]
mod alloc_fd_tests {
    use super::*;

    /// The highest hClass the scan covers: class numbers are 16 bit
    /// (`resource_list.h`), so this is exhaustive rather than a sample.
    const CLASS_MAX: u32 = 0xffff;

    /// Allocation FD offsets and guards must cover the same classes.
    /// NV0005.data must never be translated when it contains a callback.
    #[test]
    fn every_class_with_an_fd_field_has_a_guard_and_the_other_way_round() {
        let mut with_fd = Vec::new();
        for hclass in 0..=CLASS_MAX {
            let fd = alloc_fd_field(hclass);
            let guard = alloc_fd_guard(hclass);
            assert_eq!(
                fd.is_some(),
                guard.is_some(),
                "hClass {hclass:#x}: fd field {fd:?}, guard {guard:?} -- one without the other"
            );
            if let Some(off) = fd {
                // NV0005_ALLOC_PARAMETERS.data @16 (cl0005.h:40-47); the
                // NvP64 that holds the fd number.
                assert_eq!(off, 16, "hClass {hclass:#x}: fd offset");
                // The guard: translate only when the inner hClass at 8 says
                // NV01_EVENT_OS_EVENT (0x79).
                assert_eq!(guard, Some((8, 0x0079)), "hClass {hclass:#x}: guard");
                with_fd.push(hclass);
            }
        }
        // Both event classes, and only those two: 0x0079 (CUDA allocates
        // the event directly) and 0x0005 (NVIDIA's Vulkan allocates it with
        // the inner hClass set to 0x79).
        assert_eq!(with_fd, vec![0x0005, 0x0079]);
    }

    /// The offset is the same for both classes, because it is the same
    /// struct (NV0005_ALLOC_PARAMETERS) behind both.
    #[test]
    fn both_event_classes_name_the_same_field() {
        assert_eq!(alloc_fd_field(0x0005), alloc_fd_field(0x0079));
        assert_eq!(alloc_fd_guard(0x0005), alloc_fd_guard(0x0079));
    }
}

#[cfg(test)]
mod ctrl_fd_tests {
    use super::*;

    /// Every enumerated control FD must have an offset for table construction.
    #[test]
    fn every_named_control_really_carries_an_fd() {
        for &cmd in ctrl_fd_cmds() {
            assert!(
                ctrl_fd_offset(cmd).is_some(),
                "{cmd:#x} is in ctrl_fd_cmds() but ctrl_fd_offset() has nothing for it"
            );
        }
        assert!(
            !ctrl_fd_cmds().is_empty(),
            "an empty list would make this test vacuous"
        );
    }

    /// Pin ctrl0000unix.h's NvS32 inputs: EXPORT_OBJECT_TO_FD @16,
    /// IMPORT_OBJECT_FROM_FD @0. Both live in the control params buffer.
    #[test]
    fn the_two_known_controls_keep_their_offsets() {
        assert_eq!(ctrl_fd_offset(0x3d05), Some(16));
        assert_eq!(ctrl_fd_offset(0x3d06), Some(0));
        assert_eq!(ctrl_fd_cmds(), &[0x3d05, 0x3d06]);
    }

    /// Neighboring FD controls have no translation descriptor.
    /// Input-FD controls are blocked; 0x3d04 declares an output FD.
    #[test]
    fn the_unannotated_neighbours_stay_unannotated() {
        for cmd in [0x3d04u32, 0x3d08, 0x3d0a, 0x3d0b, 0x3d0c] {
            assert_eq!(ctrl_fd_offset(cmd), None, "{cmd:#x}");
            assert!(!ctrl_fd_cmds().contains(&cmd), "{cmd:#x}");
        }
        // And an ordinary control carries no fd at all.
        assert_eq!(ctrl_fd_offset(0x101), None);
    }
}
