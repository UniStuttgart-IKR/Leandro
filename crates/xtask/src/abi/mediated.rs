// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Keeping `[footprint.mediated]` from falling behind the code.
//!
//! The mediated list decides three things: what `RmAbi` abstracts over, what
//! stops the generator when it breaks, and therefore what a person is asked
//! about. A list like that is worth exactly as much as its completeness, and
//! nothing about writing code reminds anybody to extend it.
//!
//! So it is checked rather than trusted. Every `sys::NAME` the workspace
//! writes is read out of the sources; if one of them names a type that is not
//! the same on every supported version and is not in the list, this fails and
//! says which. The reverse is deliberately NOT checked: a name stays on the
//! list after its last caller becomes generic, which is the whole point --
//! otherwise making a caller generic would remove the type from the trait it
//! was made generic over.

use crate::abi::config::Footprint;
use crate::abi::emit::{Partition, Version};
use anyhow::{bail, Result};
use std::collections::BTreeSet;
use std::path::Path;

pub fn assert_complete(
    root: &Path,
    fp: &Footprint,
    part: &Partition,
    versions: &[Version],
) -> Result<()> {
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut direct_imports: Vec<String> = Vec::new();
    let crates = root.join("crates");
    scan(&crates, &mut used, &mut direct_imports)?;

    // Importing the MACHINERY by name is fine and is how a caller becomes
    // generic: `use nvrm_sys::RmAbi;`. What must not be imported by name is a
    // BOUND type or constant, because then it no longer reads as `sys::NAME`
    // and the loop below cannot see it.
    let bound: BTreeSet<&str> = versions
        .iter()
        .flat_map(|v| {
            v.manifest
                .types
                .keys()
                .chain(v.manifest.constants.keys())
                .map(String::as_str)
        })
        .collect();
    direct_imports.retain(|line| {
        line.split("nvrm_sys::")
            .skip(1)
            .flat_map(|rest| {
                rest.trim_start_matches('{')
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            })
            .any(|n| bound.contains(n))
    });

    if !direct_imports.is_empty() {
        bail!(
            "these files import items out of nvrm-sys directly:\n  {}\n\
             Everything goes through `sys::NAME` (nvrm-abi re-exports the crate as `sys`), \
             because that spelling is what the mediated-list check can see. An item \
             imported by name is invisible to it, and a type that is invisible to it can \
             move between driver versions without anybody being asked.",
            direct_imports.join("\n  ")
        );
    }

    let listed: BTreeSet<&str> = fp.mediated.values().map(String::as_str).collect();
    let mut missing: Vec<String> = Vec::new();
    let mut volatile_constants: Vec<String> = Vec::new();

    for name in &used {
        if !part.volatile.contains(name) {
            continue;
        }
        let is_type = versions.iter().any(|v| v.manifest.types.contains_key(name));
        let is_const = versions
            .iter()
            .any(|v| v.manifest.constants.contains_key(name));
        if is_type && !listed.contains(name.as_str()) {
            missing.push(name.clone());
        } else if is_const && !is_type {
            volatile_constants.push(name.clone());
        }
    }

    if !volatile_constants.is_empty() {
        // A constant cannot be an associated const when some supported
        // version does not define it at all -- there would be no value to
        // give that impl. Reached through its version module, and the caller
        // has to be conditional.
        eprintln!(
            "abi: version-specific constants the workspace uses, reachable only per module: {}",
            volatile_constants.join(", ")
        );
    }

    if missing.is_empty() {
        return Ok(());
    }
    let lines: Vec<String> = missing
        .iter()
        .map(|n| format!("{} = {n:?}", suggest_assoc(n)))
        .collect();
    bail!(
        "these types are used through `sys::` and are NOT the same on every supported \
         version, and [footprint.mediated] does not list them:\n  {}\n\n\
         Add them to crates/nvrm-sys/abi.toml, with the name they should get in RmAbi:\n\n\
         [footprint.mediated]\n{}\n",
        missing.join("\n  "),
        lines.join("\n")
    )
}

/// A starting point for the associated-type name, not an answer: NVIDIA's
/// spelling carried into Rust reads badly and a person should pick.
fn suggest_assoc(c_name: &str) -> String {
    let mut out = String::new();
    for part in c_name.split('_') {
        let mut cs = part.chars();
        if let Some(f) = cs.next() {
            out.push(f.to_ascii_uppercase());
            out.extend(cs.map(|c| c.to_ascii_lowercase()));
        }
    }
    out
}

fn scan(dir: &Path, used: &mut BTreeSet<String>, imports: &mut Vec<String>) -> Result<()> {
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            // The generated crate names its own types, and this crate names
            // them in prose. Neither is a caller.
            if name == "target" || name == "manifests" {
                continue;
            }
            if p.ends_with("crates/nvrm-sys") || p.ends_with("crates/xtask") {
                continue;
            }
            scan(&p, used, imports)?;
            continue;
        }
        if p.extension().and_then(|x| x.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&p)?;
        for (i, m) in text.match_indices("sys::") {
            // `nvrm_sys::` ends with the same five characters; both are the
            // spelling this check understands.
            let rest = &text[i + m.len()..];
            let n: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !n.is_empty() {
                used.insert(n);
            }
        }
        for line in text.lines() {
            let t = line.trim_start().trim_start_matches("pub ");
            if t.starts_with("use nvrm_sys::") && !t.contains(" as sys") {
                imports.push(format!("{}: {}", p.display(), line.trim()));
            }
        }
    }
    Ok(())
}
