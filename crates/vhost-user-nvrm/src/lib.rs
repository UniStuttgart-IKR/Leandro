// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! vhost-user-nvrm as a library -- so the session can be reached from
//! outside without starting the daemon.
//!
//! There are exactly two reasons for this target, and both are
//! testability: `cargo-fuzz` needs a library to link its target against,
//! and integration tests cannot touch `Session` otherwise. The daemon
//! itself (`main.rs`) uses the same modules.

pub mod grid;
pub mod guest_words;
pub mod host_pool;
pub mod mirror;
pub mod nvrm;
pub mod session;
pub mod syscalls;
pub mod vram;
pub mod waiters;
