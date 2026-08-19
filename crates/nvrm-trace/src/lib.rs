// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! An `LD_PRELOAD` tracer that observes the ioctl surface without changing
//! it.
//!
//! RM is NVIDIA's Resource Manager, the kernel driver behind
//! /dev/nvidiactl and /dev/nvidiaN; its ioctls are called escapes, and
//! UVM is its unified-memory driver (/dev/nvidia-uvm).
//!
//! Interposes open/openat/close/dup*/ioctl/mmap/read/poll and logs every
//! call on /dev/nvidia* as well as on FDs registered as an event channel
//! via `NV_ESC_ALLOC_OS_EVENT`. Nothing is rewritten; every call reaches
//! the real libc symbol with unmodified arguments.
//!
//! Rules on the interposed path:
//!   - no `std::io` (its initialization can re-enter these very hooks),
//!     logging goes through raw `write(2)`
//!   - no panic may unwind into the caller: every hook is `extern "C"`, and
//!     since Rust 1.81 a panic that reaches an `extern "C"` boundary aborts
//!     the process instead of unwinding (the toolchain is pinned to 1.89 in
//!     rust-toolchain.toml). A per-package `panic = "abort"` profile would
//!     be ignored by Cargo anyway (profiles count only in the workspace root).
//!   - resolve all symbols in the constructor, see `init()`

// C ABI exports of an LD_PRELOAD tracer: the safety contract of every
// function is that of the libc symbol it overrides -- a per-export
// "# Safety" prose block would be a transcript of the manpage.
#![allow(clippy::missing_safety_doc)]

mod fdtable;
mod log;

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_ulong, c_void};
use std::sync::OnceLock;

macro_rules! real {
    ($name:ident, $ty:ty) => {{
        static CELL: OnceLock<$ty> = OnceLock::new();
        *CELL.get_or_init(|| unsafe {
            let sym = concat!(stringify!($name), "\0");
            let p = libc::dlsym(libc::RTLD_NEXT, sym.as_ptr() as *const c_char);
            assert!(!p.is_null(), concat!("dlsym ", stringify!($name)));
            std::mem::transmute::<*mut c_void, $ty>(p)
        })
    }};
}

type FnIoctl = unsafe extern "C" fn(c_int, c_ulong, *mut c_void) -> c_int;
type FnOpen = unsafe extern "C" fn(*const c_char, c_int, libc::mode_t) -> c_int;
type FnOpenat = unsafe extern "C" fn(c_int, *const c_char, c_int, libc::mode_t) -> c_int;
type FnClose = unsafe extern "C" fn(c_int) -> c_int;
type FnDup = unsafe extern "C" fn(c_int) -> c_int;
type FnDup2 = unsafe extern "C" fn(c_int, c_int) -> c_int;
type FnDup3 = unsafe extern "C" fn(c_int, c_int, c_int) -> c_int;
type FnMmap =
    unsafe extern "C" fn(*mut c_void, usize, c_int, c_int, c_int, libc::off_t) -> *mut c_void;
type FnRead = unsafe extern "C" fn(c_int, *mut c_void, usize) -> isize;
type FnPoll = unsafe extern "C" fn(*mut libc::pollfd, libc::nfds_t, c_int) -> c_int;

// ---------------------------------------------------------------------------
// Device classification
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NvDev {
    Ctl,
    Gpu(u32),
    Uvm,
    UvmTools,
    /// Not a device but an FD that `NV_ESC_ALLOC_OS_EVENT` registered as an
    /// event channel. Not recognizable from the path - it is therefore
    /// added to the table when that ioctl is seen.
    Event,
    /// A DRM node: `true` for a render node (`/dev/dri/renderDN`), `false`
    /// for a card node (`/dev/dri/cardN`).
    ///
    /// Not an RM device, and nothing here decodes DRM structs - the escapes
    /// this crate knows are NVIDIA's, and a DRM ioctl carries a different
    /// ABI entirely. What it gets is the number, the size and the return
    /// value, which is enough to see WHICH call failed.
    ///
    /// It is traced at all because the question this tracer is pointed at
    /// spans both doors: NVIDIA's Vulkan WSI builds its swapchain over
    /// DRI3/Present, so the interesting call may be a
    /// `DRM_IOCTL_PRIME_HANDLE_TO_FD` on /dev/dri and not an escape on
    /// /dev/nvidia at all. Tracing only one of them answers "no RM call
    /// failed" and leaves the other half dark.
    Drm(bool),
}

