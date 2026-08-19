// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Handle allocation.
//!
//! Handles are caller-specified, so a program that allocates objects of
//! its own beside handles it did not choose needs a range of its own -
//! otherwise a `libcuda` handle eventually collides with one of ours, and
//! the result is a very hard-to-find bug.
//!
//! Convention here: high bit set = allocated here, never by `libcuda`.

/// Everything with this bit was handed out by [`HandleAllocator`], never
/// chosen by the program whose handles we ride beside (the name is a
/// leftover: no daemon uses this crate any more).
pub const DAEMON_HANDLE_BIT: u32 = 0x8000_0000;

pub struct HandleAllocator {
    root: u32,
    next: u32,
}

impl HandleAllocator {
    pub fn new(root: u32) -> Self {
        Self { root, next: 1 }
    }

    /// Next handle from the reserved range. (`take`, not `next`: this is no
    /// iterator -- handles never run out and are never given back.)
    pub fn take(&mut self) -> u32 {
        let h = DAEMON_HANDLE_BIT | (self.root & 0x00ff_0000) | self.next;
        self.next += 1;
        h
    }

    /// Did this handle come from the guest?
    pub fn is_guest(h: u32) -> bool {
        h & DAEMON_HANDLE_BIT == 0
    }
}
