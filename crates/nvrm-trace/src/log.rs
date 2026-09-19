// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Raw-write logging with shared TSV and JSONL fields.
//! Avoid `std::io`: its initialization can re-enter the interposed hooks.
//! TSV column order and JSON keys are consumed by `scripts/lib/common.sh`
//! and `probe/python/traceread.py`.
//!
//! ```text
//! open      <dev> <fd>
//! ioctl     <dev> <nr> <sub> <size> <psize> <ret> <status> <fd>
//! mmap      <dev> <fd> <len> <off> <addr>
//! read      <dev> <fd> <ret>
//! poll      <dev> <fd> <revents>
//! eventreg  <fd> <previous dev tag, or "new">
//! ```
//!
//! Detail records use key=value TSV fields and the same named JSON fields.
//! `sub` identifies the RM control, allocation class, mapping handle, or
//! NVKMS command. Dump sizes use caller metadata or the compiled driver ABI;
//! they do not prove that a caller's pointers are readable.

use crate::NvDev;
use nvrm_abi::sys;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

/// The TSV sink. 2 (stderr) until `init` says otherwise, so a tracer with
/// no `LEA_TRACE_FILE` still says what it saw; -1 disables it.
static OUT: AtomicI32 = AtomicI32::new(2);
/// The JSONL sink. -1 (off) unless `init` opens one: an unconfigured
/// tracer must not start spraying JSON at stderr beside the TSV.
static OUT_JSON: AtomicI32 = AtomicI32::new(-1);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Replace a .tsv suffix with .jsonl, or append .jsonl.
fn jsonl_path(tsv: &str) -> String {
    match tsv.strip_suffix(".tsv") {
        Some(stem) => format!("{stem}.jsonl"),
        None => format!("{tsv}.jsonl"),
    }
}

/// Append so execed children sharing LEA_TRACE_FILE do not truncate earlier records.
/// The runner is responsible for creating or truncating the file before a run.
fn open_out(path: &str) -> i32 {
    let Ok(c) = std::ffi::CString::new(path) else {
        return -1;
    };
    unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_CLOEXEC,
            0o644,
        )
    }
}

pub fn init() {
    let Ok(path) = std::env::var("LEA_TRACE_FILE") else {
        return;
    };
    // Both formats observe the same calls, allowing an equivalence check.
    let fmt = std::env::var("LEA_TRACE_FORMAT").unwrap_or_else(|_| "both".into());
    let (want_tsv, want_json) = match fmt.as_str() {
        "tsv" => (true, false),
        "jsonl" => (false, true),
        "both" => (true, true),
        other => {
            emit_fd(
                2,
                &format!(
                "nvrm-trace: LEA_TRACE_FORMAT={other} is not tsv, jsonl or both; writing both\n"
            ),
            );
            (true, true)
        }
    };

    if want_tsv {
        let fd = open_out(&path);
        if fd >= 0 {
            OUT.store(fd, Ordering::Relaxed);
        } else {
            // Report the fallback sink.
            emit_fd(
                2,
                &format!("nvrm-trace: cannot open {path}, trace goes to stderr\n"),
            );
        }
    } else {
        OUT.store(-1, Ordering::Relaxed);
    }

    if want_json {
        let jp = jsonl_path(&path);
        let fd = open_out(&jp);
        if fd >= 0 {
            OUT_JSON.store(fd, Ordering::Relaxed);
        } else {
            // A missing JSONL file has no fallback sink.
            emit_fd(
                2,
                &format!("nvrm-trace: cannot open {jp}, no JSONL trace this run\n"),
            );
        }
    }
}

fn emit_fd(fd: i32, s: &str) {
    let _errno = crate::ErrnoGuard::new();
    if fd < 0 {
        return;
    }
    let n = unsafe { libc::write(fd, s.as_ptr() as *const c_void, s.len()) };
    if n < 0 || (n as usize) < s.len() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

// One write per line. TSV/JSON pairs may interleave across threads, so
// traceread.py --check compares record multisets rather than file order.

/// A field value. `Copy`, so a record's fields live in the caller's stack
/// frame and the only allocation per line is the rendered string itself.
#[derive(Clone, Copy)]
enum V<'a> {
    /// Raw in TSV, quoted and escaped in JSON.
    S(&'a str),
    H32(u32),
    H64(u64),
    I(i64),
    /// Byte dump: space-separated in TSV, contiguous in JSON.
    Dump(&'a [u8]),
    /// An array index: `[3]` in TSV, a bare number in JSON.
    Idx(usize),
    /// Absent: `-` in TSV, `null` in JSON.
    Nil,
}

/// Named JSON field; keyed TSV fields use name=value, others are positional.
#[derive(Clone, Copy)]
struct F<'a> {
    name: &'a str,
    keyed: bool,
    v: V<'a>,
}

/// A positional TSV field (named in JSON regardless).
const fn pos<'a>(name: &'a str, v: V<'a>) -> F<'a> {
    F {
        name,
        keyed: false,
        v,
    }
}
/// A `name=value` TSV field.
const fn key<'a>(name: &'a str, v: V<'a>) -> F<'a> {
    F {
        name,
        keyed: true,
        v,
    }
}

/// Maximum bytes per dump (default 65536). LEA_TRACE_DUMP=0 disables dumps.
/// Reads are also bounded by the caller length or the compiled ABI size.
/// A truncated dump only verifies the bytes it contains.
fn dump_cap() -> usize {
    static CAP: AtomicUsize = AtomicUsize::new(usize::MAX);
    let c = CAP.load(Ordering::Relaxed);
    if c != usize::MAX {
        return c;
    }
    let v = std::env::var("LEA_TRACE_DUMP")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(65536);
    CAP.store(v, Ordering::Relaxed);
    v
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn push_hex_bytes(s: &mut String, b: &[u8], spaced: bool) {
    for (i, x) in b.iter().enumerate() {
        if spaced && i > 0 {
            s.push(' ');
        }
        s.push(HEX[(x >> 4) as usize] as char);
        s.push(HEX[(x & 0xf) as usize] as char);
    }
}

/// Escape a JSON string body, including quotes, backslashes and control bytes.
fn push_json_str(s: &mut String, x: &str) {
    for c in x.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
            c => s.push(c),
        }
    }
}

