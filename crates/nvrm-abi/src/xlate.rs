// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! What has to be known per (device, ioctl_nr) in order to carry a call
//! across the process boundary: payload size, fd field offset, embedded
//! pointer. One source for host and guest.
//!
//! Terms, once (docs/ARCHITECTURE.md has the longer story): RM is NVIDIA's
//! Resource Manager, the kernel driver behind /dev/nvidiactl and
//! /dev/nvidiaN, and its ioctls are called escapes; UVM is its
//! unified-memory driver (/dev/nvidia-uvm), whose command numbers are raw
//! integers rather than `_IOC` encodings; hClass is an RM object class
//! number; NVOS54, NVOS64, NVOS00, NVOS02, NVOS33 and NVOS41 are the
//! parameter blocks of RM_CONTROL, RM_ALLOC, RM_FREE, RM_ALLOC_MEMORY,
//! RM_MAP_MEMORY and RM_GET_EVENT_DATA, all from nvos.h.
//!
//! Where the numbers come from: struct layouts from gVisor pkg/abi/nvgpu,
//! reconciled against the bindgen types in nvgpu.rs (the same Apache-2.0
//! source). Where a size follows from a struct, the arithmetic stands
//! beside it as a comment -- at the next version change that is the
//! checklist.

use crate::sys;
use core::mem::{offset_of, size_of};

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

// ===========================================================================
// UVM command numbers (kernel-open/nvidia-uvm/uvm_ioctl.h + uvm_linux_ioctl.h)
// ===========================================================================
// UVM uses NO _IOC encoding: the request number is the raw number. So the
// size cannot be derived from the cmd here -- it lives in
// uvm_param_size().

