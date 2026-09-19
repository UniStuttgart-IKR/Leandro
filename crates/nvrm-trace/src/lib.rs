// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! `LD_PRELOAD` tracing for NVIDIA RM/UVM, NVKMS, DRM and RM event FDs.
//! Hooks forward libc arguments unchanged and preserve errno across logging.
//! Raw writes avoid re-entering hooks through `std::io` initialization.
//! Payload decoding assumes readable caller buffers and the build's NVIDIA ABI.
//! Logging allocates; the complete hook path is not async-signal-safe.

// Each export has the safety contract of the libc symbol it interposes.
// A panic at an extern "C" boundary aborts instead of unwinding into C.
#![allow(clippy::missing_safety_doc)]

mod fdtable;
mod log;

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_ulong, c_void};
use std::sync::OnceLock;

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

// Each symbol has one cache shared by its hook and the constructor.
mod real {
    use super::*;

    macro_rules! symbols {
        ($($name:ident: $ty:ty),+ $(,)?) => {$(
            pub fn $name() -> $ty {
                static CELL: OnceLock<$ty> = OnceLock::new();
                *CELL.get_or_init(|| unsafe {
                    let name = concat!(stringify!($name), "\0");
                    let p = libc::dlsym(libc::RTLD_NEXT, name.as_ptr().cast());
                    assert!(!p.is_null(), concat!("dlsym ", stringify!($name)));
                    std::mem::transmute::<*mut c_void, $ty>(p)
                })
            }
        )+};
    }

    symbols! {
        ioctl: FnIoctl,
        open: FnOpen,
        open64: FnOpen,
        openat: FnOpenat,
        openat64: FnOpenat,
        close: FnClose,
        dup: FnDup,
        dup2: FnDup2,
        dup3: FnDup3,
        mmap: FnMmap,
        mmap64: FnMmap,
        read: FnRead,
        poll: FnPoll,
    }
}

/// Restore the caller's errno after tracer I/O or allocation.
struct ErrnoGuard(c_int);

impl ErrnoGuard {
    fn new() -> Self {
        Self(unsafe { *libc::__errno_location() })
    }
}

impl Drop for ErrnoGuard {
    fn drop(&mut self) {
        unsafe { *libc::__errno_location() = self.0 };
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum NvDev {
    Ctl,
    Gpu,
    Uvm,
    UvmTools,
    /// FD registered through NV_ESC_ALLOC_OS_EVENT; no RM payload decoding.
    Event,
    /// DRM render node (`true`) or card node (`false`); no payload decoding.
    Drm(bool),
    /// NVKMS: command and size come from NvKmsIoctlParams, not RM structs.
    Modeset,
}

fn classify(path: &CStr) -> Option<NvDev> {
    let s = path.to_str().ok()?;
    match s {
        "/dev/nvidiactl" => Some(NvDev::Ctl),
        "/dev/nvidia-uvm" => Some(NvDev::Uvm),
        "/dev/nvidia-uvm-tools" => Some(NvDev::UvmTools),
        "/dev/nvidia-modeset" => Some(NvDev::Modeset),
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
                .map(|_| NvDev::Gpu)
        }
    }
}

// ioctl

/// Fixed arguments match the pointer-taking NVIDIA ioctls on Linux x86-64/aarch64.
#[no_mangle]
pub unsafe extern "C" fn ioctl(fd: c_int, req: c_ulong, arg: *mut c_void) -> c_int {
    let f = real::ioctl();
    let dev = fdtable::get(fd);
    if let Some(dev) = dev {
        let _errno = ErrnoGuard::new();
        log::detail_pre(dev, req as u32, arg);
    }

    let ret = f(fd, req, arg);
    let _errno = ErrnoGuard::new();
    if let Some(dev) = dev {
        let (nr, size, arg) = ioctl_payload(dev, req as u32, arg);
        log::ioctl(dev, fd, nr, size, ret, arg);
        if ret == 0 {
            note_event_fd(dev, nr, size, arg);
        }
    }
    ret
}

/// Unwrap only RM traffic. Other devices may use the same ioctl number.
unsafe fn ioctl_payload(dev: NvDev, cmd: u32, arg: *mut c_void) -> (u32, u32, *mut c_void) {
    if matches!(dev, NvDev::Ctl | NvDev::Gpu) {
        if let Some(Ok((nr, ptr, len))) = nvrm_abi::xfer::unwrap_xfer(cmd, arg) {
            return (nr, len as u32, ptr);
        }
    }
    let (nr, size) = log::decode(dev, cmd);
    (nr, size, arg)
}

/// Record successful RM event registration without replacing a known device tag.
unsafe fn note_event_fd(dev: NvDev, nr: u32, size: u32, arg: *const c_void) {
    if !matches!(dev, NvDev::Ctl | NvDev::Gpu)
        || nr != nvrm_abi::sys::NV_ESC_ALLOC_OS_EVENT
        || (size as usize) < size_of::<nvrm_abi::sys::nv_ioctl_alloc_os_event_t>()
        || arg.is_null()
    {
        return;
    }
    let p = (arg as *const nvrm_abi::sys::nv_ioctl_alloc_os_event_t).read_unaligned();
    if p.Status != 0 {
        return;
    }
    let efd = p.fd as c_int;
    let prev = fdtable::get(efd);
    log::event_registered(efd, prev);
    if prev.is_none() {
        fdtable::insert(efd, NvDev::Event);
    }
}

// open / close / dup

#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    let fd = real::open()(path, flags, mode);
    let _errno = ErrnoGuard::new();
    note_open(path, fd);
    fd
}

