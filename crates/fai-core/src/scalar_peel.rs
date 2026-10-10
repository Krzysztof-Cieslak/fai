//! Bounded expansion of non-tail self-calls with a scalar-only native ABI.

use std::sync::Arc;

use fai_db::Db;
use fai_resolve::DefId;

use crate::helper_inline::{build_inline, node_count};
use crate::ir::{CExpr, CoreFn, ExprKind as K, LoweredDef, Repr};

pub(crate) fn peel(db: &dyn Db, source: Arc<LoweredDef>) -> Arc<LoweredDef> {
    let abi = crate::abi_of(db, source.def);
    let scalar = |repr: &Repr| matches!(repr, Repr::ScalarInt | Repr::ScalarFloat);
    if source.fns.len() != 1
        || !abi.register_abi
        || abi.params.len() > 4
        || !scalar(&abi.ret)
        || !abi.params.iter().all(scalar)
        || !source.entry().captures.is_empty()
    {
        return source;
    }
    let size = node_count(&source.entry().body);
    if size > 64 {
        return source;
    }
    let mut result = (*source).clone();
    let mut peeler = Peeler {
        def: source.def,
        template: source.entry(),
        next: crate::inline::next_free_local(&source),
        remaining: 192,
        cost: size,
        changed: false,
        expansions: 0,
        has_tail_call: false,
    };
    peeler.walk(&mut result.fns[0].body, true);
    // Smaller branching definitions can spend the remaining shared copy budget
    // on extra layers. Each traversal leaves newly inserted bodies unvisited.
    if size <= 32 && peeler.expansions >= 2 && !peeler.has_tail_call {
        peeler.walk(&mut result.fns[0].body, true);
        if size <= 24 && peeler.remaining >= peeler.cost {
            peeler.walk(&mut result.fns[0].body, true);
        }
    }
    // Mixed tail/non-tail recursion already has a compact native loop. Copying
    // that loop's branches into its recursive argument increases register and
    // code pressure; leave those functions to ordinary tail-call lowering.
    // Keep linear integer recursion intact for accumulator-based tail lowering.
    let linear_int = abi.ret == Repr::ScalarInt && peeler.expansions == 1;
    if peeler.changed && !peeler.has_tail_call && !linear_int { Arc::new(result) } else { source }
}

struct Peeler<'a> {
    def: DefId,
    template: &'a CoreFn,
    next: usize,
    remaining: usize,
    cost: usize,
    changed: bool,
    expansions: usize,
    has_tail_call: bool,
}

impl Peeler<'_> {
    fn walk(&mut self, e: &mut CExpr, tail: bool) {
        match &mut e.kind {
            K::If { cond, then, els } => {
                self.walk(cond, false);
                self.walk(then, tail);
                self.walk(els, tail);
            }
            K::Let { value, body, .. } | K::LetMany { value, body, .. } => {
                self.walk(value, false);
                self.walk(body, tail);
            }
            K::App { func, args, .. } => {
                self.walk(func, false);
                args.iter_mut().for_each(|arg| self.walk(arg, false));
            }
            K::Prim { args, .. } | K::Foreign { args, .. } | K::MakeData { args, .. } => {
                args.iter_mut().for_each(|arg| self.walk(arg, false));
            }
            K::DataTag { base, .. } | K::DataField { base, .. } => self.walk(base, false),
            _ => {}
        }
        if tail
            && let K::App { func, .. } = &e.kind
            && matches!(func.kind, K::Global(def) if def == self.def)
        {
            self.has_tail_call = true;
        }
        if !tail
            && self.remaining >= self.cost
            && let K::App { func, args, reuse, .. } = &e.kind
            && matches!(func.kind, K::Global(def) if def == self.def)
            && args.len() == self.template.params.len()
            && reuse.is_empty()
        {
            self.remaining -= self.cost;
            self.changed = true;
            self.expansions += 1;
            // Inserted bodies are not revisited within this traversal. The
            // shared budget and bounded traversal count cap work and code growth.
            *e =
                build_inline(self.template, args.clone(), args.len(), e.ty.clone(), &mut self.next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_db::FaiDatabase;
    use fai_syntax::Symbol;

    fn compare(source: &str) -> (Arc<LoweredDef>, Arc<LoweredDef>) {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        let original = crate::helper_inlined(&db, db.source_file(id).unwrap(), Symbol::intern("f"));
        let result = peel(&db, original.clone());
        (original, result)
    }

    #[test]
    fn small_branching_scalar_recursion_uses_the_shared_copy_budget() {
        let (before, after) =
            compare("module M\nlet f n = if n <= 1 then n else f (n - 1) + f (n - 2)\n");
        assert!(!Arc::ptr_eq(&before, &after));
        assert!(node_count(&after.entry().body) <= node_count(&before.entry().body) + 208);
        assert!(node_count(&after.entry().body) > node_count(&before.entry().body) * 4);
        assert!(
            crate::pretty_def(&after).contains("@f"),
            "residual calls remain after bounded expansion"
        );
    }

    #[test]
    fn mixed_tail_recursion_keeps_its_existing_loop_shape() {
        let (before, after) = compare(
            "module M\nlet f m n = if m = 0 then n + 1 else if n = 0 then f (m - 1) 1 else f (m - 1) (f m (n - 1))\n",
        );
        assert!(Arc::ptr_eq(&before, &after));
    }

    #[test]
    fn data_parameters_keep_their_original_ownership_boundaries() {
        let (before, after) =
            compare("module M\nlet f xs = match xs with | [] -> 0 | x :: rest -> x + f rest\n");
        assert!(Arc::ptr_eq(&before, &after));
    }

    #[test]
    fn a_large_recursive_body_is_not_expanded() {
        let expression = std::iter::repeat_n("n", 50).collect::<Vec<_>>().join(" + ");
        let source =
            format!("module M\nlet f n = if n <= 0 then 0 else f (n - 1) + {expression}\n");
        let (before, after) = compare(&source);
        assert!(Arc::ptr_eq(&before, &after));
    }

    #[test]
    fn linear_integer_recursion_keeps_accumulator_lowering_eligible() {
        let (before, after) = compare("module M\nlet f n = if n <= 0 then 0 else 1 + f (n - 1)\n");
        assert!(Arc::ptr_eq(&before, &after));
    }

    #[test]
    fn argument_bindings_remain_bounded_with_four_parameters() {
        let (before, after) = compare(
            "module M\nlet f n a b c = if n <= 0 then a + b + c else f (n - 1) a b c + f (n - 1) c b a\n",
        );
        assert!(node_count(&after.entry().body) <= node_count(&before.entry().body) + 224);
    }
}
