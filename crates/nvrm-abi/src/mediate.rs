// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! What this boundary REWRITES, as data -- so the rewriting can be tested.
//!
//! Everything else in this tree describes what can be CARRIED. This
//! describes what is deliberately NOT carried unchanged: the gpuIds that
//! name a different card on the two sides, the pointers that name a
//! different address space, the answers the backend writes itself, and the
//! card's own name.
//!
//! WHY IT IS A TABLE AND NOT A COMMENT. `verify` compares the answer bytes
//! of a forwarded control native against guest, and for a mediated command
//! byte equality is the WRONG TEST -- `NV2080_CTRL_CMD_GPU_GET_NAME_STRING`
//! answers `NVID...` natively and `Lean...` in a guest, working exactly as
//! designed, and that was reported as a mismatch. The right test is
//! "differs in exactly the fields the mediation rewrites, and nowhere
//! else", and that test needs the mediation to NAME ITS OWN FIELDS. It
//! could not, so a whole class of commands was unjudgeable.
//!
//! Note which direction that makes the comparison go. This is not a list of
//! bytes to ignore. A word inside one of these fields is allowed to differ;
//! a word OUTSIDE them is a finding, on a command where the old test could
//! only shrug. It is the fourth mask and, like the other three, it is
//! DERIVED -- from `xlate`, from the BDF tables below, and from
//! `offset_of!` on the vendor structs. Nothing here is a number somebody
//! typed next to the code that uses it; `crates/vhost-user-nvrm/src/vram.rs`
//! reads its offsets from here rather than keeping a second copy.
//!
//! ONE NUMBER, ONE PLACE: the BDF tables live here and the generated C
//! header (`nvrm-genhdr`) is written from them, so the guest module and this
//! manifest cannot disagree about which field carries an address.

use core::mem::{offset_of, size_of};

use nvrm_sys as sys;

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
    /// The PCI ADDRESS itself -- domain, bus, slot. Not the same thing as a
    /// gpuId even though one is derived from the other, and it is rewritten
    /// by a separate step in the guest module.
    BdfAddress,
    /// An `NvP64` the mediation walks: it points into the caller's address
    /// space, so it is a different number on the two sides by construction.
    NestedPtr,
    /// A process-local file descriptor inside the params buffer.
    CtrlFd,
    /// The backend writes this field itself rather than forwarding RM's
    /// answer -- the VRAM ledger and the process list.
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
    /// The ESCAPE this field belongs to. Almost everything mediated is a
    /// RM_CONTROL, but not everything: `NV_ESC_CARD_INFO` carries the BDF
    /// and the gpuId in its own inline block, with no control involved, and
    /// a manifest that only had a `cmd` column could not name it. NVML reads
    /// that escape, which is why nvidia-smi kept printing the host address
    /// after every control had been mediated.
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
    /// The catalogue signature this field belongs to.
    ///
    /// Everything mediated is on the control node: mediation happens where
    /// the boundary answers, and that is `/dev/nvidiactl`. A field on
    /// another node would need a `dev` column here, and would be a finding
    /// in its own right.
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

// ---------------------------------------------------------------------------
// the address, as scalars and as arrays
// ---------------------------------------------------------------------------
// These two tables were literals inside `nvrm-genhdr`'s generator until the
// manifest needed them as well. They are here now and the generator reads
// them, because a table the C header is built from and a table the mask is
// built from must be THE SAME TABLE -- otherwise the guest module could
// translate a field the mask does not know about, and the mask would report
// the translation as a defect.
//
// Hand-listing cost a working NVKMS once: mediating GET_ID_INFO (0x202) but
// not GET_ID_INFO_V2 (0x205) let a mediated id reach RM unmapped, and
// nvidia-drm answered "Failed to allocate NvKmsKapiDevice" -- three layers
// away from the missing line. Every control whose parameter block carries a
// gpuId belongs here, and the offsets come from the SDK structs, never from
// a count of fields.

