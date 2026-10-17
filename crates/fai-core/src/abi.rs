//! The native calling-convention shape ([`FnAbi`]) of a definition.
//!
//! [`abi`] derives a definition's [`FnAbi`] from its **declared-or-inferred type
//! signature** and its syntactic source-parameter count — both body-edit-stable,
//! so a caller's compiled object (and the SROA rewrite that marshals a call)
//! depends on a callee's *signature*, never its body (the codegen firewall). It is
//! the single source of truth shared by the SROA pass (`fai-rc`), the reference
//! counter, and code generation (the driver threads it in as `signature_of`).

use std::sync::Arc;

use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, module_defs};
use fai_syntax::Symbol;
use fai_syntax::ast::ItemKind;

use crate::ir::{FnAbi, Repr};
use crate::niche::niche_scheme;

/// `name`'s native calling-convention shape (see [`FnAbi`]). Tracked so its
/// memoization boundary keeps a dependent's recompute independent of unrelated
/// edits (a callee body edit that does not change its signature does not ripple).
/// A synthesized function gets its explicit ABI from its owning transformation,
/// so ownership lowering and native emission agree on spread arguments/results.
#[salsa::tracked]
pub fn abi(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<FnAbi> {
    if let Some((owner, _)) =
        name.as_str().strip_prefix("fuse#").and_then(|rest| rest.rsplit_once('#'))
    {
        return crate::fuse_def(db, file, Symbol::intern(owner))
            .loops
            .iter()
            .find(|function| function.lowered.def.name == name)
            .map_or_else(|| Arc::new(FnAbi::default()), |function| Arc::new(function.abi.clone()));
    }
    let def = DefId::new(file.source(db), name);
    let Some(scheme) = crate::representation::definition_scheme(db, def) else {
        return Arc::new(FnAbi::default());
    };
    // A niche `Option` (and a spread float aggregate) parameter/result is carried
    // wrapper-free / exploded across a direct call.
    let niche = |ty: &fai_types::Ty| niche_scheme(db, ty);
    let arity = source_param_count(db, file, name);
    let mut abi = FnAbi::from_scheme(&scheme, arity, &niche);
    let parsed = fai_syntax::parse(db, file);
    let foreign = module_defs(db, file).get(name).is_some_and(|definition| {
        matches!(parsed.module.items[definition.binding.index()].kind, ItemKind::Foreign { .. })
    });
    let mut result = &scheme.ty;
    for _ in 0..arity {
        if let fai_types::Ty::Arrow(_, next, _) = result {
            result = next;
        }
    }
    if abi.register_abi
        && !foreign
        && scheme.vars.is_empty()
        && scheme.row_vars.is_empty()
        && let Some(widths) = tagged_result_layout(db, result)
    {
        abi.ret = Repr::TaggedResult(widths);
    }
    Arc::new(controlled(db, abi))
}

pub(crate) fn controlled(db: &dyn Db, mut abi: FnAbi) -> FnAbi {
    if fai_db::allocation_control(db) == fai_db::AllocationControl::NoScalarReplacement {
        for repr in abi.params.iter_mut().chain(std::iter::once(&mut abi.ret)) {
            if matches!(repr, Repr::Spread(_) | Repr::TaggedResult(_)) {
                *repr = crate::ir::Repr::Uniform;
            }
        }
    }
    abi
}

/// The two payload types of Prelude's Result, recognized by declaration identity.
#[must_use]
pub fn result_payloads(db: &dyn Db, ty: &fai_types::Ty) -> Option<[fai_types::Ty; 2]> {
    use fai_types::Ty;
    let Ty::App(head, error) = ty else { return None };
    let Ty::App(head, value) = head.as_ref() else { return None };
    let Ty::Adt(adt) = head.as_ref() else { return None };
    let prelude = fai_resolve::prelude_source(db)?;
    (adt.file == prelude && adt.name == Symbol::intern("Result"))
        .then(|| [(**value).clone(), (**error).clone()])
}

/// Small tuples with no resource-finalization order to preserve when split.
#[must_use]
pub fn result_tuple(ty: &fai_types::Ty) -> bool {
    use fai_types::{Con, Ty};
    matches!(ty, Ty::Tuple(fields) if (2..=7).contains(&fields.len())
        && fields.iter().all(|t| *t != Ty::Con(Con::Float) && plain_result_value(t)))
}

// Separating a tag from its payload can release the payload before a later
// tag-only observation. Admit only values with no observable finalization.
fn plain_result_value(ty: &fai_types::Ty) -> bool {
    use fai_types::{Con, RowEnd, Ty};
    fn visit(ty: &Ty, remaining: &mut usize) -> bool {
        if *remaining == 0 {
            return false;
        }
        *remaining -= 1;
        match ty {
            Ty::Unit
            | Ty::Con(Con::Int | Con::Float | Con::Bool | Con::Char | Con::String | Con::Bytes) => {
                true
            }
            Ty::App(head, value) if matches!(head.as_ref(), Ty::Con(Con::List | Con::Array)) => {
                visit(value, remaining)
            }
            Ty::Tuple(fields) => fields.iter().all(|field| visit(field, remaining)),
            Ty::Record(row) if row.tail == RowEnd::Closed => {
                row.fields.iter().all(|(_, field)| visit(field, remaining))
            }
            _ => false,
        }
    }
    visit(ty, &mut 256)
}

/// A bounded Result return layout. Float payloads retain their scalar-slot box.
#[must_use]
pub fn tagged_result_layout(db: &dyn Db, ty: &fai_types::Ty) -> Option<[u8; 2]> {
    use fai_types::{Con, Ty};
    if !cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
        return None;
    }
    let payloads = result_payloads(db, ty)?;
    if !payloads.iter().all(plain_result_value) {
        return None;
    }
    if payloads.iter().any(|t| matches!(t, Ty::Con(Con::Float) | Ty::Var(_) | Ty::Error)) {
        return None;
    }
    Some(payloads.map(|ty| {
        if result_tuple(&ty)
            && let Ty::Tuple(fields) = ty
        {
            fields.len() as u8
        } else {
            0
        }
    }))
}

