// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Logging without std::io.
//!
//! Reason: the interposer runs before (and during) libstd's own stdout
//! initialization, and that initialization can itself call open/ioctl.
//! Re-entering the hooks is a particularly nasty deadlock.
//!
//! TWO LINE FORMATS, ONE RECORD. Every line is built once as a list of
//! named fields (see `F` and `rec` below) and rendered by BOTH renderers,
//! so the formats cannot carry different information -- not because two
//! writers were kept in step, but because there is one writer and two
//! renderings of it. `LEA_TRACE_FORMAT` picks which are written: `tsv`,
//! `jsonl`, or `both` (the default, and what the migration runs on).
//!
//! The legacy format (TSV), one record per line, fields separated by tabs:
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
//! The same records as JSONL, one JSON object per line, `t` first:
//!
//! ```text
//! {"t":"open","dev":"ctl","fd":9}
//! {"t":"ioctl","dev":"gpu","nr":"0xd6","sub":null,"size":8,...,"fd":9}
//! ```
//!
//! WHY JSONL AT ALL, since TSV counts and greps fine: the reach half of
//! OPEN-QUESTIONS 55 wants answer dumps for ALLOCATIONS and UVM, whose
//! lengths are per-command and come from compiled headers. A positional
//! TSV with a fixed 32-byte tail cannot carry a variable payload without
//! becoming a format that is parsed by position AND by convention. This
//! one can.
//!
//! Scripts parse TSV lines by column, so field order and separators are
//! part of the interface; the JSON keys are the same interface by name.
//! Both are read through ONE reader per language -- `lea_trace_stream` in
//! `scripts/lib/matrix.sh` and `probe/python/traceread.py` -- and no
//! consumer opens a trace itself.
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
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

/// The TSV sink. 2 (stderr) until `init` says otherwise, so a tracer with
/// no `LEA_TRACE_FILE` still says what it saw; -1 disables it.
static OUT: AtomicI32 = AtomicI32::new(2);
/// The JSONL sink. -1 (off) unless `init` opens one: an unconfigured
/// tracer must not start spraying JSON at stderr beside the TSV.
static OUT_JSON: AtomicI32 = AtomicI32::new(-1);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Where the JSONL goes when only `LEA_TRACE_FILE` was given: the same
/// path with a `.tsv` suffix replaced, or `.jsonl` appended if there was
/// none. Deriving it rather than demanding a second variable means every
/// existing caller gets both formats by changing nothing.
fn jsonl_path(tsv: &str) -> String {
    match tsv.strip_suffix(".tsv") {
        Some(stem) => format!("{stem}.jsonl"),
        None => format!("{tsv}.jsonl"),
    }
}

/// O_APPEND, and this used to be O_TRUNC.
///
/// WHY IT CHANGED. `LEA_TRACE_FILE` is inherited by every child of the
/// traced process, and each child's constructor opens it again. Under
/// O_TRUNC that is a SECOND file description with its own offset, writing
/// over the first from byte zero -- and the damage is silent, because a
/// half-overwritten file is still a valid file. Measured 2026-08-20 on
/// `cuda-gdb`, which launches an inferior: a complete second ioctl record
/// written over the middle of the first, with a different fd, in both
/// formats.
///
/// O_APPEND fixes it at the source. Every writer's `write(2)` on a regular
/// file positions at the end and writes under the inode lock, so a record
/// from another process lands after the previous one instead of on top of
/// it. Multi-process workloads produce an interleaving of whole records --
/// which is what a multi-threaded workload has always produced within one
/// process, and what `strace -f` counts on the other side.
///
/// WHAT NOW TRUNCATES. Whoever owns the path, before the run: the matrix
/// runner hands the tracer a fresh `mktemp` file per attempt, and
/// `probe/run/trace.sh` truncates its per-stage files in `lea_trace_stage`.
/// A tracer that truncated could not be told "append to this" by anyone.
fn open_out(path: &str) -> i32 {
    let Ok(c) = std::ffi::CString::new(path) else { return -1 };
    unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND | libc::O_CLOEXEC,
            0o644,
        )
    }
}

