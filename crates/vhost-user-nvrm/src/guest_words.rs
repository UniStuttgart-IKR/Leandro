// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Guest-supplied numbers as their own types -- arithmetic exists only
//! checked.
//!
//! Why a type and not a chain of `if`s: three memory-safety holes in this
//! crate had the same shape -- a guest-supplied number, added unchecked.
//! `Arena::build` let a wrapping running sum through, `write_guest_u32`
//! turned into a directed writer at `va = u64::MAX - 3`, `GpaRun::decode`
//! overflowed in `count * WIRE`. Each spot was closed individually with
//! `checked_*`; this type makes the discipline structural: whoever holds
//! a [`GuestAddr`] or [`GuestLen`] can ONLY compute with it checked --
//! `+` and `-` do not exist.
//!
//! `get()` hands out the raw word, because eventually it must go into an
//! mmap argument or an RM struct. The rule for that: raw values flow into
//! SINKS (syscalls, wire structs, error messages), never into arithmetic
//! of their own. Where a `get()` shows up inside a computation, that
//! computation belongs here as a method.

/// An address named by the guest (GPA, GPU VA, or guest VA).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GuestAddr(u64);

/// A length named by the guest.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct GuestLen(u64);

impl GuestAddr {
    pub const fn new(v: u64) -> Self {
        GuestAddr(v)
    }

    /// The raw word -- for sinks, not for computations.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The end address `self + len`; `None` if the guest made it wrap.
    pub fn end(self, len: GuestLen) -> Option<GuestAddr> {
        self.0.checked_add(len.0).map(GuestAddr)
    }

    /// The distance `self - base`; `None` if `self` lies before `base`.
    pub fn offset_from(self, base: GuestAddr) -> Option<u64> {
        self.0.checked_sub(base.0)
    }
}

impl GuestLen {
    pub const fn new(v: u64) -> Self {
        GuestLen(v)
    }

    /// The raw word -- for sinks, not for computations.
    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Sum of two guest lengths; `None` on overflow.
    pub fn plus(self, other: GuestLen) -> Option<GuestLen> {
        self.0.checked_add(other.0).map(GuestLen)
    }
}

impl std::fmt::LowerHex for GuestAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::LowerHex::fmt(&self.0, f)
    }
}

impl std::fmt::LowerHex for GuestLen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::LowerHex::fmt(&self.0, f)
    }
}

