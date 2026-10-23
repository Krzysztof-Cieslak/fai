//! Locality proofs at publication, representation and control-flow boundaries.

use super::*;
use fai_core::NicheKind;
use fai_core::ir::FnId;
use fai_resolve::DefId;
use fai_span::SourceId;
use fai_syntax::Symbol;
use fai_types::Ty;

fn expr(kind: K) -> CExpr {
    CExpr::new(kind, Ty::Error)
}
fn id(n: usize) -> LocalId {
    LocalId::from_index(n)
}
fn local(n: usize) -> CExpr {
    expr(K::Local(id(n)))
}
fn unit() -> CExpr {
    CExpr::new(K::Lit(Lit::Unit), Ty::Unit)
}
fn data(fields: Vec<CExpr>) -> CExpr {
    expr(K::MakeData { tag: 0, args: fields, reuse: None, scalars: 0, niche: None })
}
fn bind(n: usize, value: CExpr, body: CExpr) -> CExpr {
    expr(K::Let { local: id(n), value: Box::new(value), body: Box::new(body) })
}
fn dup(n: usize, body: CExpr) -> CExpr {
    crate::dup_(id(n), body)
}
fn publish(n: usize, body: CExpr) -> CExpr {
    bind(
        900,
        expr(K::Foreign {
            symbol: Symbol::intern("fai_send"),
            args: vec![local(n)],
            marshalled: false,
        }),
        crate::drop_(id(900), body),
    )
}
fn report(body: CExpr) -> FunctionLocality {
    analyze(&CoreFn { params: vec![id(100)], captures: Vec::new(), body }, Some(0))
}

fn definition(body: CExpr) -> LoweredDef {
    LoweredDef {
        def: DefId::new(SourceId::new(0), Symbol::intern("entry")),
        fns: vec![CoreFn { params: Vec::new(), captures: Vec::new(), body }],
        entry_borrowed: Vec::new(),
        reuse_entry: None,
        entry_spread_params: Vec::new(),
        data_shapes: Vec::new(),
    }
}
fn dup_reasons(body: CExpr) -> Vec<LocalityReason> {
    report(body)
        .sites
        .into_iter()
        .filter(|s| s.operation == LocalityOperation::Dup)
        .map(|s| s.reason)
        .collect()
}

#[test]
fn fresh_root_with_unknown_child_is_local() {
    assert_eq!(
        dup_reasons(bind(0, data(vec![local(100)]), dup(0, unit()))),
        vec![LocalityReason::ProvenRoot]
    );
}

#[test]
fn local_alias_preserves_identity() {
    assert_eq!(
        dup_reasons(bind(0, data(vec![unit()]), bind(1, local(0), dup(1, unit())))),
        vec![LocalityReason::ProvenRoot]
    );
}

#[test]
fn publication_through_alias_invalidates_original() {
    let body = bind(0, data(vec![unit()]), bind(1, local(0), publish(1, dup(0, unit()))));
    assert_eq!(dup_reasons(body), vec![LocalityReason::Published]);
}

#[test]
fn publication_of_parent_invalidates_child_transitively() {
    let body = bind(
        0,
        data(vec![unit()]),
        bind(1, data(vec![local(0)]), bind(2, data(vec![local(1)]), publish(2, dup(0, unit())))),
    );
    assert_eq!(dup_reasons(body), vec![LocalityReason::Published]);
}

#[test]
fn publication_of_child_does_not_publish_parent() {
    let body =
        bind(0, data(vec![unit()]), bind(1, data(vec![local(0)]), publish(0, dup(1, unit()))));
    assert_eq!(dup_reasons(body), vec![LocalityReason::ProvenRoot]);
}

#[test]
fn publishing_closure_invalidates_captures() {
    let closure =
        expr(K::MakeClosure { func: FnId(0), captures: vec![id(0)], alloc: ClosureAlloc::Heap });
    let body = bind(0, data(vec![unit()]), bind(1, closure, publish(1, dup(0, unit()))));
    assert_eq!(dup_reasons(body), vec![LocalityReason::Published]);
}

#[test]
fn indirect_call_reaches_callee_captures() {
    let closure =
        expr(K::MakeClosure { func: FnId(0), captures: vec![id(0)], alloc: ClosureAlloc::Heap });
    let call = expr(K::App {
        func: Box::new(local(1)),
        args: vec![unit()],
        reuse: Vec::new(),
        alloc: ClosureAlloc::Heap,
    });
    let body = bind(0, data(vec![unit()]), bind(1, closure, bind(2, call, dup(0, unit()))));
    assert_eq!(dup_reasons(body), vec![LocalityReason::Call]);
}

#[test]
fn unknown_call_invalidates_its_arguments() {
    let call = expr(K::App {
        func: Box::new(local(100)),
        args: vec![local(0)],
        reuse: Vec::new(),
        alloc: ClosureAlloc::Heap,
    });
    assert_eq!(
        dup_reasons(bind(0, data(vec![unit()]), bind(1, call, dup(0, unit())))),
        vec![LocalityReason::Call]
    );
}