fn push_tsv(s: &mut String, v: V) {
    match v {
        V::S(x) => s.push_str(x),
        V::H32(x) => s.push_str(&format!("{x:#x}")),
        V::H64(x) => s.push_str(&format!("{x:#x}")),
        V::I(x) => s.push_str(&format!("{x}")),
        V::Dump(b) => push_hex_bytes(s, b, true),
        V::Idx(i) => s.push_str(&format!("[{i}]")),
        V::Nil => s.push('-'),
    }
}

fn push_json(s: &mut String, v: V) {
    match v {
        V::S(x) => {
            s.push('"');
            push_json_str(s, x);
            s.push('"');
        }
        // Keep hexadecimal values as strings in both formats for existing readers.
        V::H32(x) => s.push_str(&format!("\"{x:#x}\"")),
        V::H64(x) => s.push_str(&format!("\"{x:#x}\"")),
        V::I(x) => s.push_str(&format!("{x}")),
        V::Dump(b) => {
            s.push('"');
            push_hex_bytes(s, b, false);
            s.push('"');
        }
        V::Idx(i) => s.push_str(&format!("{i}")),
        V::Nil => s.push_str("null"),
    }
}

/// TSV appends the input phase to the kind (nvos64in); JSON has a phase field.
fn render_tsv(kind: &str, phase: Option<&str>, fields: &[F]) -> String {
    let mut s = String::with_capacity(160);
    s.push_str(kind);
    if phase == Some("in") {
        s.push_str("in");
    }
    for f in fields {
        s.push('\t');
        if f.keyed {
            s.push_str(f.name);
            s.push('=');
        }
        push_tsv(&mut s, f.v);
    }
    s.push('\n');
    s
}

fn render_json(kind: &str, phase: Option<&str>, fields: &[F]) -> String {
    let mut s = String::with_capacity(224);
    s.push_str("{\"t\":\"");
    push_json_str(&mut s, kind);
    s.push('"');
    if let Some(ph) = phase {
        s.push_str(",\"phase\":\"");
        s.push_str(ph);
        s.push('"');
    }
    for f in fields {
        s.push_str(",\"");
        push_json_str(&mut s, f.name);
        s.push_str("\":");
        push_json(&mut s, f.v);
    }
    s.push_str("}\n");
    s
}

/// Write one record to every configured sink.
fn rec(kind: &str, phase: Option<&str>, fields: &[F]) {
    let tsv = OUT.load(Ordering::Relaxed);
    if tsv >= 0 {
        emit_fd(tsv, &render_tsv(kind, phase, fields));
    }
    let js = OUT_JSON.load(Ordering::Relaxed);
    if js >= 0 {
        emit_fd(js, &render_json(kind, phase, fields));
    }
}

/// `rec` for the lines that have no `in` variant.
fn rec1(kind: &str, fields: &[F]) {
    rec(kind, None, fields)
}

/// The `in`/`out` phase for a `detail()` tag, which is `"in"` or `""`.
fn phase_of(tag: &str) -> Option<&'static str> {
    Some(if tag == "in" { "in" } else { "out" })
}

pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

// Fixed storage keeps normal-exit reporting independent of allocator locks.
struct LossReport {
    bytes: [u8; 160],
    len: usize,
}

impl std::fmt::Write for LossReport {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let dst = self
            .bytes
            .get_mut(self.len..self.len + s.len())
            .ok_or(std::fmt::Error)?;
        dst.copy_from_slice(s.as_bytes());
        self.len += s.len();
        Ok(())
    }
}

fn loss_report(dropped: u64, untracked: u64) -> LossReport {
    use std::fmt::Write;
    let mut report = LossReport {
        bytes: [0; 160],
        len: 0,
    };
    // The fixed message and two decimal u64 values fit in 160 bytes.
    let _ = writeln!(report, "nvrm-trace: incomplete trace: {dropped} failed/short writes, {untracked} untracked FD registrations");
    report
}

/// Normal exit only; forked children inherit the counters at fork time.
pub fn report_losses() {
    let dropped = dropped();
    let untracked = crate::fdtable::overflow_count();
    if dropped != 0 || untracked != 0 {
        let report = loss_report(dropped, untracked);
        // Do not count failure to report the counters as another lost record.
        unsafe { libc::write(2, report.bytes.as_ptr().cast(), report.len) };
    }
}

