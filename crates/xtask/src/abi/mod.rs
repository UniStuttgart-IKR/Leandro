// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! `cargo xtask abi` -- the multi-version RM binding generator.
//!
//! Reads `crates/nvrm-sys/abi.toml`, runs bindgen once per version against
//! that version's headers under `vendor/nvidia-rm-headers/`, writes
//! `crates/nvrm-sys/manifests/<version>.json`, and classifies every bound
//! type and constant between every pair of versions.
//!
//! The manifests are committed. They are the evidence: a claim that two
//! driver versions have the same footprint is checkable by a reader with a
//! diff, without a card, without the driver, and without running this.

pub mod classify;
pub mod config;
pub mod emit;
pub mod items;
pub mod mediated;
pub mod manifest;

use anyhow::{bail, Context, Result};
use classify::{PairReport, Verdict};
use config::AbiToml;
use manifest::Manifest;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The include roots inside a vendored header set. The same five directories
/// `crates/nvrm-sys/build.rs` names, in the same order and for the same
/// reason: header names are duplicated across them and the order decides
/// which one wins.
const INCLUDE_DIRS: &[&str] = &[
    "kernel-open/common/inc",
    "src/common/sdk/nvidia/inc",
    "src/nvidia/arch/nvalloc/unix/include",
    "kernel-open/nvidia-uvm",
    "kernel-open/nvidia-modeset",
];

