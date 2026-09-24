// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! NVIDIA RM ioctl encoding, device FDs and shared ABI definitions.
//!
//! `nvgpu` supplies layouts and bitfields; `xfer` unwraps large requests;
//! `xlate` describes forwarding layouts; `table` serializes them for the guest.
//! `mediate` records rewritten fields, `vgpu` defines profiles, `naming` derives
//! the guest-visible GPU name from a `VirtualGpuSpec`, and `share`
//! scopes RM object duplication. Raw bindings are re-exported as [`sys`].
//! Session and object ownership live in `nvrm-client`.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

pub mod mediate;
pub mod naming;
pub mod nvgpu;
pub mod share;
pub mod table;
pub mod vgpu;
pub mod xfer;
pub mod xlate;

pub use nvrm_sys as sys;

// _IOC encoding
// Written out by hand: the Linux macros live in asm-generic/ioctl.h, and
// bindgen emits nothing for function-like macros.

pub const IOC_NRBITS: u32 = 8;
pub const IOC_TYPEBITS: u32 = 8;
pub const IOC_SIZEBITS: u32 = 14;

pub const IOC_NRSHIFT: u32 = 0;
pub const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
pub const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
pub const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

pub const IOC_NONE: u32 = 0;
pub const IOC_WRITE: u32 = 1;
pub const IOC_READ: u32 = 2;

#[inline]
pub const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    (dir << IOC_DIRSHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT)
}

#[inline]
pub const fn iowr<T>(nr: u32) -> u32 {
    ioc(
        IOC_READ | IOC_WRITE,
        sys::NV_IOCTL_MAGIC as u32,
        nr,
        core::mem::size_of::<T>() as u32,
    )
}

/// Encode a frontend ioctl with a runtime byte size.
/// UVM uses raw request numbers and must not use this encoder.
#[inline]
pub const fn iowr_raw(nr: u32, size: u32) -> u32 {
    ioc(IOC_READ | IOC_WRITE, sys::NV_IOCTL_MAGIC as u32, nr, size)
}

#[inline]
pub const fn ioc_nr(cmd: u32) -> u32 {
    (cmd >> IOC_NRSHIFT) & ((1 << IOC_NRBITS) - 1)
}
#[inline]
pub const fn ioc_size(cmd: u32) -> u32 {
    (cmd >> IOC_SIZESHIFT) & ((1 << IOC_SIZEBITS) - 1)
}
#[inline]
pub const fn ioc_type(cmd: u32) -> u32 {
    (cmd >> IOC_TYPESHIFT) & ((1 << IOC_TYPEBITS) - 1)
}

/// Does this cmd belong to the NVIDIA frontend?
#[inline]
pub const fn is_nv_cmd(cmd: u32) -> bool {
    ioc_type(cmd) == sys::NV_IOCTL_MAGIC as u32
}

// Doorbell
// From swref/published/turing/tu102/dev_vm.h and class/clc361.h.
// Not via bindgen: dev_vm.h consists almost entirely of DRF macros.
pub mod doorbell {
    /// Offset of the doorbell register inside the VOLTA_USERMODE_A page.
    /// == NVC361_NOTIFY_CHANNEL_PENDING. Identical Turing..Blackwell.
    pub const NOTIFY_CHANNEL_PENDING: usize = 0x90;
    /// Size of the usermode mapping (NVC361_NV_USERMODE__SIZE).
    pub const USERMODE_SIZE: usize = 65536;
    /// BAR0 offset, documentation only - userspace never sees it.
    pub const BAR0_VIRTUAL_FUNCTION_DOORBELL: u32 = 0x30090;
}

// Errors

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ioctl {nr:#x} failed: {source}")]
    Ioctl {
        nr: u32,
        #[source]
        source: std::io::Error,
    },
    /// The ioctl succeeded, but its RM status reports failure.
    #[error("RM status {status:#x} ({}) on ioctl {nr:#x}",
            status_name(*status).unwrap_or("unknown"))]
    Rm { nr: u32, status: u32 },
    #[error("open {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

// Device FD

/// An open NVIDIA device FD. Each mapping context needs a fresh FD;
/// see [`NvDevice::open_for_mapping`].
#[derive(Debug)]
pub struct NvDevice {
    fd: OwnedFd,
    path: String,
}

