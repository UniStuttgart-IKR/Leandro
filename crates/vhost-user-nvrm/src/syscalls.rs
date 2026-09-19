// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Driver call interface used by Session validation tests.
//!
//! RealSyscalls forwards to NVIDIA RM; FakeSyscalls records calls and
//! injects replies. Validation and translation remain in session.rs.

use std::os::fd::RawFd;

/// Driver calls injectable in validation tests. Shared across backend threads.
pub trait NvSyscalls: Send + Sync {
    /// Execute an ioctl; len is the initialized buffer length used by test readers.
    ///
    /// # Safety
    /// request must match the buffer layout and driver ABI. buf and every
    /// embedded pointer must refer to live buffers with the access and lengths
    /// required by the command. The driver may write the reply in place.
    unsafe fn ioctl(&self, fd: RawFd, request: libc::c_ulong, buf: *mut u8, len: usize) -> i32;

    /// `NV0000_CTRL_CMD_SET_SUB_PROCESS_ID` on a fresh RM client. Returns
    /// the same pair as `nvrm_abi::share`'s helpers: (ioctl return, RM
    /// status).
    fn set_sub_process_id(&self, fd: RawFd, hclient: u32, sub_id: u32, name: &str) -> (i32, u32);

    /// DUP grant scoped to the backend process (`grant_dup_same_process`).
    fn grant_dup_same_process(&self, fd: RawFd, hclient: u32, hobject: u32) -> (i32, u32);
}

/// The production path: sends the three calls to the real driver.
pub struct RealSyscalls;

impl NvSyscalls for RealSyscalls {
    // `len` is unused here: the driver takes the size from the request, and
    // passing it would not change a single byte that crosses.
    unsafe fn ioctl(&self, fd: RawFd, request: libc::c_ulong, buf: *mut u8, _len: usize) -> i32 {
        libc::ioctl(fd, request as libc::Ioctl, buf as *mut libc::c_void)
    }

    fn set_sub_process_id(&self, fd: RawFd, hclient: u32, sub_id: u32, name: &str) -> (i32, u32) {
        unsafe { nvrm_abi::share::set_sub_process_id(fd, hclient, sub_id, name) }
    }

    fn grant_dup_same_process(&self, fd: RawFd, hclient: u32, hobject: u32) -> (i32, u32) {
        unsafe { nvrm_abi::share::grant_dup_same_process(fd, hclient, hobject) }
    }
}

/// Record calls and inject replies so refusal tests can assert no ioctl ran.
#[cfg(test)]
#[derive(Default)]
pub struct FakeSyscalls {
    pub calls: std::sync::Mutex<Vec<FakeCall>>,
    /// What `ioctl` should return (default 0 = success).
    pub ioctl_ret: i32,
    /// Queued (return value, errno) overrides, consumed before ioctl_ret.
    /// Every queued call sets errno, including successful calls.
    pub ioctl_rets: std::sync::Mutex<std::collections::VecDeque<(i32, i32)>>,
    /// Bytes that `ioctl` writes back into `buf` (offset, value) -- so a
    /// test can fake an RM status or a created handle.
    pub writes_back: Vec<(usize, Vec<u8>)>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeCall {
    Ioctl {
        fd: RawFd,
        request: libc::c_ulong,
        inline: Vec<u8>,
    },
    SetSubProcessId {
        fd: RawFd,
        hclient: u32,
        sub_id: u32,
        name: String,
    },
    GrantDup {
        fd: RawFd,
        hclient: u32,
        hobject: u32,
    },
}

#[cfg(test)]
impl FakeSyscalls {
    /// Every call so far.
    pub fn calls(&self) -> Vec<FakeCall> {
        self.calls.lock().unwrap().clone()
    }

    /// How many real ioctls would have been issued?
    pub fn ioctl_count(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| matches!(c, FakeCall::Ioctl { .. }))
            .count()
    }

    /// The inline buffer of the last ioctl -- as the driver would have seen
    /// it, that is, AFTER all translations.
    pub fn last_inline(&self) -> Option<Vec<u8>> {
        self.calls().iter().rev().find_map(|c| match c {
            FakeCall::Ioctl { inline, .. } => Some(inline.clone()),
            _ => None,
        })
    }
}

/// So a test can keep the very ledger it hands to the session: `Session`
/// takes a `Box<dyn NvSyscalls>`, and the test holds an `Arc` on the same
/// object alongside it.
#[cfg(test)]
impl<T: NvSyscalls + ?Sized> NvSyscalls for std::sync::Arc<T> {
    unsafe fn ioctl(&self, fd: RawFd, request: libc::c_ulong, buf: *mut u8, len: usize) -> i32 {
        (**self).ioctl(fd, request, buf, len)
    }
    fn set_sub_process_id(&self, fd: RawFd, hclient: u32, sub_id: u32, name: &str) -> (i32, u32) {
        (**self).set_sub_process_id(fd, hclient, sub_id, name)
    }
    fn grant_dup_same_process(&self, fd: RawFd, hclient: u32, hobject: u32) -> (i32, u32) {
        (**self).grant_dup_same_process(fd, hclient, hobject)
    }
}

#[cfg(test)]
impl NvSyscalls for FakeSyscalls {
    unsafe fn ioctl(&self, fd: RawFd, request: libc::c_ulong, buf: *mut u8, len: usize) -> i32 {
        // UVM request numbers do not encode a size. Never read beyond len,
        // the initialized buffer length supplied by the caller.
        let ioc = nvrm_abi::ioc_size(request as u32) as usize;
        let n = if ioc == 0 { len } else { ioc.min(len) };
        let inline = std::slice::from_raw_parts(buf, n).to_vec();
        self.calls.lock().unwrap().push(FakeCall::Ioctl {
            fd,
            request,
            inline,
        });
        for (off, bytes) in &self.writes_back {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.add(*off), bytes.len());
        }
        if let Some((ret, errno)) = self.ioctl_rets.lock().unwrap().pop_front() {
            // SAFETY: writing the calling thread's errno slot, exactly what a
            // real failing syscall does.
            *libc::__errno_location() = errno;
            return ret;
        }
        self.ioctl_ret
    }

    fn set_sub_process_id(&self, fd: RawFd, hclient: u32, sub_id: u32, name: &str) -> (i32, u32) {
        self.calls.lock().unwrap().push(FakeCall::SetSubProcessId {
            fd,
            hclient,
            sub_id,
            name: name.to_string(),
        });
        (0, 0)
    }

    fn grant_dup_same_process(&self, fd: RawFd, hclient: u32, hobject: u32) -> (i32, u32) {
        self.calls.lock().unwrap().push(FakeCall::GrantDup {
            fd,
            hclient,
            hobject,
        });
        (0, 0)
    }
}
