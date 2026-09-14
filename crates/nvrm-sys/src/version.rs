// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Which driver is actually running.
//!
//! The one hand-written file in this crate. Everything beside it is generated
//! by `cargo xtask abi` from `abi.toml` and the vendored headers; this is
//! logic rather than layout, so it lives here and the generator leaves it
//! alone.

/// Reads the version of the running kernel driver.
///
/// Must be checked at the start of every host binary that talks to RM (guest
/// side diagnostics deliberately skip it). Struct layouts are version
/// specific, and a binary that guesses wrong does not fail -- it reads the
/// wrong bytes out of a struct that is the right size.
pub fn running_driver_version() -> std::io::Result<String> {
    let s = std::fs::read_to_string("/proc/driver/nvidia/version")?;
    s.split_whitespace()
        .find(|t| t.split('.').count() == 3 && t.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_owned)
        .ok_or_else(|| std::io::Error::other("version not parsable"))
}

/// Panic rather than silently misinterpret struct offsets.
///
/// This is the SINGLE-VERSION check, and it stays what it always was: the
/// running driver must be the one this build defaults to. A binary that is
/// generic over [`crate::RmAbi`] wants [`detect`] instead, which answers
/// which of the supported versions is running rather than insisting on one.
pub fn assert_driver_version() {
    let running = running_driver_version().expect("/proc/driver/nvidia/version not readable");
    assert_eq!(
        running,
        crate::DRIVER_VERSION,
        "driver mismatch: running {running}, bindings for {}",
        crate::DRIVER_VERSION
    );
}

/// Which supported driver is running, or an error naming what was found.
///
/// The refusal is deliberate and is the whole argument of this crate: a
/// version with no entry in `abi.toml` has not been measured, and the nearest
/// version is not an approximation of it. One field moved by four bytes and
/// every call after it is wrong, with no error anywhere -- guest and host
/// agree perfectly about a struct neither of them has.
pub fn detect() -> std::io::Result<crate::DriverVersion> {
    let running = running_driver_version()?;
    crate::DriverVersion::from_version_string(&running).ok_or_else(|| {
        std::io::Error::other(format!(
            "driver {running} is not supported: this build carries {}. \
             Adding it is one entry in crates/nvrm-sys/abi.toml plus \
             `scripts/build.sh vendor-abi {running}` and `cargo xtask abi`.",
            crate::SUPPORTED_VERSIONS.join(", ")
        ))
    })
}
