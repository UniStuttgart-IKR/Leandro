// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Guest gather/scatter descriptors, generated from nvrm-abi::xlate.
//! Tables describe payload sizes, pointer/FD fields and special handling.
//! The guest also uses generated NVIDIA constants for selected special cases.
//!
//! Entries are keyed by (device, ioctl number): RM and UVM reuse numbers.
//! Fields are little-endian u32 values; header lengths/counts bound the rows.

/// "NVRT" as an LE u32.
pub const TABLE_MAGIC: u32 = u32::from_le_bytes(*b"NVRT");

/// Descriptor-format version; bump when field semantics change.
/// The guest rejects other versions independently of PROTO_VERSION.
pub const TABLE_VERSION: u32 = 1;

/// No-offset sentinel; equal to crate::NONE_U32.
pub const NONE: u32 = u32::MAX;

/// `size` value meaning: the payload size sits in the command's _IOC
/// encoding (frontend). UVM has none, so there the size is mandatory.
pub const SIZE_FROM_IOC: u32 = u32::MAX;

// emb_len_kind

/// No embedded pointer.
pub const EMB_NONE: u32 = 0;
/// Length is the inline u32 at emb_len_off (e.g. NVOS54.paramsSize).
pub const EMB_LEN_FIELD: u32 = 1;
/// Length comes from the class table, keyed by inline hClass at emb_len_off.
pub const EMB_LEN_CLASS: u32 = 2;
/// emb_len_off contains a fixed byte length. RM_GET_EVENT_DATA uses this
/// for its out-only NvUnixEvent; forwarding the raw guest pointer is invalid.
pub const EMB_LEN_FIXED: u32 = 3;

// flags in the ioctl descriptor

/// Unwrap NV_ESC_IOCTL_XFER_CMD before decoding its inner number/size/pointer.
pub const F_XFER: u32 = 1 << 0;
/// Pin guest pages and send GPA runs when hClass at emb_len_off equals osdesc_class.
pub const F_OSDESC: u32 = 1 << 1;
/// Successful RM_FREE retires object bookkeeping; aliased backing may stay pinned.
pub const F_FREE: u32 = 1 << 2;

/// Record lengths checked against the received header.
pub const HDR_LEN: usize = 96;
pub const IOCTL_DESC_LEN: usize = 48;
pub const CLASS_DESC_LEN: usize = 24;
pub const CTRL_DESC_LEN: usize = 20;
pub const NESTED_DESC_LEN: usize = 16;

/// Followed by n_ioctl IoctlDesc, n_class ClassDesc, n_ctrl CtrlDesc,
/// then n_nested NestedDescRow records, densely packed in that order.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct TableHdr {
    pub magic: u32,
    pub table_version: u32,
    /// Total length of the stream including this header.
    pub total_len: u32,
    /// FNV-1a-32 over everything AFTER the header. The gate reads it from `dmesg`.
    pub checksum: u32,
    pub n_ioctl: u32,
    pub n_class: u32,
    pub n_ctrl: u32,
    pub n_nested: u32,

    // Special-case metadata; offset fields may be NONE.
    pub xfer_nr: u32,
    /// Size of the wrapping struct `nv_ioctl_xfer_t`.
    pub xfer_struct_len: u32,
    /// Offsets inside it: cmd u32, size u32, ptr P64.
    pub xfer_cmd_off: u32,
    pub xfer_size_off: u32,
    pub xfer_ptr_off: u32,
    /// Driver payload limit (NV_ABSOLUTE_MAX_IOCTL_SIZE).
    pub max_ioctl_size: u32,

    /// hClass of the OS descriptor.
    pub osdesc_class: u32,
    /// Offsets in the inline struct of the OS-descriptor alloc (NVOS02):
    /// pointer to the guest memory, last byte address, status, new handle.
    pub osdesc_pmem_off: u32,
    pub osdesc_limit_off: u32,
    pub osdesc_status_off: u32,
    pub osdesc_handle_off: u32,

    /// Protocol bounds advertised to the guest.
    pub max_inline: u32,
    pub max_aux: u32,
    pub max_nested: u32,
    pub _pad: [u32; 2],
}

