// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Serialize the descriptor tables for the guest module.
//!
//! Terms, once: RM is NVIDIA's Resource Manager, the kernel driver behind
//! /dev/nvidiactl and /dev/nvidiaN, and its ioctls are called escapes;
//! UVM is its unified-memory driver (/dev/nvidia-uvm); hClass is an RM
//! object class number.
//!
//! `xlate.rs` is and remains the source of truth; here it is only poured
//! into the stream that `nvrm-wire::tables` describes. The module is the
//! interpreter of that stream and carries **no** NVIDIA constant of its own.
//!
//! Where possible the table is **queried rather than transcribed**: the UVM
//! sizes and the class list come out of a scan over
//! `uvm_param_size`/`alloc_param_size`/`fd_field_offset`, not out of a
//! second list that could go stale. Where a rule cannot be queried (which
//! escape carries an embedded pointer), it is spelled out here -- and
//! **checked** against `xlate::embedded_ptr` (`verify_against_xlate`), so
//! that a divergence falls over loudly instead of going quietly wrong.

use nvrm_wire::tables as t;
use nvrm_wire::DevTag;

use crate::sys;
use crate::xlate::{self, Dev};

/// Finished table stream, header and checksum filled in.
pub struct Tables {
    pub bytes: Vec<u8>,
    pub checksum: u32,
    pub version: u32,
    pub n_ioctl: u32,
    pub n_class: u32,
    pub n_ctrl: u32,
    pub n_nested: u32,
}

fn push_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn push_ioctl(v: &mut Vec<u8>, d: &t::IoctlDesc) {
    for x in [
        d.dev, d.nr, d.size, d.fd_off, d.emb_ptr_off, d.emb_len_kind, d.emb_len_off,
        d.cmd_off, d.rights_off, d.rights_if_size, d.handle_off, d.flags,
    ] {
        push_u32(v, x);
    }
}

fn desc(dev: DevTag, nr: u32) -> t::IoctlDesc {
    t::IoctlDesc {
        dev: dev as u32,
        nr,
        size: t::SIZE_FROM_IOC,
        fd_off: t::NONE,
        emb_ptr_off: t::NONE,
        emb_len_kind: t::EMB_NONE,
        emb_len_off: t::NONE,
        cmd_off: t::NONE,
        rights_off: t::NONE,
        rights_if_size: t::NONE,
        handle_off: t::NONE,
        flags: 0,
    }
}

/// The highest UVM command value the scan covers. UVM numbers its commands
/// from 1 upwards (the dense range reaches 81 today, `uvm_ioctl.h`), and
/// 2047 is the highest number the header uses at all -- the outlier
/// `UVM_IOCTL_BASE(2047)`, which `uvm_param_size` does not implement. The
/// scan reaches it anyway: an unimplemented command contributes no row
/// (the `None` arm skips it), so covering it costs one more pass over a
/// match at build time and buys the guarantee that the day it DOES get a
/// size, the tables get the descriptor instead of silently dropping it.
/// Outside the scan, knowingly: the two 0x3000_000x values from
/// `uvm_linux_ioctl.h`, added separately.
const UVM_SCAN_MAX: u32 = 2047;
const UVM_SPECIAL: [u32; 2] = [xlate::uvm::INITIALIZE, xlate::uvm::DEINITIALIZE];

/// The highest hClass the class scan covers. Classes are 16 bit
/// (`resource_list.h`), so the scan is exhaustive rather than a sample.
const CLASS_SCAN_MAX: u32 = 0xffff;

/// Frontend nrs the scan searches for fd fields. Escapes are 8 bit (they
/// sit inside the `_IOC` nr).
const FRONTEND_SCAN_MAX: u32 = 0xff;

/// The four descriptor lists before serialization -- the writer's view.
/// `build()` pours them into the stream; `expect_dump()` writes them as
/// text, against which the C interpreter
/// (`guest-module/virtio_nvrm/test/tabcheck.c`) diffs its own reading.
struct Parts {
    ioctls: Vec<t::IoctlDesc>,
    classes: Vec<t::ClassDesc>,
    ctrls: Vec<t::CtrlDesc>,
    nested: Vec<t::NestedDescRow>,
}

pub fn build() -> Tables {
    let p = collect();
    let (bytes, checksum) = serialize(&p);
    Tables {
        bytes,
        checksum,
        version: t::TABLE_VERSION,
        n_ioctl: p.ioctls.len() as u32,
        n_class: p.classes.len() as u32,
        n_ctrl: p.ctrls.len() as u32,
        n_nested: p.nested.len() as u32,
    }
}

