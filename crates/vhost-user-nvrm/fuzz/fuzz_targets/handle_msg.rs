// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
#![no_main]
//! Fuzzes `Session::handle_msg` -- the only place where guest bytes enter
//! the host.
//!
//! **The attacker model is the guest**, not the user and not the network. A
//! guest may lie as much as it likes; what it may not do is crash the host
//! daemon or get it to write outside its own buffers.
//!
//! The session is given two tokens (host-issued ids for guest-opened
//! device nodes) pointing at memfds: that way the path
//! runs through ALL checks up to immediately before the real ioctl, without
//! needing a GPU -- and the translation paths (`target_token`,
//! `fd_field_token`) are reached instead of just the EBADF branch.
//!
//! The ioctl itself is NOT executed. It is the only part a fuzzer could not
//! judge, and a real ioctl with fuzz bytes would test the NVIDIA driver
//! rather than this code.

use libfuzzer_sys::fuzz_target;
use std::os::fd::{FromRawFd, OwnedFd};
use vhost_user_nvrm::session::Session;
use vhost_user_nvrm::syscalls::NvSyscalls;

/// Does nothing and reports success -- the fuzzer's boundary is the kernel.
struct NoSyscalls;

impl NvSyscalls for NoSyscalls {
    unsafe fn ioctl(&self, _fd: i32, _request: libc::c_ulong, _buf: *mut u8, _len: usize) -> i32 {
        0
    }
    fn set_sub_process_id(&self, _: i32, _: u32, _: u32, _: &str) -> (i32, u32) {
        (0, 0)
    }
    fn grant_dup_same_user(&self, _: i32, _: u32, _: u32) -> (i32, u32) {
        (0, 0)
    }
}

fuzz_target!(|data: &[u8]| {
    // Ledger::off(): the fuzz target has no environment to set, and the
    // attacker model is the guest, not the operator. Every check the cap
    // adds still runs -- with the cap off the alloc path is the one every
    // corpus message takes.
    let mut s = match Session::detached_proc(1, vhost_user_nvrm::vram::Ledger::off()) {
        Ok(s) => s,
        Err(_) => return,
    };
    s.set_syscalls(Box::new(NoSyscalls));

    for _ in 0..2 {
        let fd = unsafe { libc::memfd_create(c"leandro-fuzz".as_ptr(), 0) };
        if fd < 0 {
            return;
        }
        s.insert_token(unsafe { OwnedFd::from_raw_fd(fd) });
    }

    // An Err is an allowed outcome (transport error); a panic or a memory
    // error is not -- libFuzzer catches those.
    let _ = s.handle_msg(data);
});
