// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Check that version-dependent workspace types appear in `[footprint.mediated]`.
//! Entries remain required after callers switch to `RmAbi` associated types.

use crate::abi::config::Footprint;
use crate::abi::emit::{Partition, Version};
use anyhow::{bail, Context, Result};
use proc_macro2::{TokenStream, TokenTree};
use std::collections::BTreeSet;
use std::path::Path;
use syn::visit::{self, Visit};
use syn::UseTree;

pub fn assert_complete(
    root: &Path,
    fp: &Footprint,
    part: &Partition,
    versions: &[Version],
) -> Result<()> {
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
    let mut used = BTreeSet::new();
    let mut direct_imports = Vec::new();
    scan(&root.join("crates"), &bound, &mut used, &mut direct_imports)?;

    if !direct_imports.is_empty() {
        direct_imports.sort();
        bail!(
            "direct or glob imports of nvrm-sys bindings:\n  {}\n\
             Use `nvrm_abi::sys` and spell bindings as `sys::NAME` so the mediated check sees them.",
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
        // Direct version-specific constants require conditional callers.
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

/// Suggest an associated-type name for review.
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

fn scan(
    dir: &Path,
    bound: &BTreeSet<&str>,
    used: &mut BTreeSet<String>,
    imports: &mut Vec<String>,
) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        if p.is_dir() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            // Exclude generated bindings and the generator itself.
            if name == "target" || name == "manifests" {
                continue;
            }
            if p.ends_with("crates/nvrm-sys") || p.ends_with("crates/xtask") {
                continue;
            }
            scan(&p, bound, used, imports)?;
            continue;
        }
        if p.extension().and_then(|x| x.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&p)?;
        let found = scan_source(&text, bound)
            .with_context(|| format!("scanning bindings in {}", p.display()))?;
        used.extend(found.used);
        imports.extend(
            found
                .direct_imports
                .into_iter()
                .map(|path| format!("{}: {path}", p.display())),
        );
    }
    Ok(())
}

struct SourceUses<'a> {
    bound: &'a BTreeSet<&'a str>,
    used: BTreeSet<String>,
    direct_imports: BTreeSet<String>,
}

fn scan_source<'a>(text: &str, bound: &'a BTreeSet<&'a str>) -> Result<SourceUses<'a>> {
    let file = syn::parse_file(text)?;
    let mut found = SourceUses {
        bound,
        used: BTreeSet::new(),
        direct_imports: BTreeSet::new(),
    };
    found.visit_file(&file);
    Ok(found)
}

