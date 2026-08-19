// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! The seam between deciding and doing (docs/OPEN-QUESTIONS.md nr 5).
//!
//! RM below is NVIDIA's Resource Manager, the kernel driver behind
//! /dev/nvidiactl and /dev/nvidiaN, whose ioctls are called escapes.
//!
//! `on_ioctl` runs fourteen checks against a possibly lying guest and then
//! performs exactly THREE syscalls. Those three sit here behind an
//! interface -- not because they are complicated, but because they were the
//! only reason the fourteen checks used to need a GPU to be tested at all.
//!
//! Explicitly NOT here: any decision. Whoever is looking for a rule will
//! find it in `session.rs`. This file only carries the transition into the
//! kernel -- and, in tests, the record of what WOULD have crossed over.

use std::os::fd::RawFd;

/// What a session does to the outside world. Three calls, no more.
///
/// `Send + Sync`, because `Session` already is: the virtio-nvrm device
/// holds it behind an RwLock and passes it between the vhost-user backend
/// threads.
pub trait NvSyscalls: Send + Sync {
    /// The real ioctl on a device FD. `buf` is the inline struct; the
    /// driver writes back in place. `len` is how many bytes at `buf` the
    /// caller has actually initialized -- the driver does not need it (it
    /// takes the size from the _IOC encoding or from the UVM command), but
    /// an implementation that READS the buffer does: for a UVM command the
    /// encoding carries no size at all, and there is no other way to tell
    /// where the caller's data ends.
    ///
    /// # Safety
    /// `request` must match `buf` (the size in the _IOC encoding, or the
    /// struct the UVM command expects). A mismatch is a silent memory error
    /// inside the driver, not an EINVAL. `buf` must be readable for `len`
    /// bytes.
    unsafe fn ioctl(&self, fd: RawFd, request: libc::c_ulong, buf: *mut u8, len: usize) -> i32;

    /// `NV0000_CTRL_CMD_SET_SUB_PROCESS_ID` on a fresh RM client. Returns
    /// the same pair as `nvrm_abi::share`'s helpers: (ioctl return, RM
    /// status).
    fn set_sub_process_id(&self, fd: RawFd, hclient: u32, sub_id: u32, name: &str) -> (i32, u32);

    /// DUP grant for the same user (`grant_dup_same_user`).
    fn grant_dup_same_user(&self, fd: RawFd, hclient: u32, hobject: u32) -> (i32, u32);
}

/// The production path: sends the three calls to the real driver.
pub struct RealSyscalls;

impl NvSyscalls for RealSyscalls {
    // `len` is unused here: the driver takes the size from the request, and
    // passing it would not change a single byte that crosses.
    unsafe fn ioctl(&self, fd: RawFd, request: libc::c_ulong, buf: *mut u8, _len: usize) -> i32 {
        libc::ioctl(fd, request, buf as *mut libc::c_void)
    }

    fn set_sub_process_id(&self, fd: RawFd, hclient: u32, sub_id: u32, name: &str) -> (i32, u32) {
        unsafe { nvrm_abi::share::set_sub_process_id(fd, hclient, sub_id, name) }
    }

    fn grant_dup_same_user(&self, fd: RawFd, hclient: u32, hobject: u32) -> (i32, u32) {
        unsafe { nvrm_abi::share::grant_dup_same_user(fd, hclient, hobject) }
    }
}

/// A ledger instead of a driver: records what a call WOULD have been.
///
/// This makes the guest-lies cases testable in both directions: that a
/// message was refused, AND that no ioctl happened while refusing it. A
/// test that only checks the error response cannot tell "a check fired"
/// from "the driver said no".
#[cfg(test)]
#[derive(Default)]
pub struct FakeSyscalls {
    pub calls: std::sync::Mutex<Vec<FakeCall>>,
    /// What `ioctl` should return (default 0 = success).
    pub ioctl_ret: i32,
    /// Per-call overrides, consumed front to back: `(return value, errno)`.
    /// While the queue has entries they win over `ioctl_ret`; the errno is
    /// stored into the thread's `errno` slot on every queued call, whether
    /// or not the call "fails" -- that is how a test reproduces a later
    /// syscall clobbering the errno of an earlier one.
    pub ioctl_rets: std::sync::Mutex<std::collections::VecDeque<(i32, i32)>>,
    /// Bytes that `ioctl` writes back into `buf` (offset, value) -- so a
    /// test can fake an RM status or a created handle.
    pub writes_back: Vec<(usize, Vec<u8>)>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeCall {
    Ioctl { fd: RawFd, request: libc::c_ulong, inline: Vec<u8> },
    SetSubProcessId { fd: RawFd, hclient: u32, sub_id: u32, name: String },
    GrantDup { fd: RawFd, hclient: u32, hobject: u32 },
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
    fn grant_dup_same_user(&self, fd: RawFd, hclient: u32, hobject: u32) -> (i32, u32) {
        (**self).grant_dup_same_user(fd, hclient, hobject)
    }
}

#[cfg(test)]
impl NvSyscalls for FakeSyscalls {
    unsafe fn ioctl(&self, fd: RawFd, request: libc::c_ulong, buf: *mut u8, len: usize) -> i32 {
        // What the ledger records is what the caller HAS, never more.
        //
        // A UVM command is a RAW number, so `ioc_size` reads a size out of
        // bits that carry none: for a small number (UVM_MM_INITIALIZE, 0x4b)
        // it yields 0 and this used to substitute a flat 256; for
        // UVM_INITIALIZE (0x3000_0001) it yields 12288 and that was used as
        // is. Both read far past a 16-byte UVM payload into the
        // uninitialized tail of the session's scratch Vec -- undefined
        // behaviour that stayed inside the allocation only because the Vec
        // is built with capacity 16384, and bytes a test could assert on
        // without ever having been written.
        let ioc = nvrm_abi::ioc_size(request as u32) as usize;
        let n = if ioc == 0 { len } else { ioc.min(len) };
        let inline = std::slice::from_raw_parts(buf, n).to_vec();
        self.calls.lock().unwrap().push(FakeCall::Ioctl { fd, request, inline });
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

    fn grant_dup_same_user(&self, fd: RawFd, hclient: u32, hobject: u32) -> (i32, u32) {
        self.calls.lock().unwrap().push(FakeCall::GrantDup { fd, hclient, hobject });
        (0, 0)
    }
}