fn dev_tag(d: NvDev) -> &'static str {
    match d {
        NvDev::Ctl => "ctl",
        NvDev::Gpu => "gpu",
        NvDev::Uvm => "uvm",
        NvDev::UvmTools => "uvmtools",
        NvDev::Event => "event",
        NvDev::Drm(false) => "drm",
        NvDev::Drm(true) => "render",
        NvDev::Modeset => "modeset",
    }
}

pub fn open(dev: NvDev, fd: i32) {
    rec1(
        "open",
        &[pos("dev", V::S(dev_tag(dev))), pos("fd", V::I(fd as i64))],
    );
}

/// UVM uses raw ioctl numbers without _IOC size/type bits.
pub fn decode(dev: NvDev, cmd: u32) -> (u32, u32) {
    match dev {
        NvDev::Uvm | NvDev::UvmTools => (cmd, 0),
        // DRM uses _IOC encoding, but its payload is never decoded as RM.
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

/// Return (subcommand, parameter size, RM status).
/// Safety: the caller must supply readable, suitably aligned ABI buffers.
unsafe fn subcode(
    dev: NvDev,
    nr: u32,
    size: u32,
    arg: *const c_void,
) -> (Option<u32>, Option<u32>, Option<u32>) {
    // Only RM and the NVKMS wrapper have the layouts decoded below.
    if arg.is_null()
        || matches!(
            dev,
            NvDev::Uvm | NvDev::UvmTools | NvDev::Drm(_) | NvDev::Event
        )
    {
        return (None, None, None);
    }
    // NVKMS uses ioctl 0 with command/size in NvKmsIoctlParams.
    // It has no common payload status field; the syscall return is logged separately.
    if matches!(dev, NvDev::Modeset) {
        if size as usize >= size_of::<sys::NvKmsIoctlParams>() {
            let p = &*(arg as *const sys::NvKmsIoctlParams);
            return (Some(p.cmd), Some(p.size), None);
        }
        return (None, None, None);
    }
    match nr {
        sys::NV_ESC_RM_CONTROL if size as usize >= size_of::<sys::NVOS54_PARAMETERS>() => {
            let p = &*(arg as *const sys::NVOS54_PARAMETERS);
            (
                Some(p.cmd as u32),
                Some(p.paramsSize),
                Some(p.status as u32),
            )
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
        sys::NV_ESC_RM_ALLOC_MEMORY if size >= 56 => (Some(w(arg, 3)), None, Some(w(arg, 10))),
        // NVOS33_PARAMETERS + fd. `sub` is hMemory, so that mappings can be
        // matched to their allocations by handle number.
        sys::NV_ESC_RM_MAP_MEMORY if size >= 56 => (Some(w(arg, 2)), None, Some(w(arg, 10))),
        _ => (None, None, None),
    }
}

/// Diagnostic payload records. Word offsets are guarded in nvrm_abi::nvgpu:
/// NVOS02+fd (56): hRoot0 hParent1 hNew2 hClass3 flags4 pMemory6 limit8 status10 fd12
/// NVOS33+fd (56): hClient0 hDevice1 hMemory2 offset4 length6 pLinear8 status10 flags11 fd12
/// NVOS46 (64): hClient0 hDevice1 hDma2 hMemory3 offset4 length6 flags8 flags2_9 kind10 dmaOffset12 status14
/// Safety: all decoded caller buffers and nested pointers must be readable.
unsafe fn detail(dev: NvDev, nr: u32, size: u32, arg: *const c_void, tag: &str) {
    if arg.is_null() {
        return;
    }
    // UVM lengths come from the compiled ABI; the request number carries no size.
    if matches!(dev, NvDev::Uvm) {
        // Use bindgen sizes independently of the forwarding table being tested.
        // DefaultAbi must match the loaded driver; this tracer does not detect its version.
        if let Some(plen) = nvrm_abi::xlate::uvm_param_size_compiled::<nvrm_sys::DefaultAbi>(nr) {
            if plen > 0 {
                let n = plen.min(dump_cap());
                let bytes = core::slice::from_raw_parts(arg as *const u8, n);
                rec(
                    "uvmout",
                    phase_of(tag),
                    &[
                        pos("nr", V::H32(nr)),
                        key("len", V::I(plen as i64)),
                        pos("dump", V::Dump(bytes)),
                    ],
                );
            }
        }
        // UVM_INITIALIZE: flags u64 @0, rmStatus u32 @8.
        if nr == 0x30000001 && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 12);
            rec1(
                "uvminit",
                &[
                    key(
                        "flags",
                        V::H64(u64::from_le_bytes(b[0..8].try_into().unwrap())),
                    ),
                    key(
                        "rmStatus",
                        V::H32(u32::from_le_bytes(b[8..12].try_into().unwrap())),
                    ),
                ],
            );
        }
        // PAGEABLE_MEM_ACCESS is 8 bytes; ON_GPU is 24 bytes with a UUID prefix.
        // Their status fields are at byte offsets 4 and 20 respectively (uvm_ioctl.h).
        if nr == nvrm_abi::xlate::uvm::PAGEABLE_MEM_ACCESS && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 8);
            rec1(
                "uvmpma",
                &[
                    key("nr", V::H32(nr)),
                    key(
                        "b0",
                        V::H32(u32::from_le_bytes(b[0..4].try_into().unwrap())),
                    ),
                    key(
                        "rmStatus",
                        V::H32(u32::from_le_bytes(b[4..8].try_into().unwrap())),
                    ),
                ],
            );
        }
        if nr == nvrm_abi::xlate::uvm::PAGEABLE_MEM_ACCESS_ON_GPU && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 24);
            rec1(
                "uvmpma",
                &[
                    key("nr", V::H32(nr)),
                    key(
                        "b0",
                        V::H32(u32::from_le_bytes(b[0..4].try_into().unwrap())),
                    ),
                    key(
                        "b16",
                        V::H32(u32::from_le_bytes(b[16..20].try_into().unwrap())),
                    ),
                    key(
                        "b20",
                        V::H32(u32::from_le_bytes(b[20..24].try_into().unwrap())),
                    ),
                ],
            );
        }
        if nr == nvrm_abi::xlate::uvm::REGISTER_GPU && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 40);
            let mut uuid = String::with_capacity(32);
            for x in &b[0..16] {
                uuid.push_str(&format!("{x:02x}"));
            }
            // Preserve the enabled/node spelling shared by both trace readers.
            let numa = format!(
                "{}/{}",
                b[16],
                i32::from_le_bytes(b[20..24].try_into().unwrap())
            );
            rec1(
                "uvmreg",
                &[
                    key("uuid", V::S(&uuid)),
                    key("numa", V::S(&numa)),
                    key(
                        "rmCtrlFd",
                        V::I(i32::from_le_bytes(b[24..28].try_into().unwrap()) as i64),
                    ),
                    key(
                        "hClient",
                        V::H32(u32::from_le_bytes(b[28..32].try_into().unwrap())),
                    ),
                    key(
                        "rmStatus",
                        V::H32(u32::from_le_bytes(b[36..40].try_into().unwrap())),
                    ),
                ],
            );
        }
        return;
    }
    // The remaining payload decoders require RM layouts.
    if matches!(
        dev,
        NvDev::UvmTools | NvDev::Drm(_) | NvDev::Event | NvDev::Modeset
    ) {
        return;
    }
    // Dump inline bytes using the decoded size. CONTROL and ALLOC have their
    // own records for pointed-to parameters; avoid a second record for those calls.
    if !matches!(nr, sys::NV_ESC_RM_CONTROL | sys::NV_ESC_RM_ALLOC) && size > 0 {
        let (sub, _, _) = subcode(dev, nr, size, arg);
        let n = (size as usize).min(dump_cap());
        let bytes = core::slice::from_raw_parts(arg as *const u8, n);
        rec(
            "escout",
            phase_of(tag),
            &[
                pos("dev", V::S(dev_tag(dev))),
                pos("nr", V::H32(nr)),
                pos("sub", sub.map(V::H32).unwrap_or(V::Nil)),
                key("len", V::I(size as i64)),
                pos("dump", V::Dump(bytes)),
            ],
        );
    }

    match nr {
        // Card-info output: one record per valid card, including mediated PCI fields.
        sys::NV_ESC_CARD_INFO
            if tag.is_empty() && size as usize >= size_of::<sys::nv_ioctl_card_info_t>() =>
        {
            let n = size as usize / size_of::<sys::nv_ioctl_card_info_t>();
            let cards = core::slice::from_raw_parts(arg as *const sys::nv_ioctl_card_info_t, n);
            for (i, c) in cards.iter().enumerate() {
                if c.valid == 0 {
                    continue;
                }
                // Preserve the composite PCI/register/framebuffer strings used by the readers.
                let pci = format!(
                    "{:04x}:{:02x}:{:02x}.{}",
                    c.pci_info.domain, c.pci_info.bus, c.pci_info.slot, c.pci_info.function
                );
                let reg = format!("{:#x}+{:#x}", c.reg_address, c.reg_size);
                let fb = format!("{:#x}+{:#x}", c.fb_address, c.fb_size);
                rec1(
                    "cardinfo",
                    &[
                        pos("i", V::Idx(i)),
                        key("gpu_id", V::H32(c.gpu_id)),
                        key("pci", V::S(&pci)),
                        key("vendor", V::S(&format!("{:#06x}", c.pci_info.vendor_id))),
                        key("device", V::S(&format!("{:#06x}", c.pci_info.device_id))),
                        key("irq", V::I(c.interrupt_line as i64)),
                        key("reg", V::S(&reg)),
                        key("fb", V::S(&fb)),
                        key("minor", V::I(c.minor_number as i64)),
                    ],
                );
            }
        }
        // Capture every control before and after the call. NVOS54 supplies params/size;
        // both phases distinguish omitted input pointers from lost output pointers.
        sys::NV_ESC_RM_CONTROL if size as usize >= size_of::<sys::NVOS54_PARAMETERS>() => {
            let p = &*(arg as *const sys::NVOS54_PARAMETERS);
            let cmd = p.cmd as u32;
            let pp = p.params as usize as *const u8;
            let plen = p.paramsSize as usize;
            if !pp.is_null() && plen > 0 {
                let n = plen.min(dump_cap());
                let bytes = core::slice::from_raw_parts(pp, n);

                // Nested layouts use bindgen offsets independently of the forwarding table.
                // Caller counts and the dump cap bound each read; skip counts above 2^20.
                let mut nested: Vec<u8> = Vec::new();
                let mut ntotal: usize = 0;
                for (ptr_off, len_off, elem) in nvrm_abi::xlate::ctrl_nested_compiled(cmd) {
                    if plen < ptr_off + 8 || plen < len_off + 4 {
                        continue;
                    }
                    let gp = (pp.add(*ptr_off) as *const u64).read_unaligned();
                    let cnt = (pp.add(*len_off) as *const u32).read_unaligned();
                    if cnt as usize > (1 << 20) {
                        continue;
                    }
                    let nl = (cnt as usize).saturating_mul(*elem as usize);
                    if gp == 0 || nl == 0 {
                        continue;
                    }
                    ntotal += nl;
                    let room = dump_cap().saturating_sub(nested.len());
                    let take = nl.min(room);
                    if take > 0 {
                        nested.extend_from_slice(core::slice::from_raw_parts(
                            gp as usize as *const u8,
                            take,
                        ));
                    }
                }
                // Object/client handles distinguish instances of the same control command.
                rec(
                    "ctrlout",
                    phase_of(tag),
                    &[
                        pos("cmd", V::H32(cmd)),
                        key("hclient", V::H32(p.hClient)),
                        key("hobject", V::H32(p.hObject)),
                        key("len", V::I(plen as i64)),
                        key("status", V::H32(p.status as u32)),
                        pos("dump", V::Dump(bytes)),
                        // Present only when there IS something behind a pointer,
                        // so an ordinary control's line is unchanged.
                        key(
                            "nlen",
                            if ntotal > 0 {
                                V::I(ntotal as i64)
                            } else {
                                V::Nil
                            },
                        ),
                        pos("nested", V::Dump(&nested)),
                    ],
                );
            }
        }
        sys::NV_ESC_RM_ALLOC_MEMORY if size >= 56 => rec(
            "nvos02",
            phase_of(tag),
            &[
                key("hRoot", V::H32(w(arg, 0))),
                key("hParent", V::H32(w(arg, 1))),
                key("hNew", V::H32(w(arg, 2))),
                key("hClass", V::H32(w(arg, 3))),
                key("flags", V::H32(w(arg, 4))),
                key("pMemory", V::H64(q(arg, 6))),
                key("limit", V::H64(q(arg, 8))),
                key("status", V::H32(w(arg, 10))),
                key("fd", V::I(w(arg, 12) as i32 as i64)),
            ],
        ),
        sys::NV_ESC_RM_MAP_MEMORY if size >= 56 => rec(
            "nvos33",
            phase_of(tag),
            &[
                key("hClient", V::H32(w(arg, 0))),
                key("hDevice", V::H32(w(arg, 1))),
                key("hMemory", V::H32(w(arg, 2))),
                key("offset", V::H64(q(arg, 4))),
                key("length", V::H64(q(arg, 6))),
                key("pLinear", V::H64(q(arg, 8))),
                key("status", V::H32(w(arg, 10))),
                key("flags", V::H32(w(arg, 11))),
                key("fd", V::I(w(arg, 12) as i32 as i64)),
            ],
        ),
        // NVOS32 video-heap allocation/free layouts are guarded in nvgpu.rs.
        // Size and attributes are IN/OUT, so both phases matter.
        sys::NV_ESC_RM_VID_HEAP_CONTROL if size >= 184 => {
            let function = w(arg, 2);
            // 2 = ALLOC_SIZE, 3 = FREE (nvos.h:636-637). Only these two
            // carry the union members whose layout is guarded.
            if function == 2 {
                rec(
                    "nvos32",
                    phase_of(tag),
                    &[
                        key("hRoot", V::H32(w(arg, 0))),
                        key("hObjectParent", V::H32(w(arg, 1))),
                        key("function", V::S("ALLOC_SIZE")),
                        key("hVASpace", V::H32(w(arg, 3))),
                        key("status", V::H32(w(arg, 5))),
                        key("owner", V::H32(w(arg, 10))),
                        key("hMemory", V::H32(w(arg, 11))),
                        key("type", V::H32(w(arg, 12))),
                        key("flags", V::H32(w(arg, 13))),
                        key("attr", V::H32(w(arg, 14))),
                        key("format", V::H32(w(arg, 15))),
                        key("width", V::H32(w(arg, 19))),
                        key("height", V::H32(w(arg, 20))),
                        key("size", V::H64(q(arg, 22))),
                        key("alignment", V::H64(q(arg, 24))),
                        key("offset", V::H64(q(arg, 26))),
                        key("limit", V::H64(q(arg, 28))),
                        key("address", V::H64(q(arg, 30))),
                        key("attr2", V::H32(w(arg, 36))),
                    ],
                );
            } else if function == 3 {
                rec(
                    "nvos32",
                    phase_of(tag),
                    &[
                        key("hRoot", V::H32(w(arg, 0))),
                        key("hObjectParent", V::H32(w(arg, 1))),
                        key("function", V::S("FREE")),
                        key("status", V::H32(w(arg, 5))),
                        key("owner", V::H32(w(arg, 10))),
                        key("hMemory", V::H32(w(arg, 11))),
                        key("flags", V::H32(w(arg, 12))),
                    ],
                );
            } else {
                rec(
                    "nvos32",
                    phase_of(tag),
                    &[
                        key("hRoot", V::H32(w(arg, 0))),
                        key("hObjectParent", V::H32(w(arg, 1))),
                        key("function", V::H32(function)),
                        key("status", V::H32(w(arg, 5))),
                    ],
                );
            }
        }
        sys::NV_ESC_RM_MAP_MEMORY_DMA if size >= 64 => rec(
            "nvos46",
            phase_of(tag),
            &[
                key("hClient", V::H32(w(arg, 0))),
                key("hDevice", V::H32(w(arg, 1))),
                key("hDma", V::H32(w(arg, 2))),
                key("hMemory", V::H32(w(arg, 3))),
                key("offset", V::H64(q(arg, 4))),
                key("length", V::H64(q(arg, 6))),
                key("flags", V::H32(w(arg, 8))),
                key("flags2", V::H32(w(arg, 9))),
                key("kind", V::H32(w(arg, 10))),
                key("dmaOffset", V::H64(q(arg, 12))),
                key("status", V::H32(w(arg, 14))),
            ],
        ),
        sys::NV_ESC_RM_ALLOC if size >= 48 => {
            let hclass = w(arg, 3);
            rec(
                "nvos64",
                phase_of(tag),
                &[
                    key("hRoot", V::H32(w(arg, 0))),
                    key("hParent", V::H32(w(arg, 1))),
                    key("hNew", V::H32(w(arg, 2))),
                    key("hClass", V::H32(hclass)),
                    key("paramsSize", V::H32(w(arg, 8))),
                    key("flags", V::H32(w(arg, 9))),
                    key("status", V::H32(w(arg, 10))),
                ],
            );

            // NV_MEMORY_ALLOCATION_PARAMS applies to classes 0x3e, 0x40 and 0x50a0.
            // Classes 0x71 and 0x70 use smaller, unrelated parameter structs.
            let pp = q(arg, 4) as usize as *const c_void;

            // Emit one allocout record: compiled class parameters when available,
            // otherwise the inline escape containing the returned handle/status.
            // The src field identifies which buffer was dumped. Compiled sizes keep this
            // measurement independent of the forwarding table; unknown sizes are not guessed.
            let params = if pp.is_null() {
                None
            } else {
                nvrm_abi::xlate::alloc_param_size_compiled::<nvrm_sys::DefaultAbi>(hclass)
                    .filter(|n| *n > 0)
            };
            let (src, plen, base) = match params {
                Some(plen) => ("params", plen, pp as *const u8),
                None => ("escape", size as usize, arg as *const u8),
            };
            if plen > 0 {
                let n = plen.min(dump_cap());
                let bytes = core::slice::from_raw_parts(base, n);
                rec(
                    "allocout",
                    phase_of(tag),
                    &[
                        pos("dev", V::S(dev_tag(dev))),
                        pos("class", V::H32(hclass)),
                        pos("src", V::S(src)),
                        key("len", V::I(plen as i64)),
                        pos("dump", V::Dump(bytes)),
                    ],
                );
            }

            if !pp.is_null() && matches!(hclass, 0x3e | 0x40 | 0x50a0) {
                rec(
                    "memparams",
                    phase_of(tag),
                    &[
                        key("hNew", V::H32(w(arg, 2))),
                        key("hClass", V::H32(hclass)),
                        key("owner", V::H32(w(pp, 0))),
                        key("type", V::H32(w(pp, 1))),
                        key("flags", V::H32(w(pp, 2))),
                        key("attr", V::H32(w(pp, 6))),
                        key("attr2", V::H32(w(pp, 7))),
                        key("rangeLo", V::H64(q(pp, 12))),
                        key("rangeHi", V::H64(q(pp, 14))),
                        key("size", V::H64(q(pp, 16))),
                        key("align", V::H64(q(pp, 18))),
                        key("offset", V::H64(q(pp, 20))),
                        key("limit", V::H64(q(pp, 22))),
                        key("address", V::H64(q(pp, 24))),
                        key("ctag", V::H32(w(pp, 26))),
                        key("hVASpace", V::H32(w(pp, 27))),
                        key("internal", V::H32(w(pp, 28))),
                        key("tag", V::H32(w(pp, 29))),
                        key("numa", V::I(w(pp, 30) as i32 as i64)),
                    ],
                );
            }
        }
        _ => {}
    }
}

