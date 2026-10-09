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
