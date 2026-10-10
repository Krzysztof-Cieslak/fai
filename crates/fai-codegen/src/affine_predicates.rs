//! Finds equality predicates whose affine loop bound can advance by a constant.
//!
//! `(x - fixed) = counter` is `x = fixed + counter`, and the opposite
//! subtraction is `x = fixed - counter`, including wrapping arithmetic. The
//! fixed operand and counter transition must agree on every back-edge.

use fai_core::ir::{CExpr, ExprKind as K, Lit, Prim};
use fai_resolve::LocalId;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy)]
pub(crate) struct Predicate {
    /// The subtraction result used only by this equality.
    pub(crate) difference: LocalId,
    /// The loop counter compared with the subtraction result.
    pub(crate) counter: LocalId,
    /// The original varying operand to compare with the maintained bound.
    pub(crate) operand: LocalId,
    /// The operand preserved by every back-edge.
    pub(crate) invariant: LocalId,
    /// Whether the maintained bound is invariant + counter, rather than minus.
    pub(crate) sum: bool,
    /// The wrapping increment of the original counter.
    pub(crate) step: i64,
}

fn children<'a>(e: &'a CExpr, out: &mut Vec<&'a CExpr>) {
    match &e.kind {
        K::Let { value, body, .. }
        | K::LetMany { value, body, .. }
        | K::Reset { value, body, .. } => {
            out.push(value);
            out.push(body);
        }
        K::If { cond, then, els } => {
            out.push(cond);
            out.push(then);
            out.push(els);
        }
        K::Prim { args, .. }
        | K::Foreign { args, .. }
        | K::MakeData { args, .. }
        | K::Recur { args }
        | K::Spread { components: args } => out.extend(args),
        K::App { func, args, .. } => {
            out.push(func);
            out.extend(args);
        }
        K::DataTag { base, .. } | K::DataField { base, .. } | K::HoleClose { base, .. } => {
            out.push(base)
        }
        K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::FreeReuse { body, .. }
        | K::HoleStart { body, .. } => out.push(body),
        K::HoleFill { cell, .. } => out.push(cell),
        _ => {}
    }
}

fn offset(
    e: &CExpr,
    params: &[LocalId],
    definitions: &FxHashMap<LocalId, &CExpr>,
    depth: usize,
) -> Option<(LocalId, i64)> {
    if depth == 0 {
        return None;
    }
    let e = fai_core::bounds::peel_rc(e);
    match &e.kind {
        K::Local(local) if params.contains(local) => Some((*local, 0)),
        K::Local(local) => offset(definitions.get(local)?, params, definitions, depth - 1),
        K::Prim { op: Prim::IntAdd | Prim::IntSub, args } => {
            let [left, CExpr { kind: K::Lit(Lit::Int(right)), .. }] = args.as_slice() else {
                return None;
            };
            let (param, current) = offset(left, params, definitions, depth - 1)?;
            let step = if matches!(e.kind, K::Prim { op: Prim::IntSub, .. }) {
                right.wrapping_neg()
            } else {
                *right
            };
            Some((param, current.wrapping_add(step)))
        }
        _ => None,
    }
}

