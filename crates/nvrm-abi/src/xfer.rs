// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Unwrap `NV_ESC_IOCTL_XFER_CMD` before decoding a frontend payload.
//!
//! `kernel-open/nvidia/nv.c` replaces the outer request number, size and
//! pointer with `nv_ioctl_xfer_t` fields. `_IOC_SIZE` describes only that
//! 16-byte wrapper; the inner payload can exceed the 14-bit size limit.

use crate::sys;

/// Maximum size the driver accepts (NV_ABSOLUTE_MAX_IOCTL_SIZE).
/// The host daemon uses it as a sanity limit against malicious guests.
pub const ABSOLUTE_MAX_IOCTL_SIZE: usize = 16384;

/// Size at which the userspace driver *must* fall back to XFER_CMD.
pub const IOC_SIZE_LIMIT: usize = (1 << crate::IOC_SIZEBITS) - 1;

/// Resolve a wrapped ioctl into `(real_nr, ptr, len)`.
///
/// `None` leaves an ordinary ioctl unchanged. `Some(Err(()))` rejects a null
/// pointer, zero length or length above `NV_ABSOLUTE_MAX_IOCTL_SIZE`; callers
/// must not build a slice from a rejected wrapper.
///
/// # Safety
/// For XFER_CMD, a non-null `arg` must point to an aligned, initialized
/// `nv_ioctl_xfer_t` that remains readable for this call. Success validates
/// the inner length, not whether the inner pointer is accessible.
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

    /// An ordinary parameter block must never be read as an XFER wrapper.
    #[test]
    fn an_ordinary_command_is_not_unwrapped() {
        let mut payload = [0u8; 32];
        let x = wrapper(sys::NV_ESC_RM_CONTROL, 32, payload.as_mut_ptr().cast());
        for nr in [
            sys::NV_ESC_RM_CONTROL,
            sys::NV_ESC_RM_ALLOC,
            sys::NV_ESC_RM_FREE,
            0,
        ] {
            assert!(call(crate::iowr_raw(nr, 32), &x).is_none(), "nr {nr:#x}");
        }
    }

    /// A null XFER argument is invalid, not an ordinary ioctl.
    #[test]
    fn a_null_argument_is_reported_as_a_broken_xfer() {
        // SAFETY: the null case must be decided before any dereference,
        // which is exactly what this call checks.
        let got = unsafe { unwrap_xfer(xfer_cmd(), core::ptr::null()) };
        assert_eq!(got, Some(Err(())));
    }

    /// Use the inner command, pointer and size rather than the wrapper size.
    #[test]
    fn a_valid_wrapper_yields_the_inner_command_pointer_and_size() {
        let mut payload = [0u8; 64];
        let p: *mut libc::c_void = payload.as_mut_ptr().cast();
        let x = wrapper(sys::NV_ESC_RM_CONTROL, 64, p);
        assert_eq!(
            call(xfer_cmd(), &x),
            Some(Ok((sys::NV_ESC_RM_CONTROL, p, 64)))
        );

        // The largest size the driver accepts (NV_ABSOLUTE_MAX_IOCTL_SIZE)
        // is still valid; the bound is inclusive.
        let x = wrapper(sys::NV_ESC_RM_CONTROL, ABSOLUTE_MAX_IOCTL_SIZE as u32, p);
        assert_eq!(
            call(xfer_cmd(), &x),
            Some(Ok((sys::NV_ESC_RM_CONTROL, p, ABSOLUTE_MAX_IOCTL_SIZE)))
        );
    }

    /// Reject invalid lengths and null inner pointers before constructing a slice.
    #[test]
    fn an_unusable_wrapper_is_rejected_rather_than_clamped() {
        let mut payload = [0u8; 64];
        let p: *mut libc::c_void = payload.as_mut_ptr().cast();

        // (1) size 0; nothing to copy, and a zero-length slice would hide
        //     a caller bug rather than report it.
        assert_eq!(
            call(xfer_cmd(), &wrapper(sys::NV_ESC_RM_CONTROL, 0, p)),
            Some(Err(()))
        );
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
            call(
                xfer_cmd(),
                &wrapper(sys::NV_ESC_RM_CONTROL, 64, core::ptr::null_mut())
            ),
            Some(Err(()))
        );
    }

    /// The escape number identifies XFER even if its encoded size is zero.
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