#[test]
fn unknown_call_cannot_reach_unexposed_fresh_root() {
    let call = expr(K::App {
        func: Box::new(local(100)),
        args: vec![unit()],
        reuse: Vec::new(),
        alloc: ClosureAlloc::Heap,
    });
    assert_eq!(
        dup_reasons(bind(0, data(vec![unit()]), bind(1, call, dup(0, unit())))),
        vec![LocalityReason::ProvenRoot]
    );
}

#[test]
fn publishing_unknown_projection_invalidates_possible_referents() {
    let projection = expr(K::DataField {
        base: Box::new(local(1)),
        index: fai_core::ir::FieldIndex::Const(0),
        scalar: false,
        niche: None,
    });
    let body = bind(
        0,
        data(vec![unit()]),
        bind(1, data(vec![local(0)]), bind(2, projection, publish(2, dup(0, unit())))),
    );
    assert_eq!(dup_reasons(body), vec![LocalityReason::Published]);
}

#[test]
fn structural_reader_preserves_root_locality() {
    let hash = expr(K::Prim { op: Prim::Hash, args: vec![local(0)] });
    assert_eq!(
        dup_reasons(bind(0, data(vec![unit()]), bind(1, hash, dup(0, unit())))),
        vec![LocalityReason::ProvenRoot]
    );
}

#[test]
fn scalar_reader_results_do_not_retain_the_input_graph() {
    let hash = expr(K::Prim { op: Prim::Hash, args: vec![local(0)] });
    let body = bind(0, data(vec![unit()]), bind(1, hash, publish(1, dup(0, unit()))));
    assert_eq!(dup_reasons(body), vec![LocalityReason::ProvenRoot]);
}

#[test]
fn updating_primitive_is_a_barrier() {
    let update = expr(K::Prim { op: Prim::ArrayPush, args: vec![local(100), local(0)] });
    assert_eq!(
        dup_reasons(bind(0, data(vec![unit()]), bind(1, update, dup(0, unit())))),
        vec![LocalityReason::Primitive]
    );
}

#[test]
fn niche_wrapper_does_not_create_a_fresh_root() {
    let niche = expr(K::MakeData {
        tag: 1,
        args: vec![local(0)],
        reuse: None,
        scalars: 0,
        niche: Some(NicheKind::B),
    });
    let body = bind(0, data(vec![unit()]), bind(1, niche, dup(1, publish(1, dup(0, unit())))));
    assert_eq!(dup_reasons(body), vec![LocalityReason::Representation, LocalityReason::Published]);
}

#[test]
fn branch_publication_invalidates_following_use() {
    let branch = expr(K::If {
        cond: Box::new(local(100)),
        then: Box::new(publish(0, unit())),
        els: Box::new(unit()),
    });
    assert_eq!(
        dup_reasons(bind(0, data(vec![unit()]), bind(1, branch, dup(0, unit())))),
        vec![LocalityReason::Published]
    );
}

#[test]
fn branch_fresh_roots_can_join() {
    let branch = expr(K::If {
        cond: Box::new(local(100)),
        then: Box::new(data(vec![unit()])),
        els: Box::new(data(vec![unit(), unit()])),
    });
    assert_eq!(dup_reasons(bind(0, branch, dup(0, unit()))), vec![LocalityReason::ProvenRoot]);
}

#[test]
fn branch_unknown_root_keeps_conservative_join() {
    let branch = expr(K::If {
        cond: Box::new(local(100)),
        then: Box::new(data(vec![unit()])),
        els: Box::new(local(100)),
    });
    assert_eq!(dup_reasons(bind(0, branch, dup(0, unit()))), vec![LocalityReason::Merge]);
}

#[test]
fn loop_inputs_do_not_claim_the_first_iterations_freshness() {
    let loop_body = expr(K::Join { params: vec![id(0)], body: Box::new(dup(0, unit())) });
    assert_eq!(dup_reasons(bind(0, data(vec![unit()]), loop_body)), vec![LocalityReason::Loop]);
}

#[test]
fn each_loop_iteration_can_create_a_new_root() {
    let loop_body = expr(K::Join {
        params: vec![id(100)],
        body: Box::new(bind(
            0,
            data(vec![unit()]),
            dup(0, expr(K::Recur { args: vec![local(0)] })),
        )),
    });
    assert_eq!(dup_reasons(loop_body), vec![LocalityReason::ProvenRoot]);
}

#[test]
fn reconstruction_starts_a_new_logical_lifetime() {
    let rebuilt = expr(K::MakeData {
        tag: 0,
        args: vec![unit()],
        reuse: Some(id(1)),
        scalars: 0,
        niche: None,
    });
    let reset = expr(K::Reset {
        value: Box::new(local(0)),
        token: id(1),
        body: Box::new(bind(2, rebuilt, dup(2, unit()))),
    });
    assert_eq!(
        dup_reasons(bind(0, data(vec![unit()]), publish(0, reset))),
        vec![LocalityReason::ProvenRoot]
    );
}

