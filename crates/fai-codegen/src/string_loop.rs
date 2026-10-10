//! Proves that literal-appending loops preserve unique, nonempty inline strings.

use fai_core::ir::{CExpr, ExprKind as K, Lit, Prim};
use fai_resolve::LocalId;
use fai_types::{Con, Ty};
use rustc_hash::{FxHashMap, FxHashSet};

/// Largest literal handled by the native inline append.
pub(crate) const MAX_LITERAL_BYTES: usize = 16;

/// Locals that preserve the incoming String's guarded ownership and layout.
pub(crate) struct Plan {
    /// The incoming String checked before entering the fast loop.
    pub(crate) root: LocalId,
    /// String results derived only through the admitted literal appends.
    pub(crate) aliases: FxHashSet<LocalId>,
}

fn scalar(ty: &Ty) -> bool {
    matches!(ty, Ty::Unit | Ty::Con(Con::Int | Con::Float | Con::Bool | Con::Char))
}

/// Selects a bounded loop whose only String operations preserve its owner.
pub(crate) fn plan(params: &[LocalId], body: &CExpr, types: &FxHashMap<usize, Ty>) -> Option<Plan> {
    let strings: Vec<_> = params
        .iter()
        .enumerate()
        .filter(|(_, local)| matches!(types.get(&local.index()), Some(Ty::Con(Con::String))))
        .collect();
    let [(position, root)] = strings.as_slice() else { return None };
    if body.ty != Ty::Con(Con::String)
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
        appends: 0,
        budget: 256,
    };
    if !scan.walk(body, true) || scan.appends == 0 {
        return None;
    }
    Some(Plan { root: **root, aliases: scan.aliases })
}

struct Scan<'a> {
    aliases: FxHashSet<LocalId>,
    types: &'a FxHashMap<usize, Ty>,
    position: usize,
    arity: usize,
    appends: usize,
    budget: usize,
}

impl Scan<'_> {
    fn string_result(&self, e: &CExpr) -> bool {
        match &e.kind {
            K::Local(local) => self.aliases.contains(local),
            K::Prim { op: Prim::StrConcat, args } => {
                args.first().is_some_and(|arg| self.string_result(arg))
            }
            K::Let { body, .. } | K::Drop { body, .. } | K::Dup { body, .. } => {
                self.string_result(body)
            }
            _ => false,
        }
    }

    fn walk(&mut self, e: &CExpr, string_value: bool) -> bool {
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        match &e.kind {
            K::Local(local) => {
                if self.aliases.contains(local) {
                    string_value
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
                if self.string_result(value) {
                    self.aliases.insert(*local);
                }
                self.walk(body, string_value)
            }
            K::If { cond, then, els } => {
                self.walk(cond, false)
                    && self.walk(then, string_value)
                    && self.walk(els, string_value)
            }
            K::Prim { op: Prim::StrConcat, args } => {
                let [left, right] = args.as_slice() else { return false };
                let K::Lit(Lit::Str(bytes)) = &right.kind else { return false };
                if !string_value
                    || bytes.len() > MAX_LITERAL_BYTES
                    || !self.string_result(left)
                    || !self.walk(left, true)
                {
                    return false;
                }
                self.appends += usize::from(!bytes.is_empty());
                true
            }
            K::Prim { args, .. } => scalar(&e.ty) && args.iter().all(|arg| self.walk(arg, false)),
            K::Dup { local, body } | K::Drop { local, body } => {
                !self.aliases.contains(local)
                    && self.types.get(&local.index()).is_some_and(scalar)
                    && self.walk(body, string_value)
            }
            K::Recur { args } => {
                args.len() == self.arity
                    && args.iter().enumerate().all(|(index, arg)| {
                        if index == self.position {
                            self.string_result(arg) && self.walk(arg, true)
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
    fn bounded_literal_appends_preserve_the_string_owner() {
        assert!(planned(
            "module M\nlet loop n text = if n <= 0 then text else loop (n - 1) (text ++ \"ab\")\n"
        ));
    }

    #[test]
    fn branching_literal_appends_preserve_the_string_owner() {
        assert!(planned(
            "module M\nlet loop n text = if n <= 0 then text else if n % 2 = 0 then loop (n - 1) (text ++ \"λ\") else loop (n - 1) (text ++ \"ab\")\n"
        ));
    }

    #[test]
    fn retaining_an_old_string_alias_prevents_versioning() {
        assert!(!planned(
            "module M\nlet loop n text = if n <= 0 then text else\n  let changed = text ++ \"ab\"\n  loop (n - 1) (changed ++ text)\n"
        ));
    }

    #[test]
    fn computed_append_operands_keep_the_ordinary_loop() {
        assert!(!planned(
            "module M\nlet loop n text = if n <= 0 then text else loop (n - 1) (text ++ Int.toString n)\n"
        ));
    }

    #[test]
    fn long_literals_keep_the_ordinary_loop() {
        assert!(!planned(
            "module M\nlet loop n text = if n <= 0 then text else loop (n - 1) (text ++ \"012345678901234567\")\n"
        ));
    }
}
