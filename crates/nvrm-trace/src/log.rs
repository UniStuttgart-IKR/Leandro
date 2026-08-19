// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Logging without std::io.
//!
//! Reason: the interposer runs before (and during) libstd's own stdout
//! initialization, and that initialization can itself call open/ioctl.
//! Re-entering the hooks is a particularly nasty deadlock.
//!
//! Line format (TSV), one record per line, fields separated by tabs:
//!
//! ```text
//! open      <dev> <fd>
//! ioctl     <dev> <nr> <sub> <size> <psize> <ret> <status> <fd>
//! mmap      <dev> <fd> <len> <off> <addr>
//! read      <dev> <fd> <ret>
//! poll      <dev> <fd> <revents>
//! eventreg  <fd> <previous dev tag, or "new" if unknown>
//! ```
//!
//! Scripts parse these lines by column, so field order and separators are
//! part of the interface.
//!
//! `detail()` emits a SECOND, key=value format beside those: `nvos02`,
//! `nvos32`, `nvos33`, `nvos46`, `nvos64`, `memparams`, `uvminit`,
//! `uvmpma`, `uvmreg`, `cardinfo`, `ctrlout`. Those are diagnostic lines,
//! not measurements -- `trace.sh analyse` filters on column 1 and never
//! sees them. NVOS02/33/46/64 are the RM parameter blocks of
//! RM_ALLOC_MEMORY, RM_MAP_MEMORY, RM_MAP_MEMORY_DMA and RM_ALLOC
//! (nvos.h); NVOS32 is that of RM_VID_HEAP_CONTROL; NVOS54's
//! (RM_CONTROL's) answers travel on the `ctrlout` line.
//!
//! `sub` is the second dispatch level: NVOS54.cmd for RM_CONTROL, hClass
//! for RM_ALLOC, "-" otherwise. Without that column all RM_CONTROLs
//! collapse into a single signature and the saturation curve looks far
//! flatter than it is.

use crate::NvDev;
use nvrm_abi::sys;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

static OUT: AtomicI32 = AtomicI32::new(2);
static DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let Ok(path) = std::env::var("LEA_TRACE_FILE") else { return };
    let c = std::ffi::CString::new(path.clone()).unwrap();
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
            0o644,
        )
    };
    if fd >= 0 {
        OUT.store(fd, Ordering::Relaxed);
    } else {
        // Do not fall back to stderr silently - that is exactly how a whole
        // run gets lost without anyone noticing.
        emit(&format!("nvrm-trace: cannot open {path}, trace goes to stderr\n"));
    }
}

fn emit(s: &str) {
    let fd = OUT.load(Ordering::Relaxed);
    let n = unsafe { libc::write(fd, s.as_ptr() as *const c_void, s.len()) };
    if n < 0 || (n as usize) < s.len() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

#[allow(dead_code)] // counterpart to the counter above, read when diagnosing
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

fn dev_tag(d: NvDev) -> &'static str {
    match d {
        NvDev::Ctl => "ctl",
        NvDev::Gpu(_) => "gpu",
        NvDev::Uvm => "uvm",
        NvDev::UvmTools => "uvmtools",
        NvDev::Event => "event",
        NvDev::Drm(false) => "drm",
        NvDev::Drm(true) => "render",
    }
}

pub fn open(dev: NvDev, fd: i32) {
    emit(&format!("open\t{}\t{}\n", dev_tag(dev), fd));
}

/// What the driver really sees for this number.
///
/// UVM and the frontend share the name "ioctl" and nothing else: on Linux
/// `uvm_ioctl.h` defines `UVM_IOCTL_BASE(i) = i`, i.e. raw numbers with no
/// `_IOC` encoding. Applying `_IOC_SIZE` to those silently yields 0, and
/// the empty payloads only puzzle you much later.
fn decode(dev: NvDev, cmd: u32) -> (u32, u32) {
    match dev {
        NvDev::Uvm | NvDev::UvmTools => (cmd, 0),
        // DRM uses the same _IOC encoding, so nr and size come out right;
        // what does NOT apply is everything below -- subcode() and detail()
        // read NVIDIA parameter blocks and a DRM ioctl is not one.
        _ => (nvrm_abi::ioc_nr(cmd), nvrm_abi::ioc_size(cmd)),
    }
}