/// `name`'s ABI located by [`DefId`], or the default (uniform) ABI for a
/// definition with no source file (a synthetic combined loop).
#[must_use]
pub fn abi_of(db: &dyn Db, def: DefId) -> Arc<FnAbi> {
    db.source_file(def.file).map_or_else(|| Arc::new(FnAbi::default()), |f| abi(db, f, def.name))
}

/// The source arity of a binding, or the expanded declared arity of a foreign
/// function. Shared with the driver so aliases cannot give conflicting ABIs.
#[must_use]
pub fn source_param_count(db: &dyn Db, file: SourceFile, name: Symbol) -> usize {
    let parsed = fai_syntax::parse(db, file);
    module_defs(db, file)
        .get(name)
        .and_then(|d| match &parsed.module.items[d.binding.index()].kind {
            ItemKind::Binding { params, .. } => Some(params.len()),
            // A `foreign` decl has no parameter patterns; its parameter count is
            // the arrow arity of its declared type.
            ItemKind::Foreign { .. } => Some(foreign_signature(db, file, name).0.len()),
            _ => None,
        })
        .unwrap_or(0)
}

/// Argument/result types of a foreign's fully expanded declared signature.
pub(crate) fn foreign_signature(
    db: &dyn Db,
    file: SourceFile,
    name: Symbol,
) -> (Vec<fai_types::Ty>, fai_types::Ty) {
    let Some(scheme) =
        crate::representation::definition_scheme(db, DefId::new(file.source(db), name))
    else {
        return (Vec::new(), fai_types::Ty::Error);
    };
    let mut parameters = Vec::new();
    let mut result = &scheme.ty;
    while let fai_types::Ty::Arrow(from, to, _) = result {
        parameters.push((**from).clone());
        result = to;
    }
    (parameters, result.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Repr;
    use fai_types::{Scheme, Ty, TyVarId};

    fn source_result_shape(source: &str) -> Option<[u8; 2]> {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        abi(&db, db.source_file(id).unwrap(), Symbol::intern("make")).tagged_result()
    }

    #[test]
    fn large_resource_free_types_have_bounded_eligibility_work() {
        assert!(!plain_result_value(&Ty::Tuple(vec![Ty::int(); 256])));
    }

    #[test]
    fn resource_bearing_results_stay_boxed() {
        assert_eq!(
            source_result_shape(
                "module M\npublic make : Channel Int -> Result (Channel Int * String) String\nlet make channel = Ok (channel, \"label\")\n"
            ),
            None
        );
    }

    #[test]
    fn resources_nested_in_collections_keep_the_result_boxed() {
        assert_eq!(
            source_result_shape(
                "module M\npublic make : List Reader -> Result (List Reader) String\nlet make readers = Ok readers\n"
            ),
            None
        );
    }

    #[test]
    fn functions_may_capture_resources_so_the_result_stays_boxed() {
        assert_eq!(
            source_result_shape(
                "module M\npublic make : (Int -> Int) -> Result (Int -> Int) String\nlet make f = Ok f\n"
            ),
            None
        );
    }

    #[test]
    fn generic_result_signatures_keep_the_uniform_result() {
        assert_eq!(
            source_result_shape(
                "module M\npublic make : 'a -> Result 'a String\nlet make value = Ok value\n"
            ),
            None
        );
    }

    #[test]
    fn direct_float_result_payloads_keep_the_scalar_slot_representation() {
        assert_eq!(
            source_result_shape(
                "module M\npublic make : Float -> Result Float String\nlet make value = Ok value\n"
            ),
            None
        );
    }

    #[test]
    fn tuples_beyond_the_return_budget_stay_intact() {
        assert_eq!(
            source_result_shape(
                "module M\npublic make : Int -> Result (Int * Int * Int * Int * Int * Int * Int * Int) String\nlet make n = Ok (n, n, n, n, n, n, n, n)\n"
            ),
            Some([0, 0])
        );
    }

    #[test]
    fn result_layout_uses_the_prelude_declaration_identity() {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source(
            "M.fai".into(),
            "module M\ntype Result 'a 'b = | Left 'a | Right 'b\nlet value n = Left n\n".into(),
        );
        let local = Ty::App(
            std::sync::Arc::new(Ty::App(
                std::sync::Arc::new(Ty::Adt(fai_resolve::AdtRef {
                    file: id,
                    name: Symbol::intern("Result"),
                })),
                std::sync::Arc::new(Ty::int()),
            )),
            std::sync::Arc::new(Ty::Con(fai_types::Con::String)),
        );
        assert_eq!(tagged_result_layout(&db, &local), None);
    }

    #[test]
    fn integer_and_array_results_use_only_supported_return_registers() {
        let result = Ty::Tuple(vec![Ty::int(), Ty::array(Ty::int())]);
        let abi = FnAbi::from_scheme(&Scheme::mono(Ty::arrow(Ty::int(), result)), 1, &|_| None);
        let supported = cfg!(any(
            target_arch = "aarch64",
            all(target_arch = "x86_64", not(target_os = "windows"))
        ));
        assert_eq!(
            abi.ret,
            if supported {
                Repr::Spread(vec![Repr::ScalarInt, Repr::Uniform])
            } else {
                Repr::Uniform
            }
        );
        assert_eq!(abi.params, [Repr::ScalarInt]);
    }

    #[test]
    fn unknown_state_representation_keeps_the_tuple_boxed() {
        let result = Ty::Tuple(vec![Ty::int(), Ty::Var(TyVarId(0))]);
        let abi = FnAbi::from_scheme(&Scheme::mono(Ty::arrow(Ty::int(), result)), 1, &|_| None);
        assert_eq!(abi.ret, Repr::Uniform);
    }

    #[test]
    fn a_float_state_keeps_the_mixed_tuple_boxed() {
        let result = Ty::Tuple(vec![Ty::int(), Ty::Con(fai_types::Con::Float)]);
        let abi = FnAbi::from_scheme(&Scheme::mono(Ty::arrow(Ty::int(), result)), 1, &|_| None);
        assert_eq!(abi.ret, Repr::Uniform);
    }
}
