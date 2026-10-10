//! Proves exact fixed points in scalar transitions with a separate loop counter.

use fai_core::ir::{CExpr, ExprKind as K, Lit, Prim};
use fai_resolve::LocalId;
use fai_types::{Con, Ty};
use rustc_hash::FxHashMap;

/// A counter-independent transition, terminating at an invariant upper bound.
#[derive(Clone)]
pub(crate) struct Plan {
    /// The unit-step induction variable.
    pub(crate) counter: usize,
    /// Its invariant terminal upper bound.
    pub(crate) bound: usize,
    /// All potentially changing state, compared by exact Float bits.
    pub(crate) floats: Vec<usize>,
}

/// Finds a finite counted loop whose scalar state transition ignores its counter.
pub(crate) fn plan(params: &[LocalId], body: &CExpr, types: &FxHashMap<usize, Ty>) -> Option<Plan> {
    if params.len() > 10
        || params
            .iter()
            .any(|p| !matches!(types.get(&p.index()), Some(Ty::Con(Con::Int | Con::Float))))
    {
        return None;
    }
    let mut prefix = body;
    let mut defs = FxHashMap::default();
    while let K::Let { local, value, body } = &prefix.kind {
        if defs.len() >= 256 {
            return None;
        }
        defs.insert(*local, (**value).clone());
        prefix = body;
    }
    let K::If { cond, then, els } = &prefix.kind else { return None };
    let cond = resolve(cond, &defs);
    let K::Prim { op: Prim::IntGe, args } = &cond.kind else { return None };
    let [left, right] = args.as_slice() else { return None };
    let K::Local(counter) = resolve(left, &defs).kind else { return None };
    let K::Local(bound) = resolve(right, &defs).kind else { return None };
    if counter == bound {
        return None;
    }
    let counter = params.iter().position(|p| *p == counter)?;
    let bound = params.iter().position(|p| *p == bound)?;
    if !matches!(types.get(&params[counter].index()), Some(Ty::Con(Con::Int)))
        || !matches!(types.get(&params[bound].index()), Some(Ty::Con(Con::Int)))
    {
        return None;
    }
    let floats: Vec<_> = params
        .iter()
        .enumerate()
        .filter_map(|(index, p)| {
            matches!(types.get(&p.index()), Some(Ty::Con(Con::Float))).then_some(index)
        })
        .collect();
    if floats.is_empty() {
        return None;
    }
    let result = Plan { counter, bound, floats };
    let mut scan = Scan {
        params,
        plan: &result,
        defs: FxHashMap::default(),
        deps: params.iter().enumerate().map(|(i, p)| (*p, i == counter)).collect(),
        budget: 256,
        recurs: 0,
    };
    // Prefix expressions still execute at the terminal counter. They must be
    // scalar and effect-free; transition/control dependence is checked below.
    let mut prefix = body;
    while let K::Let { local, value, body } = &prefix.kind {
        let dep = scan.walk(value, false)?;
        scan.deps.insert(*local, dep);
        scan.defs.insert(*local, (**value).clone());
        prefix = body;
    }
    scan.walk(then, false)?;
    scan.walk(els, true)?;
    (scan.recurs > 0).then_some(result)
}

fn resolve(e: &CExpr, defs: &FxHashMap<LocalId, CExpr>) -> CExpr {
    let mut e = e.clone();
    for _ in 0..32 {
        match e.kind {
            K::Local(local) => {
                let Some(value) = defs.get(&local) else { break };
                e = value.clone();
            }
            K::Dup { body, .. } | K::Drop { body, .. } => e = *body,
            _ => break,
        }
    }
    e
}

struct Scan<'a> {
    params: &'a [LocalId],
    plan: &'a Plan,
    defs: FxHashMap<LocalId, CExpr>,
    deps: FxHashMap<LocalId, bool>,
    budget: usize,
    recurs: usize,
}

