//! Proves which tail-loop parameters preserve their incoming array layout.

use fai_core::ir::{CExpr, ExprKind as K, Prim};
use fai_resolve::LocalId;
use rustc_hash::{FxHashMap, FxHashSet};

/// Parameters passed back unchanged or through layout-preserving array updates.
/// No ownership operation is moved; only the descriptor predicate is reused.
pub(crate) fn invariant(params: &[LocalId], body: &CExpr) -> FxHashSet<LocalId> {
    let mut scan = Scan {
        params,
        origins: params.iter().copied().map(|p| (p, p)).collect(),
        candidates: params.iter().copied().collect(),
        budget: 1024,
        saw_backedge: false,
        valid: true,
    };
    scan.walk(body);
    if scan.valid && scan.saw_backedge { scan.candidates } else { FxHashSet::default() }
}

struct Scan<'a> {
    params: &'a [LocalId],
    origins: FxHashMap<LocalId, LocalId>,
    candidates: FxHashSet<LocalId>,
    budget: usize,
    saw_backedge: bool,
    valid: bool,
}

impl Scan<'_> {
    fn origin(&self, value: &CExpr) -> Option<LocalId> {
        match &value.kind {
            K::Local(local) => self.origins.get(local).copied(),
            K::Prim { op: Prim::ArraySet | Prim::ArrayPut | Prim::ArrayUnique, args } => {
                args.first().and_then(|base| self.origin(base))
            }
            K::Dup { body, .. } | K::Drop { body, .. } => self.origin(body),
            _ => None,
        }
    }

    fn walk(&mut self, e: &CExpr) {
        if self.budget == 0 {
            self.valid = false;
            return;
        }
        self.budget -= 1;
        match &e.kind {
            K::Let { local, value, body } => {
                self.walk(value);
                if let Some(origin) = self.origin(value) {
                    self.origins.insert(*local, origin);
                } else {
                    self.origins.remove(local);
                }
                self.walk(body);
            }
            K::If { cond, then, els } => {
                self.walk(cond);
                let incoming = self.origins.clone();
                self.walk(then);
                self.origins = incoming.clone();
                self.walk(els);
                self.origins = incoming;
            }
            K::Recur { args } => {
                self.saw_backedge = true;
                if args.len() != self.params.len() {
                    self.valid = false;
                    return;
                }
                for (param, arg) in self.params.iter().zip(args) {
                    if self.origin(arg) != Some(*param) {
                        self.candidates.remove(param);
                    }
                }
            }
            K::Join { .. } => self.valid = false,
            K::LetMany { value, body, .. } | K::Reset { value, body, .. } => {
                self.walk(value);
                self.walk(body);
            }
            K::Prim { args, .. }
            | K::Foreign { args, .. }
            | K::MakeData { args, .. }
            | K::Spread { components: args } => args.iter().for_each(|arg| self.walk(arg)),
            K::App { func, args, .. } => {
                self.walk(func);
                args.iter().for_each(|arg| self.walk(arg));
            }
            K::DataTag { base, .. } | K::DataField { base, .. } | K::HoleClose { base, .. } => {
                self.walk(base)
            }
            K::Dup { body, .. }
            | K::Drop { body, .. }
            | K::FreeReuse { body, .. }
            | K::HoleStart { body, .. } => self.walk(body),
            K::HoleFill { cell, .. } => self.walk(cell),
            K::Local(_) | K::Global(_) | K::Lit(_) | K::MakeClosure { .. } | K::Error => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_db::{Db, FaiDatabase};
    use fai_syntax::Symbol;

    fn stable(source: &str) -> Vec<bool> {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), format!("module M\n{source}"));
        let lowered = fai_rc::rc(&db, db.source_file(id).unwrap(), Symbol::intern("copy"));
        let K::Join { params, body } = &lowered.entry().body.kind else {
            panic!("expected a tail loop")
        };
        let fixed = invariant(params, body);
        params.iter().map(|param| fixed.contains(param)).collect()
    }

    #[test]
    fn unchanged_and_updated_arrays_keep_layout() {
        assert_eq!(
            stable(
                "let copy n source target = if n <= 0 then target else copy (n - 1) source (Array.unsafeSet (n - 1) (Array.unsafeGet (n - 1) source) target)\n"
            ),
            [false, true, true]
        );
    }

    #[test]
    fn swapping_parameters_requires_new_layout_reads() {
        assert_eq!(
            stable(
                "let copy n source target = if n <= 0 then target else copy (n - 1) target source\n"
            ),
            [false, false, false]
        );
    }

    #[test]
    fn a_call_result_cannot_prove_layout_preservation() {
        assert_eq!(
            stable("let copy f n source = if n <= 0 then source else copy f (n - 1) (f source)\n"),
            [true, false, false]
        );
    }

    #[test]
    fn growing_an_array_requires_new_layout_reads() {
        assert_eq!(
            stable(
                "let copy n value source = if n <= 0 then source else copy (n - 1) value (Array.push value source)\n"
            ),
            [false, true, false]
        );
    }

    #[test]
    fn a_replacement_on_one_branch_prevents_hoisting() {
        assert_eq!(
            stable(
                "let copy n source target = if n <= 0 then target else if n % 2 = 0 then copy (n - 1) source target else copy (n - 1) target target\n"
            ),
            [false, false, true]
        );
    }
}