fn collect() -> Parts {
    verify_against_xlate();

    let mut ioctls: Vec<t::IoctlDesc> = Vec::new();

    // ---- UVM: sizes and fd fields, both queried from xlate ---------------
    for dev_tag in [DevTag::Uvm, DevTag::UvmTools] {
        let xdev = if dev_tag == DevTag::Uvm { Dev::Uvm } else { Dev::UvmTools };
        let nrs = (0..=UVM_SCAN_MAX).chain(UVM_SPECIAL);
        for nr in nrs {
            let Some(size) = xlate::uvm_param_size(nr) else { continue };
            let mut d = desc(dev_tag, nr);
            d.size = size;
            if let Some(off) = xlate::fd_field_offset(xdev, nr, size) {
                d.fd_off = off;
            }
            ioctls.push(d);
        }
    }

    // ---- Frontend: fd fields queried from xlate --------------------------
    // The size sits in the _IOC encoding, so `size` stays at SIZE_FROM_IOC
    // here. An escape with no entry is simply forwarded.
    for dev_tag in [DevTag::Ctl, DevTag::Gpu] {
        let xdev = if dev_tag == DevTag::Ctl { Dev::Ctl } else { Dev::Gpu };
        for nr in 0..=FRONTEND_SCAN_MAX {
            let fd_off = xlate::fd_field_offset(xdev, nr, 0);
            let special = frontend_special(nr);
            if fd_off.is_none() && special.is_none() {
                continue;
            }
            let mut d = desc(dev_tag, nr);
            if let Some(off) = fd_off {
                d.fd_off = off;
            }
            if let Some(s) = special {
                d.emb_ptr_off = s.emb_ptr_off;
                d.emb_len_kind = s.emb_len_kind;
                d.emb_len_off = s.emb_len_off;
                d.cmd_off = s.cmd_off;
                d.rights_off = s.rights_off;
                d.rights_if_size = s.rights_if_size;
                d.handle_off = s.handle_off;
                d.flags = s.flags;
            }
            ioctls.push(d);
        }
    }

    // ---- hClass -> size (+ fd inside the params buffer) ------------------
    let mut classes: Vec<t::ClassDesc> = Vec::new();
    for hclass in 0..=CLASS_SCAN_MAX {
        let Some(param_size) = xlate::alloc_param_size(hclass) else { continue };
        classes.push(t::ClassDesc {
            hclass,
            param_size,
            fd_off: xlate::alloc_fd_field(hclass).unwrap_or(t::NONE),
            fd_if_off: xlate::alloc_fd_guard(hclass).map_or(t::NONE, |g| g.0),
            fd_if_val: xlate::alloc_fd_guard(hclass).map_or(0, |g| g.1),
            // Derived from the vendor headers either way; the flag says
            // whether it has ever run on real silicon. See
            // xlate::alloc_class_verified.
            flags: if xlate::alloc_class_verified(hclass) { 0 } else { t::KF_UNVERIFIED },
        });
    }

    // ---- second-level pointers, per control command ----------------------
    let mut ctrls: Vec<t::CtrlDesc> = Vec::new();
    let mut nested: Vec<t::NestedDescRow> = Vec::new();
    for &cmd in xlate::nested_cmds() {
        let specs = xlate::nested_ptrs(cmd);
        assert!(
            !specs.is_empty(),
            "nested_cmds() names {cmd:#x}, nested_ptrs() does not know it"
        );
        let fd = xlate::ctrl_fd_offset(cmd);
        ctrls.push(t::CtrlDesc {
            cmd,
            first: nested.len() as u32,
            count: specs.len() as u32,
            flags: (if xlate::ctrl_blocked(cmd) { t::CF_BLOCK } else { 0 })
                | (if fd.is_some() { t::CF_FD } else { 0 }),
            fd_off: fd.unwrap_or(t::NONE),
        });
        for sp in specs {
            let (len_kind, len_off, elem) = match sp.len {
                xlate::LenSource::Fixed(n) => (t::NLEN_FIXED, n, 1),
                xlate::LenSource::Field { off, elem } => (t::NLEN_FIELD, off, elem),
            };
            nested.push(t::NestedDescRow { ptr_off: sp.ptr_off, len_kind, len_off, elem });
        }
    }

    // Blocked controls with no second-level pointers appear in no table
    // otherwise -- without an entry the module would never see the block.
    for &cmd in xlate::blocked_ctrls() {
        if ctrls.iter().any(|c| c.cmd == cmd) {
            continue; // already has a row, the flag was set above
        }
        ctrls.push(t::CtrlDesc {
            cmd, first: 0, count: 0, flags: t::CF_BLOCK, fd_off: t::NONE,
        });
    }

    // Controls that carry an fd but no second-level pointer have no row from
    // either loop above -- and without a row the module never learns the
    // offset. Same shape as the blocked ones directly above.
    for &cmd in xlate::ctrl_fd_cmds() {
        let off = xlate::ctrl_fd_offset(cmd).unwrap_or_else(|| {
            panic!("ctrl_fd_cmds() names {cmd:#x}, ctrl_fd_offset() does not know it")
        });
        if let Some(row) = ctrls.iter_mut().find(|c| c.cmd == cmd) {
            row.flags |= t::CF_FD; // already has a row, from nested or blocked
            row.fd_off = off;
            continue;
        }
        ctrls.push(t::CtrlDesc {
            cmd, first: 0, count: 0, flags: t::CF_FD, fd_off: off,
        });
    }

    Parts { ioctls, classes, ctrls, nested }
}

/// The 24 header words in wire order (nvrm_wire.h) -- the ONE place that
/// knows the order; `build()` and `expect_dump()` share it.
fn header_words(p: &Parts, total_len: u32, checksum: u32) -> [u32; 24] {
    [
        t::TABLE_MAGIC,
        t::TABLE_VERSION,
        total_len,
        checksum,
        p.ioctls.len() as u32,
        p.classes.len() as u32,
        p.ctrls.len() as u32,
        p.nested.len() as u32,
        // Special cases: XFER wrapping.
        sys::NV_ESC_IOCTL_XFER_CMD,
        core::mem::size_of::<sys::nv_ioctl_xfer_t>() as u32,
        core::mem::offset_of!(sys::nv_ioctl_xfer_t, cmd) as u32,
        core::mem::offset_of!(sys::nv_ioctl_xfer_t, size) as u32,
        core::mem::offset_of!(sys::nv_ioctl_xfer_t, ptr) as u32,
        crate::xfer::ABSOLUTE_MAX_IOCTL_SIZE as u32,
        // Special cases: OS descriptor.
        OSDESC_CLASS,
        NVOS02_PMEMORY_OFF,
        NVOS02_LIMIT_OFF,
        NVOS02_STATUS_OFF,
        NVOS02_HOBJECTNEW_OFF,
        // Protocol upper bounds.
        nvrm_wire::MAX_PAYLOAD as u32,
        nvrm_wire::MAX_AUX as u32,
        nvrm_wire::MAX_NESTED as u32,
        0, 0,
    ]
}