pub fn init() {
    let Ok(path) = std::env::var("LEA_TRACE_FILE") else { return };
    // `both` is the default for the duration of the migration: one run
    // produces both formats, so the equivalence check compares two
    // renderings of the SAME calls. Two runs would compare two runs, and
    // this pipeline has measured values that differ between two runs of
    // one binary -- that confound is exactly what a format check must not
    // have in it.
    let fmt = std::env::var("LEA_TRACE_FORMAT").unwrap_or_else(|_| "both".into());
    let (want_tsv, want_json) = match fmt.as_str() {
        "tsv" => (true, false),
        "jsonl" => (false, true),
        "both" => (true, true),
        other => {
            emit_fd(2, &format!(
                "nvrm-trace: LEA_TRACE_FORMAT={other} is not tsv, jsonl or both -- writing both\n"
            ));
            (true, true)
        }
    };

    if want_tsv {
        let fd = open_out(&path);
        if fd >= 0 {
            OUT.store(fd, Ordering::Relaxed);
        } else {
            // Do not fall back to stderr silently - that is exactly how a whole
            // run gets lost without anyone noticing.
            emit_fd(2, &format!("nvrm-trace: cannot open {path}, trace goes to stderr\n"));
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
            // Louder than the TSV case: a missing JSONL is not a trace that
            // went somewhere else, it is a trace that does not exist, and
            // the format gate would read that as "nothing differs".
            emit_fd(2, &format!("nvrm-trace: cannot open {jp}, no JSONL trace this run\n"));
        }
    }
}

fn emit_fd(fd: i32, s: &str) {
    if fd < 0 {
        return;
    }
    let n = unsafe { libc::write(fd, s.as_ptr() as *const c_void, s.len()) };
    if n < 0 || (n as usize) < s.len() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

// ---- the record layer -------------------------------------------------------
//
// One record, two renderings, ONE write() per line per format. The write
// stays per line because that is what makes a trace of a crashed process
// still a trace up to the crash, and because this sits on a per-frame path
// -- 86 645 lines in one measured session. A document format (a JSON array,
// an XML tree) cannot carry that: it has a closing bracket.
//
// THE PAIR IS NOT ATOMIC, and that is deliberate. `rec` writes the TSV line
// and then the JSON line, and another thread can write both of ITS lines in
// between -- so during the migration the two files hold the same records in
// a different INTERLEAVING. Measured 2026-08-20: 7 of 20 probes, all of them
// the concurrent ones. Making the pair atomic would mean holding a lock
// across two write() calls on a path that runs inside every frame, in a
// library that is preloaded into processes that fork; the cost and the
// deadlock surface are real and the benefit is an ordering nothing consumes.
// The equivalence gate therefore compares the two files as MULTISETS, which
// is what the renderers actually control -- see `traceread.py --check`.
// After the cutover only one file is written and the question disappears.

/// A field value. `Copy`, so a record's fields live in the caller's stack
/// frame and the only allocation per line is the rendered string itself.
#[derive(Clone, Copy)]
enum V<'a> {
    /// Raw in TSV, quoted and escaped in JSON.
    S(&'a str),
    H32(u32),
    H64(u64),
    I(i64),
    /// A byte dump: space-separated in TSV, because that is what the
    /// readers of the old format split on; contiguous in JSON, because a
    /// variable-length dump is easier to slice without them.
    Dump(&'a [u8]),
    /// An array index: `[3]` in TSV, a bare number in JSON.
    Idx(usize),
    /// Absent: `-` in TSV -- the spelling every awk site tests for -- and
    /// `null` in JSON.
    Nil,
}

/// One field of one record. `name` is the JSON key always, and the TSV key
/// only when `keyed`; the TSV format is positional for the measurement
/// lines and key=value for the diagnostic ones, and this carries both.
#[derive(Clone, Copy)]
struct F<'a> {
    name: &'a str,
    keyed: bool,
    v: V<'a>,
}

/// A positional TSV field (named in JSON regardless).
const fn pos<'a>(name: &'a str, v: V<'a>) -> F<'a> {
    F { name, keyed: false, v }
}
/// A `name=value` TSV field.
const fn key<'a>(name: &'a str, v: V<'a>) -> F<'a> {
    F { name, keyed: true, v }
}

/// How many bytes of a params buffer a dump carries, at most.
///
/// It was a hard-coded 32, which is two words past the first field of most
/// controls -- enough to tell an enumeration answer apart and not enough to
/// verify a struct. `answerdiff` compares the words both sides dumped, so
/// this is directly how much of each answer is under test.
///
/// It was 32, then 256, and 256 turned out to be 4.3% of the answer bytes in
/// a sweep: 38 signatures were truncated, some of them badly -- one control
/// declares 67396 bytes and 256 of them were being compared. 65536 covers
/// every answer in the current trace set whole, and costs about 36 MB of
/// dump text across a sweep against 4 MB, which is nothing against a 120 GB
/// disk. Truncation is safe in the direction that matters -- fewer bytes
/// compared, never bytes invented -- but it is still a claim about 4% of a
/// struct being read as a claim about the struct.
///
/// The length read is always the CALLER'S declared size (`paramsSize` for a
/// control, `_IOC_SIZE` for an escape, `size_of` of the compiled struct for
/// an allocation or a UVM command), so raising the cap never reads a byte
/// that the caller did not say was there.
///
/// `LEA_TRACE_DUMP` overrides it. 0 disables dumping entirely, which is the
/// way to take a cheap trace when only the call COUNTS are wanted.
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

/// JSON string body, escaped. Every string this file writes today is
/// ASCII and free of quotes, but a field added later will not be, and a
/// trace that is not parseable JSON fails in the reader rather than here.
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
        // Hex stays a hex STRING in JSON. A number would lose the spelling
        // the descriptor tables and the catalogue use, and every consumer
        // already reads these with int(x, 16).
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

/// TSV rendering. `phase` is the `in`/`out` suffix the old format spells
/// by appending `in` to the kind (`nvos64in` against `nvos64`), which is
/// why it is a suffix here and a field in JSON.
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
        NvDev::Modeset => "modeset",
    }
}