pub fn run(args: &[String]) -> Result<()> {
    let mut check = false;
    let mut report_to: Option<PathBuf> = None;
    let mut dump_to: Option<PathBuf> = None;
    let mut config_path: Option<PathBuf> = None;
    let mut manifest_out: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => check = true,
            "--report" => {
                report_to = Some(PathBuf::from(
                    it.next().context("--report needs a path")?,
                ))
            }
            // Not part of the pipeline: what bindgen produced, so that a
            // version whose manifest this refuses to build can be looked at.
            // A CANDIDATE version set, and where to put its manifests. The
            // point of this pair is to answer "what would adding this driver
            // cost" before abi.toml claims the answer is known. Neither
            // belongs in a normal run: the committed manifests come from the
            // committed abi.toml.
            "--config" => {
                config_path = Some(PathBuf::from(it.next().context("--config needs a path")?))
            }
            "--manifests" => {
                manifest_out = Some(PathBuf::from(it.next().context("--manifests needs a path")?))
            }
            "--dump-bindings" => {
                dump_to = Some(PathBuf::from(it.next().context("--dump-bindings needs a path")?))
            }
            other => bail!("unknown option: {other}"),
        }
    }

    let root = workspace_root()?;
    let abi_path = config_path.unwrap_or_else(|| root.join("crates/nvrm-sys/abi.toml"));
    let cfg = AbiToml::load(&abi_path)?;

    let versions = cfg.ordered();
    eprintln!("abi: {} versions, {} struct patterns, {} constant patterns",
        versions.len(), cfg.footprint.structs.len(), cfg.footprint.constants.len());

    let manifest_dir = manifest_out.unwrap_or_else(|| root.join("crates/nvrm-sys/manifests"));
    if !check {
        std::fs::create_dir_all(&manifest_dir)?;
    }

    let primary = std::fs::read_to_string(root.join("DRIVER_VERSION"))
        .context("reading DRIVER_VERSION")?
        .trim()
        .to_string();
    if !cfg.versions.contains_key(&primary) {
        bail!(
            "DRIVER_VERSION is {primary} and abi.toml does not have an entry for it. \
             The default feature has to point at a version this crate carries."
        );
    }

    let mut built: Vec<emit::Version> = Vec::new();
    let mut stale = Vec::new();
    for v in &versions {
        let headers = root.join("vendor/nvidia-rm-headers").join(v);
        if !headers.is_dir() {
            bail!(
                "no headers for {v} at {} -- run: scripts/build.sh vendor-abi {v}",
                headers.display()
            );
        }
        eprintln!("abi: bindgen {v}");
        let mut src = run_bindgen(&root, &headers, &cfg.footprint)
            .with_context(|| format!("bindgen for {v}"))?;
        let renames = cfg.footprint.renames_for(v);
        for (old, canonical) in &renames {
            src = rename_ident(&src, old, canonical)
                .with_context(|| format!("{v}: renaming {old} to {canonical}"))?;
        }
        if let Some(d) = &dump_to {
            std::fs::create_dir_all(d)?;
            std::fs::write(d.join(format!("{v}.rs")), &src)?;
        }
        let file = syn::parse_file(&src)
            .with_context(|| format!("parsing bindgen output for {v} as Rust"))?;
        let mut m = Manifest::parse(v, &cfg.versions[*v].headers, &provenance_commit(&headers)?, &file)?;
        m.renames = renames.iter().map(|(old, c)| (c.clone(), old.clone())).collect();
        let its = items::Items::collect(&file)
            .with_context(|| format!("cutting the bindgen output for {v} into items"))?;
        eprintln!(
            "abi: {v} -- {} types, {} constants, {} aliases",
            m.types.len(),
            m.constants.len(),
            m.aliases.len()
        );

        let path = manifest_dir.join(format!("{v}.json"));
        let json = m.to_json();
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current != json {
            if check {
                stale.push(path.clone());
            } else {
                std::fs::write(&path, &json)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
        }
        built.push(emit::Version { name: v.to_string(), manifest: m, items: its, json });
    }
    let manifests: Vec<Manifest> = built.iter().map(|v| v.manifest.clone()).collect();

    let pairs = all_pairs(&manifests);
    let report = render(&manifests, &pairs);
    let report_path = report_to.unwrap_or_else(|| manifest_dir.join("classification.md"));
    let current = std::fs::read_to_string(&report_path).unwrap_or_default();
    if current != report {
        if check {
            stale.push(report_path.clone());
        } else {
            std::fs::write(&report_path, &report)
                .with_context(|| format!("writing {}", report_path.display()))?;
        }
    }
    print!("{}", matrix(&manifests, &pairs));

    // --- the crate ---------------------------------------------------------
    let part = emit::partition(&built);
    mediated::assert_complete(&root, &cfg.footprint, &part, &built)?;
    // Every entry in [footprint.renamed] is a CLAIM, typed by a person: that
    // two of NVIDIA's names are the same type. The measurement can disagree
    // -- a rename that is really two different structs would classify as
    // breaking and then sit in a 60 KB classification nobody reads line by
    // line. So say the verdict here, on every run, one line per rename.
    for (canonical, per_version) in &cfg.footprint.renamed {
        let versions_renamed: Vec<&str> = per_version.keys().map(String::as_str).collect();
        let old: Vec<&str> = {
            let mut o: Vec<&str> = per_version.values().map(String::as_str).collect();
            o.sort();
            o.dedup();
            o
        };
        if part.stable.iter().any(|n| n == canonical) {
            eprintln!(
                "abi: rename {canonical} <- {} on {}: identical on every version, as claimed",
                old.join("/"),
                versions_renamed.join(", ")
            );
        } else {
            eprintln!(
                "abi: rename {canonical} <- {} on {}: NOT identical after renaming. \
                 Either the two names are different types and the entry is wrong, or the \
                 type was renamed AND changed in the same release -- manifests/classification.md \
                 says which.",
                old.join("/"),
                versions_renamed.join(", ")
            );
        }
    }

    emit::refuse_on_breaking(&cfg, &part, &built, &pairs)?;
    let out = emit::emit(&root, &cfg, &built, &part, &primary, check)?;
    stale.extend(out.stale);
    eprintln!(
        "abi: {} names stable, {} version-specific; RmAbi over {}",
        out.stable_count,
        out.volatile_count,
        if out.abstracted.is_empty() { "nothing".to_string() } else { out.abstracted.join(", ") }
    );
    for n in &out.not_abstractable {
        eprintln!("abi: not abstractable -- {n}");
    }

    // A manifest whose version left abi.toml. It is history and is not
    // regenerated, so a reader who diffs it against the headers of the day
    // will find it wrong -- say which ones those are on every run rather than
    // leaving the directory to be read as uniformly current.
    let mut history: Vec<String> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(&manifest_dir) {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".json") {
                if !versions.contains(&stem) {
                    history.push(stem.to_string());
                }
            }
        }
    }
    history.sort();
    if !history.is_empty() {
        eprintln!(
            "abi: kept as history, not regenerated and not classified: {}",
            history.join(", ")
        );
    }

    if !stale.is_empty() {
        bail!(
            "--check: these are out of date, run `cargo xtask abi`:\n  {}",
            stale.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n  ")
        );
    }
    Ok(())
}

