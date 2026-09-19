// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Classify layout changes as identical, append-only, or breaking.
//! Append-only requires unchanged existing fields and new fields beyond the
//! old size. Growth of a type embedded by value is always breaking.

use crate::abi::manifest::{Constant, Manifest, TypeLayout};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    Identical,
    AppendOnly,
    Breaking,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Identical => "identical",
            Verdict::AppendOnly => "append-only",
            Verdict::Breaking => "breaking",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Change {
    pub name: String,
    pub verdict: Verdict,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PairReport {
    pub from: String,
    pub to: String,
    pub identical: usize,
    pub append_only: usize,
    pub breaking: usize,
    /// Non-identical types only.
    pub types: Vec<Change>,
    /// Non-identical constants only.
    pub constants: Vec<Change>,
}

impl PairReport {
    pub fn verdict(&self) -> Verdict {
        if self.breaking > 0 || !self.constants.is_empty() {
            Verdict::Breaking
        } else if self.append_only > 0 {
            Verdict::AppendOnly
        } else {
            Verdict::Identical
        }
    }
}

/// Compare `from` with `to`. Append-only compatibility is directional.
pub fn classify(from: &Manifest, to: &Manifest) -> PairReport {
    let embedded: BTreeSet<String> = from
        .embedded_types()
        .union(&to.embedded_types())
        .cloned()
        .collect();

    let mut r = PairReport {
        from: from.version.clone(),
        to: to.version.clone(),
        identical: 0,
        append_only: 0,
        breaking: 0,
        types: Vec::new(),
        constants: Vec::new(),
    };

    let names: BTreeSet<&String> = from.types.keys().chain(to.types.keys()).collect();
    for name in names {
        let change = classify_type(
            name,
            from.types.get(name),
            to.types.get(name),
            embedded.contains(name),
            &from.version,
            &to.version,
        );
        match change.verdict {
            Verdict::Identical => r.identical += 1,
            Verdict::AppendOnly => {
                r.append_only += 1;
                r.types.push(change);
            }
            Verdict::Breaking => {
                r.breaking += 1;
                r.types.push(change);
            }
        }
    }

    let cnames: BTreeSet<&String> = from.constants.keys().chain(to.constants.keys()).collect();
    for name in cnames {
        if let Some(change) = classify_const(
            name,
            from.constants.get(name),
            to.constants.get(name),
            &from.version,
            &to.version,
        ) {
            r.constants.push(change);
        }
    }

    r
}

pub fn classify_type(
    name: &str,
    from: Option<&TypeLayout>,
    to: Option<&TypeLayout>,
    embedded: bool,
    from_v: &str,
    to_v: &str,
) -> Change {
    let breaking = |reason: String| Change {
        name: name.to_string(),
        verdict: Verdict::Breaking,
        reasons: vec![reason],
    };
    let (a, b) = match (from, to) {
        (Some(a), Some(b)) => (a, b),
        (Some(_), None) => return breaking(format!("absent in {to_v}")),
        (None, Some(_)) => return breaking(format!("absent in {from_v}, new in {to_v}")),
        (None, None) => unreachable!("a name comes from one of the two manifests"),
    };

    let mut reasons = Vec::new();
    if a.kind != b.kind {
        reasons.push(format!("{} became {}", a.kind.as_str(), b.kind.as_str()));
    }
    if a.align != b.align {
        reasons.push(format!("alignment {} -> {}", a.align, b.align));
    }

    for f in &a.fields {
        match b.fields.iter().find(|g| g.name == f.name) {
            None => reasons.push(format!("field {} removed", f.name)),
            Some(g) => {
                match (f.offset, g.offset) {
                    (Some(x), Some(y)) if x != y => {
                        reasons.push(format!("field {} moved {x} -> {y}", f.name))
                    }
                    (Some(x), None) => {
                        reasons.push(format!("field {} had offset {x} and now has none", f.name))
                    }
                    (None, Some(y)) => {
                        reasons.push(format!("field {} had no offset and now has {y}", f.name))
                    }
                    _ => {}
                }
                if g.ty != f.ty {
                    reasons.push(format!("field {} type {} -> {}", f.name, f.ty, g.ty));
                }
            }
        }
    }

    let added: Vec<&crate::abi::manifest::Field> = b
        .fields
        .iter()
        .filter(|g| !a.fields.iter().any(|f| f.name == g.name))
        .collect();

    if !reasons.is_empty() {
        return Change {
            name: name.to_string(),
            verdict: Verdict::Breaking,
            reasons,
        };
    }

    if added.is_empty() {
        if a.size == b.size {
            return Change {
                name: name.to_string(),
                verdict: Verdict::Identical,
                reasons,
            };
        }
        // Size changed without new fields, possibly through an embedded type.
        return breaking(format!(
            "size {} -> {} with no field of its own added or moved -- something it \
             contains grew",
            a.size, b.size
        ));
    }

    // New fields must lie beyond the old footprint.
    if b.size <= a.size {
        return breaking(format!("size {} -> {} with fields added", a.size, b.size));
    }
    // Unknown offsets cannot establish append-only compatibility.
    if let Some(early) = added.iter().find(|g| g.offset.is_none_or(|o| o < a.size)) {
        return breaking(match early.offset {
            Some(o) => format!(
                "field {} added at {o}, inside the old {}-byte footprint",
                early.name, a.size
            ),
            None => format!("field {} added with no measured offset", early.name),
        });
    }
    if embedded {
        return breaking(format!(
            "grew {} -> {} at the end, but it is contained by value in another \
             footprint type, so growing moves what follows it",
            a.size, b.size
        ));
    }
    Change {
        name: name.to_string(),
        verdict: Verdict::AppendOnly,
        reasons: vec![format!(
            "{} -> {} bytes, {} field(s) appended: {}",
            a.size,
            b.size,
            added.len(),
            added
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )],
    }
}

/// Constant value changes and additions/removals are breaking.
pub fn classify_const(
    name: &str,
    from: Option<&Constant>,
    to: Option<&Constant>,
    from_v: &str,
    to_v: &str,
) -> Option<Change> {
    let reason = match (from, to) {
        (Some(a), Some(b)) if a.value == b.value && a.ty == b.ty => return None,
        (Some(a), Some(b)) if a.value != b.value => format!("{} -> {}", a.value, b.value),
        (Some(a), Some(b)) => format!("type {} -> {}", a.ty, b.ty),
        (Some(_), None) => format!("absent in {to_v}"),
        (None, Some(_)) => format!("absent in {from_v}, new in {to_v}"),
        (None, None) => unreachable!("a name comes from one of the two manifests"),
    };
    Some(Change {
        name: name.to_string(),
        verdict: Verdict::Breaking,
        reasons: vec![reason],
    })
}

// ===========================================================================
// Tests
// ===========================================================================
//
// Synthetic manifests, not real ones: the point of these is that the rules
// are checked against cases somebody wrote on purpose, including the one that
// is easy to get wrong.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::manifest::{Field, Kind, TypeLayout};
    use std::collections::BTreeMap;

    fn f(name: &str, offset: u64, ty: &str) -> Field {
        Field {
            name: name.into(),
            offset: Some(offset),
            ty: ty.into(),
        }
    }

    fn ty(size: u64, fields: Vec<Field>) -> TypeLayout {
        let mut embeds = BTreeSet::new();
        for fl in &fields {
            let base = fl
                .ty
                .trim_start_matches('[')
                .split(';')
                .next()
                .unwrap_or("");
            if base.starts_with("NV_") || base.starts_with("Inner") {
                embeds.insert(base.to_string());
            }
        }
        TypeLayout {
            kind: Kind::Struct,
            size,
            align: 8,
            fields,
            embeds,
        }
    }

    fn manifest(version: &str, types: &[(&str, TypeLayout)], consts: &[(&str, &str)]) -> Manifest {
        Manifest {
            version: version.into(),
            headers: format!("open-gpu-kernel-modules@{version}"),
            commit: "0".into(),
            renames: BTreeMap::new(),
            aliases: BTreeMap::new(),
            constants: consts
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        Constant {
                            ty: "u32".into(),
                            value: v.to_string(),
                        },
                    )
                })
                .collect(),
            types: types
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }

    #[test]
    fn identical_is_identical() {
        let a = manifest(
            "1.0",
            &[("P", ty(8, vec![f("x", 0, "NvU32"), f("y", 4, "NvU32")]))],
            &[("C", "7")],
        );
        let b = manifest(
            "2.0",
            &[("P", ty(8, vec![f("x", 0, "NvU32"), f("y", 4, "NvU32")]))],
            &[("C", "7")],
        );
        let r = classify(&a, &b);
        assert_eq!(r.verdict(), Verdict::Identical);
        assert_eq!(r.identical, 1);
        assert!(r.types.is_empty() && r.constants.is_empty());
    }

    #[test]
    fn a_field_appended_at_the_end_is_append_only() {
        let a = manifest(
            "1.0",
            &[("P", ty(8, vec![f("x", 0, "NvU32"), f("y", 4, "NvU32")]))],
            &[],
        );
        let b = manifest(
            "2.0",
            &[(
                "P",
                ty(
                    16,
                    vec![f("x", 0, "NvU32"), f("y", 4, "NvU32"), f("z", 8, "NvU64")],
                ),
            )],
            &[],
        );
        let r = classify(&a, &b);
        assert_eq!(r.verdict(), Verdict::AppendOnly);
        assert_eq!(r.append_only, 1);
        assert_eq!(r.types[0].name, "P");
    }

    #[test]
    fn append_only_is_not_symmetric() {
        // Reversing an append removes fields.
        let a = manifest(
            "1.0",
            &[("P", ty(8, vec![f("x", 0, "NvU32"), f("y", 4, "NvU32")]))],
            &[],
        );
        let b = manifest(
            "2.0",
            &[(
                "P",
                ty(
                    16,
                    vec![f("x", 0, "NvU32"), f("y", 4, "NvU32"), f("z", 8, "NvU64")],
                ),
            )],
            &[],
        );
        assert_eq!(classify(&b, &a).verdict(), Verdict::Breaking);
    }

    #[test]
    fn a_field_inserted_in_the_middle_is_breaking() {
        // This is 595 -> 610 on alloc_channel.h: hHandleVASpace landed at 32
        // and everything after it moved four bytes.
        let a = manifest(
            "1.0",
            &[(
                "P",
                ty(
                    12,
                    vec![
                        f("hVASpace", 0, "NvU32"),
                        f("hUserdMemory", 4, "NvU32"),
                        f("tail", 8, "NvU32"),
                    ],
                ),
            )],
            &[],
        );
        let b = manifest(
            "2.0",
            &[(
                "P",
                ty(
                    16,
                    vec![
                        f("hVASpace", 0, "NvU32"),
                        f("hHandleVASpace", 4, "NvU32"),
                        f("hUserdMemory", 8, "NvU32"),
                        f("tail", 12, "NvU32"),
                    ],
                ),
            )],
            &[],
        );
        let r = classify(&a, &b);
        assert_eq!(r.verdict(), Verdict::Breaking);
        assert_eq!(r.breaking, 1);
        assert!(r.types[0]
            .reasons
            .iter()
            .any(|s| s.contains("hUserdMemory moved 4 -> 8")));
    }

    #[test]
    fn appending_to_an_embedded_struct_is_breaking() {
        // Growing `Inner` moves the following field in `Outer`.
        let inner_a = ty(8, vec![f("a", 0, "NvU32"), f("b", 4, "NvU32")]);
        let inner_b = ty(
            16,
            vec![f("a", 0, "NvU32"), f("b", 4, "NvU32"), f("c", 8, "NvU64")],
        );
        let outer_a = ty(16, vec![f("inner", 0, "Inner"), f("after", 8, "NvU64")]);
        let outer_b = ty(24, vec![f("inner", 0, "Inner"), f("after", 16, "NvU64")]);
        let a = manifest("1.0", &[("Inner", inner_a), ("Outer", outer_a)], &[]);
        let b = manifest("2.0", &[("Inner", inner_b), ("Outer", outer_b)], &[]);
        let r = classify(&a, &b);
        assert_eq!(r.verdict(), Verdict::Breaking);
        assert_eq!(
            r.append_only, 0,
            "an embedded struct must never be classified append-only"
        );
        let inner = r
            .types
            .iter()
            .find(|c| c.name == "Inner")
            .expect("Inner reported");
        assert_eq!(inner.verdict, Verdict::Breaking);
        assert!(inner.reasons[0].contains("contained by value"));
    }

    #[test]
    fn appending_to_a_struct_held_in_an_array_is_breaking() {
        // Growing an array element changes its stride.
        let inner_a = ty(8, vec![f("a", 0, "NvU32"), f("b", 4, "NvU32")]);
        let inner_b = ty(
            16,
            vec![f("a", 0, "NvU32"), f("b", 4, "NvU32"), f("c", 8, "NvU64")],
        );
        let outer_a = ty(32, vec![f("four", 0, "[Inner; 4usize]")]);
        let outer_b = ty(64, vec![f("four", 0, "[Inner; 4usize]")]);
        let a = manifest("1.0", &[("Inner", inner_a), ("Outer", outer_a)], &[]);
        let b = manifest("2.0", &[("Inner", inner_b), ("Outer", outer_b)], &[]);
        let r = classify(&a, &b);
        assert_eq!(r.append_only, 0);
        let inner = r
            .types
            .iter()
            .find(|c| c.name == "Inner")
            .expect("Inner reported");
        assert!(inner.reasons[0].contains("contained by value"));
    }

    #[test]
    fn growth_into_trailing_padding_is_breaking() {
        // Old callers may have written arbitrary bytes into padding.
        let a = manifest("1.0", &[("P", ty(16, vec![f("x", 0, "NvU32")]))], &[]);
        let b = manifest(
            "2.0",
            &[("P", ty(16, vec![f("x", 0, "NvU32"), f("y", 4, "NvU32")]))],
            &[],
        );
        let r = classify(&a, &b);
        assert_eq!(r.verdict(), Verdict::Breaking);
    }

    #[test]
    fn a_container_that_grew_without_a_field_of_its_own_is_breaking() {
        // An embedded type grew; the container appended no fields.
        let a = manifest(
            "1.0",
            &[("P", ty(32, vec![f("four", 0, "[Inner; 4usize]")]))],
            &[],
        );
        let b = manifest(
            "2.0",
            &[("P", ty(64, vec![f("four", 0, "[Inner; 4usize]")]))],
            &[],
        );
        let r = classify(&a, &b);
        assert_eq!(r.verdict(), Verdict::Breaking);
        assert!(r.types[0].reasons[0].contains("something it"));
    }

    #[test]
    fn a_changed_constant_is_breaking() {
        let a = manifest("1.0", &[], &[("NV2080_NOTIFIERS_GC5_GPU_READY", "34")]);
        let b = manifest("2.0", &[], &[("NV2080_NOTIFIERS_GC5_GPU_READY", "35")]);
        let r = classify(&a, &b);
        assert_eq!(r.verdict(), Verdict::Breaking);
        assert_eq!(r.constants.len(), 1);
        assert_eq!(r.constants[0].reasons[0], "34 -> 35");
    }

    #[test]
    fn a_missing_type_is_breaking_and_says_which_version() {
        // 580.178.04 has no NVA083 class at all.
        let a = manifest("580", &[], &[]);
        let b = manifest(
            "595",
            &[("NVA083_CTRL_P", ty(4, vec![f("x", 0, "NvU32")]))],
            &[],
        );
        let r = classify(&a, &b);
        assert_eq!(r.verdict(), Verdict::Breaking);
        assert!(r.types[0].reasons[0].contains("absent in 580"));
    }

    #[test]
    fn a_field_that_keeps_its_offset_and_changes_type_is_breaking() {
        let a = manifest(
            "1.0",
            &[("P", ty(8, vec![f("x", 0, "NvU32"), f("y", 4, "NvU32")]))],
            &[],
        );
        let b = manifest(
            "2.0",
            &[("P", ty(8, vec![f("x", 0, "NvHandle"), f("y", 4, "NvU32")]))],
            &[],
        );
        let r = classify(&a, &b);
        assert_eq!(r.verdict(), Verdict::Breaking);
        assert!(r.types[0].reasons[0].contains("type NvU32 -> NvHandle"));
    }
}