impl std::fmt::Display for GuestLen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: u64) -> GuestAddr {
        GuestAddr::new(v)
    }

    fn l(v: u64) -> GuestLen {
        GuestLen::new(v)
    }

    /// `end` is `self + len`, and it answers `None` exactly when the sum
    /// would wrap.
    ///
    /// This is the check behind every "does the guest's range fit"
    /// question in the crate. Computed unchecked, `addr + len` at
    /// `u64::MAX` becomes a SMALL number, and a bounds test of the form
    /// `end <= limit` then passes for a range that covers the whole
    /// address space -- which is how `Arena::build` and `write_guest_u32`
    /// were each talked into writing outside their buffer.
    #[test]
    fn end_is_none_exactly_when_the_sum_wraps() {
        assert_eq!(a(0x1000).end(l(0x1000)), Some(a(0x2000)));
        assert_eq!(a(0).end(l(0)), Some(a(0)), "zero length is not an error");
        assert_eq!(a(u64::MAX).end(l(0)), Some(a(u64::MAX)), "the last byte, no wrap");
        // The first three that DO wrap, at the exact boundary.
        assert_eq!(a(u64::MAX).end(l(1)), None);
        assert_eq!(a(1).end(l(u64::MAX)), None);
        assert_eq!(a(u64::MAX).end(l(u64::MAX)), None);
        // And the largest sum that still fits.
        assert_eq!(a(1).end(l(u64::MAX - 1)), Some(a(u64::MAX)));
    }

    /// `offset_from` is `self - base`, and it answers `None` when `self`
    /// lies BELOW `base`.
    ///
    /// Callers use the result as an index into a host buffer that starts
    /// at `base`. Unchecked, an address before `base` would produce a
    /// huge unsigned distance instead of a refusal, and the caller would
    /// index far past the end of that buffer.
    #[test]
    fn offset_from_refuses_addresses_below_the_base() {
        assert_eq!(a(0x2000).offset_from(a(0x1000)), Some(0x1000));
        assert_eq!(a(0x1000).offset_from(a(0x1000)), Some(0), "the base itself is offset 0");
        assert_eq!(a(0x0fff).offset_from(a(0x1000)), None, "one byte below the base");
        assert_eq!(a(0).offset_from(a(1)), None);
        assert_eq!(a(u64::MAX).offset_from(a(0)), Some(u64::MAX), "the widest legal distance");
    }

    /// `plus` sums two guest lengths and answers `None` on overflow --
    /// the same discipline as `end`, for the running sums that check a
    /// GPA run list against the total the guest promised.
    #[test]
    fn plus_is_none_exactly_on_overflow() {
        assert_eq!(l(0x1000).plus(l(0x2000)), Some(l(0x3000)));
        assert_eq!(l(0).plus(l(0)), Some(l(0)));
        assert_eq!(l(u64::MAX).plus(l(0)), Some(l(u64::MAX)), "the largest sum that fits");
        assert_eq!(l(u64::MAX).plus(l(1)), None);
        assert_eq!(l(u64::MAX).plus(l(u64::MAX)), None);
        // A wrapping sum must not be mistaken for a small legal one: this
        // is exactly the shape `Arena::build` was fooled by.
        let big = l(2 << 20);
        assert_eq!(big.plus(l(0x1000u64.wrapping_sub(2 << 20))), None);
    }

    /// `is_zero` is the length check that stands in front of every
    /// mapping: a zero-length range must be refused, not carried, and
    /// `get() == 0` must not be written out by hand somewhere else.
    #[test]
    fn is_zero_is_true_only_for_zero() {
        assert!(l(0).is_zero());
        assert!(GuestLen::default().is_zero(), "the default length is 0");
        assert!(!l(1).is_zero());
        assert!(!l(u64::MAX).is_zero());
    }

    /// Ordering is by the raw word, on both types -- the derived `Ord` is
    /// what every "is this address inside that range" comparison in the
    /// crate leans on, so it must not be a lexicographic accident.
    #[test]
    fn ordering_follows_the_raw_word() {
        assert!(a(0x1000) < a(0x2000));
        assert!(a(0x2000) > a(0x1000));
        assert_eq!(a(0x1000), a(0x1000));
        assert!(a(u64::MAX) > a(0x8000_0000_0000_0000));
        assert!(l(9) < l(10), "not string order: 9 comes before 10");

        let mut v = [a(0x2000), a(0), a(u64::MAX), a(0x1000)];
        v.sort();
        assert_eq!(v, [a(0), a(0x1000), a(0x2000), a(u64::MAX)]);
    }

    /// The formatting impls print the raw word and nothing else.
    ///
    /// Every log line and every error message in this crate names guest
    /// addresses through these, usually as `{:#x}`. A wrapper that printed
    /// `GuestAddr(4096)` (which is what `Debug` gives) would make the logs
    /// unreadable against the driver's own hex traces, and `{:#x}` must
    /// keep working -- the `#` and the width belong to the formatter, so
    /// they have to be forwarded, not swallowed.
    #[test]
    fn formatting_prints_the_bare_number() {
        assert_eq!(format!("{:x}", a(0xdead_beef)), "deadbeef");
        assert_eq!(format!("{:#x}", a(0xdead_beef)), "0xdeadbeef");
        assert_eq!(format!("{:#010x}", a(0x1000)), "0x00001000", "width and fill survive");
        assert_eq!(format!("{:x}", l(0x1000)), "1000");
        assert_eq!(format!("{:#x}", l(0x1000)), "0x1000");
        assert_eq!(format!("{}", l(4096)), "4096", "Display is decimal");
        assert_eq!(format!("{:>8}", l(42)), "      42", "alignment survives");
        // Debug is the other one, and it is deliberately different.
        assert_eq!(format!("{:?}", a(0x10)), "GuestAddr(16)");
    }
}