/// Replace one whole identifier throughout bindgen's output.
///
/// Textual on purpose, and before the parse: a rename has to reach the
/// struct, the `impl Default` behind it, every field that names it AND the
/// strings inside bindgen's own layout assertions, which is where the
/// manifest's keys come from. One pass over the text does all four; rewriting
/// a syntax tree would miss the strings.
///
/// These are NVIDIA's C identifiers, so a whole-word match cannot hit
/// anything else -- and if the new name were already there, this would be
/// merging two different types into one, which is refused rather than done.
fn rename_ident(src: &str, old: &str, canonical: &str) -> Result<String> {
    if src.contains(&format!("pub struct {canonical} "))
        || src.contains(&format!("pub union {canonical} "))
        || src.contains(&format!("pub type {canonical} "))
    {
        bail!(
            "{canonical} is defined in these headers AND a rename says {old} should become it. \
             Two definitions cannot share one name; the rename in abi.toml is wrong for this version."
        );
    }
    if !src.contains(old) {
        bail!("no {old} in these headers -- the rename in abi.toml names a type this version does not have");
    }
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while let Some(hit) = src[i..].find(old) {
        let at = i + hit;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let end = at + old.len();
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        out.push_str(&src[i..at]);
        out.push_str(if before_ok && after_ok { canonical } else { old });
        i = end;
    }
    out.push_str(&src[i..]);
    Ok(out)
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn workspace_root() -> Result<PathBuf> {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .context("locating the workspace root")?;
    Ok(d)
}

fn provenance_commit(headers: &Path) -> Result<String> {
    let p = headers.join("PROVENANCE");
    let text = std::fs::read_to_string(&p)
        .with_context(|| format!("reading {} -- run scripts/build.sh vendor-abi", p.display()))?;
    text.lines()
        .find_map(|l| l.strip_prefix("commit:"))
        .map(|s| s.trim().to_string())
        .with_context(|| format!("{} has no commit: line", p.display()))
}

/// bindgen, pointed at one vendored header set.
///
/// This is the only bindgen invocation in the workspace. `nvrm-sys` used to
/// carry its own in a `build.rs`, which meant every build of every consumer
/// needed libclang and the vendored tree; the crate source is committed now,
/// and bindgen runs here, when a person regenerates it.
fn run_bindgen(root: &Path, headers: &Path, fp: &config::Footprint) -> Result<String> {
    let mut b = bindgen::Builder::default()
        .header(root.join("crates/nvrm-sys/wrapper.h").display().to_string())
        .generate_comments(false)
        .use_core()
        .derive_default(true)
        .derive_debug(true)
        .layout_tests(true)
        .prepend_enum_name(false)
        .default_enum_style(bindgen::EnumVariation::Consts)
        .clang_arg("-DNV_LINUX")
        .clang_arg("-D__linux__")
        .clang_arg("-std=gnu11");
    for d in INCLUDE_DIRS {
        b = b.clang_arg(format!("-I{}", headers.join(d).display()));
    }
    for p in &fp.structs {
        b = b.allowlist_type(p);
    }
    for p in &fp.constants {
        b = b.allowlist_var(p);
    }
    Ok(b.generate()?.to_string())
}

fn all_pairs(manifests: &[Manifest]) -> Vec<PairReport> {
    let mut out = Vec::new();
    for a in manifests {
        for b in manifests {
            if a.version != b.version {
                out.push(classify::classify(a, b));
            }
        }
    }
    out
}

fn find<'a>(pairs: &'a [PairReport], from: &str, to: &str) -> &'a PairReport {
    pairs
        .iter()
        .find(|p| p.from == from && p.to == to)
        .expect("every ordered pair is classified")
}

