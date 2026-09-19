// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Guest addresses and lengths with checked arithmetic helpers.
//!
//! Overflow must fail before a guest range becomes a host pointer or mapping.
//! Prefer these helpers for arithmetic; use `get()` at syscall and wire-format
//! boundaries, checking any conversion to a narrower host type.

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

    /// Raw value for syscall and wire-format boundaries.
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

    /// Raw value for syscall and wire-format boundaries.
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

    /// A wrapping end must not pass a range's upper-bound check.
    #[test]
    fn end_is_none_exactly_when_the_sum_wraps() {
        assert_eq!(a(0x1000).end(l(0x1000)), Some(a(0x2000)));
        assert_eq!(a(0).end(l(0)), Some(a(0)), "zero length is not an error");
        assert_eq!(
            a(u64::MAX).end(l(0)),
            Some(a(u64::MAX)),
            "the last byte, no wrap"
        );
        // The first three that DO wrap, at the exact boundary.
        assert_eq!(a(u64::MAX).end(l(1)), None);
        assert_eq!(a(1).end(l(u64::MAX)), None);
        assert_eq!(a(u64::MAX).end(l(u64::MAX)), None);
        // And the largest sum that still fits.
        assert_eq!(a(1).end(l(u64::MAX - 1)), Some(a(u64::MAX)));
    }

    /// An address below the base must not become a large unsigned offset.
    #[test]
    fn offset_from_refuses_addresses_below_the_base() {
        assert_eq!(a(0x2000).offset_from(a(0x1000)), Some(0x1000));
        assert_eq!(
            a(0x1000).offset_from(a(0x1000)),
            Some(0),
            "the base itself is offset 0"
        );
        assert_eq!(
            a(0x0fff).offset_from(a(0x1000)),
            None,
            "one byte below the base"
        );
        assert_eq!(a(0).offset_from(a(1)), None);
        assert_eq!(
            a(u64::MAX).offset_from(a(0)),
            Some(u64::MAX),
            "the widest legal distance"
        );
    }

    /// Summing guest ranges must reject overflow.
    #[test]
    fn plus_is_none_exactly_on_overflow() {
        assert_eq!(l(0x1000).plus(l(0x2000)), Some(l(0x3000)));
        assert_eq!(l(0).plus(l(0)), Some(l(0)));
        assert_eq!(
            l(u64::MAX).plus(l(0)),
            Some(l(u64::MAX)),
            "the largest sum that fits"
        );
        assert_eq!(l(u64::MAX).plus(l(1)), None);
        assert_eq!(l(u64::MAX).plus(l(u64::MAX)), None);
        // A wrapping sum must not be mistaken for a small legal one: this
        // is exactly the shape `Arena::build` was fooled by.
        let big = l(2 << 20);
        assert_eq!(big.plus(l(0x1000u64.wrapping_sub(2 << 20))), None);
    }

    #[test]
    fn is_zero_is_true_only_for_zero() {
        assert!(l(0).is_zero());
        assert!(GuestLen::default().is_zero(), "the default length is 0");
        assert!(!l(1).is_zero());
        assert!(!l(u64::MAX).is_zero());
    }

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

    /// Preserve numeric formatting flags so logs match driver traces.
    #[test]
    fn formatting_prints_the_bare_number() {
        assert_eq!(format!("{:x}", a(0xdead_beef)), "deadbeef");
        assert_eq!(format!("{:#x}", a(0xdead_beef)), "0xdeadbeef");
        assert_eq!(
            format!("{:#010x}", a(0x1000)),
            "0x00001000",
            "width and fill survive"
        );
        assert_eq!(format!("{:x}", l(0x1000)), "1000");
        assert_eq!(format!("{:#x}", l(0x1000)), "0x1000");
        assert_eq!(format!("{}", l(4096)), "4096", "Display is decimal");
        assert_eq!(format!("{:>8}", l(42)), "      42", "alignment survives");
        // Debug is the other one, and it is deliberately different.
        assert_eq!(format!("{:?}", a(0x10)), "GuestAddr(16)");
    }
}
