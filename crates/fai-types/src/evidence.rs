//! Offset evidence for row-polymorphic record field access.
//!
//! A function polymorphic over a record row `'r` cannot bake in the slot of a
//! field whose position depends on the caller's record. Instead it receives, as
//! leading integer parameters, *offset evidence*: for each label `l` that some
//! open record `{ …, l : _, … | 'r }` in its type forces `'r` to lack, the count
//! of `'r`'s (unknown) fields that sort before `l`. A field access then resolves
//! to that evidence plus a statically known base — the labels named alongside
//! `l` in the same record that sort before it.
//!
//! The evidence is determined entirely by a function's type, so a function and
//! every caller derive the *same* ordered requirement list from the shared
//! scheme and the integers line up positionally.

use rustc_hash::FxHashMap;

use fai_syntax::Symbol;

use crate::ty::{RowEnd, RowVarId, Scheme, Ty};

/// One offset-evidence requirement: the position of `label` within the otherwise
/// unknown record standing in for `row_var`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceReq {
    /// The row variable whose unknown fields shift `label`'s offset.
    pub row_var: RowVarId,
    /// The field whose offset the evidence supplies.
    pub label: Symbol,
}

/// The ordered offset evidence a function of type `scheme` requires: one integer
/// per `(row variable, lacked label)` pair drawn from the open records in its
/// type. Row variables are ordered by first appearance in the type; labels
/// within a row variable by text. Callers reproduce this order from the same
/// scheme, so the integers line up positionally with the function's leading
/// evidence parameters.
#[must_use]
pub fn evidence_requirements(scheme: &Scheme) -> Vec<EvidenceReq> {
    let mut order: Vec<RowVarId> = Vec::new();
    let mut lacks: FxHashMap<RowVarId, Vec<Symbol>> = FxHashMap::default();
    collect(&scheme.ty, &mut order, &mut lacks);

    let mut reqs = Vec::new();
    for row_var in order {
        let mut labels = lacks.remove(&row_var).unwrap_or_default();
        labels.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        labels.dedup();
        for label in labels {
            reqs.push(EvidenceReq { row_var, label });
        }
    }
    reqs
}

/// The number of leading evidence parameters a function of type `scheme` carries.
#[must_use]
pub fn evidence_count(scheme: &Scheme) -> usize {
    evidence_requirements(scheme).len()
}

