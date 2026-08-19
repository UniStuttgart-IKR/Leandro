// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! token -> real device FD. The host keeps every /dev/nvidia* FD the
//! guest has opened, under the token it handed out at open time.
//!
//! A token is the host-issued id for one guest-opened device fd. What it
//! names is an OFD, an open file description, and that is the whole point:
//! RM -- NVIDIA's Resource Manager, the kernel driver behind these nodes
//! -- binds its clients to the OFD, so a forwarded ioctl has to run on
//! exactly the one the guest used and cannot be given a substitute.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

pub struct Mirror {
    next: u64,
    map: HashMap<u64, OwnedFd>,
}

/// NOT `#[derive(Default)]`. The derive would start `next` at 0, which
/// breaks two documented properties of this type at once: the first token
/// handed out would be 0 instead of 1, and `ever()` -- which computes
/// `next - 1` -- would underflow on a mirror that has handed out nothing
/// (panic in debug, 2^64-1 in release, i.e. a nonsense FD census line).
/// Every caller today goes through `new()`, so this is the same object
/// either way; the derive was simply a second, wrong constructor standing
/// next to the right one.
impl Default for Mirror {
    fn default() -> Self {
        Self::new()
    }
}

impl Mirror {
    pub fn new() -> Self {
        Self { next: 1, map: HashMap::new() }
    }

    /// Take in a new real FD, get the token back.
    pub fn insert(&mut self, fd: OwnedFd) -> u64 {
        let tok = self.next;
        self.next += 1;
        self.map.insert(tok, fd);
        tok
    }

    /// Host FD number for a token. `None` -> guest error (EBADF), no panic.
    pub fn raw(&self, token: u64) -> Option<RawFd> {
        self.map.get(&token).map(|f| f.as_raw_fd())
    }

    pub fn remove(&mut self, token: u64) {
        self.map.remove(&token);
    }
}
impl Mirror {
    /// How many guest FDs this session mirrors. For the device's FD census.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// How many FDs this session has EVER been handed (tokens start at 1 and
    /// never repeat). Against `len()` it says how many were given back --
    /// which is the one number that separates "the host never got the close"
    /// from "the host got it and kept the FD anyway".
    pub fn ever(&self) -> usize {
        (self.next - 1) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    /// A stand-in for a real /dev/nvidia* FD. Nothing here reads or writes
    /// it -- the mirror only stores it and reports its number -- so any
    /// openable file does, and /dev/null is the one that always exists.
    fn fd() -> OwnedFd {
        OwnedFd::from(File::open("/dev/null").expect("/dev/null"))
    }

    /// Tokens start at 1, rise by one, and are NEVER handed out twice --
    /// not even after the token they named was removed.
    ///
    /// Both halves are load-bearing. Token 0 must not exist because
    /// `ever()` counts `next - 1` and a mirror that never handed anything
    /// out has to answer 0, not underflow. And reuse would be a
    /// use-after-free across the VM boundary: the guest keeps tokens in its
    /// own tables, so a recycled number would silently redirect a guest's
    /// ioctl onto a device FD that a later `open` put there.
    #[test]
    fn tokens_start_at_one_and_are_never_reused() {
        let mut m = Mirror::new();
        let a = m.insert(fd());
        let b = m.insert(fd());
        let c = m.insert(fd());
        assert_eq!((a, b, c), (1, 2, 3), "tokens count up from 1");

        m.remove(b);
        m.remove(c);
        let d = m.insert(fd());
        assert_eq!(d, 4, "a freed token is not handed out again");
        assert!(d > c, "tokens are monotonic across removals");
    }

    /// `raw` resolves exactly the live tokens and answers `None` for
    /// everything else -- an unknown number, a removed one, and 0.
    ///
    /// `None` is what turns a bad guest token into EBADF for that one
    /// call. A panic here (or, worse, a hit on a recycled entry) would be
    /// reachable from any guest process that names a number it made up.
    #[test]
    fn raw_resolves_live_tokens_and_nothing_else() {
        let mut m = Mirror::new();
        let f = fd();
        let want = f.as_raw_fd();
        let tok = m.insert(f);

        assert_eq!(m.raw(tok), Some(want), "the token names the FD it was given");
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

    /// `len`/`is_empty` count what is HELD, `ever` counts what was ISSUED.
    ///
    /// The gap between the two is the whole point of `ever()`: the FD
    /// census subtracts them to tell "the host never got the close" from
    /// "the host got it and kept the FD anyway". Were `ever` to shrink with
    /// `remove`, that difference would always be 0 and the census would say
    /// nothing.
    #[test]
    fn len_is_empty_and_ever_count_different_things() {
        let mut m = Mirror::new();
        assert_eq!((m.len(), m.is_empty(), m.ever()), (0, true, 0), "fresh mirror");

        let a = m.insert(fd());
        let b = m.insert(fd());
        assert_eq!((m.len(), m.is_empty(), m.ever()), (2, false, 2));

        m.remove(a);
        assert_eq!(m.len(), 1, "one given back");
        assert_eq!(m.ever(), 2, "but two were handed out, and that does not shrink");

        m.remove(b);
        assert!(m.is_empty(), "nothing held any more");
        assert_eq!(m.ever(), 2, "still two ever");
    }

    /// `Mirror::default()` is `Mirror::new()`, not a second constructor
    /// with a different idea of where tokens start.
    ///
    /// This is the test the derive fails: it would start `next` at 0, hand
    /// out token 0, and underflow in `ever()` on the fresh mirror below.
    #[test]
    fn default_is_new() {
        let mut m = Mirror::default();
        assert_eq!(m.ever(), 0, "a fresh mirror has issued nothing");
        assert_eq!(m.len(), 0);
        assert!(m.is_empty());
        assert_eq!(m.insert(fd()), 1, "and its first token is 1, like new()");
        assert_eq!(m.ever(), 1);
    }
}
