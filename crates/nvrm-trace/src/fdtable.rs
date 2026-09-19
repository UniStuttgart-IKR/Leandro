// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! FD-to-device tags without locks or allocation.
//! A forked child cannot inherit a lock held by another thread. Each slot is
//! self-contained, so relaxed atomic loads/stores suffice. Untrackable FDs
//! increment a counter reported at normal exit.

use crate::NvDev;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

const MAX_FD: usize = 65536;

const NONE: u8 = 0;
const CTL: u8 = 1;
const UVM: u8 = 2;
const UVM_TOOLS: u8 = 3;
const EVENT: u8 = 4;
const DRM_CARD: u8 = 5;
const DRM_RENDER: u8 = 6;
const MODESET: u8 = 7;
const GPU: u8 = 8;

static TABLE: [AtomicU8; MAX_FD] = [const { AtomicU8::new(NONE) }; MAX_FD];

/// Insertions outside the table bounds.
static OVERFLOW: AtomicU64 = AtomicU64::new(0);

fn encode(d: NvDev) -> u8 {
    match d {
        NvDev::Ctl => CTL,
        NvDev::Uvm => UVM,
        NvDev::UvmTools => UVM_TOOLS,
        NvDev::Event => EVENT,
        NvDev::Drm(false) => DRM_CARD,
        NvDev::Drm(true) => DRM_RENDER,
        NvDev::Modeset => MODESET,
        NvDev::Gpu => GPU,
    }
}

fn decode(v: u8) -> Option<NvDev> {
    match v {
        NONE => None,
        CTL => Some(NvDev::Ctl),
        UVM => Some(NvDev::Uvm),
        UVM_TOOLS => Some(NvDev::UvmTools),
        EVENT => Some(NvDev::Event),
        DRM_CARD => Some(NvDev::Drm(false)),
        DRM_RENDER => Some(NvDev::Drm(true)),
        MODESET => Some(NvDev::Modeset),
        GPU => Some(NvDev::Gpu),
        _ => None,
    }
}

pub fn insert(fd: i32, dev: NvDev) {
    match slot(fd) {
        Some(s) => s.store(encode(dev), Ordering::Relaxed),
        None => {
            OVERFLOW.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Copy after a successful dup call; oldfd == newfd must preserve its tag.
pub fn duplicate(oldfd: i32, newfd: i32) {
    match get(oldfd) {
        Some(dev) => insert(newfd, dev),
        None => remove(newfd),
    }
}

pub fn get(fd: i32) -> Option<NvDev> {
    decode(slot(fd)?.load(Ordering::Relaxed))
}

pub fn remove(fd: i32) {
    if let Some(s) = slot(fd) {
        s.store(NONE, Ordering::Relaxed);
    }
}

pub fn overflow_count() -> u64 {
    OVERFLOW.load(Ordering::Relaxed)
}

fn slot(fd: i32) -> Option<&'static AtomicU8> {
    if fd < 0 {
        return None;
    }
    TABLE.get(fd as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reserved slots avoid descriptors used by the interposed test harness.
    const T: i32 = 60_000;

    #[test]
    fn every_device_kind_survives_encode_and_decode() {
        let all = [
            NvDev::Ctl,
            NvDev::Uvm,
            NvDev::UvmTools,
            NvDev::Event,
            NvDev::Drm(false),
            NvDev::Drm(true),
            NvDev::Modeset,
            NvDev::Gpu,
        ];
        for d in all {
            assert_eq!(decode(encode(d)), Some(d), "{d:?}");
        }
        assert_eq!(decode(NONE), None);
        assert!(all.iter().all(|d| encode(*d) != NONE));
    }

    #[test]
    fn insert_get_and_remove_act_on_one_fd_at_a_time() {
        assert_eq!(get(T + 4), None, "an FD nobody inserted is unknown");

        insert(T + 1, NvDev::Ctl);
        insert(T + 2, NvDev::Gpu);
        insert(T + 3, NvDev::Event);
        assert_eq!(get(T + 1), Some(NvDev::Ctl));
        assert_eq!(get(T + 2), Some(NvDev::Gpu));
        assert_eq!(get(T + 3), Some(NvDev::Event));

        insert(T + 2, NvDev::Uvm);
        assert_eq!(get(T + 2), Some(NvDev::Uvm));

        remove(T + 2);
        assert_eq!(get(T + 2), None);
        assert_eq!(get(T + 1), Some(NvDev::Ctl), "the neighbours stay");
        assert_eq!(get(T + 3), Some(NvDev::Event));

        remove(T + 1);
        remove(T + 3);
        assert_eq!(get(T + 1), None);
        assert_eq!(get(T + 3), None);
    }

    #[test]
    fn duplicate_preserves_self_and_replaces_the_destination_tag() {
        insert(T + 10, NvDev::Gpu);
        duplicate(T + 10, T + 10);
        assert_eq!(get(T + 10), Some(NvDev::Gpu));

        insert(T + 11, NvDev::Uvm);
        duplicate(T + 10, T + 11);
        assert_eq!(get(T + 11), Some(NvDev::Gpu));

        duplicate(T + 12, T + 11);
        assert_eq!(get(T + 11), None);
        assert_eq!(get(T + 10), Some(NvDev::Gpu));
        remove(T + 10);
    }

    #[test]
    fn fds_the_array_cannot_hold_are_counted_and_never_stored() {
        assert_eq!(get(-1), None);
        assert_eq!(get(MAX_FD as i32), None);

        let before = overflow_count();
        insert(MAX_FD as i32, NvDev::Ctl);
        assert_eq!(
            overflow_count(),
            before + 1,
            "an FD past the table is counted"
        );
        assert_eq!(
            get(MAX_FD as i32),
            None,
            "and it is not readable afterwards"
        );

        insert(-1, NvDev::Ctl);
        assert_eq!(overflow_count(), before + 2);
        assert_eq!(get(-1), None);

        remove(-1);
        remove(MAX_FD as i32);
    }
}