impl NvDevice {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            Error::Open {
                path: path.display().to_string(),
                source: std::io::Error::other(e),
            }
        })?;
        // Do not inherit RM state across exec.
        let raw = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(Error::Open {
                path: path.display().to_string(),
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
            path: path.display().to_string(),
        })
    }

    pub fn open_ctl() -> Result<Self> {
        Self::open("/dev/nvidiactl")
    }

    pub fn open_gpu(index: u32) -> Result<Self> {
        Self::open(format!("/dev/nvidia{index}"))
    }

    /// Open a fresh mapping FD and register it against `ctl`.
    /// This follows libcuda's per-GPU open/REGISTER_FD sequence; the new FD has
    /// its own mmap context.
    pub fn open_for_mapping(&self, ctl: &NvDevice) -> Result<Self> {
        let fd = Self::open(&self.path)?;
        fd.register_fd(ctl)?;
        Ok(fd)
    }

    /// Bind this FD to a client's ctl FD with `NV_ESC_REGISTER_FD`.
    /// Errors are reported through errno; the request has no RM status field.
    pub fn register_fd(&self, ctl: &NvDevice) -> Result<()> {
        let mut p = nvgpu::IoctlRegisterFd {
            ctl_fd: ctl.as_raw_fd(),
        };
        unsafe { self.ioctl_raw(nvgpu::NV_ESC_REGISTER_FD, &mut p) }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// A second FD for the same open file (`dup`): same RM client binding, own
    /// lifetime. Lets a holder issue escapes without borrowing the owner.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            fd: self.fd.try_clone()?,
            path: self.path.clone(),
        })
    }

    /// Issue a frontend ioctl and let the driver update `arg`.
    ///
    /// # Safety
    /// `T` must match `nr` and the driver ABI, with a size encodable by `_IOC`.
    /// Its input bytes must be initialized and its output representations valid.
    /// Every embedded pointer must satisfy the command's lifetime, bounds and
    /// access requirements until the ioctl returns.
    pub unsafe fn ioctl_raw<T>(&self, nr: u32, arg: &mut T) -> Result<()> {
        let cmd = iowr::<T>(nr);
        let r = libc::ioctl(
            self.fd.as_raw_fd(),
            cmd as libc::Ioctl,
            arg as *mut T as *mut libc::c_void,
        );
        if r < 0 {
            return Err(Error::Ioctl {
                nr,
                source: std::io::Error::last_os_error(),
            });
        }
        Ok(())
    }

    /// Transfer FD ownership to the caller.
    pub fn into_owned_fd(self) -> OwnedFd {
        self.fd
    }
}

impl AsRawFd for NvDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Name a known RM status for diagnostics; unknown values return `None`.
/// Values come from `nvstatuscodes.h` and the checked `nvgpu` mirror.
pub fn status_name(status: u32) -> Option<&'static str> {
    use nvgpu::status as s;
    Some(match status {
        s::NV_OK => "NV_OK",
        s::NV_ERR_INSUFFICIENT_PERMISSIONS => "NV_ERR_INSUFFICIENT_PERMISSIONS",
        s::NV_ERR_INVALID_ADDRESS => "NV_ERR_INVALID_ADDRESS",
        s::NV_ERR_INVALID_ARGUMENT => "NV_ERR_INVALID_ARGUMENT",
        s::NV_ERR_INVALID_CLASS => "NV_ERR_INVALID_CLASS",
        s::NV_ERR_INVALID_CLIENT => "NV_ERR_INVALID_CLIENT",
        s::NV_ERR_STATE_IN_USE => "NV_ERR_STATE_IN_USE",
        s::NV_ERR_INVALID_LIMIT => "NV_ERR_INVALID_LIMIT",
        s::NV_ERR_INVALID_OBJECT_HANDLE => "NV_ERR_INVALID_OBJECT_HANDLE",
        s::NV_ERR_NOT_SUPPORTED => "NV_ERR_NOT_SUPPORTED",
        x if x == sys::NV_ERR_NO_MEMORY => "NV_ERR_NO_MEMORY",
        x if x == sys::NV_ERR_OBJECT_NOT_FOUND => "NV_ERR_OBJECT_NOT_FOUND",
        x if x == sys::NV_ERR_INVALID_PARAM_STRUCT => "NV_ERR_INVALID_PARAM_STRUCT",
        x if x == sys::NV_ERR_INVALID_PARAMETER => "NV_ERR_INVALID_PARAMETER",
        x if x == sys::NV_ERR_INSERT_DUPLICATE_NAME => "NV_ERR_INSERT_DUPLICATE_NAME",
        x if x == sys::NV_ERR_OPERATING_SYSTEM => "NV_ERR_OPERATING_SYSTEM",
        _ => return None,
    })
}

