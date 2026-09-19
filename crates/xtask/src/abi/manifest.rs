// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Extract layouts from bindgen assertions, using `syn` to preserve formatting independence.
//! Expected assertion form:
//!
//! ```text
//! const _: () = {
//!     ["Size of NVOS64_PARAMETERS"][size_of::<NVOS64_PARAMETERS>() - 48usize];
//!     ["Alignment of NVOS64_PARAMETERS"][align_of::<NVOS64_PARAMETERS>() - 8usize];
//!     ["Offset of field: NVOS64_PARAMETERS::hRoot"][offset_of!(..) - 0usize];
//! };
//! ```

use anyhow::{bail, Context, Result};
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use syn::{Expr, Item, Lit, Stmt, Type};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Struct,
    Union,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Struct => "struct",
            Kind::Union => "union",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    /// `None` for anonymous members without a bindgen offset assertion.
    pub offset: Option<u64>,
    /// Bindgen type spelling; changes count even when layout is unchanged.
    pub ty: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeLayout {
    pub kind: Kind,
    pub size: u64,
    pub align: u64,
    pub fields: Vec<Field>,
    /// Types contained by value, including array elements. Excludes pointers.
    pub embeds: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constant {
    pub ty: String,
    pub value: String,
}

#[derive(Clone, Debug)]
pub struct Manifest {
    pub version: String,
    pub headers: String,
    pub commit: String,
    /// Canonical name to upstream spelling, applied before layout measurement.
    pub renames: BTreeMap<String, String>,
    pub aliases: BTreeMap<String, String>,
    pub constants: BTreeMap<String, Constant>,
    pub types: BTreeMap<String, TypeLayout>,
}

/// What a struct or union declared, before the layout numbers are attached.
struct Decl {
    kind: Kind,
    fields: Vec<(String, Type)>,
}

#[derive(Default)]
struct Layouts {
    size: BTreeMap<String, u64>,
    align: BTreeMap<String, u64>,
    offset: BTreeMap<(String, String), u64>,
}

impl Manifest {
    pub fn parse(version: &str, headers: &str, commit: &str, file: &syn::File) -> Result<Self> {
        let mut decls: BTreeMap<String, Decl> = BTreeMap::new();
        let mut aliases: BTreeMap<String, String> = BTreeMap::new();
        let mut constants: BTreeMap<String, Constant> = BTreeMap::new();
        let mut layouts = Layouts::default();

        for item in &file.items {
            match item {
                Item::Struct(s) => {
                    decls.insert(
                        s.ident.to_string(),
                        Decl {
                            kind: Kind::Struct,
                            fields: named(&s.fields),
                        },
                    );
                }
                Item::Union(u) => {
                    let fields = u
                        .fields
                        .named
                        .iter()
                        .filter(is_public)
                        .filter_map(|f| f.ident.as_ref().map(|i| (i.to_string(), f.ty.clone())))
                        .collect();
                    decls.insert(
                        u.ident.to_string(),
                        Decl {
                            kind: Kind::Union,
                            fields,
                        },
                    );
                }
                Item::Type(t) => {
                    aliases.insert(t.ident.to_string(), spell(&t.ty));
                }
                Item::Const(c) if c.ident == "_" => read_layout_block(c, &mut layouts)?,
                Item::Const(c) => {
                    constants.insert(
                        c.ident.to_string(),
                        Constant {
                            ty: spell(&c.ty),
                            value: tokens(&c.expr),
                        },
                    );
                }
                _ => {}
            }
        }

        if layouts.size.is_empty() {
            bail!(
                "{version}: bindgen emitted no layout assertions this parser recognises. \
                 Either layout_tests are off or bindgen changed the shape of the block -- \
                 see the module comment. An empty manifest must never be treated as a \
                 version that happens to match."
            );
        }

        // Resolve aliases to distinguish scalar fields from embedded structs.
        let resolve = |mut name: String| -> String {
            for _ in 0..32 {
                match aliases.get(&name) {
                    Some(next) if decls.contains_key(next) || aliases.contains_key(next) => {
                        name = next.clone();
                    }
                    _ => break,
                }
            }
            name
        };

        let mut types: BTreeMap<String, TypeLayout> = BTreeMap::new();
        for (name, decl) in &decls {
            let (size, align) = match (layouts.size.get(name), layouts.align.get(name)) {
                (Some(s), Some(a)) => (*s, *a),
                _ if decl.fields.is_empty() => {
                    // Opaque forward declarations have no measured layout.
                    (0, 0)
                }
                _ => bail!(
                    "{version}: {name} has {} fields and no size/alignment assertion",
                    decl.fields.len()
                ),
            };
            let mut fields = Vec::with_capacity(decl.fields.len());
            let mut embeds = BTreeSet::new();
            for (fname, fty) in &decl.fields {
                let key = (name.clone(), fname.clone());
                let offset = layouts.offset.get(&key).copied();
                if offset.is_none() && !fname.starts_with("__bindgen_anon_") {
                    bail!("{version}: no offset assertion for {name}::{fname}");
                }
                let mut refs = BTreeSet::new();
                by_value_refs(fty, &mut refs);
                for r in refs {
                    let r = resolve(r);
                    if decls.contains_key(&r) {
                        embeds.insert(r);
                    }
                }
                fields.push(Field {
                    name: fname.clone(),
                    offset,
                    ty: spell(fty),
                });
            }
            types.insert(
                name.clone(),
                TypeLayout {
                    kind: decl.kind,
                    size,
                    align,
                    fields,
                    embeds,
                },
            );
        }

        Ok(Manifest {
            version: version.to_string(),
            headers: headers.to_string(),
            commit: commit.to_string(),
            renames: BTreeMap::new(),
            aliases,
            constants,
            types,
        })
    }

    /// Types embedded by value. Growing them can move later container fields,
    /// so they cannot be classified as append-only.
    pub fn embedded_types(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for t in self.types.values() {
            out.extend(t.embeds.iter().cloned());
        }
        out
    }

    /// Sorted maps and declaration-order fields produce deterministic diffs.
    pub fn to_json(&self) -> String {
        let mut s = String::new();
        s.push_str("{\n");
        let _ = writeln!(s, "  \"version\": {},", json_str(&self.version));
        let _ = writeln!(s, "  \"headers\": {},", json_str(&self.headers));
        let _ = writeln!(s, "  \"commit\": {},", json_str(&self.commit));
        s.push_str("  \"renames\": {\n");
        for (i, (k, v)) in self.renames.iter().enumerate() {
            let comma = if i + 1 == self.renames.len() { "" } else { "," };
            let _ = writeln!(s, "    {}: {}{comma}", json_str(k), json_str(v));
        }
        s.push_str("  },\n");

        s.push_str("  \"aliases\": {\n");
        for (i, (k, v)) in self.aliases.iter().enumerate() {
            let comma = if i + 1 == self.aliases.len() { "" } else { "," };
            let _ = writeln!(s, "    {}: {}{comma}", json_str(k), json_str(v));
        }
        s.push_str("  },\n");

        s.push_str("  \"constants\": {\n");
        for (i, (k, c)) in self.constants.iter().enumerate() {
            let comma = if i + 1 == self.constants.len() {
                ""
            } else {
                ","
            };
            let _ = writeln!(
                s,
                "    {}: {{ \"type\": {}, \"value\": {} }}{comma}",
                json_str(k),
                json_str(&c.ty),
                json_str(&c.value)
            );
        }
        s.push_str("  },\n");

        s.push_str("  \"types\": {\n");
        for (i, (k, t)) in self.types.iter().enumerate() {
            let comma = if i + 1 == self.types.len() { "" } else { "," };
            let _ = writeln!(s, "    {}: {{", json_str(k));
            let _ = writeln!(s, "      \"kind\": {},", json_str(t.kind.as_str()));
            let _ = writeln!(s, "      \"size\": {},", t.size);
            let _ = writeln!(s, "      \"align\": {},", t.align);
            let embeds: Vec<String> = t.embeds.iter().map(|e| json_str(e)).collect();
            let _ = writeln!(s, "      \"embeds\": [{}],", embeds.join(", "));
            s.push_str("      \"fields\": [\n");
            for (j, f) in t.fields.iter().enumerate() {
                let fcomma = if j + 1 == t.fields.len() { "" } else { "," };
                let _ = writeln!(
                    s,
                    "        {{ \"name\": {}, \"offset\": {}, \"type\": {} }}{fcomma}",
                    json_str(&f.name),
                    match f.offset {
                        Some(o) => o.to_string(),
                        None => "null".into(),
                    },
                    json_str(&f.ty)
                );
            }
            s.push_str("      ]\n");
            let _ = writeln!(s, "    }}{comma}");
        }
        s.push_str("  }\n}\n");
        s
    }
}

fn named(fields: &syn::Fields) -> Vec<(String, Type)> {
    match fields {
        syn::Fields::Named(n) => n
            .named
            .iter()
            .filter(is_public)
            .filter_map(|f| f.ident.as_ref().map(|i| (i.to_string(), f.ty.clone())))
            .collect(),
        _ => Vec::new(),
    }
}

/// Exclude bindgen's private `_unused` fields for opaque declarations.
fn is_public(f: &&syn::Field) -> bool {
    matches!(f.vis, syn::Visibility::Public(_))
}

fn read_layout_block(c: &syn::ItemConst, out: &mut Layouts) -> Result<()> {
    let Expr::Block(block) = &*c.expr else {
        return Ok(());
    };
    for stmt in &block.block.stmts {
        let expr = match stmt {
            Stmt::Expr(e, _) => e,
            _ => continue,
        };
        let Expr::Index(idx) = expr else { continue };
        // `["Size of X"][ size_of::<X>() - 48usize ]`
        let Expr::Array(arr) = &*idx.expr else {
            continue;
        };
        let Some(Expr::Lit(lit)) = arr.elems.first() else {
            continue;
        };
        let Lit::Str(label) = &lit.lit else { continue };
        let Expr::Binary(bin) = &*idx.index else {
            continue;
        };
        if !matches!(bin.op, syn::BinOp::Sub(_)) {
            continue;
        }
        let Expr::Lit(v) = &*bin.right else { continue };
        let Lit::Int(n) = &v.lit else { continue };
        let value: u64 = n.base10_parse().context("a layout assertion's number")?;

        let label = label.value();
        if let Some(rest) = label.strip_prefix("Size of ") {
            out.size.insert(rest.to_string(), value);
        } else if let Some(rest) = label.strip_prefix("Alignment of ") {
            out.align.insert(rest.to_string(), value);
        } else if let Some(rest) = label.strip_prefix("Offset of field: ") {
            let Some((ty, field)) = rest.split_once("::") else {
                bail!("layout assertion {label:?} is not TYPE::FIELD");
            };
            out.offset
                .insert((ty.to_string(), field.to_string()), value);
        }
    }
    Ok(())
}

/// Collect by-value dependencies, including arrays but excluding pointers.
fn by_value_refs(ty: &Type, out: &mut BTreeSet<String>) {
    match ty {
        Type::Path(p) => {
            if let Some(seg) = p.path.segments.last() {
                out.insert(seg.ident.to_string());
                if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                    for a in &args.args {
                        if let syn::GenericArgument::Type(t) = a {
                            by_value_refs(t, out);
                        }
                    }
                }
            }
        }
        Type::Array(a) => by_value_refs(&a.elem, out),
        Type::Paren(p) => by_value_refs(&p.elem, out),
        Type::Group(g) => by_value_refs(&g.elem, out),
        _ => {}
    }
}

fn spell(ty: &Type) -> String {
    normalise(&ty.to_token_stream().to_string())
}

fn tokens(e: &Expr) -> String {
    normalise(&e.to_token_stream().to_string())
}

/// Normalize token spacing consistently for readable manifest comparisons.
fn normalise(s: &str) -> String {
    let mut out = s
        .replace(" :: ", "::")
        .replace(":: ", "::")
        .replace(" ::", "::");
    for (from, to) in [
        (" <", "<"),
        ("< ", "<"),
        (" >", ">"),
        ("> ", ">"),
        (" ,", ","),
        (" ;", ";"),
        ("[ ", "["),
        (" ]", "]"),
        ("( ", "("),
        (" )", ")"),
        ("* ", "*"),
        (" !", "!"),
    ] {
        out = out.replace(from, to);
    }
    out
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