/// `(name, cmd, offset)` -- one gpuId at a fixed offset.
pub fn bdf_scalars() -> &'static [(&'static str, u32, usize)] {
    &[
        ("GET_ID_INFO", sys::NV0000_CTRL_CMD_GPU_GET_ID_INFO,
         offset_of!(sys::NV0000_CTRL_GPU_GET_ID_INFO_PARAMS, gpuId)),
        ("GET_ID_INFO_V2", sys::NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2,
         offset_of!(sys::NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS, gpuId)),
        // The SAME id a second time in the same block. Measured with
        // bdf_debug: GET_ID_INFO carries 0x2d00 at +0 and again at +28, and
        // the second one is boardId -- which is what NVML prints the address
        // from. A command can appear more than once in this table.
        ("GET_ID_INFO boardId", sys::NV0000_CTRL_CMD_GPU_GET_ID_INFO,
         offset_of!(sys::NV0000_CTRL_GPU_GET_ID_INFO_PARAMS, boardId)),
        ("GET_ID_INFO_V2 boardId", sys::NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2,
         offset_of!(sys::NV0000_CTRL_GPU_GET_ID_INFO_V2_PARAMS, boardId)),
        ("GET_PCI_INFO", sys::NV0000_CTRL_CMD_GPU_GET_PCI_INFO,
         offset_of!(sys::NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS, gpuId)),
        ("GET_UUID_INFO", sys::NV0000_CTRL_CMD_GPU_GET_UUID_INFO,
         offset_of!(sys::NV0000_CTRL_GPU_GET_UUID_INFO_PARAMS, gpuId)),
        ("GET_UUID_FROM_GPU_ID", sys::NV0000_CTRL_CMD_GPU_GET_UUID_FROM_GPU_ID,
         offset_of!(sys::NV0000_CTRL_GPU_GET_UUID_FROM_GPU_ID_PARAMS, gpuId)),
        ("MODIFY_DRAIN_STATE", sys::NV0000_CTRL_CMD_GPU_MODIFY_DRAIN_STATE,
         offset_of!(sys::NV0000_CTRL_GPU_MODIFY_DRAIN_STATE_PARAMS, gpuId)),
        ("QUERY_DRAIN_STATE", sys::NV0000_CTRL_CMD_GPU_QUERY_DRAIN_STATE,
         offset_of!(sys::NV0000_CTRL_GPU_QUERY_DRAIN_STATE_PARAMS, gpuId)),
        // These two are the ones NVML actually attaches with, and leaving
        // them out was measured, not theorised: nvidia-smi kept printing the
        // host's 2D:00.0 because ASYNC_ATTACH_ID echoed the host id straight
        // back and NVML decodes the address OUT OF THE ID.
        ("ASYNC_ATTACH_ID", sys::NV0000_CTRL_CMD_GPU_ASYNC_ATTACH_ID,
         offset_of!(sys::NV0000_CTRL_GPU_ASYNC_ATTACH_ID_PARAMS, gpuId)),
        ("WAIT_ATTACH_ID", sys::NV0000_CTRL_CMD_GPU_WAIT_ATTACH_ID,
         offset_of!(sys::NV0000_CTRL_GPU_WAIT_ATTACH_ID_PARAMS, gpuId)),
        // Found by the guest sweep on 2026-08-20 (OPEN-QUESTIONS number 51),
        // and the only control in nvidia-smi's whole run that answered
        // NV_ERR_INVALID_ARGUMENT (0x1f) in a guest -- which is what RM says
        // about a gpuId it does not know, and the same failure as
        // P2P_CAPS_MATRIX in the raytracing work. Params are
        // { gpuId, pid, state } = 12 bytes, and the guest trace agrees
        // (psize 0xc). NVML asks it per GPU while building the accounting
        // section of `-q`; the report prints without the answer, which is
        // why no gate has ever seen this and only a trace diff could.
        ("GPUACCT_GET_ACCOUNTING_STATE", sys::NV0000_CTRL_CMD_GPUACCT_GET_ACCOUNTING_STATE,
         offset_of!(sys::NV0000_CTRL_GPUACCT_GET_ACCOUNTING_STATE_PARAMS, gpuId)),
    ]
}

