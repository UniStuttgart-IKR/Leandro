// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Writing the crate.
//!
//! The source is COMMITTED, not produced in `build.rs`. Two reasons, and the
//! second is the one that matters: a committed crate needs no libclang and no
//! vendored headers to build, and a committed crate can be READ -- the layout
//! a caller compiles against is in the tree, next to the manifest that
//! measured it, and a reviewer can compare them without running anything.
//!
//! What goes where:
//!
//!   * `src/stable.rs` -- every name that is the same on every version in
//!     abi.toml, and whose references are themselves stable. Used directly;
//!     these never appear in `RmAbi`.
//!   * `src/v<NNN>.rs` -- one module per version whose content differs, each
//!     carrying only what is not stable, plus `pub use super::stable::*` so
//!     that a module is a complete view of its version.
//!   * `src/lib.rs` -- the module list, `DriverVersion`, `RmAbi` and its
//!     impls.
//!   * `versions.toml` -- what was measured, hashed.
//!
//! Nothing here invents a layout. Every number written into an
//! `assert_layout!` comes out of the manifest, which came out of bindgen,
//! which came out of the vendored headers.

use crate::abi::classify::{self, Verdict};
use crate::abi::config::AbiToml;
use crate::abi::items::{render, GroupKind, ItemGroup, Items};
use crate::abi::manifest::Manifest;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use syn::Item;

pub struct Version {
    pub name: String,
    pub manifest: Manifest,
    pub items: Items,
    pub json: String,
}

/// Which names every version agrees about, and which it does not.
pub struct Partition {
    pub stable: Vec<String>,
    pub volatile: BTreeSet<String>,
}

/// A name is stable when every version spells it identically AND everything it
/// names is itself stable. The second half is not pedantry: a struct whose own
/// text never changed still cannot be shared if a struct it contains did, and
/// a struct that merely POINTS at one cannot be shared either, because the
/// module it sits in has to resolve that name to something.
pub fn partition(versions: &[Version]) -> Partition {
    let primary = &versions[0];
    let mut stable: BTreeSet<String> = primary
        .items
        .groups
        .keys()
        .filter(|n| {
            versions.iter().all(|v| {
                v.items.groups.get(*n).map(|g| &g.text)
                    == primary.items.groups.get(*n).map(|g| &g.text)
            })
        })
        .cloned()
        .collect();

    loop {
        let mut drop = Vec::new();
        for n in &stable {
            let g = &primary.items.groups[n];
            if g.refs.iter().any(|r| !stable.contains(r)) {
                drop.push(n.clone());
            }
        }
        if drop.is_empty() {
            break;
        }
        for n in drop {
            stable.remove(&n);
        }
    }

    let all: BTreeSet<String> = versions
        .iter()
        .flat_map(|v| v.items.groups.keys().cloned())
        .collect();
    let volatile = all.difference(&stable).cloned().collect();
    let ordered = primary
        .items
        .order
        .iter()
        .filter(|n| stable.contains(*n))
        .cloned()
        .collect();
    Partition {
        stable: ordered,
        volatile,
    }
}