fn classify(path: &CStr) -> Option<NvDev> {
    let s = path.to_str().ok()?;
    match s {
        "/dev/nvidiactl" => Some(NvDev::Ctl),
        "/dev/nvidia-uvm" => Some(NvDev::Uvm),
        "/dev/nvidia-uvm-tools" => Some(NvDev::UvmTools),
        _ => {
            if let Some(n) = s.strip_prefix("/dev/dri/card") {
                if n.parse::<u32>().is_ok() {
                    return Some(NvDev::Drm(false));
                }
            }
            if let Some(n) = s.strip_prefix("/dev/dri/renderD") {
                if n.parse::<u32>().is_ok() {
                    return Some(NvDev::Drm(true));
                }
            }
            s.strip_prefix("/dev/nvidia")
                .and_then(|n| n.parse::<u32>().ok())
                .map(NvDev::Gpu)
        }
    }
}

// ---------------------------------------------------------------------------
// ioctl
// ---------------------------------------------------------------------------

/// Rust cannot declare a variadic `ioctl` (`c_variadic` is nightly). Three
/// fixed arguments are reliable on Linux x86-64 and aarch64, because every
/// caller passes exactly one pointer.
#[no_mangle]
pub unsafe extern "C" fn ioctl(fd: c_int, req: c_ulong, arg: *mut c_void) -> c_int {
    let f = real!(ioctl, FnIoctl);
    let dev = fdtable::get(fd);

    // Sample the payload while it is still the caller's. Afterwards the
    // driver has overwritten status, flags and address, and for
    // NVOS33.flags in particular the input is the interesting value.
    if let Some(d) = dev {
        let cmd = req as u32;
        log::detail_pre(d, nvrm_abi::ioc_nr(cmd), nvrm_abi::ioc_size(cmd), arg);
    }

    let ret = f(fd, req, arg);

    if let Some(dev) = dev {
        let cmd = req as u32;
        match nvrm_abi::xfer::unwrap_xfer(cmd, arg) {
            // Note: unwrap_xfer returns the *unpacked* number without _IOC
            // encoding. decode() must not mask it a second time - hence the
            // separate log entry point.
            Some(Ok((real_nr, real_ptr, len))) => {
                log::ioctl_unpacked(dev, fd, real_nr, len as u32, ret, real_ptr)
            }
            // XFER with an unusable size: raw line only, no detail line.
            Some(Err(())) => log::ioctl(dev, fd, cmd, ret, arg),
            None => log::ioctl(dev, fd, cmd, ret, arg),
        }
        if ret == 0 {
            note_event_fd(dev, cmd, arg);
        }
    }
    ret
}

/// `NV_ESC_ALLOC_OS_EVENT` registers an FD as a notification channel.
/// Which kind of FD that is decides how completions have to be forwarded:
/// if the field names a `/dev/nvidia*` FD, it is already in the table; if
/// it names an eventfd, notifications have to be pumped across the VM
/// boundary separately.
///
/// The layout comes from bindgen
/// (`nv_ioctl_alloc_os_event_t { hClient, hDevice, fd, Status }` in
/// nv-ioctl.h), so the field name below is checked at compile time.
unsafe fn note_event_fd(dev: NvDev, cmd: u32, arg: *const c_void) {
    if arg.is_null() || matches!(dev, NvDev::Uvm | NvDev::UvmTools | NvDev::Event) {
        return;
    }
    if nvrm_abi::ioc_nr(cmd) != nvrm_abi::sys::NV_ESC_ALLOC_OS_EVENT {
        return;
    }
    let p = &*(arg as *const nvrm_abi::sys::nv_ioctl_alloc_os_event_t);
    let efd = p.fd as c_int;
    log::event_registered(efd, fdtable::get(efd));
    // Only record the FD if it is not already known - otherwise a
    // /dev/nvidia0 FD would be mislabelled as an event channel.
    if fdtable::get(efd).is_none() {
        fdtable::insert(efd, NvDev::Event);
    }
}