#[inline]
unsafe fn w(arg: *const c_void, i: usize) -> u32 {
    (arg as *const u32).add(i).read_unaligned()
}

#[inline]
unsafe fn q(arg: *const c_void, i: usize) -> u64 {
    ((arg as *const u32).add(i) as *const u64).read_unaligned()
}

/// Second dispatch level plus RM status.
///
/// Safety: `arg` is the caller's buffer, which the driver has already
/// validated and written to. It is only read here, and only as far as
/// `size` covers.
unsafe fn subcode(
    dev: NvDev, nr: u32, size: u32, arg: *const c_void,
) -> (Option<u32>, Option<u32>, Option<u32>) {          // (sub, psize, status)
    // DRM is excluded here and in detail() for the same reason UVM is,
    // and it is not cosmetic: every arm below casts `arg` to an NVIDIA
    // parameter struct and reads fields at ITS offsets. A DRM ioctl carries
    // a different struct, usually a smaller one, so interpreting it reads
    // past the end of somebody else's allocation. That exact bug has been
    // in this file before: 88 bytes read past a foreign struct. A DRM line
    // therefore carries nr, size and ret and stops.
    // `Event` too: an fd registered through NV_ESC_ALLOC_OS_EVENT that is
    // not a device node (an eventfd, typically) -- an ioctl on it carries
    // whatever that file's ioctls carry, never an NVIDIA block.
    if arg.is_null() || matches!(dev, NvDev::Uvm | NvDev::UvmTools | NvDev::Drm(_) | NvDev::Event) {
        return (None, None, None);
    }
    match nr {
        sys::NV_ESC_RM_CONTROL if size as usize >= size_of::<sys::NVOS54_PARAMETERS>() => {
            let p = &*(arg as *const sys::NVOS54_PARAMETERS);
            (Some(p.cmd as u32), Some(p.paramsSize), Some(p.status as u32))
        }
        sys::NV_ESC_RM_ALLOC if size >= 16 => {
            let hclass = *(arg as *const u32).add(3);
            if size as usize >= size_of::<sys::NVOS64_PARAMETERS>() {
                let p = &*(arg as *const sys::NVOS64_PARAMETERS);
                (Some(hclass), Some(p.paramsSize), Some(p.status as u32))
            } else if size as usize >= size_of::<sys::NVOS21_PARAMETERS>() {
                let p = &*(arg as *const sys::NVOS21_PARAMETERS);
                (Some(hclass), Some(p.paramsSize), Some(p.status as u32))
            } else {
                (Some(hclass), None, None)
            }
        }
        // NVOS02_PARAMETERS + fd. `sub` is hClass - the column that answers
        // whether libcuda allocates NV01_MEMORY_SYSTEM (0x3e) here.
        sys::NV_ESC_RM_ALLOC_MEMORY if size >= 56 => {
            (Some(w(arg, 3)), None, Some(w(arg, 10)))
        }
        // NVOS33_PARAMETERS + fd. `sub` is hMemory, so that mappings can be
        // matched to their allocations by handle number.
        sys::NV_ESC_RM_MAP_MEMORY if size >= 56 => {
            (Some(w(arg, 2)), None, Some(w(arg, 10)))
        }
        _ => (None, None, None),
    }
}