pub struct Emitted {
    pub stale: Vec<PathBuf>,
    pub stable_count: usize,
    pub volatile_count: usize,
    pub abstracted: Vec<String>,
    pub not_abstractable: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn emit(
    root: &Path,
    cfg: &AbiToml,
    versions: &[Version],
    part: &Partition,
    primary: &str,
    check: bool,
) -> Result<Emitted> {
    let src = root.join("crates/nvrm-sys/src");
    let rustfmt = Rustfmt::for_workspace(root)?;
    let mut stale = Vec::new();
    let primary_v = versions
        .iter()
        .find(|v| v.name == primary)
        .with_context(|| format!("{primary} (DRIVER_VERSION) is not in abi.toml"))?;

    // --- src/stable.rs ----------------------------------------------------
    let mut body = String::new();
    body.push_str(&banner(
        "Every name that is identical on every driver version in abi.toml.",
        &[
            "These are used directly and never appear in `RmAbi`: there is",
            "nothing to abstract over when the bytes do not move.",
        ],
    ));
    let names: Vec<&str> = part.stable.iter().map(String::as_str).collect();
    body.push_str(&module_body(primary_v, &names, None)?);
    write_or_check(
        &src.join("stable.rs"),
        &rustfmt.format(&body)?,
        check,
        &mut stale,
    )?;

    // --- src/v<NNN>.rs ----------------------------------------------------
    for v in versions {
        let mine: Vec<&str> = v
            .items
            .order
            .iter()
            .filter(|n| !part.stable.contains(*n))
            .map(String::as_str)
            .collect();
        let mut body = String::new();
        body.push_str(&banner(
            &format!("The driver-version-specific half of {}.", v.name),
            &[
                "Everything here differs from at least one other version in",
                "abi.toml. The layout of every type was measured from the",
                "headers of this version alone -- see",
                &format!("  manifests/{}.json", v.name),
                "and the `assert_layout!` below, whose numbers come from it.",
            ],
        ));
        body.push_str("pub use super::stable::*;\n\n");
        let _ = writeln!(
            body,
            "/// Driver versions this module's layouts were measured on.\npub const VERIFIED_VERSIONS: &[&str] = &[{:?}];\n",
            v.name
        );
        body.push_str(&module_body(v, &mine, Some(&v.name))?);
        write_or_check(
            &src.join(format!("{}.rs", feature_of(&v.name))),
            &rustfmt.format(&body)?,
            check,
            &mut stale,
        )?;
    }

    // --- src/lib.rs -------------------------------------------------------
    let (lib, abstracted, not_abstractable) = lib_rs(cfg, versions, part, primary)?;
    write_or_check(
        &src.join("lib.rs"),
        &rustfmt.format(&lib)?,
        check,
        &mut stale,
    )?;

    // --- versions.toml ----------------------------------------------------
    let vt = versions_toml(cfg, versions, part, primary, &abstracted);
    write_or_check(
        &root.join("crates/nvrm-sys/versions.toml"),
        &vt,
        check,
        &mut stale,
    )?;

    // --- Cargo.toml features ---------------------------------------------
    // Every crate that carries the markers gets the same list: nvrm-sys
    // declares them, everyone else passes them through. A new crate joins by
    // pasting the two marker lines, which is one fewer place to remember.
    for e in std::fs::read_dir(root.join("crates"))?.flatten() {
        let cargo = e.path().join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&cargo) else {
            continue;
        };
        if !text.contains(BEGIN) {
            continue;
        }
        let is_sys = e.file_name() == "nvrm-sys";
        let updated = replace_region(&text, &features_block(versions, primary, is_sys))?;
        write_or_check(&cargo, &updated, check, &mut stale)?;
    }

    Ok(Emitted {
        stale,
        stable_count: part.stable.len(),
        volatile_count: part.volatile.len(),
        abstracted,
        not_abstractable,
    })
}

fn module_body(v: &Version, names: &[&str], verified: Option<&str>) -> Result<String> {
    let mut out = String::new();
    let mut asserts = String::new();
    for n in names {
        let g = &v.items.groups[*n];
        let mut items = g.items.clone();
        if let Some(ver) = verified {
            if let Some(first) = items.first_mut() {
                add_doc(first, &format!(" Layout verified for: {ver}"));
            }
        }
        out.push_str(&render(&items));
        if g.kind == GroupKind::Type {
            if let Some(a) = layout_assert(v, g) {
                asserts.push_str(&a);
            }
        }
    }
    if !asserts.is_empty() {
        out.push_str(
            "\n// The measured layout. Every number below is read out of the manifest\n\
             // beside this crate, which was read out of bindgen's own layout assertions\n\
             // for this driver version's headers. A mismatch here means the committed\n\
             // source and the committed evidence have come apart.\n",
        );
        out.push_str(&asserts);
    }
    Ok(out)
}

fn layout_assert(v: &Version, g: &ItemGroup) -> Option<String> {
    let t = v.manifest.types.get(&g.name)?;
    // An opaque forward declaration has no measured layout and nothing can
    // depend on one.
    if t.align == 0 {
        return None;
    }
    let mut s = format!(
        "assert_layout!({}, size = {}, align = {}",
        g.name, t.size, t.align
    );
    for f in &t.fields {
        // An anonymous member has no `offset_of!` assertion to make; the
        // manifest says so with a null rather than with a guess.
        if let Some(o) = f.offset {
            let _ = write!(s, ",\n    {} @ {o}", f.name);
        }
    }
    s.push_str(");\n");
    Some(s)
}