/// `(name, cmd, offset, count, stride)` -- an array of gpuIds.
///
/// The stride column exists for ONE control so far, and it is the one that
/// broke raytracing: GET_ACTIVE_DEVICE_IDS answers an array of
/// {gpuId, gpuInstanceId, computeInstanceId} -- 12 bytes apart. Measured
/// 2026-08-15 with the tracer's answer payloads: every other enumeration
/// said gpuId 0x6 (mediated), this one said 0x2d00 (the host's), and the RT
/// device init, which asks it right after GET_ID_INFO_V2, found its active
/// device in no list it knew and returned INITIALIZATION_FAILED.
pub fn bdf_arrays() -> &'static [(&'static str, u32, usize, u32, usize)] {
    &[
        ("GET_ATTACHED_IDS", sys::NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS,
         offset_of!(sys::NV0000_CTRL_GPU_GET_ATTACHED_IDS_PARAMS, gpuIds),
         sys::NV0000_CTRL_GPU_MAX_ATTACHED_GPUS, 4),
        ("GET_PROBED_IDS", sys::NV0000_CTRL_CMD_GPU_GET_PROBED_IDS,
         offset_of!(sys::NV0000_CTRL_GPU_GET_PROBED_IDS_PARAMS, gpuIds),
         sys::NV0000_CTRL_GPU_MAX_PROBED_GPUS, 4),
        ("ATTACH_IDS", sys::NV0000_CTRL_CMD_GPU_ATTACH_IDS,
         offset_of!(sys::NV0000_CTRL_GPU_ATTACH_IDS_PARAMS, gpuIds),
         sys::NV0000_CTRL_GPU_MAX_PROBED_GPUS, 4),
        ("DETACH_IDS", sys::NV0000_CTRL_CMD_GPU_DETACH_IDS,
         offset_of!(sys::NV0000_CTRL_GPU_DETACH_IDS_PARAMS, gpuIds),
         sys::NV0000_CTRL_GPU_MAX_ATTACHED_GPUS, 4),
        ("GET_ACTIVE_DEVICE_IDS", sys::NV0000_CTRL_CMD_GPU_GET_ACTIVE_DEVICE_IDS,
         offset_of!(sys::NV0000_CTRL_GPU_GET_ACTIVE_DEVICE_IDS_PARAMS, devices)
             + offset_of!(sys::NV0000_CTRL_GPU_ACTIVE_DEVICE, gpuId),
         sys::NV0000_CTRL_GPU_MAX_ACTIVE_DEVICES,
         size_of::<sys::NV0000_CTRL_GPU_ACTIVE_DEVICE>()),
        // The QUESTION side of raytracing init: "P2P caps of GPU group A to
        // group B", both groups arrays of gpuIds. Two arrays, one control,
        // hence two rows with the same cmd (the rewrite loop takes every row
        // that matches).
        ("P2P_CAPS_MATRIX_A", sys::NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS_MATRIX,
         offset_of!(sys::NV0000_CTRL_SYSTEM_GET_P2P_CAPS_MATRIX_PARAMS, gpuIdGrpA),
         sys::NV0000_CTRL_SYSTEM_MAX_P2P_GROUP_GPUS, 4),
        ("P2P_CAPS_MATRIX_B", sys::NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS_MATRIX,
         offset_of!(sys::NV0000_CTRL_SYSTEM_GET_P2P_CAPS_MATRIX_PARAMS, gpuIdGrpB),
         sys::NV0000_CTRL_SYSTEM_MAX_P2P_GROUP_GPUS, 4),
    ]
}

/// The PCI ADDRESS inside `NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS`.
///
/// `(suffix, member, offset, bytes)`. The generated C header defines
/// `NVRM_<suffix>_OFF` from this and the guest module writes the guest's own
/// domain/bus/slot there (`bdf_rewrite_reply`, virtio_nvrm.c), so the id and
/// the address cannot contradict each other -- gpuId is DERIVED from the
/// address (`gpuGenerate32BitId()`, gpu.c:292).
///
/// It is here because the mask found it missing. `verify` reported
/// GET_PCI_INFO differing at `bus` with "this command IS mediated, and this
/// byte is in none of the fields the mediation declares" -- which is the
/// fourth mask doing exactly what it is for, on the first run, before any
/// deliberate test. The manifest was incomplete; the code was right.
pub fn pci_info_fields() -> &'static [(&'static str, &'static str, usize, u32)] {
    &[
        ("PCI_INFO_GPUID", "gpuId",
         offset_of!(sys::NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS, gpuId), 4),
        ("PCI_INFO_DOMAIN", "domain",
         offset_of!(sys::NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS, domain), 4),
        ("PCI_INFO_BUS", "bus",
         offset_of!(sys::NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS, bus), 2),
        ("PCI_INFO_SLOT", "slot",
         offset_of!(sys::NV0000_CTRL_GPU_GET_PCI_INFO_PARAMS, slot), 2),
    ]
}

