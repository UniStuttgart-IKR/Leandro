// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! NV_ESC_IOCTL_XFER_CMD - the special case that breaks the assumption
//! "the size is in the ioctl number".
//!
//! RM is NVIDIA's Resource Manager, the kernel driver behind
//! /dev/nvidiactl and /dev/nvidiaN; its ioctls are called escapes and
//! `NV_ESC_*` are their numbers.
//!
//! From kernel-open/nvidia/nv.c:
//!
//! ```text
//! if (arg_cmd == NV_ESC_IOCTL_XFER_CMD) {
//!     copy_from_user(&ioc_xfer, arg_ptr, sizeof(ioc_xfer));
//!     arg_cmd  = ioc_xfer.cmd;      // the *real* ioctl number
//!     arg_size = ioc_xfer.size;     // the *real* size
//!     arg_ptr  = ioc_xfer.ptr;      // a second user pointer
//! }
//! ```
//!
//! Consequence for anyone intercepting or forwarding ioctls: dispatch must
//! not trust `_IOC_SIZE(cmd)`. XFER_CMD has to be resolved *before* the
//! handler table, otherwise only 16 bytes get copied instead of the real
//! payload, and the mistake surfaces much later as garbage data.
//!
//! This affects large RM_CONTROLs (the reason NVIDIA built it):
//! `_IOC_SIZE` has only 14 bits, i.e. at most 16383 bytes.

use crate::sys;

/// Maximum size the driver accepts (NV_ABSOLUTE_MAX_IOCTL_SIZE).
/// The host daemon uses it as a sanity limit against malicious guests.
pub const ABSOLUTE_MAX_IOCTL_SIZE: usize = 16384;

/// Size at which the userspace driver *must* fall back to XFER_CMD.
pub const IOC_SIZE_LIMIT: usize = (1 << crate::IOC_SIZEBITS) - 1;

