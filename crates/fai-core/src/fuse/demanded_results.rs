//! One-layer expansion of small recursive helpers returning deconstructed optional tuples.
//!
//! Only same-file monomorphic helpers with pure arrows and resource-free inputs
//! qualify. Inserted bodies are not revisited. The shared copy budget bounds
//! growth; constructor reduction subsequently removes only nonescaping cells.

use super::*;
use crate::helper_inline::{build_inline, node_count};
use rustc_hash::FxHashSet;

pub(super) fn expand(db: &dyn Db, base: Arc<LoweredDef>) -> Arc<LoweredDef> {
    let mut candidate = false;
    for function in &base.fns {
        walk_pre(&function.body, &mut |e| {
            if let K::Let { value, .. } = &e.kind
                && matches!(value.kind, K::App { .. })
                && matches!(&value.ty, Ty::App(_, payload) if matches!(payload.as_ref(), Ty::Tuple(_)))
            {
                candidate = true;
            }
        });
    }
    if !candidate {
        return base;
    }
    let mut expander = Expander {
        db,
        def: base.def,
        next: crate::inline::next_free_local(&base),
        remaining: 256,
        changed: false,
    };
    let mut output = (*base).clone();
    for function in &mut output.fns {
        expander.walk(&mut function.body);
    }
    if expander.changed { Arc::new(output) } else { base }
}

struct Expander<'a> {
    db: &'a dyn Db,
    def: DefId,
    next: usize,
    remaining: usize,
    changed: bool,
}

fn deconstructed(e: &CExpr, local: LocalId) -> bool {
    match &e.kind {
        K::Local(id) if *id == local => false,
        K::MakeClosure { captures, .. } if captures.contains(&local) => false,
        K::DataField { index: crate::ir::FieldIndex::Dyn { evidence, .. }, .. }
            if *evidence == local =>
        {
            false
        }
        K::DataTag { base, .. } | K::DataField { base, .. } if matches!(base.kind, K::Local(id) if id == local) => {
            true
        }
        _ => {
            let mut result = true;
            local_constructors::children(&mut e.clone(), &mut |child| {
                result &= deconstructed(child, local);
            });
            result
        }
    }
}

fn returns_constructor(e: &CExpr) -> bool {
    match &e.kind {
        K::MakeData { .. } => true,
        K::Let { body, .. } => returns_constructor(body),
        K::If { then, els, .. } => returns_constructor(then) || returns_constructor(els),
        _ => false,
    }
}

fn effect_free(db: &dyn Db, e: &CExpr, recursive: DefId) -> bool {
    match &e.kind {
        K::Foreign { .. } | K::MakeClosure { .. } => false,
        K::Global(def) => crate::purity::global_application_pure_total(db, *def, 0),
        K::App { func, args, .. } => {
            let K::Global(def) = func.kind else { return false };
            (def == recursive || crate::purity::global_application_pure_total(db, def, args.len()))
                && args.iter().all(|arg| effect_free(db, arg, recursive))
        }
        _ => {
            let mut result = true;
            local_constructors::children(&mut e.clone(), &mut |child| {
                result &= effect_free(db, child, recursive);
            });
            result
        }
    }
}