fn add_doc(item: &mut Item, text: &str) {
    let attr: syn::Attribute = syn::parse_quote!(#[doc = #text]);
    match item {
        Item::Struct(s) => s.attrs.insert(0, attr),
        Item::Union(u) => u.attrs.insert(0, attr),
        Item::Type(t) => t.attrs.insert(0, attr),
        Item::Const(c) => c.attrs.insert(0, attr),
        _ => {}
    }
}

fn lib_rs(
    cfg: &AbiToml,
    versions: &[Version],
    part: &Partition,
    primary: &str,
) -> Result<(String, Vec<String>, Vec<String>)> {
    let mut s = String::new();
    s.push_str("// SPDX-License-Identifier: MIT\n");
    s.push_str("// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>\n");
    s.push_str("// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR\n");
    s.push_str(
        "//! Raw bindings to the NVIDIA SDK headers, for every driver version\n\
         //! `abi.toml` names.\n\
         //!\n\
         //! Generated by `cargo xtask abi`. Do not edit: the next run discards\n\
         //! hand work, and CI checks that a run produces no diff.\n\
         //!\n\
         //! The shape, and why it is this shape:\n\
         //!\n\
         //!   * [`stable`] holds every name that is the same on every version.\n\
         //!     It is re-exported at the crate root, so `nvrm_sys::NVOS64_PARAMETERS`\n\
         //!     means what it always meant.\n\
         //!   * one module per version holds what is not. Those names are NOT\n\
         //!     re-exported at the root, because two versions would collide there\n\
         //!     and the collision is the whole point: they are different types.\n\
         //!   * [`RmAbi`] names the ones a caller has to be generic over.\n\
         //!\n\
         //! Features are additive. Each `v<NNN>` enables a module and disables\n\
         //! nothing; all of them at once compiles, and with exactly one enabled\n\
         //! the dispatch in [`DriverVersion`] has a single arm.\n",
    );
    s.push_str("#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]\n");
    s.push_str("#![allow(dead_code, clippy::all)]\n\n");

    s.push_str(
        "/// Asserts a measured layout: the size, the alignment and one offset\n\
         /// per field, all of them read from `manifests/<version>.json`.\n\
         ///\n\
         /// This is what keeps the committed source and the committed evidence\n\
         /// together. Breaking it means one of the two moved without the other.\n\
         macro_rules! assert_layout {\n\
         \x20   ($t:ty, size = $size:expr, align = $align:expr $(, $field:ident @ $off:expr)* $(,)?) => {\n\
         \x20       const _: () = {\n\
         \x20           assert!(::core::mem::size_of::<$t>() == $size);\n\
         \x20           assert!(::core::mem::align_of::<$t>() == $align);\n\
         \x20           $( assert!(::core::mem::offset_of!($t, $field) == $off); )*\n\
         \x20       };\n\
         \x20   };\n\
         }\n\n",
    );

    s.push_str("mod version;\npub use version::{assert_driver_version, detect, running_driver_version};\n\n");
    s.push_str("pub mod stable;\npub use stable::*;\n\n");
    for v in versions {
        let f = feature_of(&v.name);
        let _ = writeln!(s, "#[cfg(feature = \"{f}\")]\npub mod {f};");
    }
    s.push('\n');

    let _ = writeln!(
        s,
        "/// The driver version this crate defaults to: the one in `DRIVER_VERSION`\n\
         /// at the repository root, and the one the `default` feature enables.\n\
         pub const DRIVER_VERSION: &str = {primary:?};\n"
    );
    let list: Vec<String> = versions.iter().map(|v| format!("{:?}", v.name)).collect();
    let _ = writeln!(
        s,
        "/// Every driver version this crate carries a layout for, oldest first.\n\
         pub const SUPPORTED_VERSIONS: &[&str] = &[{}];\n",
        list.join(", ")
    );

    // --- DriverVersion ----------------------------------------------------
    s.push_str(
        "/// Which supported driver a running system has.\n\
         ///\n\
         /// Guest and host driver versions are assumed equal, so this is read\n\
         /// once, on the host, and carried.\n\
         #[derive(Clone, Copy, PartialEq, Eq, Debug)]\n\
         pub enum DriverVersion {\n",
    );
    for v in versions {
        let _ = writeln!(s, "    /// {}", v.name);
        let _ = writeln!(s, "    #[cfg(feature = \"{}\")]", feature_of(&v.name));
        let _ = writeln!(s, "    {},", variant_of(&v.name));
    }
    s.push_str("}\n\n");
    s.push_str("impl DriverVersion {\n");
    s.push_str(
        "    /// The version string as `/proc/driver/nvidia/version` spells it.\n\
         \x20   pub fn as_str(self) -> &'static str {\n\
         \x20       match self {\n",
    );
    for v in versions {
        let _ = writeln!(
            s,
            "            #[cfg(feature = \"{}\")]",
            feature_of(&v.name)
        );
        let _ = writeln!(
            s,
            "            DriverVersion::{} => {:?},",
            variant_of(&v.name),
            v.name
        );
    }
    s.push_str("        }\n    }\n\n");
    s.push_str(
        "    /// The supported version this string names, or `None`.\n\
         \x20   ///\n\
         \x20   /// A version that is not here is REFUSED rather than approximated.\n\
         \x20   /// A minor release joins a branch by having the same manifest, which\n\
         \x20   /// gives it its own entry in `abi.toml`; a version with no entry has\n\
         \x20   /// not been measured, and guessing which layout it takes is the one\n\
         \x20   /// mistake this whole crate exists to prevent.\n\
         \x20   pub fn from_version_string(s: &str) -> Option<Self> {\n\
         \x20       match s {\n",
    );
    for v in versions {
        let _ = writeln!(
            s,
            "            #[cfg(feature = \"{}\")]",
            feature_of(&v.name)
        );
        let _ = writeln!(
            s,
            "            {:?} => Some(DriverVersion::{}),",
            v.name,
            variant_of(&v.name)
        );
    }
    s.push_str("            _ => None,\n        }\n    }\n}\n\n");

    let _ = writeln!(
        s,
        "/// The MODULE of [`DRIVER_VERSION`].\n\
         ///\n\
         /// For the handful of names that cannot go through [`RmAbi`] at all\n\
         /// because some supported version does not have them -- the NVA083\n\
         /// virtual-display constants, which arrive with R595. Reaching for\n\
         /// this is a statement that the code only works on drivers that have\n\
         /// the name, and the compiler cannot check that for you.\n\
         #[cfg(feature = {:?})]\n\
         pub use {} as default_version;\n",
        feature_of(primary),
        feature_of(primary)
    );

    let _ = writeln!(
        s,
        "/// The ABI of [`DRIVER_VERSION`].\n\
         ///\n\
         /// For code that is deliberately single-version -- a test, a\n\
         /// diagnostic, a tool that only ever runs beside the driver this\n\
         /// build was made for. Anything that has to work on more than one\n\
         /// takes `A: RmAbi` and gets it from [`dispatch`].\n\
         #[cfg(feature = {:?})]\n\
         pub type DefaultAbi = {};\n",
        feature_of(primary),
        marker_of(primary)
    );

    // --- RmAbi ------------------------------------------------------------
    let (trait_src, abstracted, not_abstractable) = rm_abi(cfg, versions, part)?;
    s.push_str(&trait_src);

    Ok((s, abstracted, not_abstractable))
}

fn rm_abi(
    cfg: &AbiToml,
    versions: &[Version],
    part: &Partition,
) -> Result<(String, Vec<String>, Vec<String>)> {
    let mut abstracted: Vec<(String, String)> = Vec::new();
    let mut not_abstractable: Vec<String> = Vec::new();

    for (assoc, c_name) in &cfg.footprint.mediated {
        if !part.volatile.contains(c_name) {
            // Stable: used directly, and the rule says so.
            continue;
        }
        let missing: Vec<&str> = versions
            .iter()
            .filter(|v| !v.items.groups.contains_key(c_name))
            .map(|v| v.name.as_str())
            .collect();
        if missing.is_empty() {
            abstracted.push((assoc.clone(), c_name.clone()));
        } else {
            not_abstractable.push(format!("{c_name} (absent on {})", missing.join(", ")));
        }
    }
    abstracted.sort();

    let mut s = String::new();
    s.push_str(
        "/// The types whose layout depends on the driver version, and the sizes\n\
         /// that go on the wire with them.\n\
         ///\n\
         /// The set is DERIVED: it is exactly the mediated types in `abi.toml`\n\
         /// that are not identical on every supported version. A type that does\n\
         /// not move is not here -- it lives in [`stable`] and is used directly.\n\
         ///\n\
         /// There is no trait hierarchy between versions and there is no shared\n\
         /// supertype. Two versions that agree about a type share it by\n\
         /// re-export, never by inheritance.\n\
         ///\n\
         /// `Copy + 'static` because an implementation is a marker type with no\n\
         /// data: it exists to be a type parameter, is never held, and must not\n\
         /// drag a lifetime into everything generic over it.\n\
         pub trait RmAbi: Copy + 'static {\n\
         \x20   /// Which driver this implementation is the ABI of.\n\
         \x20   const VERSION: DriverVersion;\n",
    );
    // A field offset cannot be reached through an associated TYPE: the trait
    // says nothing about what fields it has, and `offset_of!` needs to know.
    // So every field of every mediated type gets an associated CONST, derived
    // the same way the type is. Generating all of them rather than the ones
    // somebody asked for is the point -- a hand-picked subset is a list that
    // goes stale, and this trait is read by a compiler, not by a person.
    let primary_m = &versions[0].manifest;
    for (assoc, c_name) in &abstracted {
        let _ = writeln!(
            s,
            "\n    /// `{c_name}`, as this driver version lays it out."
        );
        let _ = writeln!(s, "    type {assoc}: Copy;");
        let _ = writeln!(
            s,
            "    /// `size_of::<Self::{assoc}>()`, for the `paramsSize` a call carries."
        );
        let _ = writeln!(s, "    const {}: u32;", size_const(assoc));
        for f in fields_of(versions, c_name) {
            if primary_m.types.contains_key(c_name) {
                let _ = writeln!(s, "    /// `offset_of!({c_name}, {f})`.");
                let _ = writeln!(s, "    const {}: usize;", off_const(assoc, &f));
            }
        }
    }
    let mut abstracted_consts: Vec<(String, String)> = cfg
        .footprint
        .mediated_constants
        .iter()
        .map(|(a, c)| (a.clone(), c.clone()))
        .collect();
    abstracted_consts.sort();
    for (assoc, c_name) in &abstracted_consts {
        let _ = writeln!(
            s,
            "\n    /// `{c_name}`, or `None` where this driver's headers do not define it."
        );
        let _ = writeln!(s, "    const {assoc}: Option<u32>;");
    }
    s.push_str("}\n\n");

    if !not_abstractable.is_empty() {
        s.push_str(
            "// Mediated types that are NOT in the trait above, and why. A type that\n\
             // some supported version does not have at all cannot be an associated\n\
             // type: there would be nothing to point the impl at. Reaching one means\n\
             // reaching for a module directly, and means the feature it belongs to\n\
             // does not exist on those versions.\n",
        );
        for n in &not_abstractable {
            let _ = writeln!(s, "//   {n}");
        }
        s.push('\n');
    }

    s.push_str(
        "/// A piece of work that is generic over the ABI, so that [`dispatch`]\n\
         /// can hand it the right one.\n\
         ///\n\
         /// A closure cannot do this: it would have to be generic over a type\n\
         /// parameter it does not have. A one-method trait can, and costs\n\
         /// nothing -- with a single feature enabled the match below has one arm\n\
         /// and the call is direct.\n\
         pub trait AbiVisitor {\n\
         \x20   /// What the work produces. The same type for every version, which\n\
         \x20   /// is what lets one match answer for all of them.\n\
         \x20   type Out;\n\
         \x20   fn visit<A: RmAbi>(self) -> Self::Out;\n\
         }\n\n\
         /// THE dispatch point. The only `match` on [`DriverVersion`] in the\n\
         /// tree, by construction: everything downstream of it is generic over\n\
         /// `A: RmAbi` and never asks again.\n\
         ///\n\
         /// With one version's feature enabled this match has one arm and\n\
         /// disappears; with several it is one branch, once, at start-up.\n\
         pub fn dispatch<V: AbiVisitor>(version: DriverVersion, work: V) -> V::Out {\n\
         \x20   match version {\n",
    );
    for v in versions {
        let _ = writeln!(s, "        #[cfg(feature = \"{}\")]", feature_of(&v.name));
        let _ = writeln!(
            s,
            "        DriverVersion::{} => work.visit::<{}>(),",
            variant_of(&v.name),
            marker_of(&v.name)
        );
    }
    s.push_str("    }\n}\n\n");

    for v in versions {
        let f = feature_of(&v.name);
        let marker = marker_of(&v.name);
        let _ = writeln!(
            s,
            "/// The ABI of driver {}. See [`RmAbi`].\n#[cfg(feature = \"{f}\")]\n#[derive(Clone, Copy, Debug)]\npub struct {marker};\n",
            v.name
        );
        let _ = writeln!(s, "#[cfg(feature = \"{f}\")]\nimpl RmAbi for {marker} {{");
        let _ = writeln!(
            s,
            "    const VERSION: DriverVersion = DriverVersion::{};",
            variant_of(&v.name)
        );
        for (assoc, c_name) in &abstracted {
            let module = if part.stable.iter().any(|n| n == c_name) {
                "stable"
            } else {
                &f
            };
            let _ = writeln!(s, "    type {assoc} = crate::{module}::{c_name};");
            let t = v.manifest.types.get(c_name);
            let size = t.map(|t| t.size).unwrap_or_default();
            let _ = writeln!(s, "    const {}: u32 = {size};", size_const(assoc));
            for fname in fields_of(versions, c_name) {
                // A field this version does not have. There is no offset to
                // give and there must not be a plausible one, so it is the
                // type's size: past every byte of it, and any read at it is
                // out of bounds rather than quietly wrong.
                let off = t
                    .and_then(|t| t.fields.iter().find(|x| x.name == fname))
                    .and_then(|x| x.offset)
                    .unwrap_or(size);
                let _ = writeln!(s, "    const {}: usize = {off};", off_const(assoc, &fname));
            }
        }
        for (assoc, c_name) in &abstracted_consts {
            let value = match v.manifest.constants.get(c_name) {
                Some(c) => format!("Some({})", c.value),
                None => "None".to_string(),
            };
            let _ = writeln!(s, "    const {assoc}: Option<u32> = {value};");
        }
        s.push_str("}\n\n");
    }

    Ok((
        s,
        abstracted
            .iter()
            .map(|(a, c)| format!("{a} = {c}"))
            .collect(),
        not_abstractable,
    ))
}

fn versions_toml(
    cfg: &AbiToml,
    versions: &[Version],
    part: &Partition,
    primary: &str,
    abstracted: &[String],
) -> String {
    let mut s = String::new();
    s.push_str("# SPDX-License-Identifier: MIT\n");
    s.push_str("# Generated by `cargo xtask abi`. Do not edit.\n#\n");
    s.push_str("# What was measured, and what the crate was built from. `manifest` is the\n");
    s.push_str("# SHA-256 of manifests/<version>.json, so a manifest edited by hand and a\n");
    s.push_str("# crate generated from the original do not agree here.\n#\n");
    s.push_str("# `layout` is the module a version's non-stable names live in. Versions that\n");
    s.push_str("# agree about every one of them share a module; today each version has its\n");
    s.push_str("# own, which is what `structs` below says once rather than per name.\n\n");
    let _ = writeln!(s, "default = {primary:?}\n");
    let _ = writeln!(s, "stable_names = {}", part.stable.len());
    let _ = writeln!(s, "volatile_names = {}\n", part.volatile.len());
    if !abstracted.is_empty() {
        s.push_str("# The RmAbi associated types, derived from [footprint.mediated].\n");
        let q: Vec<String> = abstracted.iter().map(|a| format!("{a:?}")).collect();
        let _ = writeln!(s, "rm_abi = [\n    {}\n]\n", q.join(",\n    "));
    }
    for v in versions {
        let _ = writeln!(s, "[versions.{:?}]", v.name);
        let _ = writeln!(s, "headers = {:?}", cfg.versions[&v.name].headers);
        let _ = writeln!(s, "commit = {:?}", v.manifest.commit);
        let _ = writeln!(s, "manifest = {:?}", format!("sha256:{}", sha256(&v.json)));
        let _ = writeln!(s, "layout = {:?}", feature_of(&v.name));
        let _ = writeln!(s, "feature = {:?}\n", feature_of(&v.name));
    }
    s
}

fn features_block(versions: &[Version], primary: &str, is_sys: bool) -> String {
    let mut s = String::new();
    s.push_str("[features]\n");
    s.push_str("# One feature per driver version, and they are ADDITIVE: each enables its\n");
    s.push_str("# module and disables nothing. There is deliberately no \"select exactly\n");
    s.push_str("# one version\" feature -- cargo unifies features across a build, so such a\n");
    s.push_str("# thing would turn one crate's choice into everyone's, silently.\n");
    s.push_str("# All of them at once must compile; that is checked in CI.\n");
    let _ = writeln!(s, "default = [{:?}]", feature_of(primary));
    for v in versions {
        let f = feature_of(&v.name);
        if is_sys {
            let _ = writeln!(s, "{f} = []");
        } else {
            let _ = writeln!(s, "{f} = [\"nvrm-sys/{f}\"]");
        }
    }
    s
}

const BEGIN: &str = "# BEGIN generated by cargo xtask abi";
const END: &str = "# END generated by cargo xtask abi";

fn replace_region(text: &str, block: &str) -> Result<String> {
    let (b, e) = match (text.find(BEGIN), text.find(END)) {
        (Some(b), Some(e)) if e > b => (b, e),
        _ => bail!(
            "crates/nvrm-sys/Cargo.toml has no\n  {BEGIN}\n  ...\n  {END}\n\
             region for the generated [features] table"
        ),
    };
    Ok(format!("{}{BEGIN}\n{block}{}", &text[..b], &text[e..]))
}

struct Rustfmt {
    root: PathBuf,
    toolchain: String,
    edition: String,
}

impl Rustfmt {
    fn for_workspace(root: &Path) -> Result<Self> {
        let toolchain: toml::Value =
            toml::from_str(&std::fs::read_to_string(root.join("rust-toolchain.toml"))?)?;
        let manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(root.join("Cargo.toml"))?)?;
        Ok(Self {
            root: root.to_path_buf(),
            toolchain: toolchain
                .get("toolchain")
                .and_then(|value| value.get("channel"))
                .and_then(toml::Value::as_str)
                .context("rust-toolchain.toml must pin a toolchain channel")?
                .to_owned(),
            edition: manifest
                .get("workspace")
                .and_then(|value| value.get("package"))
                .and_then(|value| value.get("edition"))
                .and_then(toml::Value::as_str)
                .context("Cargo.toml must set workspace.package.edition")?
                .to_owned(),
        })
    }

    fn format(&self, source: &str) -> Result<String> {
        // Format only this buffer; generated lib.rs names modules emitted separately.
        let mut child = Command::new("rustup")
            .args([
                "run",
                &self.toolchain,
                "rustfmt",
                "--edition",
                &self.edition,
            ])
            .args(["--emit", "stdout", "--config", "skip_children=true"])
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting the pinned rustfmt")?;
        let input = child.stdin.take().unwrap().write_all(source.as_bytes());
        let output = child.wait_with_output().context("waiting for rustfmt")?;
        if !output.status.success() {
            bail!(
                "rustfmt failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        input.context("writing generated Rust to rustfmt")?;
        String::from_utf8(output.stdout).context("rustfmt returned invalid UTF-8")
    }
}

fn write_or_check(path: &Path, content: &str, check: bool, stale: &mut Vec<PathBuf>) -> Result<()> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if current == content {
        return Ok(());
    }
    if check {
        stale.push(path.to_path_buf());
    } else {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

fn banner(what: &str, why: &[&str]) -> String {
    let mut s = String::new();
    s.push_str("// SPDX-License-Identifier: MIT\n");
    s.push_str("// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>\n");
    s.push_str("// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR\n");
    let _ = writeln!(s, "//! {what}");
    s.push_str("//!\n");
    for line in why {
        let _ = writeln!(s, "//! {line}");
    }
    s.push_str("//!\n//! Generated by `cargo xtask abi`. Do not edit.\n\n");
    s
}

pub fn feature_of(version: &str) -> String {
    format!("v{}", version.split('.').next().unwrap_or(version))
}

fn variant_of(version: &str) -> String {
    format!("V{}", version.split('.').next().unwrap_or(version))
}

fn marker_of(version: &str) -> String {
    variant_of(version)
}

/// Every field name any version gives this type, in the primary version's
/// order first so the generated trait reads like the struct.
fn fields_of(versions: &[Version], c_name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in versions {
        if let Some(t) = v.manifest.types.get(c_name) {
            for f in &t.fields {
                if !out.contains(&f.name) {
                    out.push(f.name.clone());
                }
            }
        }
    }
    out
}

fn off_const(assoc: &str, field: &str) -> String {
    format!("{}_OFF_{field}", screaming(assoc))
}

fn size_const(assoc: &str) -> String {
    format!("{}_SIZE", screaming(assoc))
}

fn screaming(assoc: &str) -> String {
    let mut out = String::new();
    for (i, c) in assoc.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

fn sha256(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// The refusal.
///
/// `identical` and `append-only` need nobody: the first is nothing to decide
/// and the second is a change an older caller cannot see. `breaking` on a
/// MEDIATED type is different -- it is a struct the boundary reads, laid out
/// differently, and a generator that quietly gave it a new module would be
/// making the decision that this whole design exists to put in front of a
/// person.
///
/// So it stops, names the struct, and asks for an acknowledgement in
/// abi.toml. The acknowledgement does not describe the layout; the layout is
/// still measured. It records that somebody looked.
///
/// Non-mediated types are not asked about. There are hundreds of them per
/// version pair, they are controls nothing here intercepts, and a question
/// nobody can answer usefully is not a safeguard -- it is a habit of clicking
/// through. They are all in `manifests/classification.md`.
pub fn refuse_on_breaking(
    cfg: &AbiToml,
    part: &Partition,
    versions: &[Version],
    reports: &[classify::PairReport],
) -> Result<()> {
    let mediated: BTreeSet<&str> = cfg
        .footprint
        .mediated
        .values()
        .map(String::as_str)
        .collect();
    let mut owed: Vec<(String, String, String)> = Vec::new();

    for w in versions.windows(2) {
        let (from, to) = (&w[0].name, &w[1].name);
        let Some(r) = reports.iter().find(|r| &r.from == from && &r.to == to) else {
            continue;
        };
        for c in r.types.iter().filter(|c| c.verdict == Verdict::Breaking) {
            if !mediated.contains(c.name.as_str()) || !part.volatile.contains(&c.name) {
                continue;
            }
            let declared = cfg
                .versions
                .get(to)
                .map(|e| e.layout.contains_key(&c.name))
                .unwrap_or(false);
            if !declared {
                owed.push((to.clone(), c.name.clone(), c.reasons.join("; ")));
            }
        }
    }

    // The other direction. An acknowledgement that no longer applies is worse
    // than none: it reads as "somebody looked at this move" about a move that
    // is not there any more, and the next reader believes it.
    let mut stale: Vec<(String, String)> = Vec::new();
    for (version, entry) in &cfg.versions {
        for name in entry.layout.keys() {
            let broke = versions.windows(2).any(|w| {
                &w[1].name == version
                    && reports.iter().any(|r| {
                        r.from == w[0].name
                            && &r.to == version
                            && r.types
                                .iter()
                                .any(|c| c.name == *name && c.verdict == Verdict::Breaking)
                    })
            });
            if !broke {
                stale.push((version.clone(), name.clone()));
            }
        }
    }
    if !stale.is_empty() {
        let mut msg = String::from(
            "abi.toml acknowledges a layout break that is not there any more. \
             Delete these lines:\n\n",
        );
        for (v, n) in &stale {
            let _ = writeln!(msg, "  [versions.\"{v}\".layout] {n}");
        }
        bail!("{msg}");
    }

    // And the mediated list: a type that does not move must not be on it. The
    // crate shape rests on that rule -- a stable type lives in src/stable.rs
    // and is used directly, and listing it here says the opposite.
    let mut settled: Vec<&str> = Vec::new();
    for c_name in cfg.footprint.mediated.values() {
        if !part.volatile.contains(c_name) {
            settled.push(c_name);
        }
    }
    if !settled.is_empty() {
        bail!(
            "[footprint.mediated] lists types that are the same on every supported \
             version:\n  {}\nThey are in src/stable.rs and are used directly. Remove \
             them from the list.",
            settled.join("\n  ")
        );
    }

    if owed.is_empty() {
        return Ok(());
    }
    let mut msg = String::from(
        "a mediated struct changed layout and nobody has said so. \
         The generator will not pick a module for it.\n\n",
    );
    let mut by_version: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (v, n, why) in &owed {
        by_version
            .entry(v.as_str())
            .or_default()
            .push((n.as_str(), why.as_str()));
    }
    for (v, items) in &by_version {
        let _ = writeln!(msg, "at {v}:");
        for (n, why) in items {
            let _ = writeln!(msg, "  {n} -- {why}");
        }
        let _ = writeln!(msg, "\nAdd to crates/nvrm-sys/abi.toml:\n");
        let _ = writeln!(msg, "[versions.\"{v}\".layout]");
        for (n, _) in items {
            let _ = writeln!(msg, "{n} = \"{}\"", feature_of(v));
        }
        msg.push('\n');
    }
    bail!("{msg}");
}