#[test]
fn component_groups_preserve_root_facts() {
    let values = expr(K::Spread { components: vec![data(vec![unit()]), local(100)] });
    let body = expr(K::LetMany {
        locals: vec![id(0), id(1)],
        value: Box::new(values),
        body: Box::new(dup(0, dup(1, unit()))),
    });
    assert_eq!(dup_reasons(body), vec![LocalityReason::ProvenRoot, LocalityReason::Input]);
}

#[test]
fn depth_budget_retains_all_sites_conservatively() {
    let mut body = dup(0, unit());
    for n in 1..(DEPTH_BUDGET + 8) {
        body = bind(n, unit(), body);
    }
    let result = report(bind(0, data(vec![unit()]), body));
    assert!(result.limited);
    assert!(
        result
            .sites
            .iter()
            .any(|s| s.operation == LocalityOperation::Dup && s.reason == LocalityReason::Limit)
    );
}

#[test]
fn origin_budget_falls_back_without_dropping_sites() {
    let args = (0..ORIGIN_BUDGET + 1).map(|_| data(vec![unit()])).collect();
    let result = report(data(args));
    assert!(result.limited);
    assert_eq!(result.sites.len(), ORIGIN_BUDGET + 2);
}

#[test]
fn node_budget_handles_wide_constructions_conservatively() {
    let result = report(data(vec![unit(); NODE_BUDGET + 1]));
    assert!(result.limited);
    assert_eq!(result.sites.len(), 1);
    assert_eq!(result.sites[0].reason, LocalityReason::Limit);
}

#[test]
fn unknown_component_transport_does_not_duplicate_root_proofs() {
    let body = bind(
        0,
        data(vec![unit()]),
        expr(K::LetMany {
            locals: vec![id(1), id(2)],
            value: Box::new(local(0)),
            body: Box::new(dup(1, dup(2, unit()))),
        }),
    );
    assert_eq!(
        dup_reasons(body),
        vec![LocalityReason::Representation, LocalityReason::Representation]
    );
}

#[test]
fn partial_application_can_retain_an_argument() {
    let call = expr(K::App {
        func: Box::new(expr(K::Global(DefId::new(SourceId::new(0), Symbol::intern("partial"))))),
        args: vec![local(0)],
        reuse: Vec::new(),
        alloc: ClosureAlloc::Heap,
    });
    let body = bind(0, data(vec![unit()]), bind(1, call, dup(0, unit())));
    assert_eq!(dup_reasons(body), vec![LocalityReason::Call]);
}

#[test]
fn reports_do_not_modify_any_function_or_reuse_entry() {
    let body = bind(0, data(vec![unit()]), dup(0, crate::drop_(id(0), local(0))));
    let function = CoreFn { params: Vec::new(), captures: Vec::new(), body };
    let def = LoweredDef {
        def: DefId::new(SourceId::new(0), Symbol::intern("entry")),
        fns: vec![function.clone()],
        entry_borrowed: Vec::new(),
        reuse_entry: Some(function),
        entry_spread_params: Vec::new(),
        data_shapes: Vec::new(),
    };
    let before = def.clone();
    let result = analyze_locality(&def);
    assert_eq!(def, before);
    assert_eq!(result, analyze_locality(&def));
    assert_eq!(
        result.functions.iter().map(|f| f.function).collect::<Vec<_>>(),
        vec![Some(0), None]
    );
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn publication_matches_independent_graph_reachability(parents in prop::collection::vec(0usize..16, 1..32), published in 0usize..32, observed in 0usize..32) {
            let count = parents.len();
            let published = published % count;
            let observed = observed % count;
            let mut expected_published = BTreeSet::new();
            let mut cursor = published;
            loop {
                expected_published.insert(cursor);
                if cursor == 0 { break; }
                cursor = parents[cursor] % cursor;
            }
            let mut body = unit();
            for index in (0..count).rev() { body = crate::drop_(id(index), body); }
            body = dup(observed, crate::drop_(id(observed), body));
            body = dup(published, publish(published, body));
            for index in (0..count).rev() {
                let parent = if index == 0 { 0 } else { parents[index] % index };
                let field = if index == 0 { unit() } else { dup(parent, local(parent)) };
                body = bind(index, data(vec![field]), body);
            }
            prop_assert!(crate::check_rc(&definition(body.clone()), &|_, _| Vec::new()).is_ok());
            let expected = if expected_published.contains(&observed) { LocalityReason::Published } else { LocalityReason::ProvenRoot };
            prop_assert_eq!(dup_reasons(body).last().copied(), Some(expected));
        }
    }
}
