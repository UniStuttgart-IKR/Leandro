// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Monotonic handles in the high-bit range, scoped to one RM client.
//! Callers sharing a client must reserve this range for this allocator.

/// Marks the range reserved for this allocator.
pub const DAEMON_HANDLE_BIT: u32 = 0x8000_0000;

pub struct HandleAllocator {
    root: u32,
    next: u32,
}

impl HandleAllocator {
    pub fn new(root: u32) -> Self {
        Self { root, next: 1 }
    }

    /// Allocate a handle, skipping the client's root handle.
    ///
    /// # Panics
    /// Panics when the reserved range is exhausted. Handles are never reused.
    pub fn take(&mut self) -> u32 {
        loop {
            assert!(self.next < DAEMON_HANDLE_BIT, "RM handle range exhausted");
            let h = DAEMON_HANDLE_BIT | self.next;
            self.next += 1;
            if h != self.root {
                return h;
            }
        }
    }

    /// Whether a handle is outside the allocator's reserved range.
    pub fn is_guest(h: u32) -> bool {
        h & DAEMON_HANDLE_BIT == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_do_not_repeat_when_counter_reaches_root_bits() {
        let mut allocator = HandleAllocator::new(0x0001_0000);
        let handles: std::collections::HashSet<_> = (0..65_537).map(|_| allocator.take()).collect();
        assert_eq!(handles.len(), 65_537);
        assert!(handles.iter().all(|&h| !HandleAllocator::is_guest(h)));
    }

    #[test]
    fn root_handle_is_not_allocated_again() {
        let mut allocator = HandleAllocator::new(DAEMON_HANDLE_BIT | 2);
        assert_eq!(allocator.take(), DAEMON_HANDLE_BIT | 1);
        assert_eq!(allocator.take(), DAEMON_HANDLE_BIT | 3);
    }

    #[test]
    fn exhaustion_never_wraps_or_reuses_handles() {
        let mut allocator = HandleAllocator {
            root: 0,
            next: DAEMON_HANDLE_BIT - 1,
        };
        assert_eq!(allocator.take(), u32::MAX);
        for _ in 0..2 {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| allocator.take()))
                    .is_err()
            );
        }
    }
}