// ---------------------------------------------------------------------------
// what the backend answers itself
// ---------------------------------------------------------------------------
// The offsets `crates/vhost-user-nvrm/src/vram.rs` rewrites at. They are
// defined HERE and used THERE, rather than the other way round, so the
// manifest cannot describe a field the code does not touch or miss one it
// does. Every one is an `offset_of!` on the bindgen struct -- the numbers in
// the doc comments are what those expressions evaluate to on this driver,
// recorded so a reader need not compile to follow the prose.

/// `NV2080_CTRL_CMD_GPU_GET_PIDS` (ctrl2080gpu.h:3501).
pub const CMD_GPU_GET_PIDS: u32 = 0x2080_018d;
/// `NV2080_CTRL_CMD_GPU_GET_PID_INFO` (ctrl2080gpu.h:3643).
pub const CMD_GPU_GET_PID_INFO: u32 = 0x2080_018e;
/// `NV2080_CTRL_CMD_FB_GET_INFO_V2` (ctrl2080fb.h:489).
pub const CMD_FB_GET_INFO_V2: u32 = 0x2080_1303;
/// `NV2080_CTRL_CMD_FB_GET_INFO` (ctrl2080fb.h:480), the V1 form -- the SAME
/// index list, but the array hangs off an `NvP64` instead of sitting in the
/// params buffer (`xlate::nested_ptrs`, ptr_off 8).
pub const CMD_FB_GET_INFO: u32 = 0x2080_1301;
/// `NV0080_CTRL_CMD_GPU_GET_VIRTUALIZATION_MODE` (ctrl0080gpu.h:300).
///
/// The question every client asks about the card it just opened. Measured
/// (matrix/catalog-610.57.04.json): **34 calls from 20 library classes** --
/// every CUDA probe, all four EGL platforms, GL, GLES, NVDEC, NVENC, NVML,
/// OpenCL and all three Vulkan probes -- and today the guest is handed the
/// HOST's answer, `NONE`, unchanged.
pub const CMD_GPU_GET_VIRTUALIZATION_MODE: u32 = 0x0080_0289;
/// `NV2080_CTRL_CMD_GPU_GET_ENCODER_CAPACITY` (ctrl2080gpu.h:2322). 22
/// calls, from `nvenc` alone.
pub const CMD_GPU_GET_ENCODER_CAPACITY: u32 = 0x2080_016c;

/// `NV0080_CTRL_GPU_GET_VIRTUALIZATION_MODE_PARAMS`: `virtualizationMode`
/// @0, `isGridBuild` @4 -- a `NvBool`, which is one byte.
pub const VIRTMODE_OFF: usize =
    offset_of!(sys::NV0080_CTRL_GPU_GET_VIRTUALIZATION_MODE_PARAMS, virtualizationMode);
pub const VIRTMODE_GRIDBUILD_OFF: usize =
    offset_of!(sys::NV0080_CTRL_GPU_GET_VIRTUALIZATION_MODE_PARAMS, isGridBuild);
pub const VIRTMODE_LEN: usize =
    size_of::<sys::NV0080_CTRL_GPU_GET_VIRTUALIZATION_MODE_PARAMS>();

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
pub const ENCCAP_OFF: usize =
    offset_of!(sys::NV2080_CTRL_GPU_GET_ENCODER_CAPACITY_PARAMS, encoderCapacity);
pub const ENCCAP_LEN: usize =
    size_of::<sys::NV2080_CTRL_GPU_GET_ENCODER_CAPACITY_PARAMS>();

/// `NV2080_CTRL_CMD_GPU_GET_NAME_STRING` (ctrl2080gpu.h:325).
pub const CMD_GPU_GET_NAME_STRING: u32 = 0x2080_0110;

/// `NV2080_CTRL_GPU_GET_PIDS_PARAMS`: `pidTblCount` @8, `pidTbl[950]` @12.
pub const PIDS_COUNT_OFF: usize =
    offset_of!(sys::NV2080_CTRL_GPU_GET_PIDS_PARAMS, pidTblCount);
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
/// `NV2080_CTRL_FB_INFO { u32 index; u32 data; }` -- 1028 bytes for the
/// 128-entry maximum.
pub const FBINFO_COUNT_OFF: usize = 0;
pub const FBINFO_LIST_OFF: usize = 4;
pub const FBINFO_ENTRY: usize = size_of::<sys::NV2080_CTRL_FB_INFO>();
pub const FBINFO_MAX: usize = 128;
/// Within one entry, `data` is the half that is rewritten; `index` is the
/// question and is carried unchanged. Masking the whole entry would hide a
/// wrong index, which is a real defect and has to stay visible.
pub const FBINFO_DATA_OFF: usize = offset_of!(sys::NV2080_CTRL_FB_INFO, data);

