// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Host backend modules shared by the daemon, tests, and fuzz target.

mod client_policy;
pub mod grid;
pub mod guest_words;
pub mod host_pool;
pub mod mirror;
pub mod nvrm;
mod request_shape;
pub mod session;
pub mod syscalls;
pub mod vram;
pub mod waiters;