/// Resolves a possibly wrapped ioctl into (real_nr, ptr, len).
///
/// A return of `None` means: no XFER, `cmd` and the original pointer apply
/// unchanged.
///
/// `Some(Err(()))` means: XFER, but the size is unusable. The caller must
/// then fail with EINVAL and must not build a slice — `size` is an NvU32
/// coming from the guest, and unchecked it would produce a 4 GiB read past
/// the end of the application's buffer. The driver rejects the same input
/// with EINVAL (`nv.c`, limit NV_ABSOLUTE_MAX_IOCTL_SIZE).
///
/// # Safety
/// `arg` must point to a valid `nv_ioctl_xfer_t` when `cmd` is XFER_CMD.
pub unsafe fn unwrap_xfer(
    cmd: u32,
    arg: *const libc::c_void,
) -> Option<Result<(u32, *mut libc::c_void, usize), ()>> {
    if crate::ioc_nr(cmd) != sys::NV_ESC_IOCTL_XFER_CMD {
        return None;
    }
    if arg.is_null() {
        return Some(Err(()));
    }
    let x = &*(arg as *const sys::nv_ioctl_xfer_t);
    let size = x.size as usize;
    if size == 0 || size > ABSOLUTE_MAX_IOCTL_SIZE || x.ptr.is_null() {
        return Some(Err(()));
    }
    Some(Ok((x.cmd, x.ptr as *mut libc::c_void, size)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapping ioctl number as userspace sends it: nr
    /// `NV_ESC_IOCTL_XFER_CMD`, size `sizeof(nv_ioctl_xfer_t)`.
    fn xfer_cmd() -> u32 {
        crate::iowr_raw(
            sys::NV_ESC_IOCTL_XFER_CMD,
            core::mem::size_of::<sys::nv_ioctl_xfer_t>() as u32,
        )
    }

    fn wrapper(cmd: u32, size: u32, ptr: *mut libc::c_void) -> sys::nv_ioctl_xfer_t {
        sys::nv_ioctl_xfer_t { cmd, size, ptr }
    }

    #[allow(clippy::type_complexity)]
    fn call(
        cmd: u32,
        x: &sys::nv_ioctl_xfer_t,
    ) -> Option<Result<(u32, *mut libc::c_void, usize), ()>> {
        // SAFETY: `x` is a live nv_ioctl_xfer_t on this thread's stack.
        unsafe { unwrap_xfer(cmd, x as *const sys::nv_ioctl_xfer_t as *const libc::c_void) }
    }

    /// Everything that is not the XFER escape must pass through untouched
    /// -- `None` means "use `cmd` and the original pointer unchanged". A
    /// false positive here would make the caller read a 16-byte wrapper out
    /// of an ordinary parameter block.
    #[test]
    fn an_ordinary_command_is_not_unwrapped() {
        let mut payload = [0u8; 32];
        let x = wrapper(sys::NV_ESC_RM_CONTROL, 32, payload.as_mut_ptr().cast());
        for nr in [sys::NV_ESC_RM_CONTROL, sys::NV_ESC_RM_ALLOC, sys::NV_ESC_RM_FREE, 0] {
            assert!(call(crate::iowr_raw(nr, 32), &x).is_none(), "nr {nr:#x}");
        }
    }

    /// The XFER escape with a NULL argument: `Some(Err(()))`, i.e. "this
    /// IS an XFER and it is unusable". The distinction from `None` matters
    /// -- `None` would send the caller off to dereference the same null
    /// pointer as an ordinary payload.
    #[test]
    fn a_null_argument_is_reported_as_a_broken_xfer() {
        // SAFETY: the null case must be decided before any dereference,
        // which is exactly what this call checks.
        let got = unsafe { unwrap_xfer(xfer_cmd(), core::ptr::null()) };
        assert_eq!(got, Some(Err(())));
    }

    /// A valid wrapper resolves to the REAL ioctl number, the second user
    /// pointer and the real size. Trusting `_IOC_SIZE(cmd)` instead would
    /// copy 16 bytes (the wrapper) where the payload may be up to 16 KiB,
    /// and the mistake would surface much later as garbage data.
    #[test]
    fn a_valid_wrapper_yields_the_inner_command_pointer_and_size() {
        let mut payload = [0u8; 64];
        let p: *mut libc::c_void = payload.as_mut_ptr().cast();
        let x = wrapper(sys::NV_ESC_RM_CONTROL, 64, p);
        assert_eq!(call(xfer_cmd(), &x), Some(Ok((sys::NV_ESC_RM_CONTROL, p, 64))));

        // The largest size the driver accepts (NV_ABSOLUTE_MAX_IOCTL_SIZE)
        // is still valid -- the bound is inclusive.
        let x = wrapper(sys::NV_ESC_RM_CONTROL, ABSOLUTE_MAX_IOCTL_SIZE as u32, p);
        assert_eq!(
            call(xfer_cmd(), &x),
            Some(Ok((sys::NV_ESC_RM_CONTROL, p, ABSOLUTE_MAX_IOCTL_SIZE)))
        );
    }

    /// The three ways a wrapper can be unusable. `size` is an NvU32 that
    /// comes straight from the guest: unchecked, `u32::MAX` would produce a
    /// 4 GiB read past the end of the application's buffer. The driver
    /// rejects the same input with EINVAL.
    #[test]
    fn an_unusable_wrapper_is_rejected_rather_than_clamped() {
        let mut payload = [0u8; 64];
        let p: *mut libc::c_void = payload.as_mut_ptr().cast();

        // (1) size 0 -- nothing to copy, and a zero-length slice would hide
        //     a caller bug rather than report it.
        assert_eq!(call(xfer_cmd(), &wrapper(sys::NV_ESC_RM_CONTROL, 0, p)), Some(Err(())));
        // (2) size past the driver's own limit, including the extreme.
        for size in [ABSOLUTE_MAX_IOCTL_SIZE as u32 + 1, 0x10_0000, u32::MAX] {
            assert_eq!(
                call(xfer_cmd(), &wrapper(sys::NV_ESC_RM_CONTROL, size, p)),
                Some(Err(())),
                "size {size}"
            );
        }
        // (3) the inner pointer is NULL.
        assert_eq!(
            call(xfer_cmd(), &wrapper(sys::NV_ESC_RM_CONTROL, 64, core::ptr::null_mut())),
            Some(Err(()))
        );
    }

    /// Only the nr decides, not the size encoded in the wrapping number:
    /// a caller that sends the XFER escape with an unexpected `_IOC_SIZE`
    /// still gets it unwrapped, because the payload it points at is what
    /// counts.
    #[test]
    fn the_wrapper_is_recognised_by_its_nr() {
        let mut payload = [0u8; 64];
        let p: *mut libc::c_void = payload.as_mut_ptr().cast();
        let x = wrapper(sys::NV_ESC_RM_ALLOC, 48, p);
        assert_eq!(
            call(crate::iowr_raw(sys::NV_ESC_IOCTL_XFER_CMD, 0), &x),
            Some(Ok((sys::NV_ESC_RM_ALLOC, p, 48)))
        );
    }
}