/// Checks an RM status field after a successful ioctl.
#[inline]
pub fn check_status(nr: u32, status: u32) -> Result<()> {
    if status == sys::NV_OK {
        Ok(())
    } else {
        Err(Error::Rm { nr, status })
    }
}

#[cfg(test)]
mod ioc_tests {
    use super::*;

    /// Encoding and decoding must preserve direction, type, number and size.
    #[test]
    fn iowr_raw_round_trips_through_the_decoders() {
        for &nr in &[0u32, 1, 0x27, 0x2a, 0x2b, 0xc9, 0xff] {
            for &size in &[0u32, 1, 16, 48, 376, 9264, 16383] {
                let cmd = iowr_raw(nr, size);
                assert_eq!(ioc_nr(cmd), nr, "nr of {cmd:#x}");
                assert_eq!(ioc_size(cmd), size, "size of {cmd:#x}");
                assert_eq!(
                    ioc_type(cmd),
                    sys::NV_IOCTL_MAGIC as u32,
                    "type of {cmd:#x}"
                );
                // Direction is READ|WRITE for every NVIDIA escape: the
                // driver overwrites the argument in place.
                assert_eq!(
                    cmd >> IOC_DIRSHIFT,
                    (IOC_READ | IOC_WRITE),
                    "dir of {cmd:#x}"
                );
            }
        }
    }

    /// `_IOC` has a 14-bit size field. Larger sizes overflow into direction
    /// bits and require XFER_CMD.
    #[test]
    fn the_size_field_stops_at_fourteen_bits() {
        assert_eq!(crate::xfer::IOC_SIZE_LIMIT, 16383);
        let ok = iowr_raw(0x2a, 16383);
        assert_eq!(ioc_size(ok), 16383);
        // One past the limit: the size no longer survives the round trip.
        let overflowed = iowr_raw(0x2a, 16384);
        assert_ne!(ioc_size(overflowed), 16384);
        assert_eq!(ioc_size(overflowed), 0, "the low 14 bits of 16384 are zero");
    }

    /// Only the type byte distinguishes NVIDIA frontend ioctls from another
    /// subsystem using the same number and size.
    #[test]
    fn is_nv_cmd_recognises_the_type_byte_and_nothing_else() {
        assert_eq!(
            sys::NV_IOCTL_MAGIC,
            b'F',
            "the frontend magic is 'F' (nv-ioctl-numbers.h)"
        );
        assert!(is_nv_cmd(iowr_raw(sys::NV_ESC_RM_CONTROL, 32)));
        assert!(is_nv_cmd(iowr::<u32>(0)));

        // A DRM ioctl: same shape, type 'd' (drm.h DRM_IOCTL_BASE). Same
        // nr, same size, and it must NOT be taken for an NVIDIA escape.
        let drm = ioc(
            IOC_READ | IOC_WRITE,
            b'd' as u32,
            sys::NV_ESC_RM_CONTROL,
            32,
        );
        assert!(!is_nv_cmd(drm));
        assert_eq!(
            ioc_nr(drm),
            sys::NV_ESC_RM_CONTROL,
            "the nr alone does not distinguish them"
        );
        // Type 0 is the other easy false positive (an all-zero cmd word).
        assert!(!is_nv_cmd(0));
    }

    /// The encoded size must match the parameter type.
    #[test]
    fn iowr_takes_its_size_from_the_type() {
        assert_eq!(
            iowr::<sys::NVOS54_PARAMETERS>(sys::NV_ESC_RM_CONTROL),
            iowr_raw(
                sys::NV_ESC_RM_CONTROL,
                core::mem::size_of::<sys::NVOS54_PARAMETERS>() as u32
            )
        );
        assert_eq!(
            ioc_size(iowr::<sys::NVOS64_PARAMETERS>(sys::NV_ESC_RM_ALLOC)),
            core::mem::size_of::<sys::NVOS64_PARAMETERS>() as u32
        );
        assert_eq!(ioc_size(iowr::<[u8; 0]>(0)), 0);
        assert_eq!(
            ioc_size(iowr::<nvgpu::IoctlRegisterFd>(nvgpu::NV_ESC_REGISTER_FD)),
            4
        );
    }
}