fn serialize(p: &Parts) -> (Vec<u8>, u32) {
    let mut body = Vec::new();
    for d in &p.ioctls {
        push_ioctl(&mut body, d);
    }
    for c in &p.classes {
        for x in [c.hclass, c.param_size, c.fd_off, c.flags, c.fd_if_off, c.fd_if_val] {
            push_u32(&mut body, x);
        }
    }
    for c in &p.ctrls {
        for x in [c.cmd, c.first, c.count, c.flags, c.fd_off] {
            push_u32(&mut body, x);
        }
    }
    for n in &p.nested {
        for x in [n.ptr_off, n.len_kind, n.len_off, n.elem] {
            push_u32(&mut body, x);
        }
    }

    let checksum = t::fnv1a32(&body);
    let total_len = (t::HDR_LEN + body.len()) as u32;

    let mut bytes = Vec::with_capacity(total_len as usize);
    for x in header_words(p, total_len, checksum) {
        push_u32(&mut bytes, x);
    }
    assert_eq!(bytes.len(), t::HDR_LEN, "table header has the wrong length");
    bytes.extend_from_slice(&body);
    (bytes, checksum)
}

/// The writer's view as text -- line for line the format
/// `guest-module/virtio_nvrm/test/tabcheck.c` prints when READING the
/// stream. Deliberately formatted from the source
/// structures, not from the serialized bytes: the diff of the two outputs
/// checks exactly the stretch in between (serialization in Rust, parse +
/// struct layout + find_* in C). All raw decimal -- formatting logic would
/// be surface for divergence.
pub fn expect_dump() -> String {
    use std::fmt::Write;

    let p = collect();
    let (bytes, checksum) = serialize(&p);
    let h = header_words(&p, bytes.len() as u32, checksum);

    let mut out = String::new();
    out.push_str("hdr");
    // tabcheck prints the 22 named header fields, without the two pad words.
    for x in &h[..22] {
        write!(out, " {x}").unwrap();
    }
    out.push('\n');
    for d in &p.ioctls {
        writeln!(
            out,
            "ioctl {} {} {} {} {} {} {} {} {} {} {} {}",
            d.dev, d.nr, d.size, d.fd_off, d.emb_ptr_off, d.emb_len_kind,
            d.emb_len_off, d.cmd_off, d.rights_off, d.rights_if_size,
            d.handle_off, d.flags
        )
        .unwrap();
    }
    for c in &p.classes {
        writeln!(out, "class {} {} {} {} {} {}", c.hclass, c.param_size, c.fd_off,
                 c.flags, c.fd_if_off, c.fd_if_val).unwrap();
    }
    for c in &p.ctrls {
        writeln!(out, "ctrl {} {} {} {} {}", c.cmd, c.first, c.count, c.flags, c.fd_off)
            .unwrap();
    }
    for n in &p.nested {
        writeln!(out, "nested {} {} {} {}", n.ptr_off, n.len_kind, n.len_off, n.elem).unwrap();
    }
    out
}

// ---------------------------------------------------------------------------
// The rules that cannot be queried
// ---------------------------------------------------------------------------
// `embedded_ptr` needs a filled-in payload to answer -- as a function it is
// not enumerable. The structure of its decision therefore stands here,
// field by field, with the same evidence as there. It is checked in
// verify_against_xlate(): once a number no longer agrees with xlate, the
// HOST falls over while building the table -- not the guest, at some point
// mid-run.

/// NVOS54 (`NV_ESC_RM_CONTROL`): cmd u32 @8, params P64 @16, paramsSize u32
/// @24 -- self-describing (xlate::embedded_ptr).
const NVOS54_CMD_OFF: u32 = 8;
const NVOS54_PARAMS_OFF: u32 = 16;
const NVOS54_PARAMSSIZE_OFF: u32 = 24;

/// NVOS64/NVOS21 (`NV_ESC_RM_ALLOC`): hClass u32 @12, pAllocParms P64 @16,
/// pRightsRequested P64 @24 (only in the 48-byte form, NVOS64).
const NVOS64_HCLASS_OFF: u32 = 12;
const NVOS64_PARAMS_OFF: u32 = 16;
const NVOS64_RIGHTS_OFF: u32 = 24;
const NVOS64_LEN: u32 = 48;

/// NVOS00 (`NV_ESC_RM_FREE`): hObjectOld u32 @8.
const NVOS00_HOBJECTOLD_OFF: u32 = 8;

/// NVOS02 (`NV_ESC_RM_ALLOC_MEMORY`, nvos.h:285-295): hObjectNew @8,
/// hClass @12, pMemory P64 @24, limit u64 @32, status u32 @40.
const NVOS02_HOBJECTNEW_OFF: u32 = 8;
const NVOS02_HCLASS_OFF: u32 = 12;
const NVOS02_PMEMORY_OFF: u32 = 24;
const NVOS02_LIMIT_OFF: u32 = 32;
const NVOS02_STATUS_OFF: u32 = 40;

/// `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`.
const OSDESC_CLASS: u32 = 0x71;