pub mod uvm {
    pub const INITIALIZE: u32 = 0x3000_0001;
    pub const DEINITIALIZE: u32 = 0x3000_0002;
    pub const PAGEABLE_MEM_ACCESS: u32 = 39; // 0x27  (NO collision with
                                             // frontend 0x27 -- other device)
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

/// Payload size of a UVM command in bytes.
///
/// `None` means: unknown UVM command. The caller MUST then fail loudly
/// (ENOTSUP) and never guess -- a wrong size is an out-of-bounds read on
/// the host side, in the driver's copy_from_user.
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

/// Payload size of a UVM command, **as the compiler measures it**.
///
/// The sibling above is a hand-computed table: every entry carries the
/// arithmetic that produced it in a comment, and the guest module forwards
/// UVM commands on its authority. This one asks `size_of` of the bindgen
/// struct, so it is the header's answer rather than anyone's reading of it.
///
/// WHY BOTH EXIST. `nvrm-trace` dumps a UVM answer buffer at this length,
/// and it must not take that length from `uvm_param_size`: the point of
/// dumping UVM answers is to judge the forwarding that table drives, and an
/// instrument measuring with the table under test agrees with it by
/// construction. The test below then asks the two for the same command and
/// requires the same answer, which is what turns a hand-computed table into
/// a checked one.
///
/// `None` means no compiled struct. UVM_DEINITIALIZE takes no parameter
/// block at all and is the only command in the list without one.
pub fn uvm_param_size_compiled(cmd: u32) -> Option<usize> {
    Some(match cmd {
        uvm::INITIALIZE => size_of::<sys::UVM_INITIALIZE_PARAMS>(),
        uvm::PAGEABLE_MEM_ACCESS => size_of::<sys::UVM_PAGEABLE_MEM_ACCESS_PARAMS>(),
        uvm::MM_INITIALIZE => size_of::<sys::UVM_MM_INITIALIZE_PARAMS>(),
        uvm::REGISTER_GPU_VASPACE => size_of::<sys::UVM_REGISTER_GPU_VASPACE_PARAMS>(),
        uvm::UNREGISTER_GPU_VASPACE => size_of::<sys::UVM_UNREGISTER_GPU_VASPACE_PARAMS>(),
        uvm::REGISTER_CHANNEL => size_of::<sys::UVM_REGISTER_CHANNEL_PARAMS>(),
        uvm::UNREGISTER_CHANNEL => size_of::<sys::UVM_UNREGISTER_CHANNEL_PARAMS>(),
        uvm::MAP_EXTERNAL_ALLOCATION => size_of::<sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS>(),
        uvm::FREE => size_of::<sys::UVM_FREE_PARAMS>(),
        uvm::REGISTER_GPU => size_of::<sys::UVM_REGISTER_GPU_PARAMS>(),
        uvm::MAP_DYNAMIC_PARALLELISM_REGION => size_of::<sys::UVM_MAP_DYNAMIC_PARALLELISM_REGION_PARAMS>(),
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

/// Payload size of an allocation's `pAllocParms`, **as the compiler
/// measures it**, or `None` where no compiled struct backs the class.
///
/// The sibling `alloc_param_size` answers for a hundred-odd classes, most
/// of them from arithmetic done by hand while reading a header. This one
/// answers only where `size_of` can, and it is deliberately the smaller
/// answer: `nvrm-trace` dumps an allocation's parameter block at this
/// length, and the point of dumping allocation answers is to judge the
/// forwarding that `alloc_param_size` drives. An instrument that took its
/// length from the table under test would agree with it by construction.
///
/// Reading past the end of a caller's struct is the bug this file has had
/// before -- 88 bytes past a foreign one -- so a class that cannot be
/// measured gets no dump rather than a guessed one.
pub fn alloc_param_size_compiled(hclass: u32) -> Option<usize> {
    Some(match hclass {
        0x003e | 0x0040 | 0x50a0 | 0x90ce => size_of::<sys::NV_MEMORY_ALLOCATION_PARAMS>(),
        0x0071 => size_of::<sys::NV_OS_DESC_MEMORY_ALLOCATION_PARAMS>(),
        0x0080 => size_of::<sys::NV0080_ALLOC_PARAMETERS>(),
        0x90f1 => size_of::<sys::NV_VASPACE_ALLOCATION_PARAMETERS>(),
        0xa06c => size_of::<sys::NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS>(),
        0x9067 => size_of::<sys::NV_CTXSHARE_ALLOCATION_PARAMETERS>(),
        0x906f | 0xa06f | 0xa16f | 0xb06f | 0xc06f | 0xc36f | 0xc46f | 0xc56f
        | 0xc86f | 0xc96f | 0xca6f => size_of::<sys::NV_CHANNEL_ALLOC_PARAMS>(),
        0x902d | 0xa140 | 0xc597 | 0xc5c0 | 0xc697 | 0xc6c0 | 0xc797 | 0xc7c0
        | 0xc997 | 0xc9c0 | 0xcb97 | 0xcbc0 | 0xcd40 | 0xcd97 | 0xcdc0
        | 0xce97 | 0xcec0 => size_of::<sys::NV_GR_ALLOCATION_PARAMETERS>(),
        0xb8b0 | 0xc4b0 | 0xc6b0 | 0xc7b0 | 0xc9b0 | 0xcdb0 | 0xceb0 | 0xcfb0
        | 0xd1b0 | 0xd2b0 => size_of::<sys::NV_NVDEC_ALLOCATION_PARAMETERS>(),
        0xb4b7 | 0xc4b7 | 0xc7b7 | 0xc9b7 | 0xceb7 | 0xcfb7 | 0xd1b7
            => size_of::<sys::NV_NVENC_ALLOCATION_PARAMETERS>(),
        0xb8fa | 0xc6fa | 0xc7fa | 0xc9fa | 0xcdfa | 0xcefa | 0xcffa | 0xd1fa
        | 0xd2fa => size_of::<sys::NV_OFA_ALLOCATION_PARAMETERS>(),
        0xb8d1 | 0xc4d1 | 0xc9d1 | 0xcdd1 | 0xced0 | 0xcfd1 | 0xd2d1
            => size_of::<sys::NV_NVJPG_ALLOCATION_PARAMETERS>(),
        0x0002 => size_of::<sys::NV_CONTEXT_DMA_ALLOCATION_PARAMS>(),
        0x0005 | 0x0078 | 0x0079 | 0x007e => size_of::<sys::NV0005_ALLOC_PARAMETERS>(),
        0x2080 => size_of::<sys::NV2080_ALLOC_PARAMETERS>(),
        0xc661 | 0xc761 => size_of::<sys::NV_HOPPER_USERMODE_A_PARAMS>(),
        0xc763 | 0xc863 => size_of::<sys::NV_VIDMEM_ACCESS_BIT_ALLOCATION_PARAMS>(),
        0xb0b5 | 0xc0b5 | 0xc5b5 | 0xc6b5 | 0xc7b5 | 0xc8b5 | 0xc9b5 | 0xcab5
            => size_of::<sys::NVB0B5_ALLOCATION_PARAMETERS>(),
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

/// Where a control's nested pointer and its count live, **as the compiler
/// measures them**.
///
/// `ctrlout` dumps a control's params BUFFER. For these commands that buffer
/// is the QUESTION -- a count and an `NvP64` -- and the ANSWER is behind the
/// pointer. Thirteen signatures were reported `verified` on "16 of 16 bytes"
/// because of it, which says the whole answer was compared and means the
/// whole question was.
///
/// So `nvrm-trace` follows the pointer. The two OFFSETS come from
/// `offset_of!` on the bindgen struct and not from `nested_ptrs` below,
/// which is the table under test and which the guest module forwards on --
/// an instrument that took them from there would agree with it by
/// construction. The test beside this requires the two to agree.
///
/// WHAT REMAINS HAND-DERIVED, stated because it is the honest edge: `elem`.
/// Whether a count field means BYTES or ENTRIES is prose in the header, not
/// layout, and no `size_of` can answer it -- `NV0080_CTRL_GR_GET_INFO`'s
/// `grInfoListSize` is a number of entries while `NV0080_CTRL_GR_GET_CAPS`'s
/// `capsTblSize` is a number of bytes, and the two structs are identical.
/// The element STRUCT is compiled where there is one. A wrong `elem` makes
/// the tracer read the same wrong length the boundary already reads, so it
/// adds no risk that is not already there, and it costs coverage rather than
/// correctness in the direction that matters: fewer bytes compared.
pub fn ctrl_nested_compiled(cmd: u32) -> &'static [(usize, usize, u32)] {
    /// `NVXXXX_CTRL_XXX_INFO { index; data }`, the element of every
    /// `...InfoList` below (ctrlxxxx.h:71).
    const INFO: u32 = size_of::<sys::NVXXXX_CTRL_XXX_INFO>() as u32;
    match cmd {
        // Three string buffers, one shared size, in BYTES.
        0x101 => &[
            (offset_of!(sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS, pDriverVersionBuffer),
             offset_of!(sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS, sizeOfStrings), 1),
            (offset_of!(sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS, pVersionBuffer),
             offset_of!(sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS, sizeOfStrings), 1),
            (offset_of!(sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS, pTitleBuffer),
             offset_of!(sys::NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS, sizeOfStrings), 1),
        ],
        // Entry lists: the count is a number of NVXXXX_CTRL_XXX_INFO.
        0x20800802 => &[
            (offset_of!(sys::NV2080_CTRL_BIOS_GET_INFO_PARAMS, biosInfoList),
             offset_of!(sys::NV2080_CTRL_BIOS_GET_INFO_PARAMS, biosInfoListSize), INFO)],
        0x20801201 => &[
            (offset_of!(sys::NV2080_CTRL_GR_GET_INFO_PARAMS, grInfoList),
             offset_of!(sys::NV2080_CTRL_GR_GET_INFO_PARAMS, grInfoListSize), INFO)],
        0x20801301 => &[
            (offset_of!(sys::NV2080_CTRL_FB_GET_INFO_PARAMS, fbInfoList),
             offset_of!(sys::NV2080_CTRL_FB_GET_INFO_PARAMS, fbInfoListSize), INFO)],
        0x20801802 => &[
            (offset_of!(sys::NV2080_CTRL_BUS_GET_INFO_PARAMS, busInfoList),
             offset_of!(sys::NV2080_CTRL_BUS_GET_INFO_PARAMS, busInfoListSize), INFO)],
        0x801104 => &[
            (offset_of!(sys::NV0080_CTRL_GR_GET_INFO_PARAMS, grInfoList),
             offset_of!(sys::NV0080_CTRL_GR_GET_INFO_PARAMS, grInfoListSize), INFO)],
        // NvU32 arrays: the count is a number of 4-byte items.
        0x20800123 => &[
            (offset_of!(sys::NV2080_CTRL_GPU_GET_ENGINES_PARAMS, engineList),
             offset_of!(sys::NV2080_CTRL_GPU_GET_ENGINES_PARAMS, engineCount), 4)],
        0x800201 => &[
            (offset_of!(sys::NV0080_CTRL_GPU_GET_CLASSLIST_PARAMS, classList),
             offset_of!(sys::NV0080_CTRL_GPU_GET_CLASSLIST_PARAMS, numClasses), 4)],
        0x80170d => &[
            (offset_of!(sys::NV0080_CTRL_FIFO_GET_CHANNELLIST_PARAMS, pChannelHandleList),
             offset_of!(sys::NV0080_CTRL_FIFO_GET_CHANNELLIST_PARAMS, numChannels), 4),
            (offset_of!(sys::NV0080_CTRL_FIFO_GET_CHANNELLIST_PARAMS, pChannelList),
             offset_of!(sys::NV0080_CTRL_FIFO_GET_CHANNELLIST_PARAMS, numChannels), 4)],
        // Caps tables: the count is in BYTES, however identical the struct.
        0x801102 => &[
            (offset_of!(sys::NV0080_CTRL_GR_GET_CAPS_PARAMS, capsTbl),
             offset_of!(sys::NV0080_CTRL_GR_GET_CAPS_PARAMS, capsTblSize), 1)],
        0x801301 => &[
            (offset_of!(sys::NV0080_CTRL_FB_GET_CAPS_PARAMS, capsTbl),
             offset_of!(sys::NV0080_CTRL_FB_GET_CAPS_PARAMS, capsTblSize), 1)],
        0x801701 => &[
            (offset_of!(sys::NV0080_CTRL_FIFO_GET_CAPS_PARAMS, capsTbl),
             offset_of!(sys::NV0080_CTRL_FIFO_GET_CAPS_PARAMS, capsTblSize), 1)],
        0x801b01 => &[
            (offset_of!(sys::NV0080_CTRL_NVENC_GET_CAPS_PARAMS, capsTbl),
             offset_of!(sys::NV0080_CTRL_NVENC_GET_CAPS_PARAMS, capsTblSize), 1)],
        _ => &[],
    }
}

#[cfg(test)]
mod ctrl_nested_tests {
    use super::*;

    /// The hand-written nested-pointer table against the compiler.
    ///
    /// `nested_ptrs` drives the guest module: it says where a pointer sits in
    /// a params buffer and how long the buffer behind it is, and the module
    /// copies exactly that across the boundary. A wrong offset there reads
    /// the wrong eight bytes as a pointer. The offsets are layout, so the
    /// compiler can check them, and every command the compiler knows about
    /// must agree.
    ///
    /// `elem` is checked too, but that is the two tables agreeing rather than
    /// the compiler adjudicating: whether a count means bytes or entries is
    /// prose in a header. `ctrl_nested_compiled` says so in its own words.
    #[test]
    fn the_nested_pointer_offsets_are_what_the_compiler_measures() {
        let mut checked = 0;
        for cmd in nested_cmds() {
            let compiled = ctrl_nested_compiled(*cmd);
            if compiled.is_empty() {
                continue;   // no bindgen struct for it; nothing to check
            }
            let table = nested_ptrs(*cmd);
            assert_eq!(
                compiled.len(), table.len(),
                "command {cmd:#x}: {} compiled pointers against {} in the table",
                compiled.len(), table.len()
            );
            for (i, (ptr_off, len_off, elem)) in compiled.iter().enumerate() {
                assert_eq!(table[i].ptr_off as usize, *ptr_off,
                           "command {cmd:#x} pointer {i}: table says +{}, the compiler +{ptr_off}",
                           table[i].ptr_off);
                match table[i].len {
                    LenSource::Field { off, elem: e } => {
                        assert_eq!(off as usize, *len_off,
                                   "command {cmd:#x} pointer {i}: count at +{off} against +{len_off}");
                        assert_eq!(e, *elem,
                                   "command {cmd:#x} pointer {i}: elem {e} against {elem}");
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

    /// The hand-computed ALLOCATION sizes against the compiler's.
    ///
    /// `alloc_param_size` is the largest hand-computed table in this tree:
    /// a hundred-odd classes, most of them a number a person worked out
    /// while reading a header, with the citation in a comment beside it.
    /// The guest module copies exactly that many bytes on every allocation.
    /// A number that is too small truncates the caller's request; one that
    /// is too large reads out of bounds in `copy_from_user`. Neither is
    /// visible to any sweep -- a wrong size is wrong identically on both
    /// sides of the boundary, so the guest and the host agree perfectly
    /// about a truncated struct.
    ///
    /// Each row below is one class and the params struct its comment cites.
    /// The pairs are what the bindgen allowlist can reach; the rest of the
    /// table still rests on the arithmetic in its comments, and extending
    /// this list is a matter of adding the header to nvrm-sys's allowlist.
    #[test]
    fn the_hand_computed_allocation_sizes_are_what_the_compiler_measures() {
        macro_rules! check {
            ($t:ty, $($hclass:expr),+) => {{
                $(
                    let hand = alloc_param_size($hclass)
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
        check!(sys::NV_GR_ALLOCATION_PARAMETERS,
               0xc5c0u32, 0x902d, 0xa140, 0xc597, 0xc6c0, 0xc797, 0xc9c0, 0xcdc0);
        // Video engines. NV_BSP_/NV_MSENC_ are #defines onto these.
        check!(sys::NV_NVDEC_ALLOCATION_PARAMETERS, 0xc4b0u32, 0xb8b0, 0xc6b0, 0xd2b0);
        check!(sys::NV_NVENC_ALLOCATION_PARAMETERS, 0xc4b7u32, 0xb4b7, 0xc7b7, 0xd1b7);
        check!(sys::NV_OFA_ALLOCATION_PARAMETERS, 0xb8fau32, 0xc6fa, 0xd2fa);
        check!(sys::NV_NVJPG_ALLOCATION_PARAMETERS, 0xb8d1u32, 0xc4d1, 0xd2d1);
        // NV01_CONTEXT_DMA (nvos.h:1594).
        check!(sys::NV_CONTEXT_DMA_ALLOCATION_PARAMS, 0x0002u32);
        // Hopper/Blackwell USERMODE take an optional params struct where
        // Volta/Turing/Ampere take none (nvos.h:3327).
        check!(sys::NV_HOPPER_USERMODE_A_PARAMS, 0xc661u32, 0xc761);
        // MMU access-bit buffer (nvos.h:3310).
        check!(sys::NV_VIDMEM_ACCESS_BIT_ALLOCATION_PARAMS, 0xc763u32, 0xc863);
        // The event classes, which share NV01_EVENT_OS_EVENT's struct.
        check!(sys::NV0005_ALLOC_PARAMETERS, 0x0079u32, 0x0005, 0x0078, 0x007e);
        // NV20_SUBDEVICE_0 (cl2080.h).
        check!(sys::NV2080_ALLOC_PARAMETERS, 0x2080u32);
        // The classes whose params live in their own class header.
        check!(sys::NVB0B5_ALLOCATION_PARAMETERS,
               0xc5b5u32, 0xb0b5, 0xc0b5, 0xc6b5, 0xc7b5, 0xc8b5, 0xc9b5, 0xcab5);
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

    /// The hand-computed UVM sizes against the compiler's.
    ///
    /// The guest module copies exactly `uvm_param_size` bytes to and from
    /// the host on every UVM call. An entry that is too small truncates a
    /// caller's request; one that is too large is an out-of-bounds read in
    /// `copy_from_user`. Neither is visible to any sweep, because a wrong
    /// size is wrong identically on both sides of the boundary -- the guest
    /// and the host would agree perfectly about a truncated struct. So the
    /// compiler checks the arithmetic instead.
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
            let compiled = uvm_param_size_compiled(cmd).expect("has a struct");
            assert_eq!(
                hand as usize, compiled,
                "UVM command {cmd:#x}: the table says {hand} bytes, the compiler {compiled}"
            );
        }
        assert_eq!(cmds.len(), 22, "every command with a compiled struct is checked");
        // The one command that genuinely has no parameter block.
        assert_eq!(uvm_param_size(uvm::DEINITIALIZE), Some(0));
        assert_eq!(uvm_param_size_compiled(uvm::DEINITIALIZE), None);
    }
}

// ===========================================================================
// fd field offset  (the only value translation besides the aux pointer)
// ===========================================================================

/// Byte offset of a process-local fd field in the inline struct, if the
/// call carries one. `None` means no fd field.
///
/// Frontend: FIVE escapes, not four. The four from gVisor's HasFrontendFD
/// (ALLOC/FREE_OS_EVENT, RM_ALLOC_MEMORY, RM_MAP_MEMORY) plus REGISTER_FD,
/// which nvproxy handles in a handler of its own and therefore does not
/// route through the interface. Relying on HasFrontendFD as the complete
/// list ends in EINVAL on 0xc9.
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
        // nv_ioctl_register_fd_t { ctl_fd: i32 } - ctl_fd @ 0.
        //
        // NOT in ESCAPES_WITH_FD / gVisor's HasFrontendFD: nvproxy translates
        // this escape in a handler of its own instead of through the
        // generic path. It still carries a process-local fd number and is
        // therefore the fifth escape that must be translated. libcuda calls
        // it after EVERY open of a per-GPU node (10 times in the vectorAdd
        // trace).
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

// ===========================================================================
// Embedded pointer  (RM_CONTROL / RM_ALLOC)
// ===========================================================================

/// Description of an embedded pointer that points at a second buffer,
/// which has to travel across the boundary as well.
pub struct Embedded {
    /// Byte offset of the P64 pointer field in the inline struct.
    pub ptr_off: u32,
    /// Length of the target buffer in bytes.
    pub len: u32,
}

/// Does this call carry an embedded pointer? `buf` is the inline payload
/// (at least `size` bytes), out of which length fields are read.
///
/// Returns `Ok(None)`  = no embedded pointer.
/// Returns `Err(())`   = embedded, but the length is not determinable
///                          (unknown hClass) -> the caller fails loudly.
///
/// # Safety
/// `buf` must be valid for at least `size` bytes.
#[allow(clippy::result_unit_err)] // Err(()) means "not determinable"; the caller maps it to ENOTSUP
pub unsafe fn embedded_ptr(dev: Dev, nr: u32, buf: *const u8, size: u32) -> Result<Option<Embedded>, ()> {
    if dev.is_uvm() {
        // All UVM params known so far are flat: the big MAP_EXTERNAL_-
        // ALLOCATION/ALLOC_SEMAPHORE_POOL attribute arrays are inline,
        // not pointed to.
        return Ok(None);
    }
    let rd32 = |off: u32| -> u32 {
        core::ptr::read_unaligned(buf.add(off as usize) as *const u32)
    };
    match nr {
        // NVOS54: params P64 @ 16, paramsSize u32 @ 24. Self-describing.
        sys::NV_ESC_RM_CONTROL if size >= 32 => {
            let plen = rd32(24);
            let pptr = core::ptr::read_unaligned(buf.add(16) as *const u64);
            if pptr == 0 || plen == 0 {
                Ok(None) // a control with no params, and many have none
            } else {
                Ok(Some(Embedded { ptr_off: 16, len: plen }))
            }
        }
        // RM_ALLOC comes in TWO forms. The driver accepts exactly these two
        // sizes (escape.c:325):
        //   NVOS64 (48): hClass @12, pAllocParms @16, pRightsRequested @24
        //   NVOS21 (32): hClass @12, pAllocParms @16, paramsSize @24
        // pAllocParms sits at 16 in both (guaranteed by a ct_assert in
        // escape.c:286); pRightsRequested exists only in NVOS64.
        //
        // Matching only on `size >= 48` lets the 32-byte form fall
        // through quietly -- and then RM gets a guest VA instead of a
        // pointer into the aux buffer. NVOS21 does not appear in the traced
        // runs, but "does not appear" is no reason to let it through
        // unchecked.
        sys::NV_ESC_RM_ALLOC if size == 48 || size == 32 => {
            let pptr = core::ptr::read_unaligned(buf.add(16) as *const u64);
            if pptr == 0 {
                return Ok(None); // class with no params (ROOT_CLIENT, USERMODE)
            }
            if size == 48 {
                // pRightsRequested is not supported -- it appears in no
                // measured run. Non-null here -> fail loudly.
                let rights = core::ptr::read_unaligned(buf.add(24) as *const u64);
                if rights != 0 {
                    return Err(());
                }
            }
            let hclass = rd32(12);
            match alloc_param_size(hclass) {
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
                Ok(Some(Embedded { ptr_off: 0, len: size_of::<sys::NvUnixEvent>() as u32 }))
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

/// Has this class ever been exercised on real hardware?
///
/// Everything in [`alloc_param_size`] is derived from the vendor headers, but
/// derivation is not the same as having run. The classes below are the ones a
/// gate run actually allocates on the Turing card this project is developed
/// on — measured by tracing `nvidia-smi`, `nvprobe`, `managedprobe` and a
/// PyTorch workload, not assumed. Everything else answers with a size but has
/// never moved a byte on real silicon, and says so: the descriptor table
/// carries [`KF_UNVERIFIED`](nvrm_wire::tables::KF_UNVERIFIED) for it and the
/// host logs a line the first time a guest uses one.
///
/// `0x0071` is in the verified set for a different reason than the others: a
/// guest workload does not allocate it, the host does, for the UVM pool
/// backing (see `host_pool.rs`), and the gate covers that path.
///
/// This is a statement about hardware coverage, not about correctness. Moving
/// a class into this list requires a gate run on a card of that architecture.
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
            // --- the video block, promoted 2026-08-06 ---
            //
            // Reached for the first time once NV0080_CTRL_CMD_GPU_GET_CLASSLIST
            // was annotated: before that the encoder got a truncated class
            // list and concluded there was no encoder, so nothing below was
            // ever allocated. Each of these appeared in the backend trace of
            // a guest `h264_nvenc` run and a guest `-hwaccel cuda` decode,
            // every one with status 0x0. The `encode` stage of
            // scripts/test.sh gpu runs exactly that and keeps them covered.
            | 0x0002 // NV01_CONTEXT_DMA
            | 0x0041 // NV01_ROOT_USER
            | 0x0070 // NV01_MEMORY_SYSTEM_DYNAMIC
            | 0xa0bc // NVENC_SW_SESSION
            // The two USERMODE doorbell classes are RS_NONE (parameterless)
            // and therefore have no row in alloc_param_size at all -- listed
            // here for the record of what the trace showed, not because
            // anything reads them.
            | 0xc361 // VOLTA_USERMODE_A (clc361.h)
            | 0xc461 // TURING_USERMODE_A (already the doorbell path, now traced here too)
            | 0xc4b0 // NVC4B0_VIDEO_DECODER  -- NVDEC, Turing
            | 0xc4b7 // NVC4B7_VIDEO_ENCODER  -- NVENC, Turing
    )
}

/// hClass -> size of the alloc parameter struct.
///
/// Only classes with a non-null pAllocParms need an entry; the alloc side is
/// not self-describing. Unknown -> None -> the caller fails loudly with
/// ENOTSUP. Never guess: a wrong size is an out-of-bounds read in the
/// driver's `copy_from_user`, while a clean ENOTSUP is an understood error.
///
/// **Where bindgen knows the type, the size is derived rather than
/// transcribed** — the same principle as the guards in `nvgpu.rs`. Two
/// hand-maintained numbers here were once wrong (0x90f1 at 48 instead of 56,
/// because the gVisor layout does not know `pasid`; 0x0071 at 128 instead of
/// 40, because OS_DESCRIPTOR has a struct of its own). `size_of` rules that
/// class of error out structurally. What remains are the classes whose struct
/// is not in the bindgen allowlist; for those the source header and line are
/// cited, and the `class-sizes` step of `scripts/test.sh check`
/// compares every one of them against a `sizeof` compiled from those very
/// headers — so a transcription error cannot survive a check run.
///
/// The class -> struct mapping itself is mechanical: it is
/// `src/nvidia/src/kernel/rmapi/resource_list.h` in the vendor tree, whose
/// `RS_ENTRY` rows name the Alloc Param Info of every class.
///
/// Classes with `RS_NONE` need no entry at all — their `pAllocParms` is NULL
/// and `embedded_ptr` bails out before asking. That covers `0x73`
/// (NV04_DISPLAY_COMMON, resource_list.h:1197), `0x90e7`
/// (GF100_SUBDEVICE_INFOROM, :815), `0x9096` (GF100_ZBC_CLEAR) and the
/// USERMODE doorbell classes of Volta/Turing/Ampere (:884, :895, :906) —
/// but NOT the Hopper and Blackwell doorbells, which do take params; see
/// their entry below.
///
/// `0x9096` joined this list on 2026-08-22 rather than being found in the
/// header: number 59's coverage diff reported it allocated **35 times, every
/// one NV_OK, and named in no table entry**, which is what a class that is
/// RS_NONE and undocumented here looks like from the outside. The list was
/// incomplete by exactly one, and the run is what said so.
///
/// WARNING: a class is listed here only if `resource_list.h` names ONE
/// unambiguous param struct AND that struct carries no NvP64 the host does
/// not translate. Forwarding an untranslated pointer hands the host RM a
/// guest address; where that pointer is a callback (`pProc`, `pCallbkFn`) it
/// would be an address the host kernel calls. Those classes are deliberately
/// absent, and absent means a clean ENOTSUP.
///
/// Where a class sits in this file says how it was measured, not how it was
/// derived: the entries above the "never run on real hardware" divider are
/// listed in [`alloc_class_verified`]; a class promoted into that list stays
/// where its header citation is. The two are compared by
/// `hclass_sizes_match_xlate` in `table.rs`.
pub fn alloc_param_size(hclass: u32) -> Option<u32> {
    Some(match hclass {
        // --- derived from the bindgen types (the vendor tree governs) ---
        0x0080 => sz::<sys::NV0080_ALLOC_PARAMETERS>(),          // NV01_DEVICE_0
        0x90f1 => sz::<sys::NV_VASPACE_ALLOCATION_PARAMETERS>(), // FERMI_VASPACE_A
        0xa06c => sz::<sys::NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS>(), // TSG
        0x9067 => sz::<sys::NV_CTXSHARE_ALLOCATION_PARAMETERS>(),
        0xc46f => sz::<sys::NV_CHANNEL_ALLOC_PARAMS>(),          // the 610 layout

        // --- by hand, because not in the bindgen allowlist ---
        0x2080 => 4,   // NV2080_ALLOC_PARAMETERS { subDeviceId } (cl2080.h)
        0x2081 => 4,   // NV2081_ALLOC_PARAMETERS { reserved } (cl2081.h:38)
        0x0079 => 24,  // NV0005_ALLOC_PARAMETERS (cl0005.h:40-47):
                       // hParentClient@0, hSrcResource@4, hClass@8,
                       // notifyIndex@12, data P64 @16 -> 24
        // NVC640_ALLOCATION_PARAMETERS { NvU64 capDescriptor } (clc640.h:38).
        // WARNING: on Unix capDescriptor is a FILE DESCRIPTOR, not a value --
        // a process-local FD INSIDE the alloc params. `alloc_fd_field` has
        // no entry for it, deliberately: the class is MIG-only and has
        // never been exercised here, so a guest that allocates it gets the
        // fd forwarded untranslated rather than a silent half-translation.
        //
        // MEASURED 2026-08-21 (number 65), which narrows that warning to
        // something exact rather than something feared. The capability fd
        // comes from /dev/nvidia-caps/nvidia-cap<minor>, whose minor is read
        // out of /proc/driver/nvidia/capabilities/mig/monitor -- and NEITHER
        // path exists in a guest, because they are made by the host's
        // nvidia.ko and not by this project's guest module. So no guest
        // client can obtain the fd this field carries, and the untranslated
        // forward cannot be reached from a guest at all. `probe/bin/rmdirect`
        // asks for the class directly and gets NV_ERR_INSUFFICIENT_PERMISSIONS
        // (0x1b) on both sides for capDescriptor = -1 -- identical to a
        // native run given the same input, so the boundary carries the
        // allocation faithfully and the refusal is RM's, not ours.
        //
        // The entry to add here is therefore still MISSING and still wanted,
        // but it is a prerequisite for exposing MIG to a guest, not a live
        // hole: nothing can currently drive it.
        0xc640 => 8,   // AMPERE_SMC_MONITOR_SESSION
        // NV_MEMORY_ALLOCATION_PARAMS for the ordinary memory classes
        // (resource_list.h:574, :542, :563).
        0x003e | 0x0040 | 0x50a0 => sz::<sys::NV_MEMORY_ALLOCATION_PARAMS>(),
        // NV01_MEMORY_SYSTEM_OS_DESCRIPTOR uses a struct of its OWN, much
        // smaller (resource_list.h:605) -- not the same as above.
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

        // ---- classes below here have (mostly) never run on real hardware --
        // Same derivation as above, from the same resource_list.h rows, but
        // no gate has exercised them: alloc_class_verified() excludes them,
        // the descriptor table marks them KF_UNVERIFIED, and the host logs
        // the first use of each. Promoting one needs a gate run on a card of
        // that architecture. The exceptions -- 0x0002, 0x0041, 0x0070,
        // 0xa0bc, 0xc4b0, 0xc4b7 -- were promoted on 2026-08-06 by the
        // encode stage and are in alloc_class_verified(); they keep their
        // rows here, next to the header citations they came with.

        // NV_GR_ALLOCATION_PARAMETERS (nvos.h:2729) = 16. Graphics/compute class
        // of every architecture. Turing (0xc5c0) above is the verified one; these
        // are its siblings.
        // 16 classes, resource_list.h from :2121
        0x902d | 0xa140 | 0xc597 | 0xc697 | 0xc6c0 | 0xc797 | 0xc7c0 |
        0xc997 | 0xc9c0 | 0xcb97 | 0xcbc0 | 0xcd40 | 0xcd97 | 0xcdc0 |
        0xce97 | 0xcec0 => 16,

        // NV_BSP_ALLOCATION_PARAMETERS (nvos.h:2945) = 12. NVDEC video decoder.
        // Alias of NV_NVDEC_ALLOCATION_PARAMETERS.
        // 10 classes, resource_list.h from :1748
        0xb8b0 | 0xc4b0 | 0xc6b0 | 0xc7b0 | 0xc9b0 | 0xcdb0 | 0xceb0 |
        0xcfb0 | 0xd1b0 | 0xd2b0 => 12,

        // NV_CHANNEL_ALLOC_PARAMS (alloc/alloc_channel.h:347) = 376. GPFIFO
        // channel of every architecture -- one struct for all of them, so the
        // verified Turing entry (0xc46f) fixes the size for the rest. Without
        // these the FIRST channel allocation on a non-Turing card fails.
        // 10 classes, resource_list.h from :315
        0x906f | 0xa06f | 0xa16f | 0xb06f | 0xc06f | 0xc36f | 0xc56f |
        0xc86f | 0xc96f | 0xca6f => sz::<sys::NV_CHANNEL_ALLOC_PARAMS>(),

        // NV_OFA_ALLOCATION_PARAMETERS (nvos.h:3014) = 12. Optical flow
        // accelerator.
        // 9 classes, resource_list.h from :1935
        0xb8fa | 0xc6fa | 0xc7fa | 0xc9fa | 0xcdfa | 0xcefa | 0xcffa |
        0xd1fa | 0xd2fa => 12,

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

/// Byte offset of a process-local fd field INSIDE the alloc-params buffer
/// (the aux payload of NV_ESC_RM_ALLOC), for classes whose params carry one.
/// The field is an 8-byte NvP64 holding a plain fd number; the host rewrites
/// it to its own fd number for the same OFD.
///
/// `None` = no fd in the params (the normal case).
pub fn alloc_fd_field(hclass: u32) -> Option<u32> {
    Some(match hclass {
        // NV01_EVENT_OS_EVENT: NV0005_ALLOC_PARAMETERS.data @16
        // (cl0005.h:40-47). For a user-priv client RM converts data via
        // osUserHandleToKernelPtr (event_api.c:173-183), which matches it
        // as an FD NUMBER against the events registered by
        // NV_ESC_ALLOC_OS_EVENT (os.c:1741-1761, `e->fd == fd`). Those were
        // registered with the HOST fd number, so the guest number must be
        // translated or the lookup silently misses.
        0x0079 => 16,

        // NV01_EVENT, same NV0005_ALLOC_PARAMETERS, same `data` at 16.
        //
        // Measured 2026-08-07: NVIDIA's VULKAN driver allocates its
        // event under hClass 0x0005 with the INNER hClass (params @8) set to
        // 0x79, where CUDA allocates under 0x0079 directly. Both hand RM an
        // fd on /dev/nvidia0 -- dumped with an LD_PRELOAD that reads `data`
        // BEFORE the call, which matters because RM overwrites the field in
        // place with a kernel pointer (osUserHandleToKernelPtr,
        // event_api.c:173-183). Reading it afterwards shows 0xffff8c.. and
        // hides the fd entirely.
        //
        // Untranslated the guest gets NV_ERR_OBJECT_NOT_FOUND (0x57) and
        // NVIDIA's Vulkan reports "Failed to allocate semaphore event".
        0x0005 => 16,

        _ => return None,
    })
}

/// Guard for `alloc_fd_field`: `(offset, value)` -- translate the fd only
/// when the u32 at `offset` in the params buffer equals `value`.
///
/// `None` = no condition.
///
/// The event classes need one, because NV0005_ALLOC_PARAMETERS reuses
/// `data` (@16) for two different things and says which by `hClass` (@8):
/// an fd for `NV01_EVENT_OS_EVENT` (0x79), a callback pointer for
/// `NV01_EVENT_KERNEL_CALLBACK`(_EX). Only the first may be translated, and
/// the outer class does not say which one it is -- 0x0005 appears with an
/// fd (Vulkan) and could appear with a pointer.
pub fn alloc_fd_guard(hclass: u32) -> Option<(u32, u32)> {
    match hclass {
        // hClass @8 == NV01_EVENT_OS_EVENT (cl0005.h:40-47, cl0000.h)
        0x0005 | 0x0079 => Some((8, 0x0079)),
        _ => None,
    }
}

// ===========================================================================
// SECOND-level embedded pointers
// ===========================================================================
// NVOS54.params points at a buffer that can ITSELF contain P64 pointers.
// The first level is generic (paramsSize is self-describing); the second
// is NOT -- which field is a pointer, and where its length sits, cannot be
// derived from C headers. That is exactly why nvproxy has a handler per
// control command.
//
// Without an annotation RM gets a guest VA and answers with
// NV_ERR_INVALID_PARAM_STRUCT (0x3a) or NV_ERR_INVALID_ADDRESS (0x1e) --
// both codes are the signature of a missing entry here.

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

/// The commands that carry an annotation below.
///
/// `nested_ptrs` is a function over a 32-bit key space and therefore not
/// enumerable; the serializer for the guest module (`table.rs`) needs the
/// list to fill the table. It therefore stands right here, next to the
/// entries, and `table::build()` checks that every entry here really does
/// have an annotation. Whoever adds a command below adds it here too.
pub fn nested_cmds() -> &'static [u32] {
    &[
        0x101, 0x20801802, 0x20801201, 0x80170d, 0x800201, 0x801b01,
        0x801301, 0x801102, 0x801701, 0x801104, 0x20801301, 0x20800123,
        0x410110, 0x20800802,
    ]
}

/// RM_CONTROL commands that are **never** forwarded from the guest.
///
/// Measured: both write fields the HOST assigns, and both are
/// NON_PRIVILEGED -- `accessRight` 0, flags `0x10109` in the export table
/// (`g_client_resource_nvoc.c:1935-1944`):
///
///  - `NV0000_CTRL_CMD_SET_SUB_PROCESS_ID` (0x901, `ctrl0000proc.h:93`).
///    The implementation (`client_resource.c:4856-4872`) writes
///    `pClient->SubProcessID` without any check. Since the host assigns the
///    IDs, a guest process could use it to pose as a different one -- or
///    lift itself into domain `GUEST_KERNEL` via `KERNEL_PID`
///    (`kernel_fifo.c:748-757`).
///  - `NV0000_CTRL_CMD_DISABLE_SUB_PROCESS_USERD_ISOLATION` (0x902,
///    `ctrl0000proc.h:95`). Switches off precisely the USERD separation the
///    IDs bring in the first place (`kernel_fifo.c:508-511`).
///
/// Enforced in the HOST (`session.rs`, `on_ioctl`) -- that is the boundary
/// that counts. The module gets the same list through the descriptor table
/// (`CF_BLOCK`) and saves itself the trip; a guest that ignores the table
/// runs into the host block anyway.
pub fn blocked_ctrls() -> &'static [u32] {
    &[0x901, 0x902]
}

/// Is this control blocked? One place, two consumers (host + table).
pub fn ctrl_blocked(cmd: u32) -> bool {
    blocked_ctrls().contains(&cmd)
}

/// Where a process-local fd sits inside this RM_CONTROL's params buffer.
///
/// `None` = no fd, which is the normal case.
///
/// WHY THIS EXISTS. A file descriptor is a **process** resource. The guest
/// sends its own number; on the host the same number names a different file
/// or none at all, and RM answers `NV_ERR_INVALID_PARAMETER` (0x3b). That is
/// the same family as `NV_ESC_REGISTER_FD` and as `ClassDesc.fd_off`, only
/// one level deeper: the fd is inside the *control params*, behind
/// NVOS54.params, not in the inline struct.
///
/// Measured (2026-08-06): NVIDIA's EGL is the
/// first consumer on this rig to use one. With the whole NVIDIA GL stack
/// staged into the guest, `eglInitialize` fails on every platform, and the
/// backend logs exactly one failure for the whole run --
/// `nr 0x2a cmd 0x3d05 status 0x3b`. A CUDA run makes no `0x3d..` control at
/// all, so nothing on the compute path takes this branch.
///
/// FOUR bytes. These are all `NvS32` (`ctrl0000unix.h`), unlike
/// `alloc_fd_field`, which names an `NvP64`.
pub fn ctrl_fd_offset(cmd: u32) -> Option<u32> {
    match cmd {
        // NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD (ctrl0000unix.h:147),
        // params :151-155:
        //   { EXPORT_OBJECT object @0 (type u32, hDevice, hParent, hObject)
        //     NvS32 fd @16  /* IN/OUT */ ; NvU32 flags @20 } = 24
        // The guest trace agrees: psize 0x18.
        //
        // Measured natively with an LD_PRELOAD that dumps the params: RMAPI
        // passes an ALREADY OPEN fd on /dev/nvidiactl (type=1 _TYPE_RM,
        // flags=0 i.e. EMPTY_FD_FALSE), and RM leaves the value untouched on
        // return. So it is an IN parameter at the ioctl level, and the guest
        // keeps its own number.
        0x3d05 => Some(16),

        // NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD (:181), params
        // :185-188: { NvS32 fd @0 /* IN */ ; EXPORT_OBJECT object @4 } = 20.
        // The counterpart of 0x3d05 -- an export nothing can import is not
        // worth having, and it is the same struct read the other way round.
        0x3d06 => Some(0),

        // NOT annotated on purpose, and each for its own reason:
        //   0x3d04 GET_CONTROL_FILE_DESCRIPTOR -- the fd is an OUT. RM opens
        //          it on the HOST, and handing a host fd number to the guest
        //          is a different problem from translating one that exists on
        //          both sides. It needs a new fd in the guest, not a lookup.
        //   0x3d08 GET_EXPORT_OBJECT_INFO (fd @0), 0x3d0b EXPORT_OBJECTS_TO_FD
        //          (fd @0), 0x3d0c IMPORT_OBJECTS_FROM_FD (fd @0) -- plain IN
        //          fds and mechanically identical to 0x3d06, but no run on
        //          this rig has made one. They go in when a trace shows them.
        //   0x3d0a CREATE_EXPORT_OBJECT_FD -- deprecated in the header itself.
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
        // NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION (ctrl0000system.h).
        // { SizeOfStrings u32 @0, Pad[4], pDriverVersionBuffer @8,
        //   pVersionBuffer @16, pTitleBuffer @24, ChangelistNumber @32,
        //   OfficialChangelistNumber @36 } = 40 B.
        // All three buffers are SizeOfStrings bytes. That is the
        // "KMD Version" line in nvidia-smi.
        0x101 => &[
            NestedPtr { ptr_off: 8,  len: LenSource::Field { off: 0, elem: 1 } },
            NestedPtr { ptr_off: 16, len: LenSource::Field { off: 0, elem: 1 } },
            NestedPtr { ptr_off: 24, len: LenSource::Field { off: 0, elem: 1 } },
        ],

        // NV2080_CTRL_CMD_BUS_GET_INFO (legacy, not in gVisor) -- the
        // "Bus-Id" column. The pattern of every *_GET_INFO control:
        // { listSize u32 @0, pad 4, list P64 @8 }, elements of 8 bytes
        // (NVXXXX_CTRL_XXX_INFO { index u32, data u32 }). Confirmed against
        // ctrl2080bus.h:583-586 (busInfoListSize is "the number of entries",
        // busInfoList an NV_DECLARE_ALIGNED NvP64, NV2080_CTRL_BUS_INFO is
        // the 8-byte pair).
        0x20801802 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 8 } },
        ],

        // NV2080_CTRL_CMD_BIOS_GET_INFO (ctrl2080bios.h:71, params :73-76).
        // { biosInfoListSize u32 @0; pad 4; NV_DECLARE_ALIGNED(biosInfoList
        // NvP64, 8) @8 } = 16, and the guest trace agrees (psize 0x10).
        // Elements are NV2080_CTRL_BIOS_INFO, which is the
        // NVXXXX_CTRL_XXX_INFO { index u32; data u32 } pair again
        // (ctrl2080bios.h:39) -- 8 bytes, the same shape as BUS_GET_INFO
        // above and read out of its own header rather than inherited from
        // it.
        //
        // Found by the guest sweep on 2026-08-20, and by nothing before it.
        // `nvidia-smi -q` PASSES in a guest without this entry and prints a
        // plausible report; this one control inside it answers 0x1e
        // NV_ERR_INVALID_ADDRESS, because RM was handed a guest VA. The
        // recorded answers say it outright -- biosInfoListSize 2 on both
        // sides, and a pointer that is 0x7ffe0a3f95c0 natively and
        // 0x7fff4db34730 in the guest. A workload that succeeds while one of
        // its calls is refused is exactly the case a status-code gate
        // cannot see (OPEN-QUESTIONS number 51).
        0x20800802 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 8 } },
        ],

        // NV0041_CTRL_CMD_GET_SURFACE_INFO (ctrl0041.h:279, params :283-286).
        // { surfaceInfoListSize u32 @0; pad 4; surfaceInfoList NvP64 @8 }.
        // Elements are NVXXXX_CTRL_XXX_INFO { index u32; data u32 } = 8, the
        // same typedef chain as GR_GET_INFO below.
        //
        // Reached for the first time by the VIRTUAL DISPLAY: NVKMS asks it
        // while building the display colour lookup table, and without this
        // entry RM gets a guest pointer and answers 0x1e -- measured
        // 2026-08-08 as "Failed to allocate memory for the display color
        // lookup table."
        0x410110 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 8 } },
        ],

        // NV2080_CTRL_CMD_GR_GET_INFO (ctrl2080gr.h:408, params :412-416).
        // { grInfoListSize u32 @0; pad 4; grInfoList NvP64 @8;
        //   grRouteInfo @16 } = 32. List elements are NVXXXX_CTRL_XXX_INFO
        // { index u32; data u32 } = 8 (ctrlxxxx.h:71-74, via
        // NV2080_CTRL_GR_INFO typedef chain ctrl2080gr.h:154 ->
        // ctrl0080gr.h:99). Status was 0x1e under the retired LD_PRELOAD
        // shim, 0x0 in the direct trace.
        0x20801201 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 8 } },
        ],

        // NV0080_CTRL_CMD_FIFO_GET_CHANNELLIST (ctrl0080fifo.h:178, params
        // :181-185). { numChannels u32 @0; pad 4; pChannelHandleList NvP64
        // @8; pChannelList NvP64 @16 } = 24. Both lists have numChannels
        // elements of 4 bytes (NvHandle / NvU32 channel ID). Status was
        // 0x1e under the retired LD_PRELOAD shim, 0x0 in the direct trace.
        0x80170d => &[
            NestedPtr { ptr_off: 8,  len: LenSource::Field { off: 0, elem: 4 } },
            NestedPtr { ptr_off: 16, len: LenSource::Field { off: 0, elem: 4 } },
        ],

        // NV0080_CTRL_CMD_GPU_GET_CLASSLIST (ctrl0080gpu.h:70, params :74-77).
        // { numClasses u32 @0; NV_DECLARE_ALIGNED(classList NvP64, 8) @8 }
        // = 16, and the guest trace agrees (psize 0x10). One 32-bit class
        // number per entry, so numClasses * 4.
        //
        // The header spells out the two-call pattern that is the whole
        // diagnosis: "If the classList pointer is NULL, then this command
        // returns the number of classes [...] If the classList pointer is
        // non-NULL, then this command returns the set of supported class
        // numbers". Measured in the guest, the first call passed and the
        // second returned 0x1e (NV_ERR_INVALID_ADDRESS) -- RM was handed a
        // guest VA.
        //
        // Two independent things sit behind this one entry: the encoder
        // asks which classes exist before opening a session (0xc4b7), and
        // NVKMS picks its display HAL from the same list
        // (nvkms-rm.c:3692).
        0x800201 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 4 } },
        ],

        // NV0080_CTRL_CMD_NVENC_GET_CAPS (ctrl0080nvenc.h:59, params :65-68).
        // { capsTblSize u32 @0; NV_DECLARE_ALIGNED(capsTbl NvP64, 8) @8 }
        // = 16. capsTblSize is "the size in bytes of the caps table"
        // (:47-48), i.e. already a byte count -- hence elem 1, not 4. The
        // table itself is NV0080_CTRL_NVENC_CAPS_TBL_SIZE = 6 bytes.
        //
        // Never reached in the guest so far: it sits behind 0x800201, which
        // failed first.
        0x801b01 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 1 } },
        ],