fn render(manifests: &[Manifest], pairs: &[PairReport]) -> String {
    let v: Vec<&str> = manifests.iter().map(|m| m.version.as_str()).collect();
    let mut s = String::new();
    s.push_str("<!-- SPDX-License-Identifier: MIT -->\n");
    s.push_str("# ABI classification\n\n");
    s.push_str("Generated by `cargo xtask abi`. Do not edit.\n\n");

    s.push_str("## Footprint per version\n\n");
    s.push_str("| version | headers | types | constants | aliases |\n|---|---|---|---|---|\n");
    for m in manifests {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} |",
            m.version,
            m.commit.get(..12).unwrap_or(&m.commit),
            m.types.len(),
            m.constants.len(),
            m.aliases.len()
        );
    }

    s.push_str("\n## Pairwise\n\n");
    s.push_str("Rows are the version a caller was built for, columns the version it meets. \
                `append-only` is directional: it means the row's fields are all still there, \
                at the same offsets, inside the column's larger struct.\n\n");
    let _ = write!(s, "| from \\ to |");
    for to in &v {
        let _ = write!(s, " {to} |");
    }
    s.push_str("\n|---|");
    for _ in &v {
        s.push_str("---|");
    }
    s.push('\n');
    for from in &v {
        let _ = write!(s, "| **{from}** |");
        for to in &v {
            if from == to {
                s.push_str(" -- |");
            } else {
                let p = find(pairs, from, to);
                let _ = write!(s, " {} |", p.verdict().as_str());
            }
        }
        s.push('\n');
    }

    s.push_str("\n## Neighbours, in detail\n\n");
    for w in v.windows(2) {
        let p = find(pairs, w[0], w[1]);
        let _ = writeln!(
            s,
            "### {} -> {}: **{}**\n",
            p.from,
            p.to,
            p.verdict().as_str()
        );
        let _ = writeln!(
            s,
            "{} identical, {} append-only, {} breaking; {} constant(s) changed.\n",
            p.identical,
            p.append_only,
            p.breaking,
            p.constants.len()
        );
        detail(&mut s, p);
    }

    // Every other pair, counted but not itemised. A non-neighbour pair is a
    // sum of the steps between it and nothing else; listing all of them
    // itemised buries the four comparisons anybody reads.
    s.push_str("\n## Every other pair, counted\n\n");
    s.push_str("| from | to | verdict | identical | append-only | breaking | constants |\n");
    s.push_str("|---|---|---|---|---|---|---|\n");
    for from in &v {
        for to in &v {
            if from == to || v.windows(2).any(|w| w[0] == *from && w[1] == *to) {
                continue;
            }
            let p = find(pairs, from, to);
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {} | {} |",
                p.from,
                p.to,
                p.verdict().as_str(),
                p.identical,
                p.append_only,
                p.breaking,
                p.constants.len()
            );
        }
    }
    s
}

/// The one table worth having on a terminal: which pairs can share a layout.
fn matrix(manifests: &[Manifest], pairs: &[PairReport]) -> String {
    let v: Vec<&str> = manifests.iter().map(|m| m.version.as_str()).collect();
    let w = v.iter().map(|s| s.len()).max().unwrap_or(10).max(11);
    let mut s = String::new();
    let _ = write!(s, "\n{:>w$} |", "from \\ to");
    for to in &v {
        let _ = write!(s, " {to:>w$}");
    }
    s.push('\n');
    for from in &v {
        let _ = write!(s, "{from:>w$} |");
        for to in &v {
            let cell = if from == to {
                "--".to_string()
            } else {
                find(pairs, from, to).verdict().as_str().to_string()
            };
            let _ = write!(s, " {cell:>w$}");
        }
        s.push('\n');
    }
    s.push('\n');
    s
}

fn detail(s: &mut String, p: &PairReport) {
    let mut append: Vec<&classify::Change> =
        p.types.iter().filter(|c| c.verdict == Verdict::AppendOnly).collect();
    let mut breaking: Vec<&classify::Change> =
        p.types.iter().filter(|c| c.verdict == Verdict::Breaking).collect();
    append.sort_by(|a, b| a.name.cmp(&b.name));
    breaking.sort_by(|a, b| a.name.cmp(&b.name));

    if !append.is_empty() {
        s.push_str("append-only:\n\n");
        for c in &append {
            let _ = writeln!(s, "  - `{}` -- {}", c.name, c.reasons.join("; "));
        }
        s.push('\n');
    }
    if !breaking.is_empty() {
        s.push_str("breaking:\n\n");
        for c in &breaking {
            let _ = writeln!(s, "  - `{}` -- {}", c.name, first_reasons(&c.reasons));
        }
        s.push('\n');
    }
    if !p.constants.is_empty() {
        s.push_str("constants:\n\n");
        for c in &p.constants {
            let _ = writeln!(s, "  - `{}` -- {}", c.name, c.reasons.join("; "));
        }
        s.push('\n');
    }
    if append.is_empty() && breaking.is_empty() && p.constants.is_empty() {
        s.push_str("Nothing changed.\n\n");
    }
}

/// A struct whose every field moved produces one reason per field, and forty
/// of those say nothing the first three do not.
fn first_reasons(reasons: &[String]) -> String {
    const N: usize = 3;
    if reasons.len() <= N {
        return reasons.join("; ");
    }
    format!("{}; and {} more", reasons[..N].join("; "), reasons.len() - N)
}