// Every constant above is a hand-transcribed offset into a struct bindgen
// already knows. These guards tie the two together at COMPILE time: change
// a number here and the crate no longer builds, instead of the guest module
// reading the wrong word of a parameter block at run time. The values
// themselves stay literals on purpose -- a constant defined as
// `offset_of!(..)` would agree with the struct by construction and check
// nothing.
const _: () = {
    use core::mem::{offset_of, size_of};

    // NVOS54 -- RM_CONTROL (nvos.h).
    assert!(NVOS54_CMD_OFF as usize == offset_of!(sys::NVOS54_PARAMETERS, cmd));
    assert!(NVOS54_PARAMS_OFF as usize == offset_of!(sys::NVOS54_PARAMETERS, params));
    assert!(NVOS54_PARAMSSIZE_OFF as usize == offset_of!(sys::NVOS54_PARAMETERS, paramsSize));

    // NVOS64 -- the 48-byte form of RM_ALLOC. `NVOS64_LEN` is what tells
    // the module which of the two forms it has in front of it, so it must
    // be the size of the struct and not merely "48".
    assert!(NVOS64_HCLASS_OFF as usize == offset_of!(sys::NVOS64_PARAMETERS, hClass));
    assert!(NVOS64_PARAMS_OFF as usize == offset_of!(sys::NVOS64_PARAMETERS, pAllocParms));
    assert!(NVOS64_RIGHTS_OFF as usize == offset_of!(sys::NVOS64_PARAMETERS, pRightsRequested));
    assert!(NVOS64_LEN as usize == size_of::<sys::NVOS64_PARAMETERS>());

    // NVOS00 -- RM_FREE.
    assert!(NVOS00_HOBJECTOLD_OFF as usize == offset_of!(sys::NVOS00_PARAMETERS, hObjectOld));

    // NVOS02 -- RM_ALLOC_MEMORY, the OS-descriptor path. These five ride in
    // the descriptor-table header (see `header_words`), and the guest
    // module writes the pinned pages' address and limit at exactly these
    // offsets.
    assert!(NVOS02_HOBJECTNEW_OFF as usize == offset_of!(sys::NVOS02_PARAMETERS, hObjectNew));
    assert!(NVOS02_HCLASS_OFF as usize == offset_of!(sys::NVOS02_PARAMETERS, hClass));
    assert!(NVOS02_PMEMORY_OFF as usize == offset_of!(sys::NVOS02_PARAMETERS, pMemory));
    assert!(NVOS02_LIMIT_OFF as usize == offset_of!(sys::NVOS02_PARAMETERS, limit));
    assert!(NVOS02_STATUS_OFF as usize == offset_of!(sys::NVOS02_PARAMETERS, status));
};

struct Special {
    emb_ptr_off: u32,
    emb_len_kind: u32,
    emb_len_off: u32,
    cmd_off: u32,
    rights_off: u32,
    rights_if_size: u32,
    handle_off: u32,
    flags: u32,
}

fn frontend_special(nr: u32) -> Option<Special> {
    let base = Special {
        emb_ptr_off: t::NONE,
        emb_len_kind: t::EMB_NONE,
        emb_len_off: t::NONE,
        cmd_off: t::NONE,
        rights_off: t::NONE,
        rights_if_size: t::NONE,
        handle_off: t::NONE,
        flags: 0,
    };
    Some(match nr {
        x if x == sys::NV_ESC_RM_CONTROL => Special {
            emb_ptr_off: NVOS54_PARAMS_OFF,
            emb_len_kind: t::EMB_LEN_FIELD,
            emb_len_off: NVOS54_PARAMSSIZE_OFF,
            cmd_off: NVOS54_CMD_OFF,
            ..base
        },
        x if x == sys::NV_ESC_RM_ALLOC => Special {
            emb_ptr_off: NVOS64_PARAMS_OFF,
            emb_len_kind: t::EMB_LEN_CLASS,
            emb_len_off: NVOS64_HCLASS_OFF,
            rights_off: NVOS64_RIGHTS_OFF,
            rights_if_size: NVOS64_LEN,
            ..base
        },
        x if x == sys::NV_ESC_RM_FREE => Special {
            handle_off: NVOS00_HOBJECTOLD_OFF,
            flags: t::F_FREE,
            ..base
        },
        x if x == sys::NV_ESC_RM_ALLOC_MEMORY => Special {
            // No embedded pointer: NVOS02.pMemory points at memory RM
            // PINS rather than copies. Hence F_OSDESC -- the module
            // resolves the pages into GPA runs instead of sending a guest
            // VA.
            emb_len_off: NVOS02_HCLASS_OFF,
            handle_off: NVOS02_HOBJECTNEW_OFF,
            flags: t::F_OSDESC,
            ..base
        },
        x if x == sys::NV_ESC_IOCTL_XFER_CMD => Special { flags: t::F_XFER, ..base },
        // NVOS41: pEvent P64 @0 -> ONE NvUnixEvent, out-only; the length is
        // a constant, so it rides in emb_len_off itself (EMB_LEN_FIXED).
        x if x == sys::NV_ESC_RM_GET_EVENT_DATA => Special {
            emb_ptr_off: core::mem::offset_of!(sys::NVOS41_PARAMETERS, pEvent) as u32,
            emb_len_kind: t::EMB_LEN_FIXED,
            emb_len_off: core::mem::size_of::<sys::NvUnixEvent>() as u32,
            ..base
        },
        _ => return None,
    })
}