        // ---- the *_GET_CAPS / *_GET_INFO family on the DEVICE class ----
        // All four came out of one guest run of NVIDIA's EGL (2026-08-06):
        // each returned 0x1e NV_ERR_INVALID_ADDRESS, one after the
        // next, because RM was handed a guest VA. Every shape below is read
        // out of the header it belongs to, not inferred from the one above
        // it -- the family looks uniform and the length SEMANTICS are not
        // (bytes here, element count there).

        // NV0080_CTRL_CMD_FB_GET_CAPS (ctrl0080fb.h:60, params :63-66).
        // { capsTblSize u32 @0; NV_DECLARE_ALIGNED(capsTbl NvP64, 8) @8 }
        // = 16. The header: "the size in BYTES of the caps table" -- so
        // elem 1. NV0080_CTRL_FB_CAPS_TBL_SIZE is 3 (:99).
        0x801301 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 1 } },
        ],

        // NV0080_CTRL_CMD_GR_GET_CAPS (ctrl0080gr.h, params right after).
        // Same shape, same byte semantics; NV0080_CTRL_GR_CAPS_TBL_SIZE is
        // 23 (:78).
        0x801102 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 1 } },
        ],

        // NV0080_CTRL_CMD_FIFO_GET_CAPS (ctrl0080fifo.h).
        // NV0080_CTRL_FIFO_CAPS_TBL_SIZE is 2 (:95). Bytes again.
        0x801701 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 1 } },
        ],

        // NV0080_CTRL_CMD_GR_GET_INFO (ctrl0080gr.h:72).
        // NOT bytes. The header is explicit: "grInfoListSize [...] the
        // NUMBER of entries on the caller's grInfoList", and the buffer
        // "must be at least as big as grInfoListSize multiplied by the size
        // of the NV0080_CTRL_GR_INFO structure". That structure is
        // NVXXXX_CTRL_XXX_INFO (:99) = { index u32; data u32 } = 8 -- the
        // same element the already-annotated 0x20801201 uses.
        0x801104 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 8 } },
        ],

        // NV2080_CTRL_CMD_FB_GET_INFO (ctrl2080fb.h:480, params :483-486).
        // { fbInfoListSize u32 @0; NV_DECLARE_ALIGNED(fbInfoList NvP64, 8)
        //   @8 } = 16. The header again spells out the semantics: "the
        // NUMBER of entries", buffer "at least as big as fbInfoListSize
        // multiplied by the size of the NV2080_CTRL_FB_INFO structure", and
        // that structure is NVXXXX_CTRL_XXX_INFO (:315) = 8 bytes.
        //
        // This one closes the list for NVIDIA's EGL: of the ~60 distinct
        // RM controls that workload makes natively, exactly SEVEN carry an
        // NvP64 -- 0x202, 0x801102, 0x801104, 0x801301, 0x801701, 0x20801201
        // and this one. Everything else is flat or a _V2 that embeds its
        // table.
        0x20801301 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 8 } },
        ],

        // NV2080_CTRL_CMD_GPU_GET_ENGINES (ctrl2080gpu.h, params right
        // after). { engineCount u32 @0; NV_DECLARE_ALIGNED(engineList
        // NvP64, 8) @8 } = 16.
        //
        // elem 4, not 8, and the header says why: "a pointer to a buffer
        // of NvU32 values". Not the NVXXXX_CTRL_XXX_INFO pair the other
        // *_GET_INFO entries above use -- the _V2 variant confirms it by
        // embedding `NvU32 engineList[...]` directly. The same two-call
        // pattern as 0x800201: NULL pointer first to learn the count.
        //
        // Found by NVIDIA's VULKAN driver, not by EGL: vkCreateDevice
        // fails without it (status 0x1e), and it is the only NvP64-carrying
        // control vulkaninfo adds to the seven EGL needs.
        0x20800123 => &[
            NestedPtr { ptr_off: 8, len: LenSource::Field { off: 0, elem: 4 } },
        ],

        // NOT annotated on purpose: NV0000_CTRL_CMD_GPU_GET_ID_INFO (0x202)
        // carries an NvP64 too, but measured it is NULL in every call this
        // workload makes -- 6 times status 0x0 in the guest AND natively.
        // An entry with a guessed length source is worse than no entry: it
        // turns a clean ENOTSUP into an out-of-bounds read in the driver's
        // copy_from_user. It goes in when a run shows it with a non-NULL
        // pointer, and not before.
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

    /// NV2080_CTRL_BIOS_GET_INFO_PARAMS (ctrl2080bios.h:73): biosInfoListSize
    /// u32 @0, biosInfoList NvP64 @8, elements NV2080_CTRL_BIOS_INFO =
    /// NVXXXX_CTRL_XXX_INFO { index u32; data u32 } = 8 bytes.
    ///
    /// The `elem` is the half worth a test of its own: 4 would truncate the
    /// list and hand RM half a buffer, 16 would read past the guest's. The
    /// value comes from the typedef in the header, and this is where that
    /// reading is written down.
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
        assert_eq!(unsafe { specs[0].len.resolve(params.as_ptr(), 16) }, Some(16));
    }

    /// NV0080_CTRL_GPU_GET_CLASSLIST_PARAMS (ctrl0080gpu.h:74):
    /// numClasses u32 @0, classList NvP64 @8 (8-aligned), 4 bytes per class.
    ///
    /// The offsets are read out of the vendor header, not guessed, and this
    /// test is where that reading is written down. Getting `elem` wrong is
    /// the dangerous half: too small truncates the list and the encoder
    /// concludes there is no encoder; too large reads past the guest's
    /// buffer.
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

    /// NV0080_CTRL_NVENC_GET_CAPS_PARAMS (ctrl0080nvenc.h:65):
    /// capsTblSize u32 @0, capsTbl NvP64 @8. capsTblSize is a BYTE count
    /// ("the size in bytes of the caps table"), so elem is 1 -- the one
    /// place this command differs from the *_GET_INFO family next to it,
    /// and the easy thing to copy wrong.
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
        assert_eq!(unsafe { specs[0].len.resolve(params.as_ptr(), 16) }, Some(6));
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

    /// The nr range the scan below covers: UVM (the unified-memory driver
    /// behind /dev/nvidia-uvm) numbers its commands from 1 upwards -- the
    /// dense range tops out at 81 today, and the one outlier,
    /// `UVM_IOCTL_BASE(2047)`, sits apart and is not implemented here
    /// (`uvm_ioctl.h`). The range reaches that outlier so that the day it
    /// gains a size it is inside the scan rather than outside it.
    /// `table.rs` scans the same range plus the two 0x3000_000x values
    /// from uvm_linux_ioctl.h; the two constants must stay equal.
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
            row!(uvm::PAGEABLE_MEM_ACCESS, sys::UVM_PAGEABLE_MEM_ACCESS_PARAMS),
            row!(uvm::MM_INITIALIZE, sys::UVM_MM_INITIALIZE_PARAMS),
            row!(uvm::REGISTER_GPU_VASPACE, sys::UVM_REGISTER_GPU_VASPACE_PARAMS),
            row!(uvm::UNREGISTER_GPU_VASPACE, sys::UVM_UNREGISTER_GPU_VASPACE_PARAMS),
            row!(uvm::REGISTER_CHANNEL, sys::UVM_REGISTER_CHANNEL_PARAMS),
            row!(uvm::UNREGISTER_CHANNEL, sys::UVM_UNREGISTER_CHANNEL_PARAMS),
            row!(uvm::MAP_EXTERNAL_ALLOCATION, sys::UVM_MAP_EXTERNAL_ALLOCATION_PARAMS),
            row!(uvm::FREE, sys::UVM_FREE_PARAMS),
            row!(uvm::REGISTER_GPU, sys::UVM_REGISTER_GPU_PARAMS),
            row!(
                uvm::MAP_DYNAMIC_PARALLELISM_REGION,
                sys::UVM_MAP_DYNAMIC_PARALLELISM_REGION_PARAMS
            ),
            row!(uvm::ALLOC_SEMAPHORE_POOL, sys::UVM_ALLOC_SEMAPHORE_POOL_PARAMS),
            row!(uvm::PAGEABLE_MEM_ACCESS_ON_GPU, sys::UVM_PAGEABLE_MEM_ACCESS_ON_GPU_PARAMS),
            row!(uvm::SET_PREFERRED_LOCATION, sys::UVM_SET_PREFERRED_LOCATION_PARAMS),
            row!(uvm::UNSET_PREFERRED_LOCATION, sys::UVM_UNSET_PREFERRED_LOCATION_PARAMS),
            row!(uvm::ENABLE_READ_DUPLICATION, sys::UVM_ENABLE_READ_DUPLICATION_PARAMS),
            row!(uvm::DISABLE_READ_DUPLICATION, sys::UVM_DISABLE_READ_DUPLICATION_PARAMS),
            row!(uvm::SET_ACCESSED_BY, sys::UVM_SET_ACCESSED_BY_PARAMS),
            row!(uvm::UNSET_ACCESSED_BY, sys::UVM_UNSET_ACCESSED_BY_PARAMS),
            row!(uvm::MIGRATE, sys::UVM_MIGRATE_PARAMS),
            row!(uvm::VALIDATE_VA_RANGE, sys::UVM_VALIDATE_VA_RANGE_PARAMS),
            row!(uvm::CREATE_EXTERNAL_RANGE, sys::UVM_CREATE_EXTERNAL_RANGE_PARAMS),
        ]
    }

    /// Every size in `uvm_param_size` against `size_of` of the bindgen
    /// struct from the vendor header it names in its comment.
    ///
    /// The numbers there were added up by hand, field by field, and nothing
    /// in the build checks them: UVM commands carry no `_IOC` size (the
    /// request number is a raw number, not an `_IOC` encoding), so this
    /// function is the ONLY place the payload length of a forwarded UVM
    /// call comes from. Too small truncates the guest's parameters; too
    /// large is an out-of-bounds read in the host driver's
    /// `copy_from_user`.
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

    /// UVM_DEINITIALIZE takes no parameter struct at all -- there is no
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
    /// hand-transcribed number again -- which is the exact failure this
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

    /// Every UVM fd field offset against `offset_of!` of the field it
    /// names. The fd (file descriptor) is process-local: the guest sends
    /// its own number and the host rewrites it in place at exactly this
    /// offset. A wrong offset corrupts a neighbouring field of a struct RM
    /// then acts on, and RM answers with a status rather than a crash --
    /// i.e. it fails far away from the cause.
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
        want(uvm::MM_INITIALIZE, offset_of!(sys::UVM_MM_INITIALIZE_PARAMS, uvmFd) as u32);
        want(
            uvm::REGISTER_GPU_VASPACE,
            offset_of!(sys::UVM_REGISTER_GPU_VASPACE_PARAMS, rmCtrlFd) as u32,
        );
        want(
            uvm::REGISTER_CHANNEL,
            offset_of!(sys::UVM_REGISTER_CHANNEL_PARAMS, rmCtrlFd) as u32,
        );
        want(uvm::REGISTER_GPU, offset_of!(sys::UVM_REGISTER_GPU_PARAMS, rmCtrlFd) as u32);
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
        for cmd in [uvm::INITIALIZE, uvm::DEINITIALIZE, uvm::FREE, uvm::MIGRATE,
                    uvm::UNREGISTER_CHANNEL, uvm::VALIDATE_VA_RANGE] {
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
        assert!(buf.len() >= size as usize, "the test buffer must cover `size`");
        // SAFETY: the assert above guarantees `buf` is valid for `size` bytes.
        unsafe { embedded_ptr(dev, nr, buf.as_ptr(), size) }
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
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_CONTROL, &nvos54(0, 128), 32), Ok(None));
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_CONTROL, &nvos54(0xdead_beef, 0), 32), Ok(None));
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_CONTROL, &nvos54(0, 0), 32), Ok(None));
    }

    /// RM_CONTROL is self-describing: the params buffer sits behind the
    /// pointer at 16 and its length is the `paramsSize` field at 24. This
    /// is the one escape where the guest states the length itself, and the
    /// host must use exactly that number.
    #[test]
    fn rm_control_takes_the_length_from_paramssize() {
        for len in [1u32, 4, 24, 1234, 16384] {
            assert_eq!(
                probe(Dev::Ctl, sys::NV_ESC_RM_CONTROL, &nvos54(0xdead_beef, len), 32),
                Ok(Some((16, len))),
                "paramsSize {len}"
            );
        }
        // The same on a per-GPU node, not just on /dev/nvidiactl.
        assert_eq!(
            probe(Dev::Gpu, sys::NV_ESC_RM_CONTROL, &nvos54(0xdead_beef, 40), 32),
            Ok(Some((16, 40)))
        );
    }

    /// A class that allocates with `pAllocParms == NULL` (ROOT_CLIENT,
    /// USERMODE) has no second buffer -- and no need for the hClass table
    /// either, which is why classes with no params need no entry there.
    #[test]
    fn rm_alloc_with_a_null_params_pointer_carries_nothing() {
        // 0xdead is not in the hClass table; the null pointer must be
        // decided BEFORE the class is looked up, otherwise every
        // parameterless alloc of an unlisted class would fail.
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(0xdead, 0, 0), 48), Ok(None));
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(0x0041, 0, 0), 48), Ok(None));
    }

    /// RM_ALLOC is NOT self-describing: the length comes from the hClass at
    /// 12 via `alloc_param_size`. An unknown class must fail loudly
    /// (`Err`), because any guess is either a truncated copy or an
    /// out-of-bounds read in the host driver's `copy_from_user`.
    #[test]
    fn rm_alloc_takes_the_length_from_the_hclass_table() {
        for hclass in [0x0080u32, 0x2080, 0x0079, 0x90f1, 0xc46f] {
            let want = alloc_param_size(hclass).expect("probe class missing from the table");
            assert_eq!(
                probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(hclass, 0xdead_beef, 0), 48),
                Ok(Some((16, want))),
                "hClass {hclass:#x}"
            );
        }
    }

    /// An hClass the table does not know must be `Err(())` -- the caller
    /// turns that into ENOTSUP. Silently forwarding it would hand RM a
    /// guest address, or copy a guessed number of bytes.
    #[test]
    fn rm_alloc_of_an_unknown_hclass_fails_loudly() {
        assert_eq!(alloc_param_size(0xdead), None, "the probe class must stay unknown");
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(0xdead, 0xbeef, 0), 48), Err(()));
    }

    /// `pRightsRequested` (NVOS64 @24) is not supported. It appears in no
    /// measured run, and it points at yet another buffer that nothing
    /// translates -- so a non-null value must fail rather than be ignored.
    #[test]
    fn rm_alloc_refuses_a_non_null_rights_pointer() {
        let known = 0x2080u32; // a class the table knows
        assert_eq!(
            probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(known, 0xdead_beef, 1), 48),
            Err(()),
            "pRightsRequested != 0 must not be forwarded"
        );
        // ... and the check happens only in the 48-byte form (below).
    }

    /// The 32-byte NVOS21 form of RM_ALLOC is accepted exactly like the
    /// 48-byte NVOS64 one. It has no rights pointer at all: what sits at 24
    /// there is `paramsSize`, so a non-zero word must NOT be read as
    /// "rights requested" and rejected. Matching only `size >= 48` would
    /// let this form fall through unnoticed, and RM would then get a guest
    /// address instead of a pointer into the host's aux buffer.
    #[test]
    fn the_32_byte_nvos21_form_is_accepted_like_the_48_byte_one() {
        let hclass = 0x2080u32;
        let want = alloc_param_size(hclass).unwrap();
        // Word at 24 is paramsSize here, deliberately non-zero.
        let buf = nvos64(hclass, 0xdead_beef, 4);
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &buf, 32), Ok(Some((16, want))));
        // Null params and unknown class behave the same way in both forms.
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(hclass, 0, 4), 32), Ok(None));
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_ALLOC, &nvos64(0xdead, 1, 4), 32), Err(()));
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
    /// ONE `NvUnixEvent`, which RM writes. The length is a constant -- it
    /// cannot be read out of the guest's buffer -- so it must be
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
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_GET_EVENT_DATA, &zero, size), Ok(None));
        // A payload too short to hold NVOS41 is not decoded at all -- the
        // pointer field would be read past the end of the guest's buffer.
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_GET_EVENT_DATA, &buf, size - 1), Ok(None));
    }

    /// No UVM call carries an embedded pointer: the big per-GPU attribute
    /// arrays of MAP_EXTERNAL_ALLOCATION and ALLOC_SEMAPHORE_POOL are
    /// inline, not pointed to. The dev check comes FIRST, so a UVM nr that
    /// happens to collide with a frontend escape (0x27 is both
    /// NV_ESC_RM_ALLOC_MEMORY and UVM_PAGEABLE_MEM_ACCESS) is never
    /// decoded as one.
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
        assert_eq!(probe(Dev::Ctl, sys::NV_ESC_RM_MAP_MEMORY, &buf, 56), Ok(None));
        assert_eq!(probe(Dev::Ctl, crate::nvgpu::NV_ESC_REGISTER_FD, &buf, 4), Ok(None));
    }
}