// ---------------------------------------------------------------------------
// open / close / dup
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    let fd = real!(open, FnOpen)(path, flags, mode);
    note_open(path, fd);
    fd
}

#[no_mangle]
pub unsafe extern "C" fn open64(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    let fd = real!(open64, FnOpen)(path, flags, mode);
    note_open(path, fd);
    fd
}

#[no_mangle]
pub unsafe extern "C" fn openat(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mode: libc::mode_t,
) -> c_int {
    let fd = real!(openat, FnOpenat)(dirfd, path, flags, mode);
    note_open(path, fd);
    fd
}

#[no_mangle]
pub unsafe extern "C" fn openat64(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mode: libc::mode_t,
) -> c_int {
    let fd = real!(openat64, FnOpenat)(dirfd, path, flags, mode);
    note_open(path, fd);
    fd
}

unsafe fn note_open(path: *const c_char, fd: c_int) {
    if fd < 0 || path.is_null() {
        return;
    }
    if let Some(dev) = classify(CStr::from_ptr(path)) {
        fdtable::insert(fd, dev);
        log::open(dev, fd);
    }
}

#[no_mangle]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    fdtable::remove(fd);
    real!(close, FnClose)(fd)
}

// dup/dup2/dup3 are the reason an FD table is needed and a path check does
// not suffice. CUDA is multithreaded and duplicates FDs; without tracking
// the duplicates, calls are lost from the trace unnoticed.
#[no_mangle]
pub unsafe extern "C" fn dup(oldfd: c_int) -> c_int {
    let newfd = real!(dup, FnDup)(oldfd);
    if newfd >= 0 {
        if let Some(d) = fdtable::get(oldfd) {
            fdtable::insert(newfd, d);
        }
    }
    newfd
}

