//! Proves that a numeric array loop preserves an initially unique buffer.

use fai_core::ir::{CExpr, ExprKind as K, Lit, Prim};
use fai_resolve::LocalId;
use fai_types::{Con, Ty};
use rustc_hash::{FxHashMap, FxHashSet};

/// Array aliases that retain the buffer and its initial ownership state.
pub(crate) struct Plan {
    /// The incoming array whose count is checked once.
    pub(crate) root: LocalId,
    /// Same-buffer results of the loop's in-place updates.
    pub(crate) aliases: FxHashSet<LocalId>,
}

fn scalar(ty: &Ty) -> bool {
    matches!(ty, Ty::Unit | Ty::Con(Con::Int | Con::Float | Con::Bool | Con::Char))
}

fn array_element(ty: &Ty) -> Option<&Ty> {
    match ty {
        Ty::App(head, element) if matches!(head.as_ref(), Ty::Con(Con::Array)) => Some(element),
        _ => None,
    }
}

/// Selects a bounded scalar-only update loop with one nonescaping numeric array.
pub(crate) fn plan(params: &[LocalId], body: &CExpr, types: &FxHashMap<usize, Ty>) -> Option<Plan> {
    let arrays: Vec<_> = params
        .iter()
        .enumerate()
        .filter(|(_, local)| types.get(&local.index()).and_then(array_element).is_some())
        .collect();
    let [(position, root)] = arrays.as_slice() else { return None };
    let element = types.get(&root.index()).and_then(array_element)?;
    if !scalar(element)
        || array_element(&body.ty) != Some(element)
        || params
            .iter()
            .any(|local| local != *root && !types.get(&local.index()).is_some_and(scalar))
    {
        return None;
    }
    let mut scan = Scan {
        aliases: FxHashSet::from_iter([**root]),
        types,
        position: *position,
        arity: params.len(),
        writes: 0,
        budget: 256,
    };
    if !scan.walk(body, true) || scan.writes == 0 {
        return None;
    }
    Some(Plan { root: **root, aliases: scan.aliases })
}

struct Scan<'a> {
    aliases: FxHashSet<LocalId>,
    types: &'a FxHashMap<usize, Ty>,
    position: usize,
    arity: usize,
    writes: usize,
    budget: usize,
}

impl Scan<'_> {
    fn array_result(&self, e: &CExpr) -> bool {
        match &e.kind {
            K::Local(local) => self.aliases.contains(local),
            K::Prim { op: Prim::ArraySet, args } => {
                args.first().is_some_and(|arg| self.array_result(arg))
            }
            K::Let { body, .. } | K::Drop { body, .. } | K::Dup { body, .. } => {
                self.array_result(body)
            }
            _ => false,
        }
    }

    fn walk(&mut self, e: &CExpr, array_value: bool) -> bool {
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        match &e.kind {
            K::Local(local) => {
                if self.aliases.contains(local) {
                    array_value
                } else {
                    scalar(self.types.get(&local.index()).unwrap_or(&e.ty))
                }
            }
            K::Lit(Lit::Int(_) | Lit::Float(_) | Lit::Bool(_) | Lit::Char(_) | Lit::Unit)
            | K::Error => true,
            K::Let { local, value, body } => {
                if !self.walk(value, true) {
                    return false;
                }
                if self.array_result(value) {
                    self.aliases.insert(*local);
                }
                self.walk(body, array_value)
            }
            K::If { cond, then, els } => {
                self.walk(cond, false)
                    && self.walk(then, array_value)
                    && self.walk(els, array_value)
            }
            K::Prim { op: Prim::ArrayLength | Prim::ArrayGet | Prim::ArraySet, args } => {
                if !args.first().is_some_and(|arg| self.array_result(arg)) {
                    return false;
                }
                if !self.walk(&args[0], true) {
                    return false;
                }
                if matches!(e.kind, K::Prim { op: Prim::ArraySet, .. }) {
                    if !array_value {
                        return false;
                    }
                    self.writes += 1;
                }
                args.iter().skip(1).all(|arg| self.walk(arg, false))
            }
            K::Prim { args, .. } => scalar(&e.ty) && args.iter().all(|arg| self.walk(arg, false)),
            K::Dup { local, body } | K::Drop { local, body } => {
                !self.aliases.contains(local)
                    && self.types.get(&local.index()).is_some_and(scalar)
                    && self.walk(body, array_value)
            }
            K::Recur { args } => {
                args.len() == self.arity
                    && args.iter().enumerate().all(|(index, arg)| {
                        if index == self.position {
                            self.array_result(arg) && self.walk(arg, true)
                        } else {
                            self.walk(arg, false)
                        }
                    })
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_db::{Db, FaiDatabase};
    use fai_syntax::Symbol;

    fn planned(source: &str) -> bool {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        let lowered = fai_rc::rc(&db, db.source_file(id).unwrap(), Symbol::intern("loop"));
        let K::Join { params, body } = &lowered.entry().body.kind else { return false };
        let mut types = FxHashMap::default();
        crate::emit::collect_local_types(&lowered.entry().body, &mut types);
        plan(params, body, &types).is_some()
    }

    #[test]
    fn one_numeric_array_can_stay_unique_through_updates() {
        assert!(planned(
            "module M\nlet loop i xs = if i >= Array.length xs then xs else loop (i + 1) (Array.unsafeSet i (i + 1) xs)\n"
        ));
    }

    #[test]
    fn growing_array_uses_the_ordinary_path() {
        assert!(!planned(
            "module M\nlet loop n xs = if n <= 0 then xs else loop (n - 1) (Array.push n xs)\n"
        ));
    }

    #[test]
    fn another_array_parameter_keeps_aliasing_conservative() {
        assert!(!planned(
            "module M\nlet loop i xs ys = if i >= Array.length xs then xs else loop (i + 1) (Array.unsafeSet i (Array.unsafeGet i ys) xs) ys\n"
        ));
    }

    #[test]
    fn a_callback_keeps_the_ordinary_path() {
        assert!(!planned(
            "module M\nlet loop f i xs = if i >= Array.length xs then xs else loop f (i + 1) (Array.unsafeSet i (f i) xs)\n"
        ));
    }

    #[test]
    fn retaining_a_pre_update_alias_prevents_versioning() {
        assert!(!planned(
            "module M\nlet loop i xs = if i < 0 then xs else\n  let original = xs\n  let changed = Array.unsafeSet i (i + 1) xs\n  let prior = Array.unsafeGet i original\n  loop (i - 1) (Array.unsafeSet i prior changed)\n"
        ));
    }
}
