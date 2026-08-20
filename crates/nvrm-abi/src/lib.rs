// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! ioctl level: number encoding, call wrappers, device FDs.
//!
//! This crate is the ABI layer that every user of the NVIDIA RM interface
//! in this workspace shares. RM is NVIDIA's Resource Manager -- the kernel
//! driver behind `/dev/nvidiactl` and `/dev/nvidiaN`; its ioctls are called
//! escapes (`NV_ESC_*`) and their parameter blocks are the `NVOS*` structs
//! from `nvos.h`. Here: how an ioctl number is built and issued (Linux
//! `_IOC` encoding), the struct layouts and constants of the driver ABI
//! (`nvgpu`), the XFER_CMD indirection (`xfer`), what has to be known per
//! (device, ioctl nr) to carry a call across a process or VM boundary
//! (`xlate` -- the single source of truth), the descriptor tables handed
//! to the guest module (`table`) and cross-process DUP grants (`share`).
//! Session and object lifetime logic lives in `nvrm-client`; the raw
//! bindgen output in `nvrm-sys`, re-exported as [`sys`].

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

pub mod mediate;
pub mod nvgpu;
pub mod share;
pub mod table;
pub mod xfer;
pub mod xlate;

pub use nvrm_sys as sys;

// ---------------------------------------------------------------------------
// _IOC encoding
// ---------------------------------------------------------------------------
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

/// Like `iowr`, but with a runtime size instead of a type. When forwarding
/// a frontend ioctl, the host only has the byte length, not the type.
/// Do NOT use for UVM - UVM requests are raw numbers with no _IOC encoding.
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

