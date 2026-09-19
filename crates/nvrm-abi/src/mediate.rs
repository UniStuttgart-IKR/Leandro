// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Describe fields rewritten by the guest or backend for trace comparison.
//!
//! `verify` permits differences within these fields and reports differences
//! outside them. Offsets come from `xlate`, vendor structs and the BDF tables
//! shared with `nvrm-genhdr`. Backend mediation also uses these offsets, so
//! the manifest and implementations describe the same fields.

use core::mem::{offset_of, size_of};

use nvrm_sys as sys;
use sys::RmAbi;

use crate::xlate;

/// `NV_ESC_RM_CONTROL`. Spelled here rather than reached for through `sys`
/// so that the manifest's own notion of "this is a control" is one name.
const NR_RM_CONTROL: u32 = 0x2a;

/// What kind of rewriting happens in a field.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A gpuId at a fixed offset: the host's id in, the guest's id out.
    BdfScalar,
    /// An array of gpuIds, `stride` bytes apart, `count` of them.
    BdfArray,
    /// The PCI ADDRESS itself; domain, bus, slot. Not the same thing as a
    /// gpuId even though one is derived from the other, and it is rewritten
    /// by a separate step in the guest module.
    BdfAddress,
    /// An `NvP64` the mediation walks: it points into the caller's address
    /// space, so it is a different number on the two sides by construction.
    NestedPtr,
    /// A process-local file descriptor inside the params buffer.
    CtrlFd,
    /// The backend writes this field itself rather than forwarding RM's
    /// answer; the VRAM ledger and the process list.
    BackendAnswered,
    /// The card's own name.
    IdentityString,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::BdfScalar => "bdf-scalar",
            Kind::BdfArray => "bdf-array",
            Kind::BdfAddress => "bdf-address",
            Kind::NestedPtr => "nested-ptr",
            Kind::CtrlFd => "ctrl-fd",
            Kind::BackendAnswered => "backend-answered",
            Kind::IdentityString => "identity-string",
        }
    }
}

/// One rewritten field of one command.
#[derive(Copy, Clone, Debug)]
pub struct Mediated {
    /// Escape number. CARD_INFO carries its fields inline rather than in a control.
    pub nr: u32,
    /// The control command, when `nr` is RM_CONTROL. Meaningless otherwise,
    /// and `sig()` is what a caller should use.
    pub cmd: u32,
    /// Offset of the field IN THE PARAMS BUFFER.
    pub off: u32,
    /// Length of ONE element in bytes.
    pub len: u32,
    /// Bytes between elements; 0 when the field is not an array.
    pub stride: u32,
    /// Number of elements; 0 when the field is not an array.
    pub count: u32,
    pub kind: Kind,
    /// The member's name in the vendor struct, so a reader can look it up.
    pub field: &'static str,
    /// Why this field is rewritten, in one line.
    pub why: &'static str,
}

impl Mediated {
    /// Catalogue signature, currently keyed to the control node.
    /// Mediation on other nodes would require a device field in this manifest.
    pub fn sig(&self) -> String {
        if self.nr == NR_RM_CONTROL {
            format!("ctl {:#x} {:#x}", self.nr, self.cmd)
        } else {
            format!("ctl {:#x} -", self.nr)
        }
    }

    /// The byte range this field covers, array included.
    pub fn end(&self) -> u32 {
        if self.count > 1 && self.stride > 0 {
            self.off + self.stride * (self.count - 1) + self.len
        } else {
            self.off + self.len
        }
    }
}

// GPU address fields shared by the manifest and generated C header.
// Include legacy and V2 controls: an untranslated ID can prevent NVKMS
// initialization. Offsets come from SDK structs.

