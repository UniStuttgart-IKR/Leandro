// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! The descriptor tables: what the guest module needs to know in order to
//! carry a call across the boundary -- and **only** that.
//!
//! Terms, once: RM is NVIDIA's Resource Manager, the kernel driver behind
//! /dev/nvidiactl and /dev/nvidiaN, and its ioctls are called escapes;
//! UVM is its unified-memory driver (/dev/nvidia-uvm); hClass is an RM
//! object class number; NVOS54, NVOS64, NVOS02 and NVOS41 are the
//! parameter blocks of RM_CONTROL, RM_ALLOC, RM_ALLOC_MEMORY and
//! RM_GET_EVENT_DATA (nvos.h).
//!
//! The reason for this format: the module is meant to be dumb. The C code
//! holds not one NVIDIA constant; it is the interpreter of these tables,
//! which the host sends at startup (kind `KIND_GET_TABLES`). The source of
//! truth stays `nvrm-abi::xlate` -- one number, one place, one version
//! change.
//!
//! Keyed on **(device type, nr)**, not on nr alone: 0x27 is
//! `NV_ESC_RM_ALLOC_MEMORY` on the ctl node and `PAGEABLE_MEM_ACCESS` on
//! the uvm node -- two entirely different calls.
//!
//! Everything is u32, little-endian, naturally aligned, so the stream is
//! readable on both sides without a packer. Lengths and counts live in the
//! header, so the interpreter can check before every access.

/// "NVRT" as an LE u32.
pub const TABLE_MAGIC: u32 = u32::from_le_bytes(*b"NVRT");

/// Version of the TABLE FORMAT (not of the protocol). Rises as soon as the
/// meaning of a field changes -- the module rejects foreign versions.
pub const TABLE_VERSION: u32 = 1;

/// Sentinel "no offset" in every descriptor field. Identical to
/// `crate::NONE_U32`, named separately here because the C interpreter sees
/// it under this name.
pub const NONE: u32 = u32::MAX;

/// `size` value meaning: the payload size sits in the command's _IOC
/// encoding (frontend). UVM has none, so there the size is mandatory.
pub const SIZE_FROM_IOC: u32 = u32::MAX;

// ---- emb_len_kind ---------------------------------------------------------

/// No embedded pointer.
pub const EMB_NONE: u32 = 0;
/// Length sits as a u32 in the inline struct at `emb_len_off`
/// (NVOS54.paramsSize -- self-describing).
pub const EMB_LEN_FIELD: u32 = 1;
/// Length comes from the hClass table; the hClass sits as a u32 in the
/// inline struct at `emb_len_off` (RM_ALLOC is not self-describing, so
/// the length must come from the class table).
pub const EMB_LEN_CLASS: u32 = 2;
/// Length is a CONSTANT that sits in `emb_len_off` itself. The one user:
/// NV_ESC_RM_GET_EVENT_DATA -- NVOS41.pEvent points at exactly one
/// NvUnixEvent, out-only. Before this kind existed the escape fell through
/// with a raw guest VA in pEvent, which RM's os_memcpy_to_user (osapi.c:531)
/// would have written into the DAEMON's address space -- unreachable only
/// while poll never woke anybody. The event return channel wakes them.
pub const EMB_LEN_FIXED: u32 = 3;

// ---- flags in the ioctl descriptor ---------------------------------------

/// This command wraps another one (`NV_ESC_IOCTL_XFER_CMD`): the real
/// number, size and pointer sit in the payload and have to be resolved
/// FIRST.
pub const F_XFER: u32 = 1 << 0;
/// Carries a memory description out of the caller's address space
/// (`NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`): pin the pages, send the GPA runs
/// along. Only in effect when the hClass at `emb_len_off` equals
/// `osdesc_class`.
pub const F_OSDESC: u32 = 1 << 1;
/// Frees an object: on success any pin booking for the handle at
/// `handle_off` is dropped.
pub const F_FREE: u32 = 1 << 2;

/// Lengths of the structures in the stream. The interpreter checks them
/// against the header rather than believing them.
pub const HDR_LEN: usize = 96;
pub const IOCTL_DESC_LEN: usize = 48;
pub const CLASS_DESC_LEN: usize = 24;
pub const CTRL_DESC_LEN: usize = 20;
pub const NESTED_DESC_LEN: usize = 16;