/// Full payload of the three escapes that make up the memory path.
///
/// Deliberately a separate line kind in key=value form: this is a
/// diagnostic line, not a measurement format. `trace.sh analyse` filters on
/// column 1 and therefore never sees it.
///
/// Offsets in u32 words, taken from the layout guards in
/// `nvrm_abi::nvgpu`:
///   NVOS02+fd (56): hRoot0 hParent1 hNew2 hClass3 flags4 | pMemory6 limit8 status10 | fd12
///   NVOS33+fd (56): hClient0 hDevice1 hMemory2 | offset4 length6 pLinear8 status10 flags11 | fd12
///   NVOS46    (64): hClient0 hDevice1 hDma2 hMemory3 | offset4 length6 flags8 flags2_9 kind10 dmaOffset12 status14
unsafe fn detail(dev: NvDev, nr: u32, size: u32, arg: *const c_void, tag: &str) {
    if arg.is_null() {
        return;
    }
    // UVM: no size in the request (UVM_IOCTL_BASE(n) is a bare number), so
    // only the ONE call whose answer the RT init branches on gets a line --
    // UVM_REGISTER_GPU's rmStatus, plus the uuid and the numa answer.
    // Layout from uvm_ioctl.h (UVM_REGISTER_GPU_PARAMS): uuid[16] @0,
    // numaEnabled @16, numaNodeId @20, rmCtrlFd @24, hClient @28,
    // hSmcPartRef @32, rmStatus @36.
    if matches!(dev, NvDev::Uvm) {
        // UVM_INITIALIZE (0x30000001): flags IN/OUT @0 (u64), rmStatus @8.
        // The driver reads the flags back and decides on the pageable/ATS
        // path from them -- the branch point between "calls 0x46" and
        // "does not" (OPEN-QUESTIONS nr 11).
        if nr == 0x30000001 && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 12);
            emit(&format!(
                "uvminit\tflags={:#x}\trmStatus={:#x}\n",
                u64::from_le_bytes(b[0..8].try_into().unwrap()),
                u32::from_le_bytes(b[8..12].try_into().unwrap()),
            ));
        }
        // UVM_PAGEABLE_MEM_ACCESS (0x27: pageableMemAccess NvBool @0,
        // rmStatus @4 -- EIGHT bytes, uvm_ioctl.h) and
        // UVM_PAGEABLE_MEM_ACCESS_ON_GPU (0x46: uuid @0, pageableMemAccess
        // @16, rmStatus @20 -- 24 bytes). Two structs, two lengths: reading
        // 24 bytes for the 8-byte one is the over-read this file has had
        // before, only on the UVM side.
        if nr == nvrm_abi::xlate::uvm::PAGEABLE_MEM_ACCESS && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 8);
            emit(&format!(
                "uvmpma\tnr={nr:#x}\tb0={:#x}\trmStatus={:#x}\n",
                u32::from_le_bytes(b[0..4].try_into().unwrap()),
                u32::from_le_bytes(b[4..8].try_into().unwrap()),
            ));
        }
        if nr == nvrm_abi::xlate::uvm::PAGEABLE_MEM_ACCESS_ON_GPU && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 24);
            emit(&format!(
                "uvmpma\tnr={nr:#x}\tb0={:#x}\tb16={:#x}\tb20={:#x}\n",
                u32::from_le_bytes(b[0..4].try_into().unwrap()),
                u32::from_le_bytes(b[16..20].try_into().unwrap()),
                u32::from_le_bytes(b[20..24].try_into().unwrap()),
            ));
        }
        if nr == nvrm_abi::xlate::uvm::REGISTER_GPU && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 40);
            let mut uuid = String::with_capacity(32);
            for x in &b[0..16] {
                uuid.push_str(&format!("{x:02x}"));
            }
            emit(&format!(
                "uvmreg\tuuid={uuid}\tnuma={}/{}\trmCtrlFd={}\thClient={:#x}\trmStatus={:#x}\n",
                b[16], i32::from_le_bytes(b[20..24].try_into().unwrap()),
                i32::from_le_bytes(b[24..28].try_into().unwrap()),
                u32::from_le_bytes(b[28..32].try_into().unwrap()),
                u32::from_le_bytes(b[36..40].try_into().unwrap()),
            ));
        }
        return;
    }
    // See subcode(): a DRM ioctl's argument is not an NVIDIA parameter
    // block, and every arm below assumes it is.
    if matches!(dev, NvDev::UvmTools | NvDev::Drm(_) | NvDev::Event) {
        return;
    }
    match nr {
        // The ANSWERS the RT userspace branches on (OPEN-QUESTIONS nr 11):
        // one line per valid card, all the fields the BDF mediation
        // touches. Only after the call -- the input is all zeros.
        sys::NV_ESC_CARD_INFO if tag.is_empty() && size as usize >= size_of::<sys::nv_ioctl_card_info_t>() => {
            let n = size as usize / size_of::<sys::nv_ioctl_card_info_t>();
            let cards = core::slice::from_raw_parts(arg as *const sys::nv_ioctl_card_info_t, n);
            for (i, c) in cards.iter().enumerate() {
                if c.valid == 0 {
                    continue;
                }
                emit(&format!(
                    "cardinfo\t[{i}]\tgpu_id={:#x}\tpci={:04x}:{:02x}:{:02x}.{}\tvendor={:#06x}\tdevice={:#06x}\
\tirq={}\treg={:#x}+{:#x}\tfb={:#x}+{:#x}\tminor={}\n",
                    c.gpu_id, c.pci_info.domain, c.pci_info.bus, c.pci_info.slot, c.pci_info.function,
                    c.pci_info.vendor_id, c.pci_info.device_id, c.interrupt_line,
                    c.reg_address, c.reg_size, c.fb_address, c.fb_size, c.minor_number,
                ));
            }
        }
        // Root-client controls (0x2xx): the first 32 params bytes after the
        // call, so an enumeration answer (GET_PROBED_IDS, GET_DEVICE_IDS,
        // GET_ID_INFO) can be diffed native vs guest without a struct per
        // control. NVOS54: params P64 @16, paramsSize @24.
        sys::NV_ESC_RM_CONTROL
            if tag.is_empty() && size as usize >= size_of::<sys::NVOS54_PARAMETERS>() =>
        {
            let p = &*(arg as *const sys::NVOS54_PARAMETERS);
            let cmd = p.cmd as u32;
            let pp = p.params as usize as *const u8;
            let plen = p.paramsSize as usize;
            // Root-client (0x2xx) AND subdevice (0x2080xxxx) controls: the
            // latter carry the GPU-feature answers (GSP, ECC, ...) the RT
            // init branches on.
            if ((cmd >> 8) == 0x2 || (cmd >> 16) == 0x2080) && !pp.is_null() && plen > 0 {
                let n = plen.min(32);
                let bytes = core::slice::from_raw_parts(pp, n);
                let mut hex = String::with_capacity(n * 3);
                for b in bytes {
                    hex.push_str(&format!("{b:02x} "));
                }
                emit(&format!("ctrlout\t{cmd:#x}\tlen={plen}\tstatus={:#x}\t{}\n", p.status as u32, hex.trim_end()));
            }
        }
        sys::NV_ESC_RM_ALLOC_MEMORY if size >= 56 => emit(&format!(
            "nvos02{tag}\thRoot={:#x}\thParent={:#x}\thNew={:#x}\thClass={:#x}\
             \tflags={:#x}\tpMemory={:#x}\tlimit={:#x}\tstatus={:#x}\tfd={}\n",
            w(arg, 0), w(arg, 1), w(arg, 2), w(arg, 3),
            w(arg, 4), q(arg, 6), q(arg, 8), w(arg, 10), w(arg, 12) as i32,
        )),
        sys::NV_ESC_RM_MAP_MEMORY if size >= 56 => emit(&format!(
            "nvos33{tag}\thClient={:#x}\thDevice={:#x}\thMemory={:#x}\toffset={:#x}\
             \tlength={:#x}\tpLinear={:#x}\tstatus={:#x}\tflags={:#x}\tfd={}\n",
            w(arg, 0), w(arg, 1), w(arg, 2), q(arg, 4),
            q(arg, 6), q(arg, 8), w(arg, 10), w(arg, 11), w(arg, 12) as i32,
        )),
        // NVOS32: the OTHER allocation door, and the one the graphics stack
        // actually uses. Until this arm existed the tracer emitted a bare
        // `ioctl ctl 0x4a` line with no parameters at all, which is why the
        // native-vs-guest allocation diff asked for in OPEN-QUESTIONS 22 was
        // not merely undone but IMPOSSIBLE: the fields to compare were never
        // recorded. Offsets from the guards in nvgpu.rs (NVOS32_PARAMETERS
        // and its AllocSize union member), so a layout drift breaks the
        // build rather than this reader.
        //
        // `size` and `attr` are IN/OUT -- the caller asks and RM writes back
        // what it really did (LOCATION may go in as ANY and come back
        // VIDMEM). Tracing both sides of the call is therefore the point,
        // not a nicety, and `detail_pre` gives the `in` tag.
        sys::NV_ESC_RM_VID_HEAP_CONTROL if size >= 184 => {
            let function = w(arg, 2);
            // 2 = ALLOC_SIZE, 3 = FREE (nvos.h:636-637). Only these two
            // carry the union members whose layout is guarded.
            if function == 2 {
                emit(&format!(
                    "nvos32{tag}\thRoot={:#x}\thObjectParent={:#x}\tfunction=ALLOC_SIZE\thVASpace={:#x}\tstatus={:#x}\towner={:#x}\thMemory={:#x}\ttype={:#x}\tflags={:#x}\tattr={:#x}\tformat={:#x}\twidth={:#x}\theight={:#x}\tsize={:#x}\talignment={:#x}\toffset={:#x}\tlimit={:#x}\taddress={:#x}\tattr2={:#x}\n",
                    w(arg, 0), w(arg, 1), w(arg, 3), w(arg, 5),
                    w(arg, 10), w(arg, 11), w(arg, 12), w(arg, 13),
                    w(arg, 14), w(arg, 15), w(arg, 19), w(arg, 20),
                    q(arg, 22), q(arg, 24), q(arg, 26), q(arg, 28),
                    q(arg, 30), w(arg, 36),
                ));
            } else if function == 3 {
                emit(&format!(
                    "nvos32{tag}\thRoot={:#x}\thObjectParent={:#x}\tfunction=FREE\tstatus={:#x}\towner={:#x}\thMemory={:#x}\tflags={:#x}\n",
                    w(arg, 0), w(arg, 1), w(arg, 5),
                    w(arg, 10), w(arg, 11), w(arg, 12),
                ));
            } else {
                emit(&format!(
                    "nvos32{tag}\thRoot={:#x}\thObjectParent={:#x}\tfunction={:#x}\tstatus={:#x}\n",
                    w(arg, 0), w(arg, 1), function, w(arg, 5),
                ));
            }
        }
        sys::NV_ESC_RM_MAP_MEMORY_DMA if size >= 64 => emit(&format!(
            "nvos46{tag}\thClient={:#x}\thDevice={:#x}\thDma={:#x}\thMemory={:#x}\
             \toffset={:#x}\tlength={:#x}\tflags={:#x}\tflags2={:#x}\
             \tkind={:#x}\tdmaOffset={:#x}\tstatus={:#x}\n",
            w(arg, 0), w(arg, 1), w(arg, 2), w(arg, 3),
            q(arg, 4), q(arg, 6), w(arg, 8), w(arg, 9),
            w(arg, 10), q(arg, 12), w(arg, 14),
        )),
        sys::NV_ESC_RM_ALLOC if size >= 48 => {
            let hclass = w(arg, 3);
            emit(&format!(
                "nvos64{tag}\thRoot={:#x}\thParent={:#x}\thNew={:#x}\thClass={:#x}\
                 \tparamsSize={:#x}\tflags={:#x}\tstatus={:#x}\n",
                w(arg, 0), w(arg, 1), w(arg, 2), hclass,
                w(arg, 8), w(arg, 9), w(arg, 10),
            ));

            // Follow pAllocParms. This only works because paramsSize is
            // always 0, so the size has to come from the class - these are
            // the memory classes that use NV_MEMORY_ALLOCATION_PARAMS
            // (128 bytes, field offsets from the guards in nvgpu.rs):
            // 0x3e NV01_MEMORY_SYSTEM, 0x40 NV01_MEMORY_LOCAL_USER, 0x50a0
            // NV50_MEMORY_VIRTUAL (resource_list.h:574, :542, :563).
            //
            // Two memory classes are deliberately NOT in this list. 0x71
            // (NV01_MEMORY_SYSTEM_OS_DESCRIPTOR) allocates with the 40-byte
            // NV_OS_DESC_MEMORY_ALLOCATION_PARAMS, and 0x70
            // (NV01_MEMORY_VIRTUAL) with the 24-byte
            // NV_MEMORY_VIRTUAL_ALLOCATION_PARAMS (cl0070.h,
            // resource_list.h:580); decoding either with the 128-byte layout
            // reads far past the end of the caller's struct -- 0x70 was in
            // this list until 2026-08-18.
            let pp = q(arg, 4) as usize as *const c_void;
            if !pp.is_null() && matches!(hclass, 0x3e | 0x40 | 0x50a0) {
                emit(&format!(
                    "memparams{tag}\thNew={:#x}\thClass={:#x}\towner={:#x}\ttype={:#x}\
\tflags={:#x}\tattr={:#x}\tattr2={:#x}\trangeLo={:#x}\trangeHi={:#x}\
\tsize={:#x}\talign={:#x}\toffset={:#x}\tlimit={:#x}\taddress={:#x}\
\tctag={:#x}\thVASpace={:#x}\tinternal={:#x}\ttag={:#x}\tnuma={}\n",
                    w(arg, 2), hclass,
                    w(pp, 0), w(pp, 1), w(pp, 2), w(pp, 6), w(pp, 7),
                    q(pp, 12), q(pp, 14), q(pp, 16), q(pp, 18),
                    q(pp, 20), q(pp, 22), q(pp, 24),
                    w(pp, 26), w(pp, 27), w(pp, 28), w(pp, 29), w(pp, 30) as i32,
                ));
            }
        }
        _ => {}
    }
}

