// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! The tracer's FD table: which FD refers to which NVIDIA device node.
//!
//! The dullest and most likely source of error here. CUDA is
//! multithreaded, opens /dev/nvidiactl several times (one mmap context per
//! FD!), duplicates FDs and forks.
//!
//! Deliberately NO RwLock, for three reasons:
//!   1. fork(): if another thread holds the lock at the moment of the
//!      fork, it stays locked forever in the child. No owner, no
//!      poisoning, just deadlock. pthread_atfork could catch that with
//!      prepare/parent/child, but not for std::sync::RwLock - that would
//!      require a raw pthread_rwlock_t and a guard spanning three C
//!      callbacks.
//!   2. The path is hot: every ioctl looks up here.
//!   3. An array of atomics is async-signal-safe, never allocates and
//!      lives entirely in .bss.
//!
//! Price: a fixed upper bound on FD numbers. 65536 covers every realistic
//! RLIMIT_NOFILE, and FDs above it are counted rather than silently
//! dropped.

use crate::NvDev;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

const MAX_FD: usize = 65536;

const NONE: u8 = 0;
const CTL: u8 = 1;
const UVM: u8 = 2;
const UVM_TOOLS: u8 = 3;
const EVENT: u8 = 4;
// The DRM nodes. Two codes rather than one because "which node did this
// ioctl go to" is the whole point of tracing them -- card and render node
// take different paths through the driver.
const DRM_CARD: u8 = 5;
const DRM_RENDER: u8 = 6;
const GPU_BASE: u8 = 0x80; // 0x80 | index

static TABLE: [AtomicU8; MAX_FD] = [const { AtomicU8::new(NONE) }; MAX_FD];

/// FDs beyond MAX_FD. If this is ever != 0, the trace is incomplete - and
/// that must be visible, not guessed at.
static OVERFLOW: AtomicU64 = AtomicU64::new(0);

fn encode(d: NvDev) -> u8 {
    match d {
        NvDev::Ctl => CTL,
        NvDev::Uvm => UVM,
        NvDev::UvmTools => UVM_TOOLS,
        NvDev::Event => EVENT,
        NvDev::Drm(false) => DRM_CARD,
        NvDev::Drm(true) => DRM_RENDER,
        NvDev::Gpu(n) => GPU_BASE | (n as u8 & 0x7f),
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
        g => Some(NvDev::Gpu((g & 0x7f) as u32)),
    }
}

/// Relaxed is enough: the value is self-contained, no other data has to
/// become visible together with it.
pub fn insert(fd: i32, dev: NvDev) {
    match slot(fd) {
        Some(s) => s.store(encode(dev), Ordering::Relaxed),
        None => {
            OVERFLOW.fetch_add(1, Ordering::Relaxed);
        }
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

#[allow(dead_code)] // counterpart to the counter above, read when diagnosing
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

    /// Where the tests write. The table is a process-global static and the
    /// tracer's own hooks are linked into the test binary, so a test that
    /// used a plausible FD number could collide with one the harness really
    /// holds. Nothing in a test process opens FD 60000, and no test here
    /// ever touches an FD below 1000.
    const T: i32 = 60_000;

    /// Every `NvDev` survives the single byte the table stores for it. That
    /// byte IS the table: an encoding that lost a variant would mislabel
    /// every later line for that FD, and the GPU index has the least room
    /// of all -- seven bits, so 127 is the last index that still round
    /// trips (a 128th GPU would come back as `Gpu(0)`).
    #[test]
    fn every_device_kind_survives_encode_and_decode() {
        let all = [
            NvDev::Ctl,
            NvDev::Uvm,
            NvDev::UvmTools,
            NvDev::Event,
            NvDev::Drm(false),
            NvDev::Drm(true),
            NvDev::Gpu(0),
            NvDev::Gpu(5),
            NvDev::Gpu(127),
        ];
        for d in all {
            assert_eq!(decode(encode(d)), Some(d), "{d:?}");
        }
        // `NONE` is the empty slot and the one value that must decode to
        // nothing, so no device may encode to it.
        assert_eq!(decode(NONE), None);
        assert!(all.iter().all(|d| encode(*d) != NONE));
    }

    /// The table is per FD and nothing else: an insert answers only for its
    /// own FD, a remove clears only its own. This is the whole reason the
    /// tracer follows dup(2) rather than the path -- CUDA duplicates FDs,
    /// and a neighbouring slot going along would silently move calls to the
    /// wrong device in the trace.
    #[test]
    fn insert_get_and_remove_act_on_one_fd_at_a_time() {
        assert_eq!(get(T + 4), None, "an FD nobody inserted is unknown");

        insert(T + 1, NvDev::Ctl);
        insert(T + 2, NvDev::Gpu(3));
        insert(T + 3, NvDev::Event);
        assert_eq!(get(T + 1), Some(NvDev::Ctl));
        assert_eq!(get(T + 2), Some(NvDev::Gpu(3)));
        assert_eq!(get(T + 3), Some(NvDev::Event));

        // An insert on a live FD overwrites it -- dup2(2) reuses numbers.
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

    /// An FD the array cannot hold is COUNTED, never silently dropped: a
    /// non-zero counter is the only thing that tells a reader the trace is
    /// incomplete rather than empty.
    ///
    /// A negative FD cannot be stored either and lands in the SAME counter,
    /// which is worth knowing when reading it -- `OVERFLOW` is documented as
    /// "FDs beyond MAX_FD" and a negative FD is not that. No caller can
    /// produce one today: `note_open` and dup/dup2/dup3 all test for a
    /// negative return before they get here.
    #[test]
    fn fds_the_array_cannot_hold_are_counted_and_never_stored() {
        assert_eq!(get(-1), None);
        assert_eq!(get(MAX_FD as i32), None);

        let before = overflow_count();
        insert(MAX_FD as i32, NvDev::Ctl);
        assert_eq!(overflow_count(), before + 1, "an FD past the table is counted");
        assert_eq!(get(MAX_FD as i32), None, "and it is not readable afterwards");

        insert(-1, NvDev::Ctl);
        assert_eq!(overflow_count(), before + 2);
        assert_eq!(get(-1), None);

        // Neither may fault: `close(-1)` reaches this on every failed open.
        remove(-1);
        remove(MAX_FD as i32);
    }
}
