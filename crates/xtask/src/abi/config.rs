// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! ABI versions and bindgen allowlists from `crates/nvrm-sys/abi.toml`.
//! Layouts are measured from the selected headers.

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
    /// `open-gpu-kernel-modules@<tag>`; tag names the vendored directory.
    pub headers: String,
    /// Optional branch label; does not affect generated bindings.
    #[allow(dead_code)]
    pub branch: Option<String>,
    /// Explicitly accepted layout changes: struct name to layout module.
    #[serde(default)]
    pub layout: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct Footprint {
    /// bindgen `allowlist_type` patterns, handed to bindgen verbatim.
    pub structs: Vec<String>,
    /// bindgen `allowlist_var` patterns, handed to bindgen verbatim.
    pub constants: Vec<String>,
    /// Version-dependent types mapped to `RmAbi` associated type names.
    /// Checked against workspace uses and explicit layout acknowledgements.
    #[serde(default)]
    pub mediated: BTreeMap<String, String>,
    /// Version-dependent constants emitted as `Option` to represent absence.
    #[serde(default)]
    pub mediated_constants: BTreeMap<String, String>,
    /// Canonical type names mapped to each version's upstream spelling.
    /// Renamed before layout measurement.
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

    /// Sort versions by numeric components.
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