/// Sample of the payload *before* the driver overwrites it.
///
/// Without this the trace shows only write-back values, and for
/// NVOS33.flags the input is the interesting one: input and output share
/// the same field.
pub unsafe fn detail_pre(dev: NvDev, nr: u32, size: u32, arg: *const c_void) {
    detail(dev, nr, size, arg, "in")
}

pub fn mmap(dev: NvDev, fd: i32, len: usize, off: i64, p: *mut c_void) {
    emit(&format!(
        "mmap\t{}\t{}\t{}\t{}\t{:p}\n",
        dev_tag(dev), fd, len, off, p
    ));
}

/// Wait path: `read`/`poll` on a known FD.
/// `val` is the return value for `read`, `revents` for `poll`.
pub fn wait(kind: &str, dev: NvDev, fd: i32, val: i64) {
    emit(&format!("{}\t{}\t{}\t{}\n", kind, dev_tag(dev), fd, val));
}

/// Which FD was registered as an event channel, and was it already known?
/// `prev` == None means: an eventfd, not a device, and the third column
/// then reads `new`. (It read `neu` until 2026-08-18; `probe/run/trace.sh`
/// prints that column and never matches it.)
pub fn event_registered(fd: i32, prev: Option<NvDev>) {
    emit(&format!(
        "eventreg\t{}\t{}\n",
        fd,
        prev.map(dev_tag).unwrap_or("new")
    ));
}