/// Capture input values before the driver overwrites IN/OUT fields.
pub unsafe fn detail_pre(dev: NvDev, cmd: u32, arg: *const c_void) {
    // UVM numbers must retain their raw encoding.
    let (nr, size) = decode(dev, cmd);
    detail(dev, nr, size, arg, "in")
}

pub fn mmap(dev: NvDev, fd: i32, len: usize, off: i64, p: *mut c_void) {
    rec1(
        "mmap",
        &[
            pos("dev", V::S(dev_tag(dev))),
            pos("fd", V::I(fd as i64)),
            pos("len", V::I(len as i64)),
            pos("off", V::I(off)),
            pos("addr", V::H64(p as usize as u64)),
        ],
    );
}

/// Wait path: `read`/`poll` on a known FD.
/// `val` is the return value for `read`, `revents` for `poll`.
pub fn wait(kind: &str, dev: NvDev, fd: i32, val: i64) {
    // The positional TSV value maps to ret for read and revents for poll.
    rec1(
        kind,
        &[
            pos("dev", V::S(dev_tag(dev))),
            pos("fd", V::I(fd as i64)),
            pos(if kind == "poll" { "revents" } else { "ret" }, V::I(val)),
        ],
    );
}

/// Report the registered FD and its previous tag, or new when untracked.
pub fn event_registered(fd: i32, prev: Option<NvDev>) {
    rec1(
        "eventreg",
        &[
            pos("fd", V::I(fd as i64)),
            pos("prev", V::S(prev.map(dev_tag).unwrap_or("new"))),
        ],
    );
}