#[no_mangle]
pub unsafe extern "C" fn open64(path: *const c_char, flags: c_int, mode: libc::mode_t) -> c_int {
    let fd = real::open64()(path, flags, mode);
    let _errno = ErrnoGuard::new();
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
    let fd = real::openat()(dirfd, path, flags, mode);
    let _errno = ErrnoGuard::new();
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
    let fd = real::openat64()(dirfd, path, flags, mode);
    let _errno = ErrnoGuard::new();
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
    real::close()(fd)
}

// Duplicated descriptors retain their source device tag.
#[no_mangle]
pub unsafe extern "C" fn dup(oldfd: c_int) -> c_int {
    let newfd = real::dup()(oldfd);
    if newfd >= 0 {
        fdtable::duplicate(oldfd, newfd);
    }
    newfd
}

#[no_mangle]
pub unsafe extern "C" fn dup2(oldfd: c_int, newfd: c_int) -> c_int {
    let r = real::dup2()(oldfd, newfd);
    if r >= 0 {
        fdtable::duplicate(oldfd, r);
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn dup3(oldfd: c_int, newfd: c_int, flags: c_int) -> c_int {
    let r = real::dup3()(oldfd, newfd, flags);
    if r >= 0 {
        fdtable::duplicate(oldfd, r);
    }
    r
}

// mmap
// glibc exposes mmap64 separately from mmap.

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
    let _errno = ErrnoGuard::new();
    if let Some(dev) = fdtable::get(fd) {
        // RM uses a pending map context at offset 0; UVM encodes the VA.
        log::mmap(dev, fd, len, off, p);
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn mmap(
    a: *mut c_void,
    l: usize,
    p: c_int,
    f: c_int,
    fd: c_int,
    o: libc::off_t,
) -> *mut c_void {
    mmap_common(real::mmap(), a, l, p, f, fd, o)
}

#[no_mangle]
pub unsafe extern "C" fn mmap64(
    a: *mut c_void,
    l: usize,
    p: c_int,
    f: c_int,
    fd: c_int,
    o: libc::off_t,
) -> *mut c_void {
    mmap_common(real::mmap64(), a, l, p, f, fd, o)
}

// Wait path

#[no_mangle]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize {
    let f = real::read();
    let dev = fdtable::get(fd);
    let r = f(fd, buf, n);
    let _errno = ErrnoGuard::new();
    if let Some(d) = dev {
        log::wait("read", d, fd, r as i64);
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn poll(fds: *mut libc::pollfd, n: libc::nfds_t, to: c_int) -> c_int {
    let f = real::poll();
    let r = f(fds, n, to);
    let _errno = ErrnoGuard::new();
    if r >= 0 && !fds.is_null() {
        // One record per tracked FD, including timeout results.
        for i in 0..n as usize {
            let pf = &*fds.add(i);
            if let Some(d) = fdtable::get(pf.fd) {
                log::wait("poll", d, pf.fd, pf.revents as i64);
            }
        }
    }
    r
}

// Constructor

/// Warm the shared symbol caches before configuring logging.
/// Hooks invoked before this constructor still resolve lazily.
unsafe extern "C" fn init() {
    let _errno = ErrnoGuard::new();
    let _ = real::ioctl();
    let _ = real::open();
    let _ = real::open64();
    let _ = real::openat();
    let _ = real::openat64();
    let _ = real::close();
    let _ = real::dup();
    let _ = real::dup2();
    let _ = real::dup3();
    let _ = real::mmap();
    let _ = real::mmap64();
    let _ = real::read();
    let _ = real::poll();
    log::init();
}

#[used]
#[link_section = ".init_array"]
static INIT: unsafe extern "C" fn() = init;

unsafe extern "C" fn fini() {
    let _errno = ErrnoGuard::new();
    log::report_losses();
}

#[used]
#[link_section = ".fini_array"]
static FINI: unsafe extern "C" fn() = fini;

// Tests
// Tests also link the interposed symbols. Use reserved FD slots for metadata tests.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_rm_payloads_are_not_unwrapped_as_xfer() {
        let cmd = nvrm_abi::iowr_raw(nvrm_abi::sys::NV_ESC_IOCTL_XFER_CMD, 0);
        // No readable wrapper exists here. Non-RM decoding must leave it alone.
        let arg = std::ptr::dangling_mut::<c_void>();
        for dev in [
            NvDev::Uvm,
            NvDev::UvmTools,
            NvDev::Drm(false),
            NvDev::Event,
            NvDev::Modeset,
        ] {
            let (nr, size) = log::decode(dev, cmd);
            assert_eq!(unsafe { ioctl_payload(dev, cmd, arg) }, (nr, size, arg));
        }
    }

    #[test]
    fn rm_xfer_uses_the_inner_command_and_buffer() {
        let mut data = [0_u8; 32];
        let mut wrapper = nvrm_abi::sys::nv_ioctl_xfer_t {
            cmd: nvrm_abi::sys::NV_ESC_RM_CONTROL,
            size: data.len() as u32,
            ptr: data.as_mut_ptr().cast(),
        };
        let cmd = nvrm_abi::iowr_raw(
            nvrm_abi::sys::NV_ESC_IOCTL_XFER_CMD,
            size_of_val(&wrapper) as u32,
        );
        assert_eq!(
            unsafe { ioctl_payload(NvDev::Ctl, cmd, std::ptr::from_mut(&mut wrapper).cast()) },
            (wrapper.cmd, wrapper.size, data.as_mut_ptr().cast())
        );
    }

    #[test]
    fn only_successful_complete_event_registrations_change_the_fd_table() {
        const FD: i32 = 61_000;
        let mut event: nvrm_abi::sys::nv_ioctl_alloc_os_event_t = unsafe { std::mem::zeroed() };
        event.fd = FD as u32;
        event.Status = 1;
        let nr = nvrm_abi::sys::NV_ESC_ALLOC_OS_EVENT;
        let size = size_of_val(&event) as u32;
        unsafe { note_event_fd(NvDev::Ctl, nr, size, std::ptr::from_ref(&event).cast()) };
        assert_eq!(fdtable::get(FD), None);

        event.Status = 0;
        unsafe { note_event_fd(NvDev::Ctl, nr, size - 1, std::ptr::from_ref(&event).cast()) };
        assert_eq!(fdtable::get(FD), None);
        unsafe {
            note_event_fd(
                NvDev::Drm(false),
                nr,
                size,
                std::ptr::from_ref(&event).cast(),
            )
        };
        assert_eq!(fdtable::get(FD), None);

        unsafe { note_event_fd(NvDev::Ctl, nr, size, std::ptr::from_ref(&event).cast()) };
        assert_eq!(fdtable::get(FD), Some(NvDev::Event));
        fdtable::insert(FD, NvDev::Gpu);
        unsafe { note_event_fd(NvDev::Ctl, nr, size, std::ptr::from_ref(&event).cast()) };
        assert_eq!(fdtable::get(FD), Some(NvDev::Gpu));
        fdtable::remove(FD);
    }

    #[test]
    fn a_failed_poll_does_not_read_the_invalid_caller_array() {
        let _errno = ErrnoGuard::new();
        let fds = std::ptr::dangling_mut::<libc::pollfd>();
        assert_eq!(unsafe { poll(fds, 1, 0) }, -1);
        assert_eq!(unsafe { *libc::__errno_location() }, libc::EFAULT);
    }

    #[test]
    fn classify_recognizes_the_nvidia_and_drm_nodes_and_nothing_else() {
        let cases: &[(&str, Option<NvDev>)] = &[
            ("/dev/nvidiactl", Some(NvDev::Ctl)),
            ("/dev/nvidia0", Some(NvDev::Gpu)),
            ("/dev/nvidia7", Some(NvDev::Gpu)),
            ("/dev/nvidia-uvm", Some(NvDev::Uvm)),
            ("/dev/nvidia-uvm-tools", Some(NvDev::UvmTools)),
            ("/dev/dri/card1", Some(NvDev::Drm(false))),
            ("/dev/dri/renderD128", Some(NvDev::Drm(true))),
            // NVKMS uses a separate ioctl ABI.
            ("/dev/nvidia-modeset", Some(NvDev::Modeset)),
            // Parse the entire numeric suffix.
            ("/dev/nvidiactl2", None),
            ("/dev/null", None),
            ("/dev/dri/cardX", None),
        ];
        for (path, want) in cases {
            let c = std::ffi::CString::new(*path).unwrap();
            assert_eq!(classify(&c), *want, "classify({path})");
        }
    }
}