/// The `NV2080_CTRL_FB_INFO_INDEX_*` values that carry a MEMORY SIZE, all
/// of them in kilobytes (ctrl2080fb.h:76-112, :254-260).
///
/// They live here rather than beside their one consumer because there is
/// more than one now: the backend rewrites them on the way back to the
/// guest, and the vGPU-shaped catalogue ([`crate::vgpu`]) reads the same
/// two on the way in, from the host's own card. Two lists that could
/// disagree about which index is a size is exactly the drift this module
/// exists to prevent.
pub const FB_INFO_INDEX_RAM_SIZE: u32 = 0x07;
pub const FB_INFO_INDEX_TOTAL_RAM_SIZE: u32 = 0x08;
pub const FB_INFO_INDEX_HEAP_SIZE: u32 = 0x09;
pub const FB_INFO_INDEX_HEAP_FREE: u32 = 0x16;
pub const FB_INFO_INDEX_USABLE_RAM_SIZE: u32 = 0x20;

/// `NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS`: `gpuNameStringFlags` @0,
/// `ascii[64]` @4 (`NV2080_GPU_MAX_NAME_STRING_LENGTH` = 64).
pub const NAME_OFF: usize =
    offset_of!(sys::NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS, gpuNameString);
/// bindgen emits the name field as a nested type of its own (the header
/// writes it as a union of `ascii` and `unicode`, of which this driver's
/// header carries only `ascii`), so its SIZE is the max name length.
pub const NAME_MAX: usize = size_of::<sys::NV2080_CTRL_GPU_GET_NAME_STRING_PARAMS__bindgen_ty_1>();

// The numbers the prose above quotes, so a change in the vendor headers
// fails the build here rather than turning every doc line into a lie.
const _: () = {
    assert!(PIDS_COUNT_OFF == 8 && PIDS_TBL_OFF == 12 && PIDS_LEN == 3812);
    assert!(PIDINFO_COUNT_OFF == 0 && PIDINFO_LIST_OFF == 8);
    assert!(PIDINFO_ENTRY == 72 && PIDINFO_LEN == 14408 && PIDINFO_MEM_PRIVATE == 16);
    assert!(FBINFO_ENTRY == 8 && FBINFO_DATA_OFF == 4);
    assert!(NAME_OFF == 4 && NAME_MAX == 64);
};

// ---------------------------------------------------------------------------
// the manifest
// ---------------------------------------------------------------------------

