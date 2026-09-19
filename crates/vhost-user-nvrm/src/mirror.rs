// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Own host device FDs behind guest-visible tokens.
//!
//! RM binds clients to an open file description. Forwarded ioctls must use
//! the original FD, and removed tokens must not resolve to a later open.

use nvrm_abi::xlate::Dev;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

struct Entry {
    fd: OwnedFd,
    kind: Dev,
}

pub struct Mirror {
    next: u64,
    map: HashMap<u64, Entry>,
}

// Tokens start at 1; deriving Default would violate this invariant.
impl Default for Mirror {
    fn default() -> Self {
        Self::new()
    }
}

impl Mirror {
    pub fn new() -> Self {
        Self {
            next: 1,
            map: HashMap::new(),
        }
    }

    /// Take ownership of a host FD and return its token.
    pub fn insert(&mut self, fd: OwnedFd, kind: Dev) -> u64 {
        let tok = self.next;
        self.next = tok.checked_add(1).expect("FD token space exhausted");
        self.map.insert(tok, Entry { fd, kind });
        tok
    }

    /// Resolve a live token. Callers map `None` to guest EBADF.
    pub fn raw(&self, token: u64) -> Option<RawFd> {
        self.map.get(&token).map(|entry| entry.fd.as_raw_fd())
    }

    pub fn kind(&self, token: u64) -> Option<Dev> {
        self.map.get(&token).map(|entry| entry.kind)
    }

    pub fn remove(&mut self, token: u64) {
        self.map.remove(&token);
    }
    /// How many guest FDs this session mirrors. For the device's FD census.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Total FDs issued; subtract len() to count completed closes.
    pub fn ever(&self) -> usize {
        (self.next - 1) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    /// The mirror only needs an owned FD, so tests use /dev/null.
    fn fd() -> OwnedFd {
        OwnedFd::from(File::open("/dev/null").expect("/dev/null"))
    }

    /// Reusing a removed token would redirect stale guest ioctls to a new FD.
    #[test]
    fn tokens_start_at_one_and_are_never_reused() {
        let mut m = Mirror::new();
        let a = m.insert(fd(), Dev::Ctl);
        let b = m.insert(fd(), Dev::Ctl);
        let c = m.insert(fd(), Dev::Ctl);
        assert_eq!((a, b, c), (1, 2, 3), "tokens count up from 1");

        m.remove(b);
        m.remove(c);
        let d = m.insert(fd(), Dev::Ctl);
        assert_eq!(d, 4, "a freed token is not handed out again");
        assert!(d > c, "tokens are monotonic across removals");
    }

    /// Unknown, removed and zero tokens resolve to None.
    #[test]
    fn raw_resolves_live_tokens_and_nothing_else() {
        let mut m = Mirror::new();
        let f = fd();
        let want = f.as_raw_fd();
        let tok = m.insert(f, Dev::Ctl);

        assert_eq!(
            m.raw(tok),
            Some(want),
            "the token names the FD it was given"
        );
        assert_eq!(m.raw(0), None, "0 is never a token");
        assert_eq!(m.raw(tok + 1), None, "a token never issued");
        assert_eq!(m.raw(u64::MAX), None, "a number the guest invented");

        m.remove(tok);
        assert_eq!(m.raw(tok), None, "a removed token resolves to nothing");
        // Removing twice is the normal case, not an error: the guest may
        // send Close for a token the host already dropped.
        m.remove(tok);
        m.remove(12345);
    }

    /// Live FD counts shrink on close; the issued count does not.
    #[test]
    fn len_is_empty_and_ever_count_different_things() {
        let mut m = Mirror::new();
        assert_eq!(
            (m.len(), m.is_empty(), m.ever()),
            (0, true, 0),
            "fresh mirror"
        );

        let a = m.insert(fd(), Dev::Ctl);
        let b = m.insert(fd(), Dev::Ctl);
        assert_eq!((m.len(), m.is_empty(), m.ever()), (2, false, 2));

        m.remove(a);
        assert_eq!(m.len(), 1, "one given back");
        assert_eq!(
            m.ever(),
            2,
            "but two were handed out, and that does not shrink"
        );

        m.remove(b);
        assert!(m.is_empty(), "nothing held any more");
        assert_eq!(m.ever(), 2, "still two ever");
    }

    /// Both constructors preserve nonzero tokens and an initially zero census.
    #[test]
    fn default_is_new() {
        let mut m = Mirror::default();
        assert_eq!(m.ever(), 0, "a fresh mirror has issued nothing");
        assert_eq!(m.len(), 0);
        assert!(m.is_empty());
        assert_eq!(
            m.insert(fd(), Dev::Ctl),
            1,
            "and its first token is 1, like new()"
        );
        assert_eq!(m.ever(), 1);
    }
}