/// `(name, cmd, offset)`; one gpuId at a fixed offset.
pub fn bdf_scalars() -> &'static [(&'static str, u32, usize)] {
    &[
        (
            "GET_ID_INFO",
            sys::NV0000_CTRL_CMD_GPU_GET_ID_INFO,
            offset_of!(sys::NV0000_CTRL_GPU_GET_ID_INFO_PARAMS, gpuId),
        ),
        (
            "GET_ID_INFO_V2",
            sys::NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2,
            offset_of!(sys::NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS, gpuId),
        ),
        // GET_ID_INFO also returns the ID as boardId; NVML uses that address.
        // Multiple rows may describe different fields of one command.
        (
            "GET_ID_INFO boardId",
            sys::NV0000_CTRL_CMD_GPU_GET_ID_INFO,
            offset_of!(sys::NV0000_CTRL_GPU_GET_ID_INFO_PARAMS, boardId),
        ),
        (
            "GET_ID_INFO_V2 boardId",
            sys::NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2,
            offset_of!(sys::NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS, boardId),
        ),
        (
            "GET_PCI_INFO",
            sys::NV0000_CTRL_CMD_GPU_GET_PCI_INFO,
            offset_of!(sys::NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS, gpuId),
        ),
        (
            "GET_UUID_INFO",
            sys::NV0000_CTRL_CMD_GPU_GET_UUID_INFO,
            offset_of!(sys::NV0000_CTRL_GPU_GET_UUID_INFO_PARAMS, gpuId),
        ),
        (
            "GET_UUID_FROM_GPU_ID",
            sys::NV0000_CTRL_CMD_GPU_GET_UUID_FROM_GPU_ID,
            offset_of!(sys::NV0000_CTRL_GPU_GET_UUID_FROM_GPU_ID_PARAMS, gpuId),
        ),
        (
            "MODIFY_DRAIN_STATE",
            sys::NV0000_CTRL_CMD_GPU_MODIFY_DRAIN_STATE,
            offset_of!(sys::NV0000_CTRL_GPU_MODIFY_DRAIN_STATE_PARAMS, gpuId),
        ),
        (
            "QUERY_DRAIN_STATE",
            sys::NV0000_CTRL_CMD_GPU_QUERY_DRAIN_STATE,
            offset_of!(sys::NV0000_CTRL_GPU_QUERY_DRAIN_STATE_PARAMS, gpuId),
        ),
        // NVML derives the PCI address from these attach-control IDs.
        (
            "ASYNC_ATTACH_ID",
            sys::NV0000_CTRL_CMD_GPU_ASYNC_ATTACH_ID,
            offset_of!(sys::NV0000_CTRL_GPU_ASYNC_ATTACH_ID_PARAMS, gpuId),
        ),
        (
            "WAIT_ATTACH_ID",
            sys::NV0000_CTRL_CMD_GPU_WAIT_ATTACH_ID,
            offset_of!(sys::NV0000_CTRL_GPU_WAIT_ATTACH_ID_PARAMS, gpuId),
        ),
        // Accounting query: { gpuId, pid, state }, 12 bytes.
        // Missing gpuId translation produced INVALID_ARGUMENT in the 2026-08-20
        // nvidia-smi sweep (OPEN-QUESTIONS 51), despite a successful report.
        (
            "GPUACCT_GET_ACCOUNTING_STATE",
            sys::NV0000_CTRL_CMD_GPUACCT_GET_ACCOUNTING_STATE,
            offset_of!(sys::NV0000_CTRL_GPUACCT_GET_ACCOUNTING_STATE_PARAMS, gpuId),
        ),
    ]
}

/// `(name, cmd, offset, count, stride)` for gpuId arrays.
/// GET_ACTIVE_DEVICE_IDS uses 12-byte { gpuId, gpuInstanceId,
/// computeInstanceId } entries. Translating only contiguous u32 arrays
/// misses it and prevented raytracing initialization (2026-08-15 trace).
pub fn bdf_arrays<A: RmAbi>() -> Vec<(&'static str, u32, usize, u32, usize)> {
    vec![
        (
            "GET_ATTACHED_IDS",
            sys::NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS,
            offset_of!(sys::NV0000_CTRL_GPU_GET_ATTACHED_IDS_PARAMS, gpuIds),
            sys::NV0000_CTRL_GPU_MAX_ATTACHED_GPUS,
            4,
        ),
        (
            "GET_PROBED_IDS",
            sys::NV0000_CTRL_CMD_GPU_GET_PROBED_IDS,
            offset_of!(sys::NV0000_CTRL_GPU_GET_PROBED_IDS_PARAMS, gpuIds),
            sys::NV0000_CTRL_GPU_MAX_PROBED_GPUS,
            4,
        ),
        (
            "ATTACH_IDS",
            sys::NV0000_CTRL_CMD_GPU_ATTACH_IDS,
            offset_of!(sys::NV0000_CTRL_GPU_ATTACH_IDS_PARAMS, gpuIds),
            sys::NV0000_CTRL_GPU_MAX_PROBED_GPUS,
            4,
        ),
        (
            "DETACH_IDS",
            sys::NV0000_CTRL_CMD_GPU_DETACH_IDS,
            offset_of!(sys::NV0000_CTRL_GPU_DETACH_IDS_PARAMS, gpuIds),
            sys::NV0000_CTRL_GPU_MAX_ATTACHED_GPUS,
            4,
        ),
        (
            "GET_ACTIVE_DEVICE_IDS",
            sys::NV0000_CTRL_CMD_GPU_GET_ACTIVE_DEVICE_IDS,
            offset_of!(sys::NV0000_CTRL_GPU_GET_ACTIVE_DEVICE_IDS_PARAMS, devices)
                + offset_of!(sys::NV0000_CTRL_GPU_ACTIVE_DEVICE, gpuId),
            sys::NV0000_CTRL_GPU_MAX_ACTIVE_DEVICES,
            size_of::<sys::NV0000_CTRL_GPU_ACTIVE_DEVICE>(),
        ),
        // P2P_CAPS_MATRIX contains two gpuId arrays; rewrite both matching rows.
        (
            "P2P_CAPS_MATRIX_A",
            sys::NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS_MATRIX,
            A::P2P_CAPS_MATRIX_PARAMS_OFF_gpuIdGrpA,
            sys::NV0000_CTRL_SYSTEM_MAX_P2P_GROUP_GPUS,
            4,
        ),
        (
            "P2P_CAPS_MATRIX_B",
            sys::NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS_MATRIX,
            A::P2P_CAPS_MATRIX_PARAMS_OFF_gpuIdGrpB,
            sys::NV0000_CTRL_SYSTEM_MAX_P2P_GROUP_GPUS,
            4,
        ),
    ]
}