// ---------------------------------------------------------------------------
// Doorbell
// ---------------------------------------------------------------------------
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

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ioctl {nr:#x} failed: {source}")]
    Ioctl {
        nr: u32,
        #[source]
        source: std::io::Error,
    },
    /// The ioctl itself returned 0, but RM reports a status. This is the
    /// more common failure and the one that is easily overlooked.
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

// ---------------------------------------------------------------------------
// Device FD
// ---------------------------------------------------------------------------

/// An open FD on `/dev/nvidiactl` or `/dev/nvidia<N>`.
///
/// Important: exactly *one* mmap context per FD is allowed. Anything that
/// wants to map opens a new FD. See `NvDevice::open_for_mapping`.
#[derive(Debug)]
pub struct NvDevice {
    fd: OwnedFd,
    path: String,
}

impl NvDevice {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|e| Error::Open { path: path.display().to_string(), source: std::io::Error::other(e) })?;
        // O_CLOEXEC on purpose: a process that survives fork/exec must not
        // hand a half-built RM state down to the exec'd image.
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

    /// Fresh FD on the same device, registered against `ctl` - good for
    /// exactly one mmap context.
    ///
    /// The `register_fd` is not optional: a freshly opened
    /// `/dev/nvidia<N>` FD is associated with no client as far as the
    /// driver is concerned, and NV_ESC_RM_MAP_MEMORY on it does nothing.
    /// NV_ESC_REGISTER_FD establishes the link to the ctl FD the RM client
    /// lives on. libcuda does this after *every* open of a per-GPU node.
    ///
    /// (Unverified against the vendor tree in this exact wording; the
    /// registration is harmless either way and is what libcuda does.)
    pub fn open_for_mapping(&self, ctl: &NvDevice) -> Result<Self> {
        let fd = Self::open(&self.path)?;
        fd.register_fd(ctl)?;
        Ok(fd)
    }

    /// `NV_ESC_REGISTER_FD` -- binds this FD to a client's ctl FD.
    ///
    /// No RM status field: the driver reports errors here as errno.
    pub fn register_fd(&self, ctl: &NvDevice) -> Result<()> {
        let mut p = nvgpu::IoctlRegisterFd { ctl_fd: ctl.as_raw_fd() };
        unsafe { self.ioctl_raw(nvgpu::NV_ESC_REGISTER_FD, &mut p) }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Raw call: the driver overwrites `arg` in place.
    ///
    /// # Safety
    /// `T` must have the same layout as the struct the driver expects for
    /// `nr`. The size is encoded in the ioctl number; a mismatch is a
    /// silent memory error, not an EINVAL.
    pub unsafe fn ioctl_raw<T>(&self, nr: u32, arg: &mut T) -> Result<()> {
        let cmd = iowr::<T>(nr);
        let r = libc::ioctl(self.fd.as_raw_fd(), cmd as libc::c_ulong, arg as *mut T as *mut libc::c_void);
        if r < 0 {
            return Err(Error::Ioctl { nr, source: std::io::Error::last_os_error() });
        }
        Ok(())
    }

    /// Hands over ownership of the raw FD. For the host daemon, which puts
    /// the FD into its mirror and no longer needs the NvDevice wrapper.
    pub fn into_owned_fd(self) -> OwnedFd {
        self.fd
    }
}

impl AsRawFd for NvDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// The symbolic name of an RM status code, for log lines and error text.
///
/// Only the codes this workspace names elsewhere are spelled out; anything
/// else is `None` and callers print the number. The values come from the
/// bindings (`nvstatuscodes.h`); the guards in `nvgpu.rs` cross-check the
/// hand-written mirror.
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

    /// The `_IOC` encoding packs direction, type, nr and size into one
    /// 32-bit word, and this file rebuilds the Linux macros by hand
    /// (bindgen emits nothing for function-like macros). Everything the
    /// host does with a forwarded ioctl starts by taking that word apart
    /// again, so building and decoding must be exact inverses.
    #[test]
    fn iowr_raw_round_trips_through_the_decoders() {
        for &nr in &[0u32, 1, 0x27, 0x2a, 0x2b, 0xc9, 0xff] {
            for &size in &[0u32, 1, 16, 48, 376, 9264, 16383] {
                let cmd = iowr_raw(nr, size);
                assert_eq!(ioc_nr(cmd), nr, "nr of {cmd:#x}");
                assert_eq!(ioc_size(cmd), size, "size of {cmd:#x}");
                assert_eq!(ioc_type(cmd), sys::NV_IOCTL_MAGIC as u32, "type of {cmd:#x}");
                // Direction is READ|WRITE for every NVIDIA escape: the
                // driver overwrites the argument in place.
                assert_eq!(cmd >> IOC_DIRSHIFT, (IOC_READ | IOC_WRITE), "dir of {cmd:#x}");
            }
        }
    }

    /// The size field is 14 bits wide, so 16383 is the largest size an
    /// ioctl number can carry. This is the whole reason
    /// `NV_ESC_IOCTL_XFER_CMD` exists (see `xfer.rs`): one byte more and
    /// the size overflows into the direction bits, silently producing a
    /// different ioctl number rather than an error.
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

    /// `is_nv_cmd` decides whether an intercepted ioctl belongs to the
    /// NVIDIA frontend at all. It must key on the TYPE byte only: an ioctl
    /// of another subsystem can carry the same nr and size, and forwarding
    /// one of those to RM would be a call into the wrong driver.
    #[test]
    fn is_nv_cmd_recognises_the_type_byte_and_nothing_else() {
        assert_eq!(sys::NV_IOCTL_MAGIC, b'F', "the frontend magic is 'F' (nv-ioctl-numbers.h)");
        assert!(is_nv_cmd(iowr_raw(sys::NV_ESC_RM_CONTROL, 32)));
        assert!(is_nv_cmd(iowr::<u32>(0)));

        // A DRM ioctl: same shape, type 'd' (drm.h DRM_IOCTL_BASE). Same
        // nr, same size, and it must NOT be taken for an NVIDIA escape.
        let drm = ioc(IOC_READ | IOC_WRITE, b'd' as u32, sys::NV_ESC_RM_CONTROL, 32);
        assert!(!is_nv_cmd(drm));
        assert_eq!(ioc_nr(drm), sys::NV_ESC_RM_CONTROL, "the nr alone does not distinguish them");
        // Type 0 is the other easy false positive (an all-zero cmd word).
        assert!(!is_nv_cmd(0));
    }

    /// `iowr::<T>` is the typed form: the size comes from the type, and
    /// that size is what the driver's `copy_from_user` uses. A mismatch
    /// between the type and the number is a silent memory error, not an
    /// EINVAL -- hence the pairing is pinned here.
    #[test]
    fn iowr_takes_its_size_from_the_type() {
        assert_eq!(
            iowr::<sys::NVOS54_PARAMETERS>(sys::NV_ESC_RM_CONTROL),
            iowr_raw(sys::NV_ESC_RM_CONTROL, core::mem::size_of::<sys::NVOS54_PARAMETERS>() as u32)
        );
        assert_eq!(
            ioc_size(iowr::<sys::NVOS64_PARAMETERS>(sys::NV_ESC_RM_ALLOC)),
            core::mem::size_of::<sys::NVOS64_PARAMETERS>() as u32
        );
        assert_eq!(ioc_size(iowr::<[u8; 0]>(0)), 0);
        assert_eq!(ioc_size(iowr::<nvgpu::IoctlRegisterFd>(nvgpu::NV_ESC_REGISTER_FD)), 4);
    }
}