impl SourceUses<'_> {
    fn path(&mut self, path: &[String]) {
        for pair in path.windows(2) {
            if matches!(pair[0].as_str(), "sys" | "nvrm_sys") {
                self.used.insert(pair[1].clone());
            }
        }
    }

    fn use_tree(&mut self, tree: &UseTree, path: &mut Vec<String>) {
        let name = match tree {
            UseTree::Path(p) => {
                path.push(p.ident.to_string());
                self.use_tree(&p.tree, path);
                path.pop();
                return;
            }
            UseTree::Group(g) => {
                for item in &g.items {
                    self.use_tree(item, path);
                }
                return;
            }
            UseTree::Name(n) => n.ident.to_string(),
            UseTree::Rename(r) => r.ident.to_string(),
            UseTree::Glob(_) => "*".to_string(),
        };
        path.push(name);
        self.path(path);
        if path[0] == "nvrm_sys"
            && path
                .iter()
                .skip(1)
                .any(|n| n == "*" || self.bound.contains(n.as_str()))
        {
            self.direct_imports.insert(path.join("::"));
        }
        path.pop();
    }

    // Macro bodies are unparsed tokens; inspect paths without counting literals.
    fn macro_tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<_> = tokens.into_iter().collect();
        for token in &tokens {
            if let TokenTree::Group(group) = token {
                self.macro_tokens(group.stream());
            }
        }
        for window in tokens.windows(4) {
            if let [TokenTree::Ident(root), TokenTree::Punct(a), TokenTree::Punct(b), TokenTree::Ident(name)] =
                window
            {
                if a.as_char() == ':' && b.as_char() == ':' {
                    self.path(&[root.to_string(), name.to_string()]);
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for SourceUses<'_> {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.path(
            &path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>(),
        );
        visit::visit_path(self, path);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.use_tree(&item.tree, &mut Vec::new());
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.macro_tokens(mac.tokens.clone());
        visit::visit_macro(self, mac);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_ignore_comments_literals_and_identifier_suffixes() {
        let bound = BTreeSet::new();
        let found = scan_source(
            r#"
                // sys::COMMENT
                /// nvrm_sys::DOC
                const TEXT: &str = "sys::STRING";
                type A = sys ::
                    TYPE_A;
                type B = nvrm_sys::TYPE_B;
                type C = unrelated_sys::TYPE_C;
                fn f() { let _: Option<sys::TYPE_D> = None; }
            "#,
            &bound,
        )
        .unwrap();
        assert_eq!(
            found.used,
            BTreeSet::from_iter(["TYPE_A", "TYPE_B", "TYPE_D"].map(String::from))
        );
        assert!(found.direct_imports.is_empty());
    }

    #[test]
    fn direct_imports_include_multiline_groups_visibility_and_renames() {
        let bound = BTreeSet::from(["TYPE_A", "TYPE_B", "CONST_C", "TYPE_D"]);
        let found = scan_source(
            r#"
                use nvrm_sys::{
                    TYPE_A,
                    TYPE_B as sys,
                    RmAbi,
                };
                pub(crate) use ::nvrm_sys::v610::{CONST_C as Other};
                use {nvrm_sys::TYPE_D, std::fmt::Debug};
            "#,
            &bound,
        )
        .unwrap();
        assert_eq!(
            found.direct_imports,
            BTreeSet::from_iter(
                [
                    "nvrm_sys::TYPE_A",
                    "nvrm_sys::TYPE_B",
                    "nvrm_sys::v610::CONST_C",
                    "nvrm_sys::TYPE_D",
                ]
                .map(String::from)
            )
        );
    }

    #[test]
    fn glob_imports_are_rejected_but_sys_reexports_and_rmabi_are_allowed() {
        let bound = BTreeSet::from(["TYPE_A"]);
        let found = scan_source(
            r#"
                pub use nvrm_sys as sys;
                use nvrm_sys::{self as sys, RmAbi};
                mod nested {
                    use nvrm_sys::*;
                    fn f() { use nvrm_sys::v610::*; }
                }
            "#,
            &bound,
        )
        .unwrap();
        assert_eq!(
            found.direct_imports,
            BTreeSet::from_iter(["nvrm_sys::*", "nvrm_sys::v610::*"].map(String::from))
        );
    }

    #[test]
    fn grouped_sys_imports_record_original_names() {
        let bound = BTreeSet::new();
        let found = scan_source("use sys::{TYPE_A as Local, TYPE_B};", &bound).unwrap();
        assert_eq!(
            found.used,
            BTreeSet::from_iter(["TYPE_A", "TYPE_B"].map(String::from))
        );
    }

    #[test]
    fn macro_paths_are_scanned_without_string_contents() {
        let bound = BTreeSet::new();
        let found = scan_source(
            r#"
                macro_rules! layout {
                    () => { size_of::<sys::TYPE_A>() };
                }
                layout_check!(nvrm_sys::TYPE_B, [sys::TYPE_C]);
                log!("sys::STRING", r"nvrm_sys::RAW_STRING");
            "#,
            &bound,
        )
        .unwrap();
        assert_eq!(
            found.used,
            BTreeSet::from_iter(["TYPE_A", "TYPE_B", "TYPE_C"].map(String::from))
        );
    }
}
