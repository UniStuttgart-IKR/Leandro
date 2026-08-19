// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Raw bindings to the NVIDIA SDK headers, for exactly one driver version.
//!
//! Everything here is generated. If something looks ugly (MaybeUninit,
//! anonymous unions, `__bindgen_anon_1`) that is expected and no reason to
//! touch it up by hand - the next version bump would discard the hand
//! work anyway.
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]
#![allow(dead_code, clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// Driver version these bindings were generated against.
pub const DRIVER_VERSION: &str = env!("LEA_DRIVER_VERSION");

/// Reads the version of the running kernel driver.
///
/// Must be checked against [`DRIVER_VERSION`] at the start of every host
/// binary that talks to RM (guest-side diagnostics deliberately skip it).
/// Struct layouts are version specific, so this lockstep is the real price
/// of the approach and must not be broken silently.
pub fn running_driver_version() -> std::io::Result<String> {
    let s = std::fs::read_to_string("/proc/driver/nvidia/version")?;
    s.split_whitespace()
        .find(|t| t.split('.').count() == 3 && t.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_owned)
        .ok_or_else(|| std::io::Error::other("version not parsable"))
}

/// Panic rather than silently misinterpret struct offsets.
pub fn assert_driver_version() {
    let running = running_driver_version().expect("/proc/driver/nvidia/version not readable");
    assert_eq!(
        running, DRIVER_VERSION,
        "driver mismatch: running {running}, bindings for {DRIVER_VERSION}"
    );
}