/// PCI address fields in `NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS`.
/// `(suffix, member, offset, bytes)` generates the C rewrite offsets.
/// The guest rewrites domain/bus/slot consistently with gpuId, which RM
/// derives from the PCI address (`gpuGenerate32BitId`, gpu.c:292).
pub fn pci_info_fields() -> &'static [(&'static str, &'static str, usize, u32)] {
    &[
        (
            "PCI_INFO_GPUID",
            "gpuId",
            offset_of!(sys::NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS, gpuId),
            4,
        ),
        (
            "PCI_INFO_DOMAIN",
            "domain",
            offset_of!(sys::NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS, domain),
            4,
        ),
        (
            "PCI_INFO_BUS",
            "bus",
            offset_of!(sys::NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS, bus),
            2,
        ),
        (
            "PCI_INFO_SLOT",
            "slot",
            offset_of!(sys::NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS, slot),
            2,
        ),
    ]
}

// Offsets used by backend answer mediation and the comparison manifest.
// Derive them from vendor structs; do not keep another copy in vram.rs.

/// `NV2080_CTRL_CMD_GPU_GET_PIDS` (ctrl2080gpu.h:3501).
pub const CMD_GPU_GET_PIDS: u32 = 0x2080_018d;
/// `NV2080_CTRL_CMD_GPU_GET_PID_INFO` (ctrl2080gpu.h:3643).
pub const CMD_GPU_GET_PID_INFO: u32 = 0x2080_018e;
/// `NV2080_CTRL_CMD_FB_GET_INFO_V2` (ctrl2080fb.h:489).
pub const CMD_FB_GET_INFO_V2: u32 = 0x2080_1303;
/// `NV2080_CTRL_CMD_FB_GET_INFO` (ctrl2080fb.h:480), the V1 form; the SAME
/// index list, but the array hangs off an `NvP64` instead of sitting in the
/// params buffer (`xlate::nested_ptrs`, ptr_off 8).
pub const CMD_FB_GET_INFO: u32 = 0x2080_1301;
/// `NV0080_CTRL_CMD_GPU_GET_VIRTUALIZATION_MODE` (ctrl0080gpu.h:300).
/// The 610.57.04 catalogue records 34 calls across 20 library classes.
pub const CMD_GPU_GET_VIRTUALIZATION_MODE: u32 = 0x0080_0289;
/// `NV2080_CTRL_CMD_GPU_GET_ENCODER_CAPACITY` (ctrl2080gpu.h:2322). 22
/// calls, from `nvenc` alone.
pub const CMD_GPU_GET_ENCODER_CAPACITY: u32 = 0x2080_016c;

/// `NV0080_CTRL_GPU_GET_VIRTUALIZATION_MODE_PARAMS`: `virtualizationMode`
/// @0, `isGridBuild` @4; a `NvBool`, which is one byte.
pub const VIRTMODE_OFF: usize = offset_of!(
    sys::NV0080_CTRL_GPU_GET_VIRTUALIZATION_MODE_PARAMS,
    virtualizationMode
);
pub const VIRTMODE_GRIDBUILD_OFF: usize = offset_of!(
    sys::NV0080_CTRL_GPU_GET_VIRTUALIZATION_MODE_PARAMS,
    isGridBuild
);
pub const VIRTMODE_LEN: usize = size_of::<sys::NV0080_CTRL_GPU_GET_VIRTUALIZATION_MODE_PARAMS>();