/// Counter-check: the same decision `xlate::embedded_ptr` makes, recomputed
/// from the numbers in `frontend_special`. Runs while the table is built --
/// that is, at every host start.
fn verify_against_xlate() {
    // (1) RM_CONTROL: params @16, length from the field @24.
    let mut buf = [0u8; 32];
    buf[16..24].copy_from_slice(&0xdead_beef_u64.to_le_bytes()); // params != 0
    buf[24..28].copy_from_slice(&1234u32.to_le_bytes()); // paramsSize
    let got = unsafe { xlate::embedded_ptr(Dev::Ctl, sys::NV_ESC_RM_CONTROL, buf.as_ptr(), 32) };
    match got {
        Ok(Some(e)) => assert!(
            e.ptr_off == NVOS54_PARAMS_OFF && e.len == 1234,
            "xlate::embedded_ptr(RM_CONTROL) = ({}, {}), the table says \
             ({NVOS54_PARAMS_OFF}, field @{NVOS54_PARAMSSIZE_OFF})",
            e.ptr_off, e.len
        ),
        other => panic!("xlate::embedded_ptr(RM_CONTROL) unexpected: {:?}", other.is_ok()),
    }

    // (2) RM_ALLOC: params @16, length from the hClass table.
    let mut buf = [0u8; 48];
    let probe_class = 0x2080u32; // NV20_SUBDEVICE_0, 4-byte params
    let want = xlate::alloc_param_size(probe_class).expect("probe class missing from xlate");
    buf[12..16].copy_from_slice(&probe_class.to_le_bytes());
    buf[16..24].copy_from_slice(&0xdead_beef_u64.to_le_bytes());
    let got = unsafe { xlate::embedded_ptr(Dev::Ctl, sys::NV_ESC_RM_ALLOC, buf.as_ptr(), 48) };
    match got {
        Ok(Some(e)) => assert!(
            e.ptr_off == NVOS64_PARAMS_OFF && e.len == want,
            "xlate::embedded_ptr(RM_ALLOC) = ({}, {}), the table says \
             ({NVOS64_PARAMS_OFF}, hClass @{NVOS64_HCLASS_OFF} -> {want})",
            e.ptr_off, e.len
        ),
        other => panic!("xlate::embedded_ptr(RM_ALLOC) unexpected: {:?}", other.is_ok()),
    }

    // (3) pRightsRequested != 0 must fail loudly -- the table tells the
    //     module the same thing via rights_off/rights_if_size.
    buf[24..32].copy_from_slice(&1u64.to_le_bytes());
    let got = unsafe { xlate::embedded_ptr(Dev::Ctl, sys::NV_ESC_RM_ALLOC, buf.as_ptr(), 48) };
    assert!(got.is_err(), "xlate accepts pRightsRequested != 0, the table does not");

    // (4) GET_EVENT_DATA: pEvent @0, length = one NvUnixEvent, constant.
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&0xdead_beef_u64.to_le_bytes());
    let got = unsafe { xlate::embedded_ptr(Dev::Ctl, sys::NV_ESC_RM_GET_EVENT_DATA, buf.as_ptr(), 16) };
    let want = core::mem::size_of::<sys::NvUnixEvent>() as u32;
    match got {
        Ok(Some(e)) => assert!(
            e.ptr_off == 0 && e.len == want,
            "xlate::embedded_ptr(GET_EVENT_DATA) = ({}, {}), the table says (0, {want})",
            e.ptr_off, e.len
        ),
        other => panic!("xlate::embedded_ptr(GET_EVENT_DATA) unexpected: {:?}", other.is_ok()),
    }

    // (5) The 0x27 collision, which is why the table keys on (device, nr).
    assert_eq!(
        sys::NV_ESC_RM_ALLOC_MEMORY, xlate::uvm::PAGEABLE_MEM_ACCESS,
        "the 0x27 collision ctl/uvm is gone -- check the table key"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-side decoder of the stream -- deliberately independent of the
    /// builder code above (reads raw LE bytes, the way the C module does).
    struct Decoded {
        hdr: Vec<u32>,
        ioctls: Vec<Vec<u32>>,
        classes: Vec<Vec<u32>>,
        ctrls: Vec<Vec<u32>>,
        nested: Vec<Vec<u32>>,
    }

    fn decode(tb: &Tables) -> Decoded {
        let u32s = |bytes: &[u8]| -> Vec<u32> {
            bytes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
        };
        let rows = |base: usize, n: usize, len: usize| -> Vec<Vec<u32>> {
            (0..n).map(|i| u32s(&tb.bytes[base + i * len..base + (i + 1) * len])).collect()
        };
        let hdr = u32s(&tb.bytes[..t::HDR_LEN]);
        let (ni, nc, nk, nn) =
            (hdr[4] as usize, hdr[5] as usize, hdr[6] as usize, hdr[7] as usize);
        let b0 = t::HDR_LEN;
        let b1 = b0 + ni * t::IOCTL_DESC_LEN;
        let b2 = b1 + nc * t::CLASS_DESC_LEN;
        let b3 = b2 + nk * t::CTRL_DESC_LEN;
        Decoded {
            hdr,
            ioctls: rows(b0, ni, t::IOCTL_DESC_LEN),
            classes: rows(b1, nc, t::CLASS_DESC_LEN),
            ctrls: rows(b2, nk, t::CTRL_DESC_LEN),
            nested: rows(b3, nn, t::NESTED_DESC_LEN),
        }
    }

    /// Word indices into an IoctlDesc row (order from push_ioctl / nvrm_wire.h).
    const D_DEV: usize = 0;
    const D_NR: usize = 1;
    const D_SIZE: usize = 2;
    const D_FD_OFF: usize = 3;
    const D_EMB_PTR: usize = 4;
    const D_EMB_KIND: usize = 5;
    const D_EMB_LEN: usize = 6;
    const D_CMD_OFF: usize = 7;
    const D_RIGHTS: usize = 8;
    const D_RIGHTS_IF: usize = 9;
    const D_HANDLE: usize = 10;
    const D_FLAGS: usize = 11;

    fn find(d: &Decoded, dev: DevTag, nr: u32) -> &Vec<u32> {
        d.ioctls
            .iter()
            .find(|r| r[D_DEV] == dev as u32 && r[D_NR] == nr)
            .unwrap_or_else(|| panic!("({dev:?}, {nr:#x}) missing from the stream"))
    }

    #[test]
    fn stream_is_self_consistent() {
        let tb = build();
        assert_eq!(tb.bytes.len(), u32::from_le_bytes(tb.bytes[8..12].try_into().unwrap()) as usize);
        assert_eq!(t::fnv1a32(&tb.bytes[t::HDR_LEN..]), tb.checksum);
        let n = tb.n_ioctl as usize * t::IOCTL_DESC_LEN
            + tb.n_class as usize * t::CLASS_DESC_LEN
            + tb.n_ctrl as usize * t::CTRL_DESC_LEN
            + tb.n_nested as usize * t::NESTED_DESC_LEN;
        assert_eq!(tb.bytes.len(), t::HDR_LEN + n);
    }

    /// The five UVM and five frontend fd fields must all be in the stream
    /// -- otherwise the module fails to translate an fd and the host gets a
    /// guest-local number.
    #[test]
    fn fd_fields_survive_serialisation() {
        let tb = build();
        let mut found = 0;
        for i in 0..tb.n_ioctl as usize {
            let o = t::HDR_LEN + i * t::IOCTL_DESC_LEN;
            let f = |k: usize| u32::from_le_bytes(tb.bytes[o + k * 4..o + k * 4 + 4].try_into().unwrap());
            if f(D_FD_OFF) != t::NONE {
                found += 1;
            }
        }
        // 5 frontend fields each on ctl and gpu, 5 UVM fields each on uvm and
        // uvm-tools = 20.
        assert_eq!(found, 20, "fd fields in the stream: {found}, expected 20");
    }

    /// Every blocked control is in the stream and carries CF_BLOCK. Drop the
    /// entry and the module sends the command out again -- and the block
    /// would rest on the host alone instead of on both sides.
    #[test]
    fn blocked_ctrls_are_in_the_stream() {
        let tb = build();
        let base = t::HDR_LEN
            + tb.n_ioctl as usize * t::IOCTL_DESC_LEN
            + tb.n_class as usize * t::CLASS_DESC_LEN;
        for &cmd in xlate::blocked_ctrls() {
            let mut seen = false;
            for i in 0..tb.n_ctrl as usize {
                let o = base + i * t::CTRL_DESC_LEN;
                let f = |k: usize| {
                    u32::from_le_bytes(tb.bytes[o + k * 4..o + k * 4 + 4].try_into().unwrap())
                };
                if f(0) == cmd {
                    assert!(f(3) & t::CF_BLOCK != 0, "{cmd:#x} is in the stream, but without CF_BLOCK");
                    seen = true;
                }
            }
            assert!(seen, "blocked control {cmd:#x} is missing from the table stream");
        }
    }

    /// The three length rules, as the module has to see them in the stream:
    /// (1) RM_CONTROL is self-describing (paramsSize @24),
    /// (2) RM_ALLOC takes the length from the hClass table (hClass @12),
    /// (3) UVM has no _IOC size -- every row MUST carry a fixed length, or
    ///     the host would guess and the driver's copy_from_user would read
    ///     past the end.
    /// Between (2) and (3) sits NVOS41's constant-length variant of (1):
    /// the length is a constant riding in the length field itself
    /// (EMB_LEN_FIXED).
    /// All offsets: nvos.h, evidenced at the NVOS* constants above.
    #[test]
    fn the_three_length_rules() {
        let d = decode(&build());

        // (1) NVOS54: params P64 @16, paramsSize u32 @24, cmd u32 @8.
        let ctl = find(&d, DevTag::Ctl, sys::NV_ESC_RM_CONTROL);
        assert_eq!(ctl[D_SIZE], t::SIZE_FROM_IOC);
        assert_eq!(ctl[D_EMB_PTR], 16);
        assert_eq!(ctl[D_EMB_KIND], t::EMB_LEN_FIELD);
        assert_eq!(ctl[D_EMB_LEN], 24);
        assert_eq!(ctl[D_CMD_OFF], 8);

        // (2) NVOS64: params P64 @16, hClass u32 @12; pRightsRequested @24
        //     exists only in the 48-byte form.
        let al = find(&d, DevTag::Ctl, sys::NV_ESC_RM_ALLOC);
        assert_eq!(al[D_EMB_PTR], 16);
        assert_eq!(al[D_EMB_KIND], t::EMB_LEN_CLASS);
        assert_eq!(al[D_EMB_LEN], 12);
        assert_eq!(al[D_RIGHTS], 24);
        assert_eq!(al[D_RIGHTS_IF], 48);

        // (2b) NVOS41: pEvent P64 @0, and the length is a CONSTANT that
        //      rides in the length field itself. This variant was the
        //      last case this test learned to state, and it is the one whose
        //      value cannot be read out of the guest's own buffer -- if it
        //      drifts from sizeof(NvUnixEvent) the module copies the wrong
        //      number of bytes back and nothing else here would notice.
        let ev = find(&d, DevTag::Ctl, sys::NV_ESC_RM_GET_EVENT_DATA);
        assert_eq!(ev[D_EMB_PTR], 0);
        assert_eq!(ev[D_EMB_KIND], t::EMB_LEN_FIXED);
        assert_eq!(
            ev[D_EMB_LEN] as usize,
            core::mem::size_of::<sys::NvUnixEvent>(),
            "EMB_LEN_FIXED carries the length itself, so it must BE sizeof(NvUnixEvent)"
        );

        // (3) Every UVM row carries the size from xlate, never
        //     SIZE_FROM_IOC; and every size xlate knows is in the stream.
        let mut uvm_rows = 0;
        for r in &d.ioctls {
            if r[D_DEV] == DevTag::Uvm as u32 || r[D_DEV] == DevTag::UvmTools as u32 {
                uvm_rows += 1;
                assert_ne!(r[D_SIZE], t::SIZE_FROM_IOC, "UVM nr {:#x} without a size", r[D_NR]);
                assert_eq!(
                    Some(r[D_SIZE]),
                    xlate::uvm_param_size(r[D_NR]),
                    "UVM nr {:#x}: size in the stream != xlate",
                    r[D_NR]
                );
            }
        }
        let known = (0..=UVM_SCAN_MAX)
            .chain(UVM_SPECIAL)
            .filter(|&nr| xlate::uvm_param_size(nr).is_some())
            .count();
        assert_eq!(uvm_rows, known * 2, "one row each for uvm and uvm-tools");
    }

    /// The special cases in the header and their descriptor flags: XFER
    /// wrapping, OS descriptor, FREE, and the protocol bounds.
    #[test]
    fn header_specials_match_the_source() {
        let tb = build();
        let d = decode(&tb);
        let h = &d.hdr;

        // Header word indices as in nvrm_wire.h (one u32 each).
        assert_eq!(h[0], t::TABLE_MAGIC);
        assert_eq!(h[1], t::TABLE_VERSION);

        // XFER: number and wrapping layout from nv-ioctl.h (bindgen).
        assert_eq!(h[8], sys::NV_ESC_IOCTL_XFER_CMD);
        assert_eq!(h[9], core::mem::size_of::<sys::nv_ioctl_xfer_t>() as u32);
        assert_eq!(h[10], core::mem::offset_of!(sys::nv_ioctl_xfer_t, cmd) as u32);
        assert_eq!(h[11], core::mem::offset_of!(sys::nv_ioctl_xfer_t, size) as u32);
        assert_eq!(h[12], core::mem::offset_of!(sys::nv_ioctl_xfer_t, ptr) as u32);
        assert_eq!(h[13], crate::xfer::ABSOLUTE_MAX_IOCTL_SIZE as u32);
        let xf = find(&d, DevTag::Ctl, sys::NV_ESC_IOCTL_XFER_CMD);
        assert_eq!(xf[D_FLAGS] & t::F_XFER, t::F_XFER);

        // OS descriptor: hClass 0x71, NVOS02 offsets (nvos.h:285-295).
        assert_eq!(h[14], 0x71);
        assert_eq!(h[15], 24, "NVOS02.pMemory");
        assert_eq!(h[16], 32, "NVOS02.limit");
        assert_eq!(h[17], 40, "NVOS02.status");
        assert_eq!(h[18], 8, "NVOS02.hObjectNew");
        let os = find(&d, DevTag::Ctl, sys::NV_ESC_RM_ALLOC_MEMORY);
        assert_eq!(os[D_FLAGS] & t::F_OSDESC, t::F_OSDESC);
        assert_eq!(os[D_HANDLE], 8);
        assert_eq!(os[D_EMB_LEN], 12, "the hClass field that arms F_OSDESC");

        // FREE: hObjectOld @8 (NVOS00), Flag F_FREE.
        let fr = find(&d, DevTag::Ctl, sys::NV_ESC_RM_FREE);
        assert_eq!(fr[D_FLAGS] & t::F_FREE, t::F_FREE);
        assert_eq!(fr[D_HANDLE], 8);

        // Protocol bounds travel from proto into the header, not into the C code.
        assert_eq!(h[19], nvrm_wire::MAX_PAYLOAD as u32);
        assert_eq!(h[20], nvrm_wire::MAX_AUX as u32);
        assert_eq!(h[21], nvrm_wire::MAX_NESTED as u32);
    }

    /// The hClass table in the stream is exactly the xlate scan: same set,
    /// same sizes, same fd fields. Plus the two numbers that once carried
    /// hand-maintenance errors, nailed down as literals.
    #[test]
    fn hclass_sizes_match_xlate() {
        let d = decode(&build());
        let mut seen = std::collections::BTreeSet::new();
        for c in &d.classes {
            let (hclass, param_size, fd_off, flags) = (c[0], c[1], c[2], c[3]);
            assert!(seen.insert(hclass), "hClass {hclass:#x} twice in the stream");
            assert_eq!(Some(param_size), xlate::alloc_param_size(hclass), "{hclass:#x}");
            assert_eq!(fd_off, xlate::alloc_fd_field(hclass).unwrap_or(t::NONE), "{hclass:#x}");
            // The flag must reach the WIRE, not just the struct: the row is
            // serialised field by field, so a hardcoded constant there would
            // leave every class looking verified.
            let want = if xlate::alloc_class_verified(hclass) { 0 } else { t::KF_UNVERIFIED };
            assert_eq!(flags, want, "{hclass:#x} flags");
        }
        // Both states must actually occur, otherwise the assertion above is
        // vacuous -- a scan that marked everything the same way would pass.
        assert!(d.classes.iter().any(|c| c[3] & t::KF_UNVERIFIED != 0), "no unverified classes");
        assert!(d.classes.iter().any(|c| c[3] & t::KF_UNVERIFIED == 0), "no verified classes");
        let known: std::collections::BTreeSet<u32> =
            (0..=CLASS_SCAN_MAX).filter(|&h| xlate::alloc_param_size(h).is_some()).collect();
        assert_eq!(seen, known, "class set in the stream != xlate scan");

        // The two former errors: 0x90f1 with pasid = 56 (not 48, gVisor),
        // 0x71 with its own 40-byte struct (not 128, not NVOS32).
        assert_eq!(xlate::alloc_param_size(0x90f1), Some(56));
        assert_eq!(xlate::alloc_param_size(0x0071), Some(40));
        // And the one fd field inside the aux buffer: NV0005.data @16 (cl0005.h).
        assert_eq!(xlate::alloc_fd_field(0x0079), Some(16));
    }

    /// No command carries more second-level pointers than `MAX_NESTED`.
    ///
    /// This is the invariant an index access in `session.rs` hangs off
    /// -- an index into a fixed-size array (`req.nested[i]`, `MAX_NESTED = 4`)
    /// -- and because it holds, that barrier is unreachable today. If it
    /// breaks, the barrier has to bite; which is why it is checked HERE and
    /// not assumed there.
    #[test]
    fn nested_ptrs_stay_within_max_nested() {
        for &cmd in xlate::nested_cmds() {
            let n = xlate::nested_ptrs(cmd).len();
            assert!(
                n <= xlate::MAX_NESTED,
                "{cmd:#x} has {n} second-level pointers, MAX_NESTED is {}",
                xlate::MAX_NESTED
            );
        }
        assert_eq!(xlate::MAX_NESTED, nvrm_wire::MAX_NESTED, "both sides, one number");
    }

    /// Every control entry points INTO the nested table -- the C
    /// interpreter relies on that once it has checked the header. And every
    /// command from xlate::nested_cmds() is in the stream with exactly its
    /// second-level pointers.
    #[test]
    fn ctrl_rows_are_in_bounds_and_complete() {
        let d = decode(&build());
        for c in &d.ctrls {
            let (cmd, first, count) = (c[0], c[1] as usize, c[2] as usize);
            assert!(
                first + count <= d.nested.len(),
                "{cmd:#x}: first {first} + count {count} > n_nested {}",
                d.nested.len()
            );
        }
        for &cmd in xlate::nested_cmds() {
            let row = d.ctrls.iter().find(|c| c[0] == cmd).expect("nested cmd missing from the stream");
            let specs = xlate::nested_ptrs(cmd);
            assert_eq!(row[2] as usize, specs.len(), "{cmd:#x}: count != nested_ptrs");
            for (i, sp) in specs.iter().enumerate() {
                let n = &d.nested[row[1] as usize + i];
                assert_eq!(n[0], sp.ptr_off, "{cmd:#x}[{i}].ptr_off");
                let (want_kind, want_off, want_elem) = match sp.len {
                    xlate::LenSource::Fixed(len) => (t::NLEN_FIXED, len, 1),
                    xlate::LenSource::Field { off, elem } => (t::NLEN_FIELD, off, elem),
                };
                assert_eq!((n[1], n[2], n[3]), (want_kind, want_off, want_elem), "{cmd:#x}[{i}]");
            }
        }
    }

    /// No two rows in the stream share a key.
    ///
    /// The C interpreter in the guest module looks a row up with a linear
    /// scan that returns the FIRST match (`find_ioctl` by (dev, nr),
    /// `find_class` by hclass, `find_ctrl` by cmd). A duplicate row would
    /// therefore not be a loud error but a silent shadow: the second row --
    /// possibly the one with the fd offset or the size -- would never be
    /// reached, and the call it describes would be forwarded untranslated.
    ///
    /// The ioctl table is the one where a collision is plausible: the UVM
    /// scan and the frontend scan write into the same list, and UVM 39
    /// (PAGEABLE_MEM_ACCESS) collides with frontend 0x27
    /// (NV_ESC_RM_ALLOC_MEMORY) on the nr alone -- only the device tag
    /// keeps them apart.
    #[test]
    fn no_two_rows_in_the_stream_share_a_key() {
        use std::collections::BTreeSet;
        let d = decode(&build());

        let mut keys = BTreeSet::new();
        for r in &d.ioctls {
            assert!(
                keys.insert((r[D_DEV], r[D_NR])),
                "ioctl row (dev {}, nr {:#x}) appears twice -- the second one is dead",
                r[D_DEV], r[D_NR]
            );
        }
        let mut classes = BTreeSet::new();
        for c in &d.classes {
            assert!(classes.insert(c[0]), "class row hClass {:#x} appears twice", c[0]);
        }
        let mut ctrls = BTreeSet::new();
        for c in &d.ctrls {
            assert!(ctrls.insert(c[0]), "ctrl row cmd {:#x} appears twice", c[0]);
        }

        // Non-vacuity: an empty table would satisfy every assertion above.
        assert!(d.ioctls.len() > 20, "ioctl table suspiciously short");
        assert!(d.classes.len() > 20, "class table suspiciously short");
        assert!(!d.ctrls.is_empty() && !d.nested.is_empty());

        // And the collision that motivates the (dev, nr) key really is in
        // the stream: the same nr under two different device tags.
        assert!(keys.contains(&(DevTag::Uvm as u32, xlate::uvm::PAGEABLE_MEM_ACCESS)));
        assert!(keys.contains(&(DevTag::Ctl as u32, sys::NV_ESC_RM_ALLOC_MEMORY)));
    }

    /// The three hand-maintained command lists in `xlate` contain each
    /// command at most once.
    ///
    /// They are written by hand beside the match arms they enumerate, and
    /// `collect()` walks them in order: a duplicate in `nested_cmds()` would
    /// push the same control TWICE into the ctrl table (the test above then
    /// catches the shadowed row, but not what caused it), and a duplicate in
    /// `blocked_ctrls()`/`ctrl_fd_cmds()` hides a copy-paste slip that will
    /// bite the next time a real command is added next to it.
    #[test]
    fn the_xlate_command_lists_have_no_duplicates() {
        use std::collections::BTreeSet;
        let lists: [(&str, &[u32]); 3] = [
            ("nested_cmds", xlate::nested_cmds()),
            ("ctrl_fd_cmds", xlate::ctrl_fd_cmds()),
            ("blocked_ctrls", xlate::blocked_ctrls()),
        ];
        for (name, list) in lists {
            assert!(!list.is_empty(), "{name}() is empty -- the check below would be vacuous");
            let mut seen = BTreeSet::new();
            for &cmd in list {
                assert!(seen.insert(cmd), "{name}() names {cmd:#x} twice");
            }
        }
    }
}