#[no_mangle]
pub unsafe extern "C" fn dup2(oldfd: c_int, newfd: c_int) -> c_int {
    let r = real!(dup2, FnDup2)(oldfd, newfd);
    if r >= 0 {
        fdtable::remove(newfd);
        if let Some(d) = fdtable::get(oldfd) {
            fdtable::insert(r, d);
        }
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn dup3(oldfd: c_int, newfd: c_int, flags: c_int) -> c_int {
    let r = real!(dup3, FnDup3)(oldfd, newfd, flags);
    if r >= 0 {
        fdtable::remove(newfd);
        if let Some(d) = fdtable::get(oldfd) {
            fdtable::insert(r, d);
        }
    }
    r
}

// ---------------------------------------------------------------------------
// mmap
// ---------------------------------------------------------------------------
// On glibc/x86-64 mmap64 is a separate symbol. Hooking only mmap
// undercounts mappings - and the number of mappings is one-to-one the
// number of memory regions a forwarding backend has to manage.

unsafe fn mmap_common(
    f: FnMmap,
    addr: *mut c_void,
    len: usize,
    prot: c_int,
    flags: c_int,
    fd: c_int,
    off: libc::off_t,
) -> *mut c_void {
    let p = f(addr, len, prot, flags, fd, off);
    if let Some(dev) = fdtable::get(fd) {
        // Expectation on the RM frontend: off == 0, the offset refers to a
        // context created by NV_ESC_RM_MAP_MEMORY. UVM instead encodes the
        // VA range there, so a non-zero offset is normal for UVM.
        log::mmap(dev, fd, len, off, p);
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn mmap(
    a: *mut c_void, l: usize, p: c_int, f: c_int, fd: c_int, o: libc::off_t,
) -> *mut c_void {
    mmap_common(real!(mmap, FnMmap), a, l, p, f, fd, o)
}

#[no_mangle]
pub unsafe extern "C" fn mmap64(
    a: *mut c_void, l: usize, p: c_int, f: c_int, fd: c_int, o: libc::off_t,
) -> *mut c_void {
    mmap_common(real!(mmap64, FnMmap), a, l, p, f, fd, o)
}

// ---------------------------------------------------------------------------
// Wait path
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize {
    let f = real!(read, FnRead);
    let dev = fdtable::get(fd);
    let r = f(fd, buf, n);
    if let Some(d) = dev {
        log::wait("read", d, fd, r as i64);
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn poll(fds: *mut libc::pollfd, n: libc::nfds_t, to: c_int) -> c_int {
    let f = real!(poll, FnPoll);
    let r = f(fds, n, to);
    if !fds.is_null() {
        // One line per FD in the array, not per syscall - keep that in mind
        // when evaluating, otherwise the count is array size times calls.
        for i in 0..n as usize {
            let pf = &*fds.add(i);
            if let Some(d) = fdtable::get(pf.fd) {
                log::wait("poll", d, pf.fd, pf.revents as i64);
            }
        }
    }
    r
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

/// No `ctor` crate: a function pointer in `.init_array` does the same and
/// saves a dependency on the interposed path.
///
/// All symbols are resolved here, before any hook can fire. Otherwise the
/// following happens: the first `read` calls `dlsym`, `dlsym` internally
/// calls `read`, and the hook recurses into its own still-running
/// `OnceLock` initialization -- a deadlock, or a wordless abort at program
/// start.
unsafe extern "C" fn init() {
    let _ = real!(ioctl, FnIoctl);
    let _ = real!(open, FnOpen);
    let _ = real!(open64, FnOpen);
    let _ = real!(openat, FnOpenat);
    let _ = real!(openat64, FnOpenat);
    let _ = real!(close, FnClose);
    let _ = real!(dup, FnDup);
    let _ = real!(dup2, FnDup2);
    let _ = real!(dup3, FnDup3);
    let _ = real!(mmap, FnMmap);
    let _ = real!(mmap64, FnMmap);
    let _ = real!(read, FnRead);
    let _ = real!(poll, FnPoll);
    log::init();
}

#[used]
#[link_section = ".init_array"]
static INIT: unsafe extern "C" fn() = init;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
// NOTE for anyone extending these: the `#[no_mangle] extern "C"` hooks above
// are linked into the test binary too, so inside it they interpose libc for
// the harness itself. A test that opens a file or issues an ioctl therefore
// runs the tracer on its own process. Keep the tests on the pure helpers --
// the hooks are exercised in `probe/run/`, against a real driver.

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly which paths enter the FD table, and -- just as important --
    /// which do not. `classify` is the only gate: a false positive puts a
    /// foreign FD on the decoding path, where `log::subcode` reads NVIDIA
    /// parameter structs out of a buffer that is not one; a false negative
    /// drops a whole device from the trace without a word.
    #[test]
    fn classify_recognizes_the_nvidia_and_drm_nodes_and_nothing_else() {
        let cases: &[(&str, Option<NvDev>)] = &[
            ("/dev/nvidiactl", Some(NvDev::Ctl)),
            ("/dev/nvidia0", Some(NvDev::Gpu(0))),
            ("/dev/nvidia7", Some(NvDev::Gpu(7))),
            ("/dev/nvidia-uvm", Some(NvDev::Uvm)),
            ("/dev/nvidia-uvm-tools", Some(NvDev::UvmTools)),
            ("/dev/dri/card1", Some(NvDev::Drm(false))),
            ("/dev/dri/renderD128", Some(NvDev::Drm(true))),
            // The modeset node is not an RM device and carries neither the
            // frontend ABI nor a minor number in its name.
            ("/dev/nvidia-modeset", None),
            // Trailing junk must not be truncated into a minor number: the
            // suffix is parsed whole, so "ctl2" is not GPU 2.
            ("/dev/nvidiactl2", None),
            ("/dev/null", None),
            // Same on the DRM side -- the number decides, not the prefix.
            ("/dev/dri/cardX", None),
        ];
        for (path, want) in cases {
            let c = std::ffi::CString::new(*path).unwrap();
            assert_eq!(classify(&c), *want, "classify({path})");
        }
    }
}