/// `NV0080_CTRL_GPU_VIRTUALIZATION_MODE_*` (ctrl0080gpu.h:302-307). `VGX`
/// is what a vGPU GUEST reports; `HOST` is what the machine running the
/// plugin reports.
pub const VIRTUALIZATION_MODE_NONE: u32 = 0;
pub const VIRTUALIZATION_MODE_VGX: u32 = 2;

/// `NV2080_CTRL_GPU_GET_ENCODER_CAPACITY_PARAMS`: `queryType` @0 is the
/// question (H264 / HEVC / AV1) and is carried unchanged; `encoderCapacity`
/// @4 is the answer, a percentage.
pub const ENCCAP_QUERY_OFF: usize =
    offset_of!(sys::NV2080_CTRL_GPU_GET_ENCODER_CAPACITY_PARAMS, queryType);
pub const ENCCAP_OFF: usize = offset_of!(
    sys::NV2080_CTRL_GPU_GET_ENCODER_CAPACITY_PARAMS,
    encoderCapacity
);
pub const ENCCAP_LEN: usize = size_of::<sys::NV2080_CTRL_GPU_GET_ENCODER_CAPACITY_PARAMS>();

/// `NV2080_CTRL_CMD_GPU_GET_GID_INFO` (ctrl2080gpu.h:1749).
/// UUID mediation distinguishes VMs sharing one physical card.
pub const CMD_GPU_GET_GID_INFO: u32 = 0x2080_014a;

/// `NV2080_CTRL_CMD_GPU_GET_NAME_STRING` (ctrl2080gpu.h:325).
pub const CMD_GPU_GET_NAME_STRING: u32 = 0x2080_0110;

/// `NV2080_CTRL_GPU_GET_PIDS_PARAMS`: `pidTblCount` @8, `pidTbl[950]` @12.
pub const PIDS_COUNT_OFF: usize = offset_of!(sys::NV2080_CTRL_GPU_GET_PIDS_PARAMS, pidTblCount);
pub const PIDS_TBL_OFF: usize = offset_of!(sys::NV2080_CTRL_GPU_GET_PIDS_PARAMS, pidTbl);
pub const PIDS_MAX: usize = 950;
pub const PIDS_LEN: usize = size_of::<sys::NV2080_CTRL_GPU_GET_PIDS_PARAMS>();

/// `NV2080_CTRL_GPU_GET_PID_INFO_PARAMS`: `pidInfoListCount` @0,
/// `pidInfoList[200]` @8.
pub const PIDINFO_COUNT_OFF: usize =
    offset_of!(sys::NV2080_CTRL_GPU_GET_PID_INFO_PARAMS, pidInfoListCount);
pub const PIDINFO_LIST_OFF: usize =
    offset_of!(sys::NV2080_CTRL_GPU_GET_PID_INFO_PARAMS, pidInfoList);
pub const PIDINFO_MAX: usize = 200;
/// One `NV2080_CTRL_GPU_PID_INFO`: 72 bytes (ctrl2080gpu.h:3617).
pub const PIDINFO_ENTRY: usize = size_of::<sys::NV2080_CTRL_GPU_PID_INFO>();
pub const PIDINFO_LEN: usize = size_of::<sys::NV2080_CTRL_GPU_GET_PID_INFO_PARAMS>();
/// Inside one entry: `data.vidMemUsage.memPrivate` sits at the start of the
/// union, i.e. at entry offset 16.
pub const PIDINFO_MEM_PRIVATE: usize = offset_of!(sys::NV2080_CTRL_GPU_PID_INFO, data);
/// `NV2080_CTRL_GPU_PID_INFO_INDEX_VIDEO_MEMORY_USAGE` (ctrl2080gpu.h:3570).
pub const PIDINFO_INDEX_VIDEO_MEMORY_USAGE: u32 = 0;