/// Returns at most four predicates after a complete bounded walk of one loop.
pub(crate) fn plan(params: &[LocalId], body: &CExpr) -> Vec<Predicate> {
    let mut pending = vec![body];
    let mut nodes = Vec::new();
    let mut definitions = FxHashMap::default();
    let mut uses = FxHashMap::<LocalId, usize>::default();
    let mut calls = Vec::new();
    while let Some(node) = pending.pop() {
        if nodes.len() >= 512 || matches!(node.kind, K::Join { .. }) {
            return Vec::new();
        }
        if let K::Let { local, value, .. } = &node.kind
            && definitions.insert(*local, value.as_ref()).is_some()
        {
            return Vec::new();
        }
        if let K::Local(local) = node.kind {
            *uses.entry(local).or_default() += 1;
        }
        if let K::MakeClosure { captures, .. } = &node.kind {
            for local in captures {
                *uses.entry(*local).or_default() += 1;
            }
        }
        if let K::Recur { args } = &node.kind {
            if args.len() != params.len() {
                return Vec::new();
            }
            calls.push(args);
        }
        nodes.push(node);
        children(node, &mut pending);
    }
    if calls.is_empty() {
        return Vec::new();
    }
    let steps: FxHashMap<_, _> = params
        .iter()
        .enumerate()
        .filter_map(|(index, param)| {
            let expected = offset(&calls[0][index], params, &definitions, 8)?;
            (expected.0 == *param
                && calls
                    .iter()
                    .all(|args| offset(&args[index], params, &definitions, 8) == Some(expected)))
            .then_some((*param, expected.1))
        })
        .collect();
    let mut result = Vec::new();
    for node in nodes {
        let K::Prim { op: Prim::Eq, args } = &node.kind else { continue };
        let [CExpr { kind: K::Local(a), .. }, CExpr { kind: K::Local(b), .. }] = args.as_slice()
        else {
            continue;
        };
        for (difference, counter) in [(*a, *b), (*b, *a)] {
            let Some(step) = steps.get(&counter).copied().filter(|step| *step != 0) else {
                continue;
            };
            if uses.get(&difference) != Some(&1) {
                continue;
            }
            let Some(value) = definitions.get(&difference) else { continue };
            let K::Prim { op: Prim::IntSub, args } = &fai_core::bounds::peel_rc(value).kind else {
                continue;
            };
            let [CExpr { kind: K::Local(left), .. }, CExpr { kind: K::Local(right), .. }] =
                args.as_slice()
            else {
                continue;
            };
            let (invariant, operand, sum) = if steps.get(right) == Some(&0) {
                (*right, *left, true)
            } else if steps.get(left) == Some(&0) {
                (*left, *right, false)
            } else {
                continue;
            };
            if result.len() < 4 {
                result.push(Predicate { difference, counter, operand, invariant, sum, step });
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_db::{Db, FaiDatabase};
    use fai_syntax::Symbol;

    fn predicates(source: &str) -> Vec<Predicate> {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        let file = db.source_file(id).unwrap();
        let diagnostics = fai_types::check_file::accumulated::<fai_db::Diag>(&db, file);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let lowered = fai_rc::rc(&db, file, Symbol::intern("scan"));
        let K::Join { params, body } = &lowered.entry().body.kind else { return Vec::new() };
        plan(params, body)
    }

    #[test]
    fn both_directions_of_wrapping_subtraction_are_recognized() {
        let found = predicates(
            "module M\nlet scan fixed distance xs = match xs with | [] -> true | x :: rest -> if x - fixed = distance then false else if fixed - x = distance then false else scan fixed (distance + 1) rest\n",
        );
        assert_eq!(found.len(), 2);
        assert!(found.iter().any(|predicate| predicate.sum));
        assert!(found.iter().any(|predicate| !predicate.sum));
    }

    #[test]
    fn a_changing_fixed_operand_is_not_hoisted() {
        let found = predicates(
            "module M\nlet scan fixed distance xs = match xs with | [] -> true | x :: rest -> if x - fixed = distance then false else scan (fixed + 1) (distance + 1) rest\n",
        );
        assert!(found.is_empty());
    }

    #[test]
    fn differing_back_edge_steps_are_not_hoisted() {
        let found = predicates(
            "module M\nlet scan fixed distance xs = match xs with | [] -> true | x :: rest -> if x - fixed = distance then false else if x < 0 then scan fixed (distance + 1) rest else scan fixed (distance + 2) rest\n",
        );
        assert!(found.is_empty());
    }

    #[test]
    fn a_subtraction_result_with_other_uses_keeps_its_computation() {
        let found = predicates(
            "module M\nlet scan fixed distance xs =\n  match xs with\n  | [] -> true\n  | x :: rest ->\n    let diff = x - fixed\n    if diff = distance then false else if diff < 0 then false else scan fixed (distance + 1) rest\n",
        );
        assert!(found.is_empty());
    }
}
