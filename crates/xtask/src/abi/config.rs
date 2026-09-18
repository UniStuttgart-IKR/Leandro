// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! `crates/nvrm-sys/abi.toml` -- the version set and the footprint.
//!
//! This is one of exactly two hand-maintained inputs; the other is the
//! vendored headers it names. Nothing here describes a LAYOUT, and nothing
//! here may: a layout is measured from the headers or it is an error.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct AbiToml {
    pub versions: BTreeMap<String, VersionEntry>,
    pub footprint: Footprint,
}

#[derive(Debug, Deserialize)]
pub struct VersionEntry {
    /// The upstream tag, spelled `open-gpu-kernel-modules@<tag>`. The tag is
    /// also the directory name under `vendor/nvidia-rm-headers/`.
    pub headers: String,
    /// Informational. Datacenter drivers are tagged minor releases of the
    /// same branch numbers, so they are entries like any other.
    #[allow(dead_code)]
    pub branch: Option<String>,
    /// Written by a person when the generator refuses to emit a mediated
    /// struct whose layout broke at this version: the key is the struct, the
    /// value is the layout module it takes. It is an acknowledgement, not a
    /// description -- the layout itself is still measured.
    #[serde(default)]
    pub layout: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct Footprint {
    /// bindgen `allowlist_type` patterns, handed to bindgen verbatim.
    pub structs: Vec<String>,
    /// bindgen `allowlist_var` patterns, handed to bindgen verbatim.
    pub constants: Vec<String>,
    /// The types the BOUNDARY reads that are not the same on every supported
    /// version, mapped to the name they get in `RmAbi`.
    ///
    /// Three jobs, one list. It is the set of associated types; it is the set
    /// whose breakage stops the generator until a person acknowledges it; and
    /// it is checked for completeness against the types the workspace
    /// actually names, so it cannot quietly fall behind the code.
    ///
    /// A type that never moves does not belong here: it lives in `stable` and
    /// is used directly.
    #[serde(default)]
    pub mediated: BTreeMap<String, String>,
    /// The CONSTANTS the boundary reads whose value is not the same on every
    /// supported version -- including "this version does not define it".
    ///
    /// They cannot be associated types and they cannot be plain associated
    /// consts either, because there is no value to give an implementation for
    /// a version whose headers do not have the name. They are emitted as
    /// `Option`, so "absent here" is a value a caller has to handle rather
    /// than a compile error it routes around.
    #[serde(default)]
    pub mediated_constants: BTreeMap<String, String>,
    /// Types NVIDIA renamed: the name this crate uses, mapped to what each
    /// version's headers call it. Applied before anything is measured, so
    /// everything downstream sees one name.
    #[serde(default)]
    pub renamed: BTreeMap<String, BTreeMap<String, String>>,
}

impl Footprint {
    /// The renames that apply to one version: old name -> the name to use.
    pub fn renames_for(&self, version: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .renamed
            .iter()
            .filter_map(|(canonical, per_version)| {
                per_version
                    .get(version)
                    .map(|old| (old.clone(), canonical.clone()))
            })
            .collect();
        out.sort();
        out
    }
}

impl AbiToml {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: AbiToml =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if cfg.versions.is_empty() {
            bail!("{} names no versions", path.display());
        }
        for (v, e) in &cfg.versions {
            let want = format!("open-gpu-kernel-modules@{v}");
            if e.headers != want {
                bail!(
                    "{}: [versions.\"{v}\"] headers = \"{}\", expected \"{want}\" -- \
                     the entry key is the tag and the directory name, so the two cannot differ",
                    path.display(),
                    e.headers
                );
            }
        }
        Ok(cfg)
    }

    /// Versions in driver order rather than in string order: 610.57.04 comes
    /// after 610.43.02, which `sort` on the string would also get right, and
    /// 595.99.02 comes before 610.43.02, which it would not.
    pub fn ordered(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.versions.keys().map(String::as_str).collect();
        v.sort_by_key(|s| numeric_key(s));
        v
    }
}

fn numeric_key(v: &str) -> Vec<u64> {
    v.split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect()
}
