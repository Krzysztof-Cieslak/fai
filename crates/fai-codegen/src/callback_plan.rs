//! Finds one unchanged, uniformly applied callback in a bounded tail loop.

use fai_core::ir::{CExpr, CoreFn, ExprKind as K};
use fai_resolve::LocalId;
use fai_types::Ty;
use rustc_hash::FxHashMap;

/// Finds one callback whose identity and application arity stay fixed in the loop.
pub(crate) fn invariant(
    function: &CoreFn,
    types: &FxHashMap<usize, Ty>,
) -> Option<(LocalId, usize)> {
    for (position, &local) in function.params.iter().enumerate() {
        if !matches!(types.get(&local.index()), Some(Ty::Arrow(..))) {
            continue;
        }
        let mut scan = Scan { local, position, arity: None, loop_seen: false, budget: 1024 };
        if scan.walk(&function.body)
            && scan.loop_seen
            && let Some(arity) = scan.arity
        {
            return Some((local, arity));
        }
    }
    None
}

struct Scan {
    local: LocalId,
    position: usize,
    arity: Option<usize>,
    loop_seen: bool,
    budget: usize,
}

impl Scan {
    fn walk(&mut self, e: &CExpr) -> bool {
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        match &e.kind {
            K::App { func, args, .. } => {
                if matches!(func.kind, K::Local(local) if local == self.local) {
                    if self.arity.is_some_and(|arity| arity != args.len()) {
                        return false;
                    }
                    self.arity = Some(args.len());
                }
                self.walk(func) && args.iter().all(|arg| self.walk(arg))
            }
            K::Join { params, body } => {
                self.loop_seen = true;
                params.get(self.position) == Some(&self.local) && self.walk(body)
            }
            K::Recur { args } => {
                args.get(self.position)
                    .is_some_and(|arg| matches!(arg.kind, K::Local(local) if local == self.local))
                    && args.iter().all(|arg| self.walk(arg))
            }
            K::Let { value, body, .. }
            | K::LetMany { value, body, .. }
            | K::Reset { value, body, .. } => self.walk(value) && self.walk(body),
            K::If { cond, then, els } => self.walk(cond) && self.walk(then) && self.walk(els),
            K::Prim { args, .. }
            | K::Foreign { args, .. }
            | K::MakeData { args, .. }
            | K::Spread { components: args } => args.iter().all(|arg| self.walk(arg)),
            K::DataTag { base, .. } | K::DataField { base, .. } | K::HoleClose { base, .. } => {
                self.walk(base)
            }
            K::Dup { body, .. }
            | K::Drop { body, .. }
            | K::FreeReuse { body, .. }
            | K::HoleStart { body, .. } => self.walk(body),
            K::HoleFill { cell, .. } => self.walk(cell),
            K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_db::{Db, FaiDatabase};
    use fai_resolve::DefId;
    use fai_syntax::Symbol;

    fn planned(source: &str) -> Option<usize> {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        let file = db.source_file(id).unwrap();
        let name = Symbol::intern("loop");
        let lowered = fai_rc::rc(&db, file, name);
        let scheme =
            fai_core::representation::definition_scheme(&db, DefId::new(id, name)).unwrap();
        let mut types = FxHashMap::default();
        let mut ty = &scheme.ty;
        for local in &lowered.entry().params {
            let Ty::Arrow(from, to, _) = ty else { break };
            types.insert(local.index(), (**from).clone());
            ty = to;
        }
        invariant(lowered.entry(), &types).map(|(_, arity)| arity)
    }

    #[test]
    fn unchanged_callback_has_one_entry_arity() {
        assert_eq!(
            planned("module M\nlet loop f n x = if n <= 0 then x else loop f (n - 1) (f x)\n"),
            Some(1)
        );
    }

    #[test]
    fn swapped_callbacks_are_not_invariant() {
        assert_eq!(
            planned("module M\nlet loop f g n x = if n <= 0 then x else loop g f (n - 1) (f x)\n"),
            None
        );
    }

    #[test]
    fn mixed_application_arities_keep_dynamic_dispatch() {
        assert_eq!(
            planned(
                "module M\nlet loop f n x = if n <= 0 then f x 0 else\n  let g = f x\n  loop f (n - 1) (g 1)\n"
            ),
            None
        );
    }

    #[test]
    fn non_loop_calls_do_not_precompute_an_entry() {
        assert_eq!(planned("module M\nlet loop f x = f x\n"), None);
    }
}