pub fn open(dev: NvDev, fd: i32) {
    rec1("open", &[pos("dev", V::S(dev_tag(dev))), pos("fd", V::I(fd as i64))]);
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
    // NVKMS. The whole interface goes through ONE ioctl number, so `nr` is
    // 0 on every line and says nothing; the command is a field of the
    // 16-byte indirection struct (nvkms-ioctl.h, offsets guarded by
    // nvrm-sys's layout tests) and that is what `sub` carries. `psize` is
    // the size of the block the struct points AT.
    //
    // No status. NVKMS answers inside that block, per command, with no
    // field in a shared position -- so `ret` is the only verdict this line
    // can carry without inventing one. Reading the block would need a
    // decoder for the NVKMS command namespace, which is a separate piece of
    // work and deliberately not here: those commands resolve against no
    // `ctrl*.h`, and a made-up name is worse than a number.
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
        // THE UVM ANSWER, for every command whose parameter block the
        // compiler can measure. UVM is where a large part of the governed
        // class lives and it had no answer evidence of any kind: its ioctls
        // carry no size (UVM_IOCTL_BASE(i) is a bare number, so _IOC_SIZE is
        // 0), which is exactly why the length has to come from somewhere
        // else.
        //
        // AND NOT FROM `xlate::uvm_param_size`, which is the hand-computed
        // table the guest module forwards on. Dumping UVM answers exists to
        // JUDGE that forwarding, and an instrument that measured with the
        // table under test would agree with it by construction. The length
        // is `size_of` of the bindgen struct; nvrm-abi's own test requires
        // the two to agree, so the table is checked rather than trusted.
        if let Some(plen) = nvrm_abi::xlate::uvm_param_size_compiled(nr) {
            if plen > 0 {
                let n = plen.min(dump_cap());
                let bytes = core::slice::from_raw_parts(arg as *const u8, n);
                rec("uvmout", phase_of(tag), &[
                    pos("nr", V::H32(nr)),
                    key("len", V::I(plen as i64)),
                    pos("dump", V::Dump(bytes)),
                ]);
            }
        }
        // UVM_INITIALIZE (0x30000001): flags IN/OUT @0 (u64), rmStatus @8.
        // The driver reads the flags back and decides on the pageable/ATS
        // path from them -- the branch point between "calls 0x46" and
        // "does not" (OPEN-QUESTIONS nr 11).
        if nr == 0x30000001 && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 12);
            rec1("uvminit", &[
                key("flags", V::H64(u64::from_le_bytes(b[0..8].try_into().unwrap()))),
                key("rmStatus", V::H32(u32::from_le_bytes(b[8..12].try_into().unwrap()))),
            ]);
        }
        // UVM_PAGEABLE_MEM_ACCESS (0x27: pageableMemAccess NvBool @0,
        // rmStatus @4 -- EIGHT bytes, uvm_ioctl.h) and
        // UVM_PAGEABLE_MEM_ACCESS_ON_GPU (0x46: uuid @0, pageableMemAccess
        // @16, rmStatus @20 -- 24 bytes). Two structs, two lengths: reading
        // 24 bytes for the 8-byte one is the over-read this file has had
        // before, only on the UVM side.
        if nr == nvrm_abi::xlate::uvm::PAGEABLE_MEM_ACCESS && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 8);
            rec1("uvmpma", &[
                key("nr", V::H32(nr)),
                key("b0", V::H32(u32::from_le_bytes(b[0..4].try_into().unwrap()))),
                key("rmStatus", V::H32(u32::from_le_bytes(b[4..8].try_into().unwrap()))),
            ]);
        }
        if nr == nvrm_abi::xlate::uvm::PAGEABLE_MEM_ACCESS_ON_GPU && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 24);
            rec1("uvmpma", &[
                key("nr", V::H32(nr)),
                key("b0", V::H32(u32::from_le_bytes(b[0..4].try_into().unwrap()))),
                key("b16", V::H32(u32::from_le_bytes(b[16..20].try_into().unwrap()))),
                key("b20", V::H32(u32::from_le_bytes(b[20..24].try_into().unwrap()))),
            ]);
        }
        if nr == nvrm_abi::xlate::uvm::REGISTER_GPU && tag.is_empty() {
            let b = core::slice::from_raw_parts(arg as *const u8, 40);
            let mut uuid = String::with_capacity(32);
            for x in &b[0..16] {
                uuid.push_str(&format!("{x:02x}"));
            }
            // `numa` stays the composite `enabled/node` string it has
            // always been. Splitting it would be a better JSON shape and a
            // worse migration: the equivalence gate compares the two
            // renderings of this record, and a field that exists on one
            // side only cannot be compared at all.
            let numa = format!(
                "{}/{}",
                b[16],
                i32::from_le_bytes(b[20..24].try_into().unwrap())
            );
            rec1("uvmreg", &[
                key("uuid", V::S(&uuid)),
                key("numa", V::S(&numa)),
                key("rmCtrlFd", V::I(i32::from_le_bytes(b[24..28].try_into().unwrap()) as i64)),
                key("hClient", V::H32(u32::from_le_bytes(b[28..32].try_into().unwrap()))),
                key("rmStatus", V::H32(u32::from_le_bytes(b[36..40].try_into().unwrap()))),
            ]);
        }
        return;
    }
    // See subcode(): a DRM ioctl's argument is not an NVIDIA parameter
    // block, and every arm below assumes it is.
    // Modeset for the same reason, one step further: its argument IS a
    // known struct, but everything it points at belongs to a namespace
    // nothing here decodes.
    if matches!(dev, NvDev::UvmTools | NvDev::Drm(_) | NvDev::Event | NvDev::Modeset) {
        return;
    }
    // THE ESCAPE'S OWN PARAMETER BLOCK, for every escape that does not
    // already have a dump of its own.
    //
    // This needs no table at all, and that is the point: the length is
    // `_IOC_SIZE` of the request, which is the caller's declared size of the
    // struct it is passing, encoded in the ioctl number by the caller
    // itself. Self-describing, so it is safe to read at any width and there
    // is nothing here that could disagree with the descriptor table.
    //
    // It covers what nothing else did: RM_FREE, REGISTER_FD, the OS_EVENT
    // pair, DUP_OBJECT, IDLE_CHANNELS, VID_HEAP_CONTROL, both MAP_MEMORY
    // escapes and ALLOC_MEMORY -- every one of which had a signature in the
    // catalogue and no answer evidence of any kind.
    //
    // RM_CONTROL and RM_ALLOC are excluded deliberately. Their answer is not
    // in the ioctl struct but in the buffer it POINTS at, `ctrlout` and
    // `allocout` dump that, and emitting a second stream under the same
    // signature would interleave two different things in one list.
    if !matches!(nr, sys::NV_ESC_RM_CONTROL | sys::NV_ESC_RM_ALLOC) && size > 0 {
        let (sub, _, _) = subcode(dev, nr, size, arg);
        let n = (size as usize).min(dump_cap());
        let bytes = core::slice::from_raw_parts(arg as *const u8, n);
        rec("escout", phase_of(tag), &[
            pos("dev", V::S(dev_tag(dev))),
            pos("nr", V::H32(nr)),
            pos("sub", sub.map(V::H32).unwrap_or(V::Nil)),
            key("len", V::I(size as i64)),
            pos("dump", V::Dump(bytes)),
        ]);
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
                // Three composite fields (`pci`, `reg`, `fb`) keep the
                // spelling they have in the old format, for the reason
                // `uvmreg`'s `numa` does.
                let pci = format!(
                    "{:04x}:{:02x}:{:02x}.{}",
                    c.pci_info.domain, c.pci_info.bus, c.pci_info.slot, c.pci_info.function
                );
                let reg = format!("{:#x}+{:#x}", c.reg_address, c.reg_size);
                let fb = format!("{:#x}+{:#x}", c.fb_address, c.fb_size);
                rec1("cardinfo", &[
                    pos("i", V::Idx(i)),
                    key("gpu_id", V::H32(c.gpu_id)),
                    key("pci", V::S(&pci)),
                    key("vendor", V::S(&format!("{:#06x}", c.pci_info.vendor_id))),
                    key("device", V::S(&format!("{:#06x}", c.pci_info.device_id))),
                    key("irq", V::I(c.interrupt_line as i64)),
                    key("reg", V::S(&reg)),
                    key("fb", V::S(&fb)),
                    key("minor", V::I(c.minor_number as i64)),
                ]);
            }
        }
        // The params buffer of EVERY control, on both sides of the call, so
        // that what a forwarded control ANSWERED can be diffed native
        // against guest without a struct per control. NVOS54: params P64
        // @16, paramsSize @24.
        //
        // EVERY control, and it used to be `(cmd >> 8) == 0x2 ||
        // (cmd >> 16) == 0x2080`. That covered the root-client and
        // subdevice namespaces and silently covered nothing else -- no
        // NV0073 display control, no NV0080 device control, and none of the
        // class-specific ones the graphics stack actually calls (0x906f,
        // 0xc36f, 0xa06c). Measured 2026-08-21: those are about half the
        // distinct controls in a trace, and every one of them had no answer
        // evidence at all, so no amount of sweeping could ever verify them.
        // The length is NVOS54's own `paramsSize`, which is the caller's
        // declared size of its own buffer -- self-describing, and therefore
        // safe to read at any width.
        //
        // BOTH PHASES, and the `in` one is the point of number 60. A dump
        // taken only after the call cannot tell an OUT pointer the boundary
        // dropped from an IN pointer the guest's caller never supplied:
        // both read as zero afterwards. With the before-call sample the two
        // are different rows.
        sys::NV_ESC_RM_CONTROL if size as usize >= size_of::<sys::NVOS54_PARAMETERS>() => {
            let p = &*(arg as *const sys::NVOS54_PARAMETERS);
            let cmd = p.cmd as u32;
            let pp = p.params as usize as *const u8;
            let plen = p.paramsSize as usize;
            if !pp.is_null() && plen > 0 {
                let n = plen.min(dump_cap());
                let bytes = core::slice::from_raw_parts(pp, n);

                // AND WHAT THE PARAMS POINT AT. For these commands the params
                // buffer is the QUESTION -- a count and an NvP64 -- and the
                // ANSWER is behind the pointer. Without this, thirteen
                // signatures were reported verified on "16 of 16 bytes",
                // which says the whole answer was compared and means the
                // whole question was.
                //
                // The offsets come from `offset_of!` on the bindgen struct,
                // NOT from `xlate::nested_ptrs`: that is the table the guest
                // module forwards on, and an instrument that took its
                // offsets from the table under test would agree with it by
                // construction. nvrm-abi's own test requires the two to
                // agree.
                //
                // The read is bounded twice over: by the caller's own count
                // field, which is the same length RM's copy_from_user and
                // the guest module both read, and by the dump cap. A count
                // beyond any plausible list is skipped rather than trusted.
                let mut nested: Vec<u8> = Vec::new();
                let mut ntotal: usize = 0;
                for (ptr_off, len_off, elem) in
                    nvrm_abi::xlate::ctrl_nested_compiled(cmd)
                {
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
                            gp as usize as *const u8, take));
                    }
                }
                // `hobject` is the object the control was issued ON, which
                // is what tells two calls of one command apart when they
                // differ: a device control and a subdevice control of the
                // same number are different questions. Diagnostic -- the
                // comparison reads `cmd`, `len`, `status` and `dump` by
                // name and never sees it.
                rec("ctrlout", phase_of(tag), &[
                    pos("cmd", V::H32(cmd)),
                    key("hclient", V::H32(p.hClient)),
                    key("hobject", V::H32(p.hObject)),
                    key("len", V::I(plen as i64)),
                    key("status", V::H32(p.status as u32)),
                    pos("dump", V::Dump(bytes)),
                    // Present only when there IS something behind a pointer,
                    // so an ordinary control's line is unchanged.
                    key("nlen", if ntotal > 0 { V::I(ntotal as i64) } else { V::Nil }),
                    pos("nested", V::Dump(&nested)),
                ]);
            }
        }
        sys::NV_ESC_RM_ALLOC_MEMORY if size >= 56 => rec("nvos02", phase_of(tag), &[
            key("hRoot", V::H32(w(arg, 0))),
            key("hParent", V::H32(w(arg, 1))),
            key("hNew", V::H32(w(arg, 2))),
            key("hClass", V::H32(w(arg, 3))),
            key("flags", V::H32(w(arg, 4))),
            key("pMemory", V::H64(q(arg, 6))),
            key("limit", V::H64(q(arg, 8))),
            key("status", V::H32(w(arg, 10))),
            key("fd", V::I(w(arg, 12) as i32 as i64)),
        ]),
        sys::NV_ESC_RM_MAP_MEMORY if size >= 56 => rec("nvos33", phase_of(tag), &[
            key("hClient", V::H32(w(arg, 0))),
            key("hDevice", V::H32(w(arg, 1))),
            key("hMemory", V::H32(w(arg, 2))),
            key("offset", V::H64(q(arg, 4))),
            key("length", V::H64(q(arg, 6))),
            key("pLinear", V::H64(q(arg, 8))),
            key("status", V::H32(w(arg, 10))),
            key("flags", V::H32(w(arg, 11))),
            key("fd", V::I(w(arg, 12) as i32 as i64)),
        ]),
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
                rec("nvos32", phase_of(tag), &[
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
                ]);
            } else if function == 3 {
                rec("nvos32", phase_of(tag), &[
                    key("hRoot", V::H32(w(arg, 0))),
                    key("hObjectParent", V::H32(w(arg, 1))),
                    key("function", V::S("FREE")),
                    key("status", V::H32(w(arg, 5))),
                    key("owner", V::H32(w(arg, 10))),
                    key("hMemory", V::H32(w(arg, 11))),
                    key("flags", V::H32(w(arg, 12))),
                ]);
            } else {
                rec("nvos32", phase_of(tag), &[
                    key("hRoot", V::H32(w(arg, 0))),
                    key("hObjectParent", V::H32(w(arg, 1))),
                    key("function", V::H32(function)),
                    key("status", V::H32(w(arg, 5))),
                ]);
            }
        }
        sys::NV_ESC_RM_MAP_MEMORY_DMA if size >= 64 => rec("nvos46", phase_of(tag), &[
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
        ]),
        sys::NV_ESC_RM_ALLOC if size >= 48 => {
            let hclass = w(arg, 3);
            rec("nvos64", phase_of(tag), &[
                key("hRoot", V::H32(w(arg, 0))),
                key("hParent", V::H32(w(arg, 1))),
                key("hNew", V::H32(w(arg, 2))),
                key("hClass", V::H32(hclass)),
                key("paramsSize", V::H32(w(arg, 8))),
                key("flags", V::H32(w(arg, 9))),
                key("status", V::H32(w(arg, 10))),
            ]);

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

            // THE ALLOCATION ANSWER, as bytes, for every class whose
            // parameter block the compiler can measure. RM_ALLOC is the
            // largest part of the governed class with no answer evidence:
            // paramsSize is 0 on every one of these calls (the size comes
            // from the CLASS, which is the whole reason the descriptor table
            // exists), so nothing self-describing said how much to read.
            //
            // The length is `size_of` of the bindgen struct and NOT
            // `xlate::alloc_param_size`, for the reason the UVM dump does
            // not use `uvm_param_size`: the point of dumping allocation
            // answers is to judge the forwarding those tables drive. A class
            // the compiler cannot measure gets NO dump rather than a guessed
            // one -- reading past the end of a caller's struct is a bug this
            // file has had before.
            //
            // `memparams` below stays: it is the same bytes NAMED, which is
            // what a person reads, where this is the same bytes COMPARABLE,
            // which is what `answerdiff` reads.
            // EXACTLY ONE RECORD PER ALLOCATION, whatever the class does.
            //
            // Preferably the parameter block: that is where the answer is.
            // But many classes allocate with a NULL `pAllocParms` -- the
            // graphics objects do, and so does NV01_ROOT_CLIENT -- and for
            // those the only answer there is is the escape's OWN struct, the
            // status and the handle RM assigned. Measured 2026-08-21: four
            // signatures with 142 allocations between them had no answer
            // evidence for exactly this reason, because `escout` skips
            // RM_ALLOC and this arm only fired when there were params.
            //
            // One record either way, and `src` says which, so the two sides
            // produce the same number of records for the same calls and the
            // comparison pairs them. Where the sides disagree about whether
            // params were passed at all, the dumps differ in LENGTH, which
            // is reported as a difference rather than hidden -- and it is
            // one.
            let params = if pp.is_null() {
                None
            } else {
                nvrm_abi::xlate::alloc_param_size_compiled(hclass).filter(|n| *n > 0)
            };
            let (src, plen, base) = match params {
                Some(plen) => ("params", plen, pp as *const u8),
                None => ("escape", size as usize, arg as *const u8),
            };
            if plen > 0 {
                let n = plen.min(dump_cap());
                let bytes = core::slice::from_raw_parts(base, n);
                rec("allocout", phase_of(tag), &[
                    pos("dev", V::S(dev_tag(dev))),
                    pos("class", V::H32(hclass)),
                    pos("src", V::S(src)),
                    key("len", V::I(plen as i64)),
                    pos("dump", V::Dump(bytes)),
                ]);
            }

            if !pp.is_null() && matches!(hclass, 0x3e | 0x40 | 0x50a0) {
                rec("memparams", phase_of(tag), &[
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
                ]);
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
pub unsafe fn detail_pre(dev: NvDev, cmd: u32, arg: *const c_void) {
    // DECODE HERE, not at the call site. The caller used to unpack the
    // number with `ioc_nr`, which is right for every device except the two
    // that matter most here: UVM numbers carry no _IOC encoding, so masking
    // one yields a number no arm below matches, and UVM has therefore never
    // had a before-call sample at all.
    let (nr, size) = decode(dev, cmd);
    detail(dev, nr, size, arg, "in")
}

pub fn mmap(dev: NvDev, fd: i32, len: usize, off: i64, p: *mut c_void) {
    rec1("mmap", &[
        pos("dev", V::S(dev_tag(dev))),
        pos("fd", V::I(fd as i64)),
        pos("len", V::I(len as i64)),
        pos("off", V::I(off)),
        pos("addr", V::H64(p as usize as u64)),
    ]);
}

/// Wait path: `read`/`poll` on a known FD.
/// `val` is the return value for `read`, `revents` for `poll`.
pub fn wait(kind: &str, dev: NvDev, fd: i32, val: i64) {
    // `read` names its last field `ret` and `poll` names it `revents`:
    // the old format is positional here and said neither, and a JSON key
    // has to be one or the other.
    rec1(kind, &[
        pos("dev", V::S(dev_tag(dev))),
        pos("fd", V::I(fd as i64)),
        pos(if kind == "poll" { "revents" } else { "ret" }, V::I(val)),
    ]);
}

/// Which FD was registered as an event channel, and was it already known?
/// `prev` == None means: an eventfd, not a device, and the third column
/// then reads `new`. (It read `neu` until 2026-08-18; `probe/run/trace.sh`
/// prints that column and never matches it.)
pub fn event_registered(fd: i32, prev: Option<NvDev>) {
    rec1("eventreg", &[
        pos("fd", V::I(fd as i64)),
        pos("prev", V::S(prev.map(dev_tag).unwrap_or("new"))),
    ]);
}

/// The eight fields of an `ioctl` record, in the order the TSV format has
/// always had them.
///
/// THIS IS THE COUNTING SURFACE. `lea_matrix_n_tracer` and its two
/// siblings select on `t`/column 1 and on `dev`/column 2, and the strace
/// counter-check -- the trust anchor of every probe in the matrix -- is
/// their difference. Reordering these or renaming `dev` changes what the
/// pipeline counts, so it is one function that both call sites share and
/// the tests below pin both renderings of it.
#[allow(clippy::too_many_arguments)]
fn ioctl_fields<'a>(
    dev: NvDev, fd: i32, nr: u32, size: u32, ret: i32,
    sub: Option<u32>, psize: Option<u32>, status: Option<u32>,
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

pub fn ioctl(dev: NvDev, fd: i32, cmd: u32, ret: i32, arg: *mut c_void) {
    let (nr, size) = decode(dev, cmd);
    let (sub, psize, status) = unsafe { subcode(dev, nr, size, arg) };
    rec1("ioctl", &ioctl_fields(dev, fd, nr, size, ret, sub, psize, status));
    unsafe { detail(dev, nr, size, arg, "") };
}

/// The same line as `ioctl`, for a call that arrived wrapped in
/// `NV_ESC_IOCTL_XFER_CMD`: `nr` and `size` are the UNPACKED ones and must
/// not go through `decode()` a second time, and `arg` is the inner pointer.
pub fn ioctl_unpacked(dev: NvDev, fd: i32, nr: u32, size: u32, ret: i32, arg: *mut c_void) {
    let (sub, psize, status) = unsafe { subcode(dev, nr, size, arg) };
    rec1("ioctl", &ioctl_fields(dev, fd, nr, size, ret, sub, psize, status));
    unsafe { detail(dev, nr, size, arg, "") };
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvrm_abi::iowr_raw;

    /// The `ioctl` line is what the pipeline COUNTS, and the counting rule
    /// (`lea_matrix_n_*` in scripts/lib/matrix.sh) selects on column 1 and
    /// column 2. This is a golden line, not a formatting preference: the
    /// strace counter-check that gates every probe is a difference of two
    /// counts, and a column that moved would make one of them wrong
    /// silently -- a short trace is still a valid file.
    #[test]
    fn the_ioctl_line_still_has_the_columns_the_counting_rule_selects_on() {
        let f = ioctl_fields(NvDev::Gpu(0), 9, 0xd6, 8, 0, None, None, None);
        assert_eq!(render_tsv("ioctl", None, &f), "ioctl\tgpu\t0xd6\t-\t8\t-\t0\t-\t9\n");
        // Absent is `-` in TSV -- the spelling the awk sites test for --
        // and `null` in JSON, never the string "-".
        assert_eq!(
            render_json("ioctl", None, &f),
            r#"{"t":"ioctl","dev":"gpu","nr":"0xd6","sub":null,"size":8,"psize":null,"ret":0,"status":null,"fd":9}"#.to_owned() + "\n"
        );
    }

    /// A control line with every optional field present, so the hex
    /// spelling is pinned on both sides. Hex stays a STRING in JSON: the
    /// catalogue and the descriptor tables spell these `0x...` and a
    /// number would lose that.
    #[test]
    fn an_ioctl_line_with_every_field_spells_hex_the_same_in_both_formats() {
        let f = ioctl_fields(NvDev::Ctl, 3, 0x2a, 32, 0, Some(0x20800802), Some(16), Some(0x1e));
        assert_eq!(
            render_tsv("ioctl", None, &f),
            "ioctl\tctl\t0x2a\t0x20800802\t32\t0x10\t0\t0x1e\t3\n"
        );
        assert_eq!(
            render_json("ioctl", None, &f),
            r#"{"t":"ioctl","dev":"ctl","nr":"0x2a","sub":"0x20800802","size":32,"psize":"0x10","ret":0,"status":"0x1e","fd":3}"#.to_owned() + "\n"
        );
    }

    /// The IN sample and the OUT sample are one kind with two phases. The
    /// old format spells that by appending `in` to the kind; JSON spells
    /// it as a field. Both directions matter, because the projection that
    /// gates the migration has to be invertible.
    #[test]
    fn the_in_sample_is_a_kind_suffix_in_tsv_and_a_field_in_json() {
        let f = [key("hNew", V::H32(0x5c000003)), key("status", V::H32(0))];
        assert_eq!(render_tsv("nvos64", phase_of("in"), &f), "nvos64in\thNew=0x5c000003\tstatus=0x0\n");
        assert_eq!(render_tsv("nvos64", phase_of(""), &f), "nvos64\thNew=0x5c000003\tstatus=0x0\n");
        assert_eq!(
            render_json("nvos64", phase_of("in"), &f),
            "{\"t\":\"nvos64\",\"phase\":\"in\",\"hNew\":\"0x5c000003\",\"status\":\"0x0\"}\n"
        );
        assert_eq!(
            render_json("nvos64", phase_of(""), &f),
            "{\"t\":\"nvos64\",\"phase\":\"out\",\"hNew\":\"0x5c000003\",\"status\":\"0x0\"}\n"
        );
    }

    /// A payload dump is space-separated in the old format and contiguous
    /// in the new one -- the one deliberate difference between the two
    /// renderings, and therefore the one the projection has to undo.
    #[test]
    fn a_dump_is_spaced_in_tsv_and_contiguous_in_json() {
        let b = [0x00u8, 0x2d, 0x00, 0x00, 0xff];
        let f = [
            pos("cmd", V::H32(0x214)),
            key("len", V::I(384)),
            key("status", V::H32(0)),
            pos("dump", V::Dump(&b)),
        ];
        assert_eq!(render_tsv("ctrlout", None, &f), "ctrlout\t0x214\tlen=384\tstatus=0x0\t00 2d 00 00 ff\n");
        assert_eq!(
            render_json("ctrlout", None, &f),
            r#"{"t":"ctrlout","cmd":"0x214","len":384,"status":"0x0","dump":"002d0000ff"}"#.to_owned() + "\n"
        );
        // An empty dump is a field, not a missing one: `ctrlout` is only
        // emitted for plen > 0, but the renderer must not invent a `-`.
        let e = [pos("dump", V::Dump(&[]))];
        assert_eq!(render_json("ctrlout", None, &e), "{\"t\":\"ctrlout\",\"dump\":\"\"}\n");
    }

    /// Nothing this file writes today contains a quote or a backslash.
    /// Something added later will, and the failure mode is a trace file
    /// that is not JSON at all -- every consumer of it reporting nothing
    /// rather than an error.
    #[test]
    fn a_string_field_that_needs_escaping_still_leaves_valid_json() {
        let f = [key("s", V::S("a\"b\\c\td"))];
        assert_eq!(
            render_json("x", None, &f),
            "{\"t\":\"x\",\"s\":\"a\\\"b\\\\c\\td\"}\n"
        );
    }

    /// Deriving the JSONL path rather than demanding a second environment
    /// variable is what lets every existing caller get both formats by
    /// changing nothing.
    #[test]
    fn the_jsonl_path_is_derived_from_the_tsv_one() {
        assert_eq!(jsonl_path("/t/cuda-core.tsv"), "/t/cuda-core.jsonl");
        assert_eq!(jsonl_path("/t/raw"), "/t/raw.jsonl");
        // Only the suffix, never a `.tsv` inside a directory name.
        assert_eq!(jsonl_path("/t.tsv/raw"), "/t.tsv/raw.jsonl");
    }

    /// The device tag is column 2 of every line above (except `eventreg`,
    /// whose column 2 is the fd), and
    /// `probe/run/trace.sh` selects on it by string. These eight spellings
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
        assert_eq!(dev_tag(NvDev::Modeset), "modeset");
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
            NvDev::Modeset,
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

    /// The NVKMS line's whole information content is in `sub`: every call
    /// on /dev/nvidia-modeset carries the same ioctl number, so a trace
    /// that recorded only `nr` would be 451 identical lines.
    #[test]
    fn a_modeset_line_carries_the_command_out_of_the_indirection_struct() {
        let full = size_of::<sys::NvKmsIoctlParams>() as u32;
        assert_eq!(full, 16, "NvKmsIoctlParams is 16 bytes (nvrm-sys layout test)");

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

        // A caller that passed something shorter than the struct is not
        // read at all -- the rule the RM arms follow.
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
