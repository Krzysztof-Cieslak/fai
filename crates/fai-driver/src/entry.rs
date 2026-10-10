//! Type checking and offset-evidence elaboration of the runtime-to-main call.

use fai_core::ir::{CExpr, ClosureAlloc, CoreFn, ExprKind, Lit, LoweredDef};
use fai_db::{Db, SourceFile};
use fai_diagnostics::{Diagnostic, Label};
use fai_resolve::{DefId, LocalId, module_defs};
use fai_span::Span;
use fai_syntax::Symbol;
use fai_types::{EffectRow, InferCtx, RowEnd, SolveTy, Ty, UnifyResult};

use crate::backend::{def_arity, runtime_root};
use crate::{INVALID_ENTRY_POINT, tooling_span};

/// A native-only entry specialization for simple, projected default runtimes.
/// Every retained field is the real initializer, evaluated once in field order.
pub(crate) fn projected_default(
    db: &dyn Db,
    file: SourceFile,
    launch: &Entry,
) -> Option<(LoweredDef, LoweredDef)> {
    use fai_core::ir::{ExprKind as K, FieldIndex};
    use std::collections::BTreeMap;

    if launch.adapter.is_some() || launch.concurrent {
        return None;
    }
    let prelude = fai_resolve::module_file(db, fai_resolve::ModuleName(Symbol::intern("Prelude")))?;
    if launch.runtime != DefId::new(prelude.source(db), Symbol::intern("defaultRuntime")) {
        return None;
    }
    let runtime = fai_core::core_inlined(db, prelude, launch.runtime.name);
    let K::MakeData { tag: 0, args: fields, scalars: 0, niche: None, reuse: None } =
        &runtime.entry().body.kind
    else {
        return None;
    };
    for field in fields {
        let K::Global(def) = field.kind else { return None };
        let source = db.source_file(def.file)?;
        let value = fai_core::core_inlined(db, source, def.name);
        if !value.entry().params.is_empty()
            || !literal_initializer(db, &value.entry().body, &mut 256)
        {
            return None;
        }
    }
    let mut main = (*fai_core::helper_inlined(db, file, launch.entry.name)).clone();
    if main.fns.len() != 1 || main.entry().params.len() != 1 {
        return None;
    }
    let root = main.entry().params[0];
    let mut next = fai_core::inline::next_free_local(&main);
    let mut bindings = BTreeMap::new();

    fn replace(
        e: &mut CExpr,
        root: LocalId,
        fields: &[CExpr],
        bindings: &mut BTreeMap<u32, (LocalId, CExpr)>,
        next: &mut usize,
        budget: &mut usize,
    ) -> bool {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        if let K::DataField { base, index: FieldIndex::Const(index), scalar: false, niche: None } =
            &e.kind
            && matches!(base.kind, K::Local(local) if local == root)
        {
            let Some(field) = fields.get(*index as usize) else { return false };
            let (local, _) = bindings.entry(*index).or_insert_with(|| {
                let local = LocalId::from_index(*next);
                *next += 1;
                (local, field.clone())
            });
            e.kind = K::Local(*local);
            return true;
        }
        let mut child = |e: &mut CExpr| replace(e, root, fields, bindings, next, budget);
        match &mut e.kind {
            K::Local(local) => *local != root,
            K::Lit(_) | K::Global(_) | K::Error => true,
            K::Prim { args, .. } | K::Foreign { args, .. } | K::MakeData { args, .. } => {
                args.iter_mut().all(child)
            }
            K::App { func, args, .. } => child(func) && args.iter_mut().all(child),
            K::If { cond, then, els } => child(cond) && child(then) && child(els),
            K::Let { value, body, .. } => child(value) && child(body),
            K::DataField { base, .. } | K::DataTag { base, .. } => child(base),
            _ => false,
        }
    }

    if !replace(&mut main.fns[0].body, root, fields, &mut bindings, &mut next, &mut 256) {
        return None;
    }
    for (_, (local, value)) in bindings.into_iter().rev() {
        let body = std::mem::replace(&mut main.fns[0].body, CExpr::new(K::Error, Ty::Unit));
        main.fns[0].body =
            CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(body) }, Ty::Unit);
    }
    main.def = DefId::new(file.source(db), Symbol::intern("entry#projected"));
    let unit = LoweredDef {
        def: DefId::new(file.source(db), Symbol::intern("entry#unit-runtime")),
        fns: vec![CoreFn {
            params: Vec::new(),
            captures: Vec::new(),
            body: CExpr::new(K::Lit(Lit::Unit), Ty::Unit),
        }],
        entry_borrowed: Vec::new(),
        reuse_entry: None,
        entry_spread_params: Vec::new(),
        data_shapes: Vec::new(),
    };
    Some((main, unit))
}

fn literal_initializer(db: &dyn Db, e: &CExpr, budget: &mut usize) -> bool {
    use fai_core::ir::ExprKind as K;
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match &e.kind {
        K::Lit(_) => true,
        K::MakeClosure { captures, .. } => captures.is_empty(),
        K::MakeData { args, reuse: None, .. } => {
            args.iter().all(|arg| literal_initializer(db, arg, budget))
        }
        K::Global(def) => {
            db.source_file(def.file).is_some_and(|file| def_arity(db, file, def.name) > 0)
        }
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) entry: DefId,
    pub(crate) runtime: DefId,
    pub(crate) adapter: Option<LoweredDef>,
    pub(crate) concurrent: bool,
}