/// Walks `ty`, recording each open record's tail variable (in first-appearance
/// order) and the labels that variable must lack (its sibling fields).
fn collect(ty: &Ty, order: &mut Vec<RowVarId>, lacks: &mut FxHashMap<RowVarId, Vec<Symbol>>) {
    match ty {
        Ty::Record(row) => {
            if let RowEnd::Open(r) = row.tail {
                if !order.contains(&r) {
                    order.push(r);
                }
                let entry = lacks.entry(r).or_default();
                for (label, _) in &row.fields {
                    entry.push(*label);
                }
            }
            for (_, t) in &row.fields {
                collect(t, order, lacks);
            }
        }
        Ty::App(f, a) | Ty::Arrow(f, a, _) => {
            collect(f, order, lacks);
            collect(a, order, lacks);
        }
        Ty::Tuple(elems) => {
            for e in elems {
                collect(e, order, lacks);
            }
        }
        Ty::Var(_) | Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::Unit | Ty::Error => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::{Con, RecordRow, Ty};

    fn sym(s: &str) -> Symbol {
        Symbol::intern(s)
    }

    fn open_record(fields: &[(&str, Ty)], tail: u32) -> Ty {
        let mut row: Vec<(Symbol, Ty)> = fields.iter().map(|(l, t)| (sym(l), t.clone())).collect();
        row.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        Ty::Record(RecordRow { fields: row, tail: RowEnd::Open(RowVarId(tail)) })
    }

    fn reqs(ty: Ty) -> Vec<(u32, String)> {
        evidence_requirements(&Scheme::mono(ty))
            .into_iter()
            .map(|r| (r.row_var.0, r.label.as_str().to_owned()))
            .collect()
    }

    #[test]
    fn closed_and_monomorphic_types_need_no_evidence() {
        assert!(reqs(Ty::arrow(Ty::int(), Ty::int())).is_empty());
        let closed =
            Ty::Record(RecordRow { fields: vec![(sym("x"), Ty::int())], tail: RowEnd::Closed });
        assert!(reqs(closed).is_empty());
    }

    #[test]
    fn one_open_field_yields_one_requirement() {
        let ty = Ty::arrow(open_record(&[("console", Ty::Con(Con::String))], 0), Ty::Unit);
        assert_eq!(reqs(ty), vec![(0, "console".to_owned())]);
    }

    #[test]
    fn labels_within_a_row_are_ordered_by_text() {
        // Written `{ b, a | 'r }`; evidence is requested in label-text order.
        let ty = Ty::arrow(open_record(&[("b", Ty::int()), ("a", Ty::int())], 0), Ty::int());
        assert_eq!(reqs(ty), vec![(0, "a".to_owned()), (0, "b".to_owned())]);
    }

    #[test]
    fn row_variables_are_ordered_by_first_appearance() {
        let ty = Ty::arrow(
            open_record(&[("x", Ty::int())], 1),
            Ty::arrow(open_record(&[("y", Ty::int())], 0), Ty::int()),
        );
        // `'r` (id 1) appears before `'s` (id 0) in the type.
        assert_eq!(reqs(ty), vec![(1, "x".to_owned()), (0, "y".to_owned())]);
    }

    #[test]
    fn a_repeated_label_on_one_row_is_deduplicated() {
        let ty = Ty::arrow(
            open_record(&[("a", Ty::int())], 0),
            Ty::arrow(open_record(&[("a", Ty::int()), ("b", Ty::int())], 0), Ty::int()),
        );
        assert_eq!(reqs(ty), vec![(0, "a".to_owned()), (0, "b".to_owned())]);
    }

    use proptest::prelude::*;

    const LABELS: &[&str] = &["a", "b", "c", "d"];

    /// A chain of open records, each a `(row variable id, non-empty distinct
    /// labels)`, folded into an arrow type.
    fn open_record_chain() -> impl Strategy<Value = Vec<(u32, Vec<&'static str>)>> {
        let record = (0u32..3, proptest::sample::subsequence(LABELS.to_vec(), 1..=LABELS.len()));
        proptest::collection::vec(record, 1..=5)
    }

    proptest! {
        /// Over arbitrary open-record chains, the requirements are exactly the set
        /// of `(tail variable, label)` pairs, deduplicated, ordered by the row
        /// variable's first appearance and then label text.
        #[test]
        fn requirements_are_complete_deduped_and_ordered(records in open_record_chain()) {
            let mut ty = Ty::Unit;
            for (id, labels) in records.iter().rev() {
                ty = Ty::arrow(
                    open_record(
                        &labels.iter().map(|l| (*l, Ty::int())).collect::<Vec<_>>(),
                        *id,
                    ),
                    ty,
                );
            }
            let result = evidence_requirements(&Scheme::mono(ty));

            // No duplicate (row variable, label) pairs.
            let mut seen = std::collections::HashSet::new();
            for r in &result {
                prop_assert!(seen.insert((r.row_var.0, r.label)), "duplicate {r:?}");
            }
            // Completeness: the same set as every open record's (tail, label) pairs.
            let expected: std::collections::HashSet<(u32, String)> = records
                .iter()
                .flat_map(|(id, labels)| labels.iter().map(move |l| (*id, (*l).to_owned())))
                .collect();
            let got: std::collections::HashSet<(u32, String)> =
                result.iter().map(|r| (r.row_var.0, r.label.as_str().to_owned())).collect();
            prop_assert_eq!(got, expected);

            // Ordering: by the row variable's first appearance, then label text.
            let mut first_seen: Vec<u32> = Vec::new();
            for (id, _) in &records {
                if !first_seen.contains(id) {
                    first_seen.push(*id);
                }
            }
            let rank = |id: u32| first_seen.iter().position(|x| *x == id).unwrap();
            for pair in result.windows(2) {
                let a = (rank(pair[0].row_var.0), pair[0].label.as_str());
                let b = (rank(pair[1].row_var.0), pair[1].label.as_str());
                prop_assert!(a < b, "out of order: {:?} then {:?}", pair[0], pair[1]);
            }
        }
    }
}