impl Expander<'_> {
    fn walk(&mut self, e: &mut CExpr) {
        if self.remaining == 0 {
            return;
        }
        // Traverse original children only: a newly inserted recursive body must
        // keep its residual calls, including a call to the current definition.
        local_constructors::children(e, &mut |child| self.walk(child));
        let K::Let { local, value, body } = &mut e.kind else { return };
        if node_count(body) > 64 || !deconstructed(body, *local) {
            return;
        }
        let mut inspected = false;
        walk_pre(body, &mut |e| {
            if let K::DataTag { base, .. } | K::DataField { base, .. } = &e.kind {
                inspected |= matches!(base.kind, K::Local(id) if id == *local);
            }
        });
        if !inspected {
            return;
        }
        // Optional tuples are common parser results and otherwise require a
        // temporary payload cell even with a wrapper-free Option. Other result
        // shapes retain their established reuse and interprocedural bounds facts.
        let Ty::App(_, payload) = &value.ty else { return };
        let Ty::Tuple(fields) = payload.as_ref() else { return };
        if !(2..=4).contains(&fields.len())
            || crate::niche::niche_scheme(self.db, &value.ty) != Some(crate::niche::NicheKind::A)
            || !local_constructors::resource_free(
                self.db,
                payload,
                &mut FxHashSet::default(),
                &mut 128,
            )
        {
            return;
        }
        let K::App { func, args, reuse, .. } = &value.kind else { return };
        let K::Global(callee) = func.kind else { return };
        if callee.file != self.def.file || callee == self.def || !reuse.is_empty() {
            return;
        }
        let Some(file) = self.db.source_file(callee.file) else { return };
        if !fai_resolve::recursive_defs(self.db, file).contains(&callee) {
            return;
        }
        let Some(scheme) = crate::representation::definition_scheme(self.db, callee) else {
            return;
        };
        if !scheme.vars.is_empty() || !scheme.row_vars.is_empty() || !scheme.effect_vars.is_empty()
        {
            return;
        }
        let mut result = &scheme.ty;
        for arg in args {
            let Ty::Arrow(input, output, effect) = result else { return };
            if !effect.is_pure()
                || **input != arg.ty
                || !local_constructors::resource_free(
                    self.db,
                    input,
                    &mut FxHashSet::default(),
                    &mut 128,
                )
            {
                return;
            }
            result = output;
        }
        if result != &value.ty || matches!(result, Ty::Arrow(..)) {
            return;
        }
        let helper = crate::helper_inlined(self.db, file, callee.name);
        if helper.fns.len() != 1
            || !helper.entry().captures.is_empty()
            || args.len() != helper.entry().params.len()
            || node_count(&helper.entry().body) > 192
        {
            return;
        }
        let helper = local_constructors::reduce(self.db, helper);
        let size = node_count(&helper.entry().body) + args.len() * 2;
        if size > 128
            || size > self.remaining
            || !returns_constructor(&helper.entry().body)
            || !effect_free(self.db, &helper.entry().body, callee)
        {
            return;
        }
        self.remaining -= size;
        self.changed = true;
        **value = build_inline(
            helper.entry(),
            args.clone(),
            args.len(),
            value.ty.clone(),
            &mut self.next,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_db::FaiDatabase;

    fn lowered(source: &str) -> (Arc<LoweredDef>, LoweredDef) {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        let file = db.source_file(id).unwrap();
        let before = crate::helper_inlined(&db, file, Symbol::intern("run"));
        let after = crate::fuse_def(&db, file, Symbol::intern("run")).body.clone();
        (before, after)
    }

    #[test]
    fn immediate_destructuring_exposes_one_recursive_return_layer() {
        let (before, after) = lowered(
            "module M\nlet step n = if n <= 0 then Some (n, 42) else step (n - 1)\nlet run n = match step n with | None -> 0 | Some (left, right) -> left + right\n",
        );
        let body = crate::pretty_def(&after);
        assert!(body.contains("<=") && body.contains("@step"), "{body}");
        assert!(!body.contains("(data "), "local result cells should disappear: {body}");
        assert!(node_count(&after.entry().body) <= node_count(&before.entry().body) + 256 + 256);
    }

    #[test]
    fn escaping_results_keep_their_existing_call_boundary() {
        let (before, after) = lowered(
            "module M\nlet step n = if n <= 0 then Some (n, 42) else step (n - 1)\nlet run n =\n  let result = step n\n  result\n",
        );
        assert_eq!(crate::pretty_def(&before), crate::pretty_def(&after));
    }

    #[test]
    fn effectful_recursive_helpers_keep_their_call_boundary() {
        let (_, after) = lowered(
            "module M\nlet step n =\n  let _ = stdConsole.writeLine \"step\"\n  if n <= 0 then Some (n, 42) else step (n - 1)\nlet run n = match step n with | None -> 0 | Some (left, right) -> left + right\n",
        );
        let body = crate::pretty_def(&after);
        assert!(
            body.contains("@step") && !body.contains("<=") && !body.contains("\"step\""),
            "{body}"
        );
    }

    #[test]
    fn generic_recursive_helpers_keep_one_shared_body() {
        let (_, after) = lowered(
            "module M\nlet step n value = if n <= 0 then Some (value, 42) else step (n - 1) value\nlet run n = match step n 3 with | None -> 0 | Some (left, right) -> left + right\n",
        );
        let body = crate::pretty_def(&after);
        assert!(body.contains("@step") && !body.contains("<="), "{body}");
    }

    #[test]
    fn ordinary_tuple_results_keep_their_existing_bounds_boundary() {
        let (_, after) = lowered(
            "module M\nlet step n = if n <= 0 then (n, 42) else step (n - 1)\nlet run n =\n  let (left, right) = step n\n  left + right\n",
        );
        let body = crate::pretty_def(&after);
        assert!(body.contains("@step") && !body.contains("<="), "{body}");
    }
}