/// Header of the table stream. After it follow, in this order and each
/// densely packed: `n_ioctl` ioctl descriptors, `n_class` classes,
/// `n_ctrl` control commands, `n_nested` second-level pointers.
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

    // ---- Special cases that need no table of their own, but must not
    //      land in the C code either. All can be set to NONE.
    /// `_IOC` nr of `NV_ESC_IOCTL_XFER_CMD`.
    pub xfer_nr: u32,
    /// Size of the wrapping struct `nv_ioctl_xfer_t`.
    pub xfer_struct_len: u32,
    /// Offsets inside it: cmd u32, size u32, ptr P64.
    pub xfer_cmd_off: u32,
    pub xfer_size_off: u32,
    pub xfer_ptr_off: u32,
    /// Largest payload the driver accepts at all
    /// (NV_ABSOLUTE_MAX_IOCTL_SIZE).
    pub max_ioctl_size: u32,

    /// hClass of the OS descriptor.
    pub osdesc_class: u32,
    /// Offsets in the inline struct of the OS-descriptor alloc (NVOS02):
    /// pointer to the guest memory, last byte address, status, new handle.
    pub osdesc_pmem_off: u32,
    pub osdesc_limit_off: u32,
    pub osdesc_status_off: u32,
    pub osdesc_handle_off: u32,

    /// The protocol's upper bounds, so that they too stay out of the C code.
    pub max_inline: u32,
    pub max_aux: u32,
    pub max_nested: u32,
    pub _pad: [u32; 2],
}

/// One (device type, nr) entry. If one is missing: frontend = simply
/// forward (size from `_IOC`), UVM = **ENOTSUP** (size unknown, and
/// guessing would be an out-of-bounds read on the host side, in the
/// driver's `copy_from_user`).
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
    /// Offset of a P64 that MUST be 0 (NVOS64.pRightsRequested); otherwise
    /// fail loudly rather than forward something quietly wrong.
    pub rights_off: u32,
    /// Only valid when `rights_off != NONE`: the inline size at which this
    /// field exists (NVOS64 = 48; the 32-byte form NVOS21 does not have it).
    pub rights_if_size: u32,
    /// Offset of an object handle (u32) -- for F_FREE and F_OSDESC.
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
    /// Guard for `fd_off`: translate only if the u32 at this offset equals
    /// `fd_if_val`. `NONE` = translate unconditionally.
    ///
    /// WHY A GUARD. NV0005_ALLOC_PARAMETERS carries `data` at 16 and what
    /// that field MEANS depends on `hClass` at 8, inside the same buffer:
    /// for `NV01_EVENT_OS_EVENT` (0x79) it is a file descriptor, for the
    /// kernel-callback classes it is a callback pointer. Translating a
    /// pointer as if it were an fd does not fail cleanly -- it either finds
    /// a foreign token or truncates a 64-bit address to an int.
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
    /// `CF_*`. One entry can carry all three: second-level pointers, a
    /// block, and an fd field.
    pub flags: u32,
    /// Byte offset of a process-local fd INSIDE the params buffer, or
    /// `NONE_U32`.
    ///
    /// FOUR bytes, not eight, and the difference is not cosmetic: the fd
    /// inside a *control's* params is an `NvS32` (`ctrl0000unix.h`), while
    /// `ClassDesc.fd_off` names an `NvP64` in alloc params. Writing eight
    /// bytes at a four-byte field would overwrite the neighbouring member
    /// -- for `EXPORT_OBJECT_TO_FD` that neighbour is `flags`.
    pub fd_off: u32,
}

/// Never forward this control: the guest gets `EPERM` without the message
/// ever reaching the boundary. Which commands those are, and why, is at
/// `xlate::blocked_ctrls()` -- here there is only the number.
///
/// WARNING: this is relief, not enforcement -- the block that counts sits
/// in the host. A guest that ignores the table runs into it there.
pub const CF_BLOCK: u32 = 1 << 0;

/// This control carries a process-local fd in its params (`fd_off`).
///
/// A separate flag rather than "fd_off != NONE_U32" so that a module reading
/// an OLDER table -- one whose rows are 16 bytes and carry no fd_off at all
/// -- cannot mistake whatever follows the row for an offset.
pub const CF_FD: u32 = 1 << 1;

/// This class has never been allocated on real hardware.
///
/// Its parameter size is derived from the vendor headers like every other
/// entry, so it is as correct as the headers are -- but no gate run has ever
/// moved a byte through it. The flag exists so that fact is visible AT
/// RUNTIME rather than only to someone reading the source: the host logs a
/// line the first time a guest allocates such a class, which is what turns
/// "it did not work on my card" into a specific, reportable observation.
///
/// Lives in `ClassDesc.flags`, a different field from `CtrlDesc.flags` above
/// -- hence the separate `KF_` prefix despite the identical bit value.
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

/// FNV-1a-32. Small enough to stand identically on both sides, and what is
/// at stake here is confusion, not attack.
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

    /// Known FNV-1a-32 vectors. Both sides (host Rust, module C) must
    /// compute exactly this function -- if the Rust side drifts from the
    /// reference, the module reports every table stream as corrupt.
    #[test]
    fn fnv1a32_reference_vectors() {
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
        assert_eq!(fnv1a32(b"foobar"), 0xbf9c_f968);
    }
}