impl Scan<'_> {
    fn walk(&mut self, e: &CExpr, tail: bool) -> Option<bool> {
        if self.budget == 0 {
            return None;
        }
        self.budget -= 1;
        match &e.kind {
            K::Local(local) => self.deps.get(local).copied(),
            K::Lit(Lit::Int(_) | Lit::Float(_) | Lit::Bool(_) | Lit::Unit | Lit::Char(_))
            | K::Error => Some(false),
            K::Let { local, value, body } => {
                let dep = self.walk(value, false)?;
                self.deps.insert(*local, dep);
                self.defs.insert(*local, (**value).clone());
                self.walk(body, tail)
            }
            K::If { cond, then, els } => {
                if self.walk(cond, false)? {
                    return None;
                }
                let a = self.walk(then, tail)?;
                let b = self.walk(els, tail)?;
                Some(a || b)
            }
            K::Prim { op, args } if numeric(*op) => {
                let mut dep = false;
                for arg in args {
                    dep |= self.walk(arg, false)?;
                }
                if dep && matches!(op, Prim::IntDiv | Prim::IntRem) {
                    return None;
                }
                Some(dep)
            }
            K::Dup { body, .. } | K::Drop { body, .. } => self.walk(body, tail),
            K::Recur { args } if tail && args.len() == self.params.len() => {
                let count = resolve(&args[self.plan.counter], &self.defs);
                let K::Prim { op: Prim::IntAdd, args: increment } = count.kind else { return None };
                let [a, b] = increment.as_slice() else { return None };
                if !matches!(resolve(a, &self.defs).kind, K::Local(id) if id == self.params[self.plan.counter])
                    || !matches!(resolve(b, &self.defs).kind, K::Lit(Lit::Int(1)))
                {
                    return None;
                }
                for (i, arg) in args.iter().enumerate() {
                    if i == self.plan.counter {
                        continue;
                    }
                    if self.plan.floats.contains(&i) {
                        if self.walk(arg, false)? {
                            return None;
                        }
                    } else if !matches!(resolve(arg, &self.defs).kind, K::Local(id) if id == self.params[i])
                    {
                        return None;
                    }
                }
                self.recurs += 1;
                Some(false)
            }
            _ => None,
        }
    }
}

fn numeric(op: Prim) -> bool {
    matches!(
        op,
        Prim::IntAdd
            | Prim::IntSub
            | Prim::IntMul
            | Prim::IntDiv
            | Prim::IntRem
            | Prim::IntAnd
            | Prim::IntOr
            | Prim::IntXor
            | Prim::IntShl
            | Prim::IntShr
            | Prim::IntShrLogical
            | Prim::IntComplement
            | Prim::IntLt
            | Prim::IntLe
            | Prim::IntGt
            | Prim::IntGe
            | Prim::FloatAdd
            | Prim::FloatSub
            | Prim::FloatMul
            | Prim::FloatDiv
            | Prim::FloatNeg
            | Prim::FloatLt
            | Prim::FloatLe
            | Prim::FloatGt
            | Prim::FloatGe
            | Prim::Eq
            | Prim::Compare
            | Prim::IntToFloat
            | Prim::FloatToInt
            | Prim::Sqrt
            | Prim::FloatFromBits
            | Prim::FloatToBits
            | Prim::Not
    )
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
        let lowered = fai_rc::rc_emit(&db, db.source_file(id).unwrap(), Symbol::intern("loop"));
        let mut types = FxHashMap::default();
        crate::emit::collect_local_types(&lowered.entry().body, &mut types);
        let K::Join { params, body } = &lowered.entry().body.kind else { return false };
        plan(params, body, &types).is_some()
    }

    #[test]
    fn scalar_counted_transition_can_have_an_exact_fixed_point() {
        assert!(planned(
            "module M\nlet loop i n x = if i >= n then x else loop (i + 1) n (x * 0.5)\n"
        ));
    }

    #[test]
    fn counter_dependent_state_cannot_skip_later_steps() {
        assert!(!planned(
            "module M\nlet loop i n x = if i >= n then x else loop (i + 1) n (x + Int.toFloat i)\n"
        ));
    }

    #[test]
    fn counter_dependent_traps_cannot_skip_later_steps() {
        assert!(!planned(
            "module M\nlet loop i n x = if i >= n then x else\n  let _ = 1 / (i - 1)\n  loop (i + 1) n (x * 0.5)\n"
        ));
    }

    #[test]
    fn effects_are_not_fixed_point_transitions() {
        assert!(!planned(
            "module M\nlet loop i n x = if i >= n then x else\n  let _ = stdConsole.writeLine \"step\"\n  loop (i + 1) n (x * 0.5)\n"
        ));
    }

    #[test]
    fn a_counter_step_that_can_wrap_keeps_the_original_loop() {
        assert!(!planned(
            "module M\nlet loop i n x = if i >= n then x else loop (i + 2) n (x * 0.5)\n"
        ));
    }

    #[test]
    fn a_recursive_terminal_branch_is_not_a_counted_exit() {
        assert!(!planned(
            "module M\nlet loop i n x = if i >= n then loop 0 n x else loop (i + 1) n (x * 0.5)\n"
        ));
    }

    #[test]
    fn constant_matrix_iteration_has_counter_independent_state() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let source = include_str!("../../../samples/algorithms/VecMat.fai");
        let id = db.add_source("VecMat.fai".into(), source.into());
        let lowered = fai_rc::rc_emit(&db, db.source_file(id).unwrap(), Symbol::intern("simulate"));
        let mut types = FxHashMap::default();
        crate::emit::collect_local_types(&lowered.entry().body, &mut types);
        let K::Join { params, body } = &lowered.entry().body.kind else {
            panic!("{}", fai_core::pretty_def(&lowered))
        };
        assert!(
            plan(params, body, &types).is_some(),
            "{}\n{types:?}",
            fai_core::pretty_def(&lowered)
        );
    }
}
