// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Group bindgen declarations with their impl blocks.
//! Rendered text detects type/value changes; referenced names propagate
//! version differences through dependent declarations.

use anyhow::{bail, Result};
use proc_macro2::TokenTree;
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use syn::{File, Item, Type};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GroupKind {
    /// A `struct` or `union`: it has a layout, and the manifest measured it.
    Type,
    /// A `type X = Y;`. No layout of its own.
    Alias,
    /// A `const`.
    Const,
}

#[derive(Clone)]
pub struct ItemGroup {
    pub name: String,
    pub kind: GroupKind,
    pub items: Vec<Item>,
    pub text: String,
    pub refs: BTreeSet<String>,
}

pub struct Items {
    /// Preserve bindgen's declaration order in generated modules.
    pub order: Vec<String>,
    pub groups: BTreeMap<String, ItemGroup>,
}

impl Items {
    pub fn collect(file: &File) -> Result<Self> {
        let mut order: Vec<String> = Vec::new();
        let mut groups: BTreeMap<String, ItemGroup> = BTreeMap::new();

        for item in &file.items {
            let (name, kind) = match item {
                Item::Struct(s) => (s.ident.to_string(), GroupKind::Type),
                Item::Union(u) => (u.ident.to_string(), GroupKind::Type),
                Item::Type(t) => (t.ident.to_string(), GroupKind::Alias),
                // Layout assertions are regenerated from the manifest.
                Item::Const(c) if c.ident == "_" => continue,
                Item::Const(c) => (c.ident.to_string(), GroupKind::Const),
                Item::Impl(i) => match base_ident(&i.self_ty) {
                    Some(n) => (n, GroupKind::Type),
                    None => bail!("an impl block whose self type is not a plain path"),
                },
                other => bail!(
                    "bindgen emitted an item this generator does not place: {}",
                    other.to_token_stream()
                ),
            };
            match groups.get_mut(&name) {
                Some(g) => g.items.push(item.clone()),
                None => {
                    order.push(name.clone());
                    groups.insert(
                        name.clone(),
                        ItemGroup {
                            name,
                            kind,
                            items: vec![item.clone()],
                            text: String::new(),
                            refs: BTreeSet::new(),
                        },
                    );
                }
            }
        }

        let names: BTreeSet<String> = groups.keys().cloned().collect();
        for g in groups.values_mut() {
            g.text = render(&g.items);
            let mut idents = BTreeSet::new();
            for item in &g.items {
                walk_idents(item.to_token_stream(), &mut idents);
            }
            g.refs = idents.intersection(&names).cloned().collect();
            g.refs.remove(&g.name);
        }

        Ok(Items { order, groups })
    }
}

fn base_ident(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

pub fn render(items: &[Item]) -> String {
    let file = File {
        shebang: None,
        attrs: Vec::new(),
        items: items.to_vec(),
    };
    prettyplease::unparse(&file)
}

fn walk_idents(ts: proc_macro2::TokenStream, out: &mut BTreeSet<String>) {
    for t in ts {
        match t {
            TokenTree::Ident(i) => {
                out.insert(i.to_string());
            }
            TokenTree::Group(g) => walk_idents(g.stream(), out),
            _ => {}
        }
    }
}