/// The eight ioctl fields retain their TSV order and JSON names.
/// Count/comparison scripts depend on this schema.
#[allow(clippy::too_many_arguments)]
fn ioctl_fields<'a>(
    dev: NvDev,
    fd: i32,
    nr: u32,
    size: u32,
    ret: i32,
    sub: Option<u32>,
    psize: Option<u32>,
    status: Option<u32>,
) -> [F<'a>; 8] {
    let f = |o: Option<u32>| o.map(V::H32).unwrap_or(V::Nil);
    [
        pos("dev", V::S(dev_tag(dev))),
        pos("nr", V::H32(nr)),
        pos("sub", f(sub)),
        pos("size", V::I(size as i64)),
        pos("psize", f(psize)),
        pos("ret", V::I(ret as i64)),
        pos("status", f(status)),
        pos("fd", V::I(fd as i64)),
    ]
}

/// Log a decoded command. XFER callers supply the inner number, size and pointer.
pub fn ioctl(dev: NvDev, fd: i32, nr: u32, size: u32, ret: i32, arg: *mut c_void) {
    let (sub, psize, status) = unsafe { subcode(dev, nr, size, arg) };
    rec1(
        "ioctl",
        &ioctl_fields(dev, fd, nr, size, ret, sub, psize, status),
    );
    unsafe { detail(dev, nr, size, arg, "") };
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvrm_abi::iowr_raw;

    #[test]
    fn failed_trace_writes_preserve_errno_and_count_loss() {
        let _errno = crate::ErrnoGuard::new();
        unsafe { *libc::__errno_location() = libc::EFAULT };
        let before = dropped();
        emit_fd(i32::MAX, "record\n");
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EFAULT);
        assert!(dropped() > before);
    }

    #[test]
    fn loss_report_fits_both_maximum_counters_without_allocation() {
        let report = loss_report(u64::MAX, u64::MAX);
        assert_eq!(std::str::from_utf8(&report.bytes[..report.len]).unwrap(),
            "nvrm-trace: incomplete trace: 18446744073709551615 failed/short writes, 18446744073709551615 untracked FD registrations\n");
    }

    #[test]
    fn the_ioctl_line_still_has_the_columns_the_counting_rule_selects_on() {
        let f = ioctl_fields(NvDev::Gpu, 9, 0xd6, 8, 0, None, None, None);
        assert_eq!(
            render_tsv("ioctl", None, &f),
            "ioctl\tgpu\t0xd6\t-\t8\t-\t0\t-\t9\n"
        );
        // Missing values have format-specific sentinels.
        assert_eq!(
            render_json("ioctl", None, &f),
            r#"{"t":"ioctl","dev":"gpu","nr":"0xd6","sub":null,"size":8,"psize":null,"ret":0,"status":null,"fd":9}"#.to_owned() + "\n"
        );
    }

    #[test]
    fn an_ioctl_line_with_every_field_spells_hex_the_same_in_both_formats() {
        let f = ioctl_fields(
            NvDev::Ctl,
            3,
            0x2a,
            32,
            0,
            Some(0x20800802),
            Some(16),
            Some(0x1e),
        );
        assert_eq!(
            render_tsv("ioctl", None, &f),
            "ioctl\tctl\t0x2a\t0x20800802\t32\t0x10\t0\t0x1e\t3\n"
        );
        assert_eq!(
            render_json("ioctl", None, &f),
            r#"{"t":"ioctl","dev":"ctl","nr":"0x2a","sub":"0x20800802","size":32,"psize":"0x10","ret":0,"status":"0x1e","fd":3}"#.to_owned() + "\n"
        );
    }

    #[test]
    fn the_in_sample_is_a_kind_suffix_in_tsv_and_a_field_in_json() {
        let f = [key("hNew", V::H32(0x5c000003)), key("status", V::H32(0))];
        assert_eq!(
            render_tsv("nvos64", phase_of("in"), &f),
            "nvos64in\thNew=0x5c000003\tstatus=0x0\n"
        );
        assert_eq!(
            render_tsv("nvos64", phase_of(""), &f),
            "nvos64\thNew=0x5c000003\tstatus=0x0\n"
        );
        assert_eq!(
            render_json("nvos64", phase_of("in"), &f),
            "{\"t\":\"nvos64\",\"phase\":\"in\",\"hNew\":\"0x5c000003\",\"status\":\"0x0\"}\n"
        );
        assert_eq!(
            render_json("nvos64", phase_of(""), &f),
            "{\"t\":\"nvos64\",\"phase\":\"out\",\"hNew\":\"0x5c000003\",\"status\":\"0x0\"}\n"
        );
    }

    #[test]
    fn a_dump_is_spaced_in_tsv_and_contiguous_in_json() {
        let b = [0x00u8, 0x2d, 0x00, 0x00, 0xff];
        let f = [
            pos("cmd", V::H32(0x214)),
            key("len", V::I(384)),
            key("status", V::H32(0)),
            pos("dump", V::Dump(&b)),
        ];
        assert_eq!(
            render_tsv("ctrlout", None, &f),
            "ctrlout\t0x214\tlen=384\tstatus=0x0\t00 2d 00 00 ff\n"
        );
        assert_eq!(
            render_json("ctrlout", None, &f),
            r#"{"t":"ctrlout","cmd":"0x214","len":384,"status":"0x0","dump":"002d0000ff"}"#
                .to_owned()
                + "\n"
        );
        // An empty dump is a field, not a missing one: `ctrlout` is only
        // emitted for plen > 0, but the renderer must not invent a `-`.
        let e = [pos("dump", V::Dump(&[]))];
        assert_eq!(
            render_json("ctrlout", None, &e),
            "{\"t\":\"ctrlout\",\"dump\":\"\"}\n"
        );
    }

    #[test]
    fn a_string_field_that_needs_escaping_still_leaves_valid_json() {
        let f = [key("s", V::S("a\"b\\c\td"))];
        assert_eq!(
            render_json("x", None, &f),
            "{\"t\":\"x\",\"s\":\"a\\\"b\\\\c\\td\"}\n"
        );
    }

    #[test]
    fn the_jsonl_path_is_derived_from_the_tsv_one() {
        assert_eq!(jsonl_path("/t/cuda-core.tsv"), "/t/cuda-core.jsonl");
        assert_eq!(jsonl_path("/t/raw"), "/t/raw.jsonl");
        // Only the suffix, never a `.tsv` inside a directory name.
        assert_eq!(jsonl_path("/t.tsv/raw"), "/t.tsv/raw.jsonl");
    }

    #[test]
    fn the_device_tags_are_the_strings_the_scripts_filter_on() {
        assert_eq!(dev_tag(NvDev::Ctl), "ctl");
        assert_eq!(dev_tag(NvDev::Gpu), "gpu");
        assert_eq!(dev_tag(NvDev::Uvm), "uvm");
        assert_eq!(dev_tag(NvDev::UvmTools), "uvmtools");
        assert_eq!(dev_tag(NvDev::Event), "event");
        assert_eq!(dev_tag(NvDev::Drm(false)), "drm");
        assert_eq!(dev_tag(NvDev::Drm(true)), "render");
        assert_eq!(dev_tag(NvDev::Modeset), "modeset");
        // The FD identifies the node; the tag identifies the device type.
        assert_eq!(dev_tag(NvDev::Gpu), dev_tag(NvDev::Gpu));
    }

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
            NvDev::Gpu,
            NvDev::Event,
            NvDev::Drm(false),
            NvDev::Drm(true),
            NvDev::Modeset,
        ] {
            assert_eq!(decode(dev, cmd), (0x2a, 32), "{dev:?}");
        }
    }

    #[test]
    fn subcode_reads_a_control_only_at_the_full_struct_size() {
        let mut p = sys::NVOS54_PARAMETERS::default();
        p.cmd = 0x2080_0110;
        p.paramsSize = 0x44;
        p.status = 0x56;
        let arg = &p as *const _ as *const c_void;
        let full = size_of::<sys::NVOS54_PARAMETERS>() as u32;
        assert_eq!(
            full, 32,
            "NVOS54 is the 32-byte form (nvgpu.rs layout guard)"
        );

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

        // NVOS21 has different size/status offsets from NVOS64.
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
        for dev in [
            NvDev::Drm(false),
            NvDev::Drm(true),
            NvDev::Uvm,
            NvDev::UvmTools,
        ] {
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

    #[test]
    fn a_modeset_line_carries_the_command_out_of_the_indirection_struct() {
        let full = size_of::<sys::NvKmsIoctlParams>() as u32;
        assert_eq!(
            full, 16,
            "NvKmsIoctlParams is 16 bytes (nvrm-sys layout test)"
        );

        let mut p = sys::NvKmsIoctlParams::default();
        p.cmd = 42;
        p.size = 0x340;
        p.address = 0xdead_beef;
        let arg = &p as *const _ as *const c_void;

        // The real number: _IOWR('m', 0, struct NvKmsIoctlParams), i.e.
        // nr 0 and size 16 for every command NVKMS has.
        let cmd = iowr_raw(sys::NVKMS_IOCTL_CMD, full);
        assert_eq!(decode(NvDev::Modeset, cmd), (0, full));
        assert_eq!(
            unsafe { subcode(NvDev::Modeset, 0, full, arg) },
            (Some(42), Some(0x340), None),
            "sub is the NVKMS command, psize the block it points at, and              there is no status field to report",
        );

        // A short wrapper must not be read as the full NVKMS struct.
        assert_eq!(
            unsafe { subcode(NvDev::Modeset, 0, full - 1, arg) },
            (None, None, None),
        );
        assert_eq!(
            unsafe { subcode(NvDev::Modeset, 0, full, std::ptr::null()) },
            (None, None, None),
        );
    }
}