/// `NV2080_CTRL_FB_GET_INFO_V2_PARAMS`: `fbInfoListSize` @0, then
/// `NV2080_CTRL_FB_INFO { u32 index; u32 data; }`; 1028 bytes for the
/// 128-entry maximum.
pub const FBINFO_COUNT_OFF: usize = 0;
pub const FBINFO_LIST_OFF: usize = 4;
pub const FBINFO_ENTRY: usize = size_of::<sys::NV2080_CTRL_FB_INFO>();
pub const FBINFO_MAX: usize = 128;
/// Within one entry, `data` is the half that is rewritten; `index` is the
/// question and is carried unchanged. Masking the whole entry would hide a
/// wrong index, which is a real defect and has to stay visible.
pub const FBINFO_DATA_OFF: usize = offset_of!(sys::NV2080_CTRL_FB_INFO, data);

/// Framebuffer size indexes, in KiB (ctrl2080fb.h:76-112,254-260).
/// Shared by backend answer rewriting and host vGPU profile queries.
pub const FB_INFO_INDEX_RAM_SIZE: u32 = 0x07;
pub const FB_INFO_INDEX_TOTAL_RAM_SIZE: u32 = 0x08;
pub const FB_INFO_INDEX_HEAP_SIZE: u32 = 0x09;
pub const FB_INFO_INDEX_HEAP_FREE: u32 = 0x16;
pub const FB_INFO_INDEX_USABLE_RAM_SIZE: u32 = 0x20;

/// `NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS`: `gpuNameStringFlags` @0,
/// `ascii[64]` @4 (`NV2080_GPU_MAX_NAME_STRING_LENGTH` = 64).
pub fn name_off<A: RmAbi>() -> usize {
    A::GPU_NAME_STRING_PARAMS_OFF_gpuNameString
}
/// bindgen emits the name field as a nested type of its own (the header
/// writes it as a union of `ascii` and `unicode`, of which this driver's
/// header carries only `ascii`), so its SIZE is the max name length.
pub fn name_max<A: RmAbi>() -> usize {
    size_of::<A::GpuNameStringBuffer>()
}

// The numbers the prose above quotes, so a change in the vendor headers
// fails the build here rather than turning every doc line into a lie.
const _: () = {
    assert!(PIDS_COUNT_OFF == 8 && PIDS_TBL_OFF == 12 && PIDS_LEN == 3812);
    assert!(PIDINFO_COUNT_OFF == 0 && PIDINFO_LIST_OFF == 8);
    assert!(PIDINFO_ENTRY == 72 && PIDINFO_LEN == 14408 && PIDINFO_MEM_PRIVATE == 16);
    assert!(FBINFO_ENTRY == 8 && FBINFO_DATA_OFF == 4);
};

// Default-ABI name layout. Use name_off/name_max for other ABIs;
// 580.178.04 uses a 128-byte union.
#[cfg(feature = "v610")]
const _: () = {
    assert!(
        offset_of!(
            sys::v610::NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS,
            gpuNameString
        ) == 4
    );
    assert!(size_of::<sys::v610::NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS__bindgen_ty_1>() == 64);
};

// the manifest