fn definition_span(db: &dyn Db, def: DefId) -> Span {
    let Some(file) = db.source_file(def.file) else { return tooling_span() };
    let defs = module_defs(db, file);
    let Some(info) = defs.get(def.name) else { return tooling_span() };
    let parsed = fai_syntax::parse(db, file);
    Span::new(def.file, parsed.module.items[info.binding.index()].span)
}

fn scheduler_effect(effect: &EffectRow) -> bool {
    effect.labels.iter().any(|i| matches!(i.name.as_str(), "Concurrency" | "Net"))
}

pub(crate) fn prepare(db: &dyn Db, file: SourceFile) -> Result<Entry, Box<Diagnostic>> {
    let main = DefId::new(file.source(db), Symbol::intern("main"));
    let invalid = |message: String| {
        Diagnostic::error(INVALID_ENTRY_POINT, message, definition_span(db, main))
    };
    let runtime = runtime_root(db, file)
        .ok_or_else(|| invalid("no runtime value is available for `main`".into()))?;
    let runtime_file = db
        .source_file(runtime.file)
        .ok_or_else(|| invalid("the selected runtime has no source definition".into()))?;
    if def_arity(db, runtime_file, runtime.name) != 0 {
        return Err(Box::new(Diagnostic::error(
            INVALID_ENTRY_POINT,
            "the runtime builder must be a zero-argument value with no unresolved offset evidence",
            definition_span(db, runtime),
        )));
    }
    let main_scheme = fai_types::declared_or_inferred_scheme(db, main)
        .ok_or_else(|| invalid("could not determine the type of `main`".into()))?;
    let runtime_scheme = fai_types::declared_or_inferred_scheme(db, runtime)
        .ok_or_else(|| invalid("could not determine the runtime type".into()))?;
    let mut cx = InferCtx::new();
    let main_ty = cx.instantiate(&main_scheme);
    let runtime_ty = cx.instantiate(&runtime_scheme);
    let SolveTy::Arrow(input, output, _) = cx.resolve_shallow(&main_ty) else {
        return Err(invalid("`main` must accept one runtime value and return `Unit`".into()).into());
    };
    if cx.unify(&output, &SolveTy::Unit) != UnifyResult::Ok {
        return Err(invalid("`main` must accept one runtime value and return `Unit`".into()).into());
    }
    if cx.subsume_types(&runtime_ty, &input, true) != UnifyResult::Ok {
        return Err(invalid(
            "the selected runtime value does not match `main`'s argument type".into(),
        )
        .with_label(Label::new(definition_span(db, runtime), "runtime value selected here"))
        .into());
    }
    let instantiated = cx.reify(&main_ty);
    let Ty::Arrow(input, _, effect) = &instantiated else { unreachable!("validated main arrow") };
    let concurrent = scheduler_effect(effect)
        || scheduler_effect(&fai_types::def_effect(db, file, main.name))
        || scheduler_effect(&fai_types::def_effect(db, runtime_file, runtime.name));
    let requirements = fai_types::evidence_requirements(&main_scheme);
    if requirements.is_empty() {
        return Ok(Entry { entry: main, runtime, adapter: None, concurrent });
    }
    let rows = fai_types::evidence::row_instantiations(&main_scheme.ty, &instantiated);
    let mut args = Vec::with_capacity(requirements.len() + 1);
    for requirement in requirements {
        let row = rows
            .get(&requirement.row_var)
            .filter(|row| row.tail == RowEnd::Closed)
            .ok_or_else(|| {
                invalid(
                    "`main` requires offset evidence that the runtime type does not determine"
                        .into(),
                )
            })?;
        let offset = row
            .fields
            .iter()
            .filter(|(label, _)| label.as_str() < requirement.label.as_str())
            .count();
        args.push(CExpr::new(ExprKind::Lit(Lit::Int(offset as i64)), Ty::int()));
    }
    let argument = LocalId::from_index(0);
    let runtime_arg = CExpr::new(ExprKind::Local(argument), (**input).clone());
    // Row-polymorphic definitions use the uniform ABI. Match ordinary lowering:
    // first bind offset evidence, then apply the real source argument.
    let with_evidence = CExpr::new(
        ExprKind::App {
            func: Box::new(CExpr::new(ExprKind::Global(main), instantiated.clone())),
            args,
            reuse: Vec::new(),
            alloc: ClosureAlloc::Heap,
        },
        instantiated,
    );
    let body = CExpr::new(
        ExprKind::App {
            func: Box::new(with_evidence),
            args: vec![runtime_arg],
            reuse: Vec::new(),
            alloc: ClosureAlloc::Heap,
        },
        Ty::Unit,
    );
    let entry = DefId::new(main.file, Symbol::intern("entry#main"));
    let adapter = LoweredDef {
        def: entry,
        fns: vec![CoreFn { params: vec![argument], captures: Vec::new(), body }],
        entry_borrowed: Vec::new(),
        reuse_entry: None,
        entry_spread_params: Vec::new(),
        data_shapes: Vec::new(),
    };
    Ok(Entry { entry, runtime, adapter: Some(adapter), concurrent })
}