pub fn ioctl(dev: NvDev, fd: i32, cmd: u32, ret: i32, arg: *mut c_void) {
    let (nr, size) = decode(dev, cmd);
    let (sub, psize, status) = unsafe { subcode(dev, nr, size, arg) };
    let f = |o: Option<u32>| o.map(|v| format!("{v:#x}")).unwrap_or_else(|| "-".into());
    emit(&format!(
        "ioctl\t{}\t{:#x}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        dev_tag(dev), nr, f(sub), size, f(psize), ret, f(status), fd,
    ));
    unsafe { detail(dev, nr, size, arg, "") };
}

/// The same line as `ioctl`, for a call that arrived wrapped in
/// `NV_ESC_IOCTL_XFER_CMD`: `nr` and `size` are the UNPACKED ones and must
/// not go through `decode()` a second time, and `arg` is the inner pointer.
pub fn ioctl_unpacked(dev: NvDev, fd: i32, nr: u32, size: u32, ret: i32, arg: *mut c_void) {
    let (sub, psize, status) = unsafe { subcode(dev, nr, size, arg) };
    let f = |o: Option<u32>| o.map(|v| format!("{v:#x}")).unwrap_or_else(|| "-".into());
    emit(&format!(
        "ioctl\t{}\t{:#x}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        dev_tag(dev), nr, f(sub), size, f(psize), ret, f(status), fd,
    ));
    unsafe { detail(dev, nr, size, arg, "") };
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvrm_abi::iowr_raw;

    /// The device tag is column 2 of every line above (except `eventreg`,
    /// whose column 2 is the fd), and
    /// `probe/run/trace.sh` selects on it by string. These seven spellings
    /// are therefore an interface, not a label: renaming one silently
    /// empties whatever an analysis run filters for (that is exactly how
    /// `eventreg`'s third column read `neu` until 2026-08-18 and matched
    /// nothing).
    #[test]
    fn the_device_tags_are_the_strings_the_scripts_filter_on() {
        assert_eq!(dev_tag(NvDev::Ctl), "ctl");
        assert_eq!(dev_tag(NvDev::Gpu(0)), "gpu");
        assert_eq!(dev_tag(NvDev::Uvm), "uvm");
        assert_eq!(dev_tag(NvDev::UvmTools), "uvmtools");
        assert_eq!(dev_tag(NvDev::Event), "event");
        assert_eq!(dev_tag(NvDev::Drm(false)), "drm");
        assert_eq!(dev_tag(NvDev::Drm(true)), "render");
        // The GPU index deliberately does NOT reach the tag -- the FD
        // column says which node, the tag says which kind.
        assert_eq!(dev_tag(NvDev::Gpu(0)), dev_tag(NvDev::Gpu(7)));
    }

    /// UVM shares the name "ioctl" with the frontend and nothing else:
    /// `UVM_IOCTL_BASE(i) = i`, i.e. raw numbers with no `_IOC` encoding.
    /// Masking one yields a different number and a size of 0, and empty
    /// payloads in a trace only puzzle you much later. Everything else --
    /// including DRM, which does use `_IOC` -- is unpacked.
    #[test]
    fn decode_leaves_uvm_numbers_whole_and_unpacks_every_other_device() {
        // UVM_INITIALIZE: a bare number, with bits in what would be the
        // _IOC size and type fields.
        let raw = 0x3000_0001;
        assert_eq!(decode(NvDev::Uvm, raw), (raw, 0));
        assert_eq!(decode(NvDev::UvmTools, raw), (raw, 0));

        // NV_ESC_RM_CONTROL (0x2a) carrying the 32-byte NVOS54.
        let cmd = iowr_raw(0x2a, 32);
        for dev in [
            NvDev::Ctl,
            NvDev::Gpu(0),
            NvDev::Event,
            NvDev::Drm(false),
            NvDev::Drm(true),
        ] {
            assert_eq!(decode(dev, cmd), (0x2a, 32), "{dev:?}");
        }
    }

    /// `sub`/`psize`/`status` for RM_CONTROL come out of NVOS54, and the
    /// full struct has to be there before any of it is read: `size` is the
    /// caller's own `_IOC_SIZE`, so a shorter one means the caller passed a
    /// shorter buffer.
    #[test]
    fn subcode_reads_a_control_only_at_the_full_struct_size() {
        let mut p = sys::NVOS54_PARAMETERS::default();
        p.cmd = 0x2080_0110;
        p.paramsSize = 0x44;
        p.status = 0x56;
        let arg = &p as *const _ as *const c_void;
        let full = size_of::<sys::NVOS54_PARAMETERS>() as u32;
        assert_eq!(full, 32, "NVOS54 is the 32-byte form (nvgpu.rs layout guard)");

        assert_eq!(
            unsafe { subcode(NvDev::Ctl, sys::NV_ESC_RM_CONTROL, full, arg) },
            (Some(0x2080_0110), Some(0x44), Some(0x56)),
        );
        // One byte short is not the struct, and reading it would take
        // fields past what the caller sent.
        assert_eq!(
            unsafe { subcode(NvDev::Ctl, sys::NV_ESC_RM_CONTROL, full - 1, arg) },
            (None, None, None),
        );
    }

    /// RM_ALLOC arrives in three lengths, and `sub` is hClass in all of
    /// them -- the column that says WHICH class was allocated. Only the
    /// two longer forms also carry paramsSize and status, and each is read
    /// at its own layout: 48 = NVOS64, 32 = NVOS21, and from 16 bytes on
    /// there is a hClass and nothing more.
    #[test]
    fn subcode_reads_an_alloc_in_each_of_its_three_lengths() {
        let mut p64 = sys::NVOS64_PARAMETERS::default();
        p64.hClass = 0x50a0;
        p64.paramsSize = 0x11;
        p64.status = 0x1f;
        let a64 = &p64 as *const _ as *const c_void;
        assert_eq!(size_of::<sys::NVOS64_PARAMETERS>(), 48);
        assert_eq!(
            unsafe { subcode(NvDev::Ctl, sys::NV_ESC_RM_ALLOC, 48, a64) },
            (Some(0x50a0), Some(0x11), Some(0x1f)),
        );

        // The short form. paramsSize and status sit at different offsets
        // here, so decoding it with the NVOS64 layout would report the
        // wrong two numbers rather than fail.
        let mut p21 = sys::NVOS21_PARAMETERS::default();
        p21.hClass = 0x0040;
        p21.paramsSize = 0x22;
        p21.status = 0x1f;
        let a21 = &p21 as *const _ as *const c_void;
        assert_eq!(size_of::<sys::NVOS21_PARAMETERS>(), 32);
        assert_eq!(
            unsafe { subcode(NvDev::Ctl, sys::NV_ESC_RM_ALLOC, 32, a21) },
            (Some(0x0040), Some(0x22), Some(0x1f)),
        );

        // 16 bytes reach hClass (word 3) and stop there.
        assert_eq!(
            unsafe { subcode(NvDev::Ctl, sys::NV_ESC_RM_ALLOC, 16, a21) },
            (Some(0x0040), None, None),
        );
        // 15 do not even reach that.
        assert_eq!(
            unsafe { subcode(NvDev::Ctl, sys::NV_ESC_RM_ALLOC, 15, a21) },
            (None, None, None),
        );
    }

    /// The over-read guard from the module header, pinned.
    ///
    /// Every arm of `subcode` casts `arg` to an NVIDIA parameter struct and
    /// reads fields at ITS offsets. A DRM ioctl carries a different and
    /// usually smaller struct, so interpreting one reads past the end of
    /// somebody else's allocation -- this file has had exactly that bug, 88
    /// bytes past a foreign struct. UVM is excluded for a related reason:
    /// its `size` is not a length at all, because the number carries no
    /// `_IOC` encoding. (`subcode` also skips the event device, whose
    /// reads are not parameter structs either.)
    #[test]
    fn subcode_never_decodes_a_drm_or_uvm_argument() {
        let mut p = sys::NVOS54_PARAMETERS::default();
        p.cmd = 0x2080_0110;
        p.paramsSize = 0x44;
        p.status = 0x56;
        let arg = &p as *const _ as *const c_void;

        // The same buffer that decodes on an RM device ...
        assert_eq!(
            unsafe { subcode(NvDev::Ctl, sys::NV_ESC_RM_CONTROL, 32, arg) },
            (Some(0x2080_0110), Some(0x44), Some(0x56)),
        );
        // ... must be left alone on every device that is not one, whatever
        // the number and the size claim.
        for dev in [NvDev::Drm(false), NvDev::Drm(true), NvDev::Uvm, NvDev::UvmTools] {
            assert_eq!(
                unsafe { subcode(dev, sys::NV_ESC_RM_CONTROL, 32, arg) },
                (None, None, None),
                "{dev:?} control",
            );
            assert_eq!(
                unsafe { subcode(dev, sys::NV_ESC_RM_ALLOC, 48, arg) },
                (None, None, None),
                "{dev:?} alloc",
            );
        }

        // A null argument is not a buffer either; ioctl(2) callers may
        // legitimately pass one.
        assert_eq!(
            unsafe { subcode(NvDev::Ctl, sys::NV_ESC_RM_CONTROL, 32, std::ptr::null()) },
            (None, None, None),
        );
    }
}