/// Gather rule for (device, ioctl number). An absent frontend row uses _IOC
/// size; absent UVM rows fail with ENOTSUP because their sizes are unknown.
/// The host independently validates every request before forwarding.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct IoctlDesc {
    /// `DevTag` (0 ctl, 1 gpu, 2 uvm, 3 uvm-tools).
    pub dev: u32,
    pub nr: u32,
    /// Payload size in bytes, or `SIZE_FROM_IOC`.
    pub size: u32,
    /// Byte offset of a process-local fd field (i32) in the inline struct.
    pub fd_off: u32,
    /// Byte offset of the P64 that points at the second buffer.
    pub emb_ptr_off: u32,
    /// Where its length comes from: `EMB_*`.
    pub emb_len_kind: u32,
    /// Offset of the length or hClass field (u32) in the inline struct.
    pub emb_len_off: u32,
    /// Offset of a u32 subcommand that keys the control table.
    pub cmd_off: u32,
    /// Offset of a P64 required to be zero (NVOS64.pRightsRequested).
    pub rights_off: u32,
    /// Minimum inline size containing rights_off; only used when it is not NONE.
    /// NVOS64 has the field at 48 bytes; NVOS21 is 32 bytes without it.
    pub rights_if_size: u32,
    /// Offset of a u32 object handle for F_FREE and F_OSDESC.
    pub handle_off: u32,
    pub flags: u32,
}

/// hClass -> size of the alloc parameter struct (+ any fd field inside it).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct ClassDesc {
    pub hclass: u32,
    pub param_size: u32,
    /// Byte offset of a process-local fd (P64) INSIDE the params buffer.
    pub fd_off: u32,
    /// `KF_*`.
    pub flags: u32,
    /// Translate fd_off only if the u32 at fd_if_off equals fd_if_val.
    /// NONE selects unconditional translation. NV0005.data is an FD only for
    /// OS_EVENT; other classes store a kernel callback pointer there.
    pub fd_if_off: u32,
    pub fd_if_val: u32,
}

/// Control command -> range in the nested table, plus flags.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct CtrlDesc {
    pub cmd: u32,
    /// Index of the first `NestedDesc` that belongs to it.
    pub first: u32,
    pub count: u32,
    /// CF_* flags; nested pointers, blocking and FD translation can coexist.
    pub flags: u32,
    /// Offset of an NvS32 FD in control params, or NONE. ClassDesc uses NvP64;
    /// writing eight bytes here would overwrite the adjacent control field.
    pub fd_off: u32,
}

/// Guest rejects this control with EPERM (see xlate::blocked_ctrls).
/// The host repeats the check; guest tables are not a security boundary.
pub const CF_BLOCK: u32 = 1 << 0;

/// Control params contain an FD at fd_off. The flag prevents interpreting
/// older, shorter rows as if they carried that field.
pub const CF_FD: u32 = 1 << 1;

/// Class size comes from vendor headers but is not marked hardware-verified.
/// The host logs first use. Class flags are separate from CtrlDesc.flags.
pub const KF_UNVERIFIED: u32 = 1 << 0;

/// Where the length of a second-level pointer comes from.
pub const NLEN_FIXED: u32 = 0;
/// u32 at `len_off` in the params buffer, times `elem` bytes.
pub const NLEN_FIELD: u32 = 1;

/// A P64 field INSIDE the params buffer, pointing at a further buffer.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct NestedDescRow {
    pub ptr_off: u32,
    pub len_kind: u32,
    /// With `NLEN_FIXED` the length itself, with `NLEN_FIELD` the offset.
    pub len_off: u32,
    pub elem: u32,
}

/// FNV-1a-32 detects table corruption; it does not authenticate the sender.
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

const _: () = assert!(core::mem::size_of::<TableHdr>() == HDR_LEN);
const _: () = assert!(core::mem::size_of::<IoctlDesc>() == IOCTL_DESC_LEN);
const _: () = assert!(core::mem::size_of::<ClassDesc>() == CLASS_DESC_LEN);
const _: () = assert!(core::mem::size_of::<CtrlDesc>() == CTRL_DESC_LEN);
const _: () = assert!(core::mem::size_of::<NestedDescRow>() == NESTED_DESC_LEN);

#[cfg(test)]
mod tests {
    use super::*;

    /// Known vectors shared with the guest checksum implementation.
    #[test]
    fn fnv1a32_reference_vectors() {
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
        assert_eq!(fnv1a32(b"foobar"), 0xbf9c_f968);
    }
}