/// One record per rewritten field, derived from the BDF tables, xlate
/// pointer/FD descriptors and vendor struct offsets.
pub fn manifest<A: RmAbi>() -> Vec<Mediated> {
    let mut out: Vec<Mediated> = Vec::new();

    for (name, cmd, off) in bdf_scalars() {
        out.push(Mediated {
            nr: NR_RM_CONTROL,
            cmd: *cmd,
            off: *off as u32,
            len: 4,
            stride: 0,
            count: 0,
            kind: Kind::BdfScalar,
            field: name,
            why: "a gpuId: the host's card id in, this guest's id out",
        });
    }
    for (name, cmd, off, count, stride) in bdf_arrays::<A>() {
        out.push(Mediated {
            nr: NR_RM_CONTROL,
            cmd,
            off: off as u32,
            len: 4,
            stride: stride as u32,
            count,
            kind: Kind::BdfArray,
            field: name,
            why: "an array of gpuIds, translated element by element",
        });
    }
    // The address beside the id. gpuId is already a bdf-scalar row above, so
    // only the three address fields are added here.
    for (_, member, off, len) in pci_info_fields() {
        if *member == "gpuId" {
            continue;
        }
        out.push(Mediated {
            nr: NR_RM_CONTROL,
            cmd: sys::NV0000_CTRL_CMD_GPU_GET_PCI_INFO,
            off: *off as u32,
            len: *len,
            stride: 0,
            count: 0,
            kind: Kind::BdfAddress,
            field: member,
            why: "the guest's own PCI address, so the address and the gpuId \
                  derived from it cannot contradict each other",
        });
    }
    for cmd in xlate::nested_cmds() {
        for p in xlate::nested_ptrs(*cmd) {
            out.push(Mediated {
                nr: NR_RM_CONTROL,
                cmd: *cmd,
                off: p.ptr_off,
                len: 8,
                stride: 0,
                count: 0,
                kind: Kind::NestedPtr,
                field: "NvP64",
                why: "a pointer into the caller's address space -- a different \
                      number on the two sides by construction",
            });
        }
    }
    for cmd in xlate::ctrl_fd_cmds() {
        if let Some(off) = xlate::ctrl_fd_offset(*cmd) {
            out.push(Mediated {
                nr: NR_RM_CONTROL,
                cmd: *cmd,
                off,
                len: 4,
                stride: 0,
                count: 0,
                kind: Kind::CtrlFd,
                field: "fd",
                why: "a process-local file descriptor; the same number names \
                      a different file on the two sides",
            });
        }
    }

    // The backend's own answers.
    out.push(Mediated {
        nr: NR_RM_CONTROL,
        cmd: CMD_GPU_GET_PIDS,
        off: PIDS_COUNT_OFF as u32,
        len: 4,
        stride: 0,
        count: 0,
        kind: Kind::BackendAnswered,
        field: "pidTblCount",
        why: "how many of this VM's processes were written, not the host's count",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL,
        cmd: CMD_GPU_GET_PIDS,
        off: PIDS_TBL_OFF as u32,
        len: 4,
        stride: 4,
        count: PIDS_MAX as u32,
        kind: Kind::BackendAnswered,
        field: "pidTbl",
        why: "this VM's guest PIDs replace the host's, which are not \
              resolvable in a guest",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL,
        cmd: CMD_GPU_GET_PID_INFO,
        off: PIDINFO_COUNT_OFF as u32,
        len: 4,
        stride: 0,
        count: 0,
        kind: Kind::BackendAnswered,
        field: "pidInfoListCount",
        why: "how many entries the backend wrote",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL,
        cmd: CMD_GPU_GET_PID_INFO,
        off: PIDINFO_LIST_OFF as u32,
        len: PIDINFO_ENTRY as u32,
        stride: PIDINFO_ENTRY as u32,
        count: PIDINFO_MAX as u32,
        kind: Kind::BackendAnswered,
        field: "pidInfoList",
        why: "per-process video memory usage out of this VM's own ledger",
    });
    for (cmd, base) in [(CMD_FB_GET_INFO_V2, FBINFO_LIST_OFF), (CMD_FB_GET_INFO, 0)] {
        out.push(Mediated {
            nr: NR_RM_CONTROL,
            cmd,
            off: (base + FBINFO_DATA_OFF) as u32,
            len: 4,
            stride: FBINFO_ENTRY as u32,
            count: FBINFO_MAX as u32,
            kind: Kind::BackendAnswered,
            field: "fbInfoList[].data",
            why: "the VRAM ledger's capped sizes. Only `data` -- `index` is \
                  the question and is carried unchanged, so a wrong index \
                  stays visible",
        });
    }
    // Include all fields a policy may rewrite, independent of configuration,
    // so the same manifest applies to native and guest comparisons.
    out.push(Mediated {
        nr: NR_RM_CONTROL,
        cmd: CMD_GPU_GET_VIRTUALIZATION_MODE,
        off: VIRTMODE_OFF as u32,
        len: 4,
        stride: 0,
        count: 0,
        kind: Kind::BackendAnswered,
        field: "virtualizationMode",
        why: "VGX under the vGPU-shaped policy, where the guest IS on a \
              profile out of a catalogue and every client asks this",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL,
        cmd: CMD_GPU_GET_VIRTUALIZATION_MODE,
        off: VIRTMODE_GRIDBUILD_OFF as u32,
        len: 1,
        stride: 0,
        count: 0,
        kind: Kind::BackendAnswered,
        field: "isGridBuild",
        why: "the boolean beside the mode, kept consistent with it",
    });
    // Per-VM UUID fields have the same length as the native UUID.
    for (cmd, off, field) in [
        (
            CMD_GPU_GET_GID_INFO,
            offset_of!(sys::NV2080_CTRL_GPU_GET_GID_INFO_PARAMS, data),
            "data",
        ),
        (
            sys::NV0000_CTRL_CMD_GPU_GET_UUID_INFO,
            offset_of!(sys::NV0000_CTRL_GPU_GET_UUID_INFO_PARAMS, gpuUuid),
            "gpuUuid",
        ),
        (
            sys::NV0000_CTRL_CMD_GPU_GET_UUID_FROM_GPU_ID,
            offset_of!(sys::NV0000_CTRL_GPU_GET_UUID_FROM_GPU_ID_PARAMS, gpuUuid),
            "gpuUuid",
        ),
    ] {
        out.push(Mediated {
            nr: NR_RM_CONTROL,
            cmd,
            off: off as u32,
            len: 256,
            stride: 0,
            count: 0,
            kind: Kind::IdentityString,
            field,
            why: "this VM's own UUID where RM wrote the card's",
        });
    }
    out.push(Mediated {
        nr: NR_RM_CONTROL,
        cmd: CMD_GPU_GET_ENCODER_CAPACITY,
        off: ENCCAP_OFF as u32,
        len: 4,
        stride: 0,
        count: 0,
        kind: Kind::BackendAnswered,
        field: "encoderCapacity",
        why: "the profile's NVENC share. Only the ANSWER -- queryType is \
              the question and is carried unchanged",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL,
        cmd: CMD_GPU_GET_NAME_STRING,
        off: name_off::<A>() as u32,
        len: name_max::<A>() as u32,
        stride: 0,
        count: 0,
        kind: Kind::IdentityString,
        field: "gpuNameString",
        why: "the mediated card name (`Leandro ...`), whose content depends on \
              the VRAM cap and is therefore not a constant",
    });
    // CARD_INFO rewrites PCI address and gpuId in the inline array.
    // Describe its first entry only: current single-GPU dumps do not validate
    // the remaining NV_MAX_DEVICES entries.
    for (member, off, len) in [
        (
            "pci_info.domain",
            offset_of!(sys::nv_ioctl_card_info_t, pci_info)
                + offset_of!(sys::nv_pci_info_t, domain),
            4u32,
        ),
        (
            "pci_info.bus",
            offset_of!(sys::nv_ioctl_card_info_t, pci_info) + offset_of!(sys::nv_pci_info_t, bus),
            1,
        ),
        (
            "pci_info.slot",
            offset_of!(sys::nv_ioctl_card_info_t, pci_info) + offset_of!(sys::nv_pci_info_t, slot),
            1,
        ),
        (
            "pci_info.function",
            offset_of!(sys::nv_ioctl_card_info_t, pci_info)
                + offset_of!(sys::nv_pci_info_t, function),
            1,
        ),
    ] {
        out.push(Mediated {
            nr: crate::nvgpu::NV_ESC_CARD_INFO, cmd: 0,
            off: off as u32, len, stride: 0, count: 0,
            kind: Kind::BdfAddress, field: member,
            why: "the guest's own PCI address, in the escape NVML reads it                   from -- the same rewrite as on GET_PCI_INFO, one namespace                   further down",
        });
    }
    out.push(Mediated {
        nr: crate::nvgpu::NV_ESC_CARD_INFO, cmd: 0,
        off: offset_of!(sys::nv_ioctl_card_info_t, gpu_id) as u32,
        len: 4, stride: 0, count: 0,
        kind: Kind::BdfScalar, field: "gpu_id",
        why: "a gpuId: the host's card id in, this guest's id out. Already               covered by the derived gpuId mask, and declared here anyway --               the manifest describes what the code REWRITES, not what some               other mask happens to catch",
    });

    // NVOS32 INFO returns total/free inline; the backend rewrites both under
    // a cap. These output fields are zero for other functions.
    for (member, off) in [
        ("total", offset_of!(sys::NVOS32_PARAMETERS, total)),
        ("free", offset_of!(sys::NVOS32_PARAMETERS, free)),
    ] {
        out.push(Mediated {
            nr: sys::NV_ESC_RM_VID_HEAP_CONTROL,
            cmd: 0,
            off: off as u32,
            len: 8,
            stride: 0,
            count: 0,
            kind: Kind::BackendAnswered,
            field: member,
            why: "the VRAM ledger's capped sizes in bytes, on the NVOS32 door \
                  the host RM answers from its own FB_GET_INFO_V2",
        });
    }

    out.sort_by_key(|m| (m.nr, m.cmd, m.off, m.len));
    out
}