/// Every field this boundary rewrites, one record per `(command, field)`.
///
/// Derived in full: the BDF halves from the two tables above, the pointers
/// and the fd fields from `xlate` (the same functions `table::build()` pours
/// into the descriptor stream), and the backend's own from `offset_of!` on
/// the vendor structs. There is no literal offset in this function.
pub fn manifest() -> Vec<Mediated> {
    let mut out: Vec<Mediated> = Vec::new();

    for (name, cmd, off) in bdf_scalars() {
        out.push(Mediated {
            nr: NR_RM_CONTROL, cmd: *cmd, off: *off as u32, len: 4, stride: 0, count: 0,
            kind: Kind::BdfScalar, field: name,
            why: "a gpuId: the host's card id in, this guest's id out",
        });
    }
    for (name, cmd, off, count, stride) in bdf_arrays() {
        out.push(Mediated {
            nr: NR_RM_CONTROL, cmd: *cmd, off: *off as u32, len: 4,
            stride: *stride as u32, count: *count,
            kind: Kind::BdfArray, field: name,
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
            nr: NR_RM_CONTROL, cmd: sys::NV0000_CTRL_CMD_GPU_GET_PCI_INFO,
            off: *off as u32, len: *len, stride: 0, count: 0,
            kind: Kind::BdfAddress, field: member,
            why: "the guest's own PCI address, so the address and the gpuId \
                  derived from it cannot contradict each other",
        });
    }
    for cmd in xlate::nested_cmds() {
        for p in xlate::nested_ptrs(*cmd) {
            out.push(Mediated {
                nr: NR_RM_CONTROL, cmd: *cmd, off: p.ptr_off, len: 8, stride: 0, count: 0,
                kind: Kind::NestedPtr, field: "NvP64",
                why: "a pointer into the caller's address space -- a different \
                      number on the two sides by construction",
            });
        }
    }
    for cmd in xlate::ctrl_fd_cmds() {
        if let Some(off) = xlate::ctrl_fd_offset(*cmd) {
            out.push(Mediated {
                nr: NR_RM_CONTROL, cmd: *cmd, off, len: 4, stride: 0, count: 0,
                kind: Kind::CtrlFd, field: "fd",
                why: "a process-local file descriptor; the same number names \
                      a different file on the two sides",
            });
        }
    }

    // The backend's own answers.
    out.push(Mediated {
        nr: NR_RM_CONTROL, cmd: CMD_GPU_GET_PIDS, off: PIDS_COUNT_OFF as u32, len: 4,
        stride: 0, count: 0, kind: Kind::BackendAnswered, field: "pidTblCount",
        why: "how many of this VM's processes were written, not the host's count",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL, cmd: CMD_GPU_GET_PIDS, off: PIDS_TBL_OFF as u32, len: 4,
        stride: 4, count: PIDS_MAX as u32,
        kind: Kind::BackendAnswered, field: "pidTbl",
        why: "this VM's guest PIDs replace the host's, which are not \
              resolvable in a guest",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL, cmd: CMD_GPU_GET_PID_INFO, off: PIDINFO_COUNT_OFF as u32, len: 4,
        stride: 0, count: 0, kind: Kind::BackendAnswered, field: "pidInfoListCount",
        why: "how many entries the backend wrote",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL, cmd: CMD_GPU_GET_PID_INFO, off: PIDINFO_LIST_OFF as u32,
        len: PIDINFO_ENTRY as u32, stride: PIDINFO_ENTRY as u32,
        count: PIDINFO_MAX as u32,
        kind: Kind::BackendAnswered, field: "pidInfoList",
        why: "per-process video memory usage out of this VM's own ledger",
    });
    for (cmd, base) in [(CMD_FB_GET_INFO_V2, FBINFO_LIST_OFF), (CMD_FB_GET_INFO, 0)] {
        out.push(Mediated {
            nr: NR_RM_CONTROL, cmd, off: (base + FBINFO_DATA_OFF) as u32, len: 4,
            stride: FBINFO_ENTRY as u32, count: FBINFO_MAX as u32,
            kind: Kind::BackendAnswered, field: "fbInfoList[].data",
            why: "the VRAM ledger's capped sizes. Only `data` -- `index` is \
                  the question and is carried unchanged, so a wrong index \
                  stays visible",
        });
    }
    // The two the vGPU-shaped policy answers (number 69). They are listed
    // unconditionally, exactly like the VRAM sizes above: the manifest says
    // which fields this boundary MAY rewrite, not which policy happens to
    // be running -- a manifest that changed with the configuration could
    // not be compared against a native run at all.
    out.push(Mediated {
        nr: NR_RM_CONTROL, cmd: CMD_GPU_GET_VIRTUALIZATION_MODE,
        off: VIRTMODE_OFF as u32, len: 4, stride: 0, count: 0,
        kind: Kind::BackendAnswered, field: "virtualizationMode",
        why: "VGX under the vGPU-shaped policy, where the guest IS on a \
              profile out of a catalogue and every client asks this",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL, cmd: CMD_GPU_GET_VIRTUALIZATION_MODE,
        off: VIRTMODE_GRIDBUILD_OFF as u32, len: 1, stride: 0, count: 0,
        kind: Kind::BackendAnswered, field: "isGridBuild",
        why: "the boolean beside the mode, kept consistent with it",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL, cmd: CMD_GPU_GET_ENCODER_CAPACITY,
        off: ENCCAP_OFF as u32, len: 4, stride: 0, count: 0,
        kind: Kind::BackendAnswered, field: "encoderCapacity",
        why: "the profile's NVENC share. Only the ANSWER -- queryType is \
              the question and is carried unchanged",
    });
    out.push(Mediated {
        nr: NR_RM_CONTROL, cmd: CMD_GPU_GET_NAME_STRING, off: NAME_OFF as u32, len: NAME_MAX as u32,
        stride: 0, count: 0, kind: Kind::IdentityString, field: "gpuNameString",
        why: "the mediated card name (`Leandro ...`), whose content depends on \
              the VRAM cap and is therefore not a constant",
    });
    // NV_ESC_CARD_INFO, the mediated field that is not a control at all.
    //
    // The guest module rewrites the BDF and the gpuId of every valid entry
    // of the inline block (`virtio_nvrm.c`, at NVRM_CARD_INFO_PCI_OFF and
    // NVRM_CARD_INFO_GPUID_OFF -- offsets this file generates). Nothing said
    // so, so `verify` reported the rewrite as a MISMATCH the moment escape
    // payloads were dumped at all: 0x2d natively, 0x05 in the guest, which
    // is this rig's bus number against the guest's slot number and the
    // mediation working exactly as designed.
    //
    // ONE ENTRY, not the whole array. The block holds NV_MAX_DEVICES of
    // them, but only the first is filled on a single-GPU rig and a count
    // here would be a promise about the others that nothing has measured.
    // The comparison covers what the dump covers.
    for (member, off, len) in [
        ("pci_info.domain",
         offset_of!(sys::nv_ioctl_card_info_t, pci_info)
             + offset_of!(sys::nv_pci_info_t, domain), 4u32),
        ("pci_info.bus",
         offset_of!(sys::nv_ioctl_card_info_t, pci_info)
             + offset_of!(sys::nv_pci_info_t, bus), 1),
        ("pci_info.slot",
         offset_of!(sys::nv_ioctl_card_info_t, pci_info)
             + offset_of!(sys::nv_pci_info_t, slot), 1),
        ("pci_info.function",
         offset_of!(sys::nv_ioctl_card_info_t, pci_info)
             + offset_of!(sys::nv_pci_info_t, function), 1),
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

    out.sort_by_key(|m| (m.nr, m.cmd, m.off, m.len));
    out
}

/// The manifest as text, one record per line, for the artefact beside
/// `tables.txt`. Same spirit as `table::expect_dump()`: a stream a reader
/// and a script can both take apart, written by the code that owns the
/// numbers.
pub fn dump() -> String {
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
    for m in manifest() {
        o.push_str(&format!(
            "mediated {} {} {} {} {} {} {}\n",
            m.sig(), m.off, m.len, m.stride, m.count, m.kind.as_str(), m.field
        ));
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_record_has_a_length_and_a_home() {
        for m in manifest() {
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
        let m = manifest();
        for member in ["domain", "bus", "slot"] {
            assert!(
                m.iter().any(|x| x.cmd == sys::NV0000_CTRL_CMD_GPU_GET_PCI_INFO
                             && x.field == member && x.kind == Kind::BdfAddress),
                "GET_PCI_INFO.{member} is rewritten by the guest module and \
                 named in no manifest record"
            );
        }
    }

    /// The mask must cover the identity string that made this necessary.
    #[test]
    fn the_mediated_name_is_in_the_manifest() {
        let m = manifest();
        let name = m.iter().find(|x| x.kind == Kind::IdentityString).expect("no identity string");
        assert_eq!(name.cmd, CMD_GPU_GET_NAME_STRING);
        assert_eq!(name.off, 4);
        assert_eq!(name.end(), 68);
    }

    /// Every command the backend answers itself has at least one record.
    /// The catalogue reports seven such commands; the two semaphore-surface
    /// controls are intercepted rather than answered and rewrite no field of
    /// the params buffer, so they are deliberately not here.
    #[test]
    fn every_backend_answered_command_is_described() {
        let m = manifest();
        for cmd in [CMD_GPU_GET_PIDS, CMD_GPU_GET_PID_INFO, CMD_FB_GET_INFO,
                    CMD_FB_GET_INFO_V2, CMD_GPU_GET_NAME_STRING] {
            assert!(m.iter().any(|x| x.cmd == cmd),
                    "{cmd:#x} is answered by the backend and named in no record");
        }
    }

    /// The pointer half must agree with the descriptor stream, because the
    /// module walks that stream and the mask has to describe the same walk.
    #[test]
    fn the_pointer_records_match_the_descriptor_table() {
        let m = manifest();
        for cmd in xlate::nested_cmds() {
            let want = xlate::nested_ptrs(*cmd).len();
            let have = m.iter()
                .filter(|x| x.cmd == *cmd && x.kind == Kind::NestedPtr)
                .count();
            assert_eq!(want, have, "{cmd:#x}: {want} pointer(s) in xlate, {have} in the manifest");
        }
    }
}