#[cfg(test)]
mod alloc_fd_tests {
    use super::*;

    /// The highest hClass the scan covers: class numbers are 16 bit
    /// (`resource_list.h`), so this is exhaustive rather than a sample.
    const CLASS_MAX: u32 = 0xffff;

    /// `alloc_fd_field` (where the fd sits) and `alloc_fd_guard` (when it
    /// may be translated at all) must name the same set of classes. A class
    /// with an offset but no guard would have its `data` field rewritten
    /// even when it holds a kernel CALLBACK pointer rather than an fd
    /// (NV0005_ALLOC_PARAMETERS reuses the field, keyed on the inner
    /// hClass); a class with a guard but no offset is a dead guard.
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
                // NV0005_ALLOC_PARAMETERS.data @16 (cl0005.h:40-47) -- the
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

    /// `ctrl_fd_cmds()` is the hand-maintained enumeration of
    /// `ctrl_fd_offset`'s match arms -- the table builder cannot enumerate a
    /// function over a 32-bit key space, so the list stands beside it. Every
    /// entry must really have an offset, or `table::collect()` panics at
    /// host start.
    #[test]
    fn every_named_control_really_carries_an_fd() {
        for &cmd in ctrl_fd_cmds() {
            assert!(
                ctrl_fd_offset(cmd).is_some(),
                "{cmd:#x} is in ctrl_fd_cmds() but ctrl_fd_offset() has nothing for it"
            );
        }
        assert!(!ctrl_fd_cmds().is_empty(), "an empty list would make this test vacuous");
    }