/// Print one manifest record per line alongside tables.txt.
pub fn dump<A: RmAbi>() -> String {
    let mut o = String::from(
        "# GENERATED by nvrm-genhdr --mediation-dump -- do not edit.\n\
         # Every field this boundary REWRITES, derived from the code that\n\
         # rewrites it (crates/nvrm-abi/src/mediate.rs).\n\
         #\n\
         # A word inside one of these fields is ALLOWED to differ between a\n\
         # native run and a guest run. A word outside them is a finding. That\n\
         # is the opposite of the other three masks and is the point: this\n\
         # makes the comparison sharper, not looser.\n\
         #\n\
         # mediated <device> <nr> <sub> <off> <len> <stride> <count> <kind> <field>\n\
         #\n\
         # The first three columns are the catalogue's own signature key, so\n\
         # a reader can join this against catalog-<drv>.json without knowing\n\
         # anything. A control is `ctl 0x2a <cmd>`; an escape that carries\n\
         # its answer inline, like NV_ESC_CARD_INFO, is `ctl <nr> -`.\n",
    );
    for m in manifest::<A>() {
        o.push_str(&format!(
            "mediated {} {} {} {} {} {} {}\n",
            m.sig(),
            m.off,
            m.len,
            m.stride,
            m.count,
            m.kind.as_str(),
            m.field
        ));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_record_has_a_length_and_a_home() {
        for m in manifest::<sys::DefaultAbi>() {
            assert!(m.len > 0, "{m:?} has no length");
            assert!(m.end() > m.off, "{m:?} covers nothing");
            assert!(!m.field.is_empty(), "{m:?} names no field");
            assert!(!m.why.is_empty(), "{m:?} says no why");
            // An array record needs both halves or neither: a stride with no
            // count silently covers one element, which is the kind of mask
            // that is quietly too narrow.
            assert_eq!(m.stride == 0, m.count == 0, "{m:?}: half an array");
        }
    }

    /// The three address fields the guest module rewrites beside the id.
    /// This is the record the first run of the fourth mask found missing.
    #[test]
    fn the_pci_address_is_in_the_manifest() {
        let m = manifest::<sys::DefaultAbi>();
        for member in ["domain", "bus", "slot"] {
            assert!(
                m.iter()
                    .any(|x| x.cmd == sys::NV0000_CTRL_CMD_GPU_GET_PCI_INFO
                        && x.field == member
                        && x.kind == Kind::BdfAddress),
                "GET_PCI_INFO.{member} is rewritten by the guest module and \
                 named in no manifest record"
            );
        }
    }

    /// The mask must cover the identity string that made this necessary.
    #[test]
    fn the_mediated_name_is_in_the_manifest() {
        let m = manifest::<sys::DefaultAbi>();
        let name = m
            .iter()
            .find(|x| x.cmd == CMD_GPU_GET_NAME_STRING)
            .expect("no identity string");
        assert_eq!(name.kind, Kind::IdentityString);
        assert_eq!(name.cmd, CMD_GPU_GET_NAME_STRING);
        assert_eq!(name.off, 4);
        assert_eq!(name.end(), 68);
    }

    /// Each backend-answered control needs a manifest record.
    /// Semaphore waiter controls are intercepted separately and omitted here.
    #[test]
    fn every_backend_answered_command_is_described() {
        let m = manifest::<sys::DefaultAbi>();
        for cmd in [
            CMD_GPU_GET_PIDS,
            CMD_GPU_GET_PID_INFO,
            CMD_FB_GET_INFO,
            CMD_FB_GET_INFO_V2,
            CMD_GPU_GET_NAME_STRING,
            CMD_GPU_GET_GID_INFO,
            sys::NV0000_CTRL_CMD_GPU_GET_UUID_INFO,
            sys::NV0000_CTRL_CMD_GPU_GET_UUID_FROM_GPU_ID,
        ] {
            assert!(
                m.iter().any(|x| x.cmd == cmd),
                "{cmd:#x} is answered by the backend and named in no record"
            );
        }
        // And the one that is not a control: NVOS32_FUNCTION_INFO's sizes.
        for field in ["total", "free"] {
            assert!(
                m.iter().any(|x| x.nr == sys::NV_ESC_RM_VID_HEAP_CONTROL
                    && x.field == field
                    && x.len == 8),
                "NVOS32 {field} is answered by the backend and named in no record"
            );
        }
    }

    /// The pointer half must agree with the descriptor stream, because the
    /// module walks that stream and the mask has to describe the same walk.
    #[test]
    fn the_pointer_records_match_the_descriptor_table() {
        let m = manifest::<sys::DefaultAbi>();
        for cmd in xlate::nested_cmds() {
            let want = xlate::nested_ptrs(*cmd).len();
            let have = m
                .iter()
                .filter(|x| x.cmd == *cmd && x.kind == Kind::NestedPtr)
                .count();
            assert_eq!(
                want, have,
                "{cmd:#x}: {want} pointer(s) in xlate, {have} in the manifest"
            );
        }
    }
}