    /// The two offsets themselves, read out of ctrl0000unix.h and pinned
    /// here. Both are `NvS32` fields inside the RM_CONTROL params buffer --
    /// one level deeper than the fd fields of the inline struct.
    ///
    ///  - 0x3d05 EXPORT_OBJECT_TO_FD: `{ EXPORT_OBJECT object @0; NvS32 fd
    ///    @16; NvU32 flags @20 }`
    ///  - 0x3d06 IMPORT_OBJECT_FROM_FD: `{ NvS32 fd @0; EXPORT_OBJECT
    ///    object @4 }` -- the same struct read the other way round, hence
    ///    offset 0 and not 16.
    ///
    /// Swapping the two is the mistake this pins: the guest's fd number
    /// would be written over `object.type`, and RM answers
    /// NV_ERR_INVALID_PARAMETER (0x3b).
    #[test]
    fn the_two_known_controls_keep_their_offsets() {
        assert_eq!(ctrl_fd_offset(0x3d05), Some(16));
        assert_eq!(ctrl_fd_offset(0x3d06), Some(0));
        assert_eq!(ctrl_fd_cmds(), &[0x3d05, 0x3d06]);
    }

    /// The neighbouring 0x3d.. controls are deliberately NOT annotated,
    /// each for a reason spelled out at `ctrl_fd_offset`: 0x3d04 hands back
    /// an fd RM opened on the HOST (a different problem), and 0x3d08 /
    /// 0x3d0b / 0x3d0c have simply never been seen on this rig. An
    /// annotation added on a guess would rewrite a field nothing verified.
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
