//! Call-result identities and publication obligations at diagnostic boundaries.

use super::*;
use fai_core::ir::FnId;
use fai_span::SourceId;
use fai_syntax::Symbol;

fn id(n: usize) -> LocalId {
    LocalId::from_index(n)
}
fn ty() -> Ty {
    Ty::Tuple(vec![Ty::Unit, Ty::Unit])
}
fn e(kind: K, ty: Ty) -> CExpr {
    CExpr::new(kind, ty)
}
fn unit() -> CExpr {
    e(K::Lit(Lit::Unit), Ty::Unit)
}
fn local(n: usize) -> CExpr {
    e(K::Local(id(n)), ty())
}
fn data(a: CExpr) -> CExpr {
    let ty = Ty::Tuple(vec![a.ty.clone(), Ty::Unit]);
    e(K::MakeData { tag: 0, args: vec![a, unit()], reuse: None, scalars: 0, niche: None }, ty)
}
fn fresh() -> CExpr {
    data(unit())
}
fn bind(n: usize, value: CExpr, body: CExpr) -> CExpr {
    let ty = body.ty.clone();
    e(K::Let { local: id(n), value: Box::new(value), body: Box::new(body) }, ty)
}
fn dup(n: usize) -> CExpr {
    crate::dup_(id(n), unit())
}
fn name(n: &str) -> DefId {
    DefId::new(SourceId::new(0), Symbol::intern(n))
}
fn call(n: &str, args: Vec<CExpr>) -> CExpr {
    e(
        K::App {
            func: Box::new(e(K::Global(name(n)), Ty::arrow(ty(), ty()))),
            args,
            reuse: Vec::new(),
            alloc: ClosureAlloc::Heap,
        },
        ty(),
    )
}
fn publish(value: CExpr, body: CExpr) -> CExpr {
    bind(
        999,
        e(
            K::Foreign { symbol: Symbol::intern("fai_send"), args: vec![value], marshalled: false },
            Ty::Unit,
        ),
        crate::drop_(id(999), body),
    )
}
fn field(base: CExpr, index: u32) -> CExpr {
    e(
        K::DataField {
            base: Box::new(base),
            index: FieldIndex::Const(index),
            scalar: false,
            niche: None,
        },
        ty(),
    )
}
fn def(n: &str, params: Vec<LocalId>, body: CExpr) -> LoweredDef {
    LoweredDef {
        def: name(n),
        fns: vec![CoreFn { params, captures: Vec::new(), body }],
        entry_borrowed: Vec::new(),
        reuse_entry: None,
        entry_spread_params: Vec::new(),
        data_shapes: Vec::new(),
    }
}
fn reasons(defs: &[LoweredDef]) -> Vec<Why> {
    analyze_program_locality(defs).definitions[0].functions[0]
        .sites
        .iter()
        .filter(|s| s.operation == Op::Dup)
        .map(|s| s.reason)
        .collect()
}

#[test]
fn helper_returning_a_fresh_root_establishes_locality() {
    let defs = [
        def("caller", vec![], bind(0, call("make", vec![unit()]), dup(0))),
        def("make", vec![id(0)], fresh()),
    ];
    assert_eq!(reasons(&defs), vec![Why::ProvenRoot]);
    assert_eq!(analyze_program_locality(&defs).summaries[1].result, ReturnProvenance::Fresh);
}

#[test]
fn identity_result_keeps_the_callers_origin() {
    let defs = [
        def(
            "caller",
            vec![],
            bind(0, fresh(), bind(1, call("identity", vec![local(0)]), publish(local(1), dup(0)))),
        ),
        def("identity", vec![id(0)], local(0)),
    ];
    assert_eq!(reasons(&defs), vec![Why::Published]);
    assert_eq!(analyze_program_locality(&defs).summaries[1].result, ReturnProvenance::Input(0));
}

#[test]
fn a_pure_reader_helper_does_not_publish_its_argument() {
    let read = e(K::DataTag { base: Box::new(local(0)), niche: None }, Ty::int());
    let defs = [
        def("caller", vec![], bind(0, fresh(), bind(1, call("read", vec![local(0)]), dup(0)))),
        def("read", vec![id(0)], read),
    ];
    assert_eq!(reasons(&defs), vec![Why::ProvenRoot]);
    assert_eq!(analyze_program_locality(&defs).summaries[1].may_publish, vec![false]);
}

#[test]
fn a_publishing_helper_invalidates_the_callers_origin() {
    let defs = [
        def("caller", vec![], bind(0, fresh(), bind(1, call("send", vec![local(0)]), dup(0)))),
        def("send", vec![id(0)], publish(local(0), unit())),
    ];
    assert_eq!(reasons(&defs), vec![Why::Call]);
}

#[test]
fn a_published_fresh_return_is_not_local() {
    let defs = [
        def("caller", vec![], bind(0, call("send", vec![unit()]), dup(0))),
        def("send", vec![id(0)], bind(1, fresh(), publish(local(1), local(1)))),
    ];
    assert_eq!(reasons(&defs), vec![Why::Published]);
}

#[test]
fn returned_container_retains_its_input_reachability() {
    let defs = [
        def(
            "caller",
            vec![],
            bind(0, fresh(), bind(1, call("wrap", vec![local(0)]), publish(local(1), dup(0)))),
        ),
        def("wrap", vec![id(0)], data(local(0))),
    ];
    assert_eq!(reasons(&defs), vec![Why::Published]);
}

#[test]
fn fresh_call_instances_do_not_alias_one_another() {
    let defs = [
        def(
            "caller",
            vec![],
            bind(
                0,
                call("make", vec![unit()]),
                bind(1, call("make", vec![unit()]), publish(local(0), dup(1))),
            ),
        ),
        def("make", vec![id(0)], fresh()),
    ];
    assert_eq!(reasons(&defs), vec![Why::ProvenRoot]);
}

#[test]
fn helper_field_return_reuses_the_exact_child_origin() {
    let defs = [
        def(
            "caller",
            vec![],
            bind(0, fresh(), bind(1, data(local(0)), bind(2, call("get", vec![local(1)]), dup(2)))),
        ),
        def("get", vec![id(0)], field(local(0), 0)),
    ];
    assert_eq!(reasons(&defs), vec![Why::ProvenRoot]);
    assert_eq!(
        analyze_program_locality(&defs).summaries[1].result,
        ReturnProvenance::Field { input: 0, path: vec![0] }
    );
}

#[test]
fn publishing_one_field_does_not_publish_its_sibling_or_parent() {
    let parent = e(
        K::MakeData {
            tag: 0,
            args: vec![local(0), local(1)],
            reuse: None,
            scalars: 0,
            niche: None,
        },
        Ty::Tuple(vec![ty(), ty()]),
    );
    let defs = [def(
        "caller",
        vec![],
        bind(
            0,
            fresh(),
            bind(
                1,
                fresh(),
                bind(
                    2,
                    parent,
                    bind(3, field(local(2), 0), publish(local(3), crate::dup_(id(1), dup(2)))),
                ),
            ),
        ),
    )];
    assert_eq!(reasons(&defs), vec![Why::ProvenRoot, Why::ProvenRoot]);
}

#[test]
fn incoming_roots_remain_unknown_even_with_local_callers() {
    let defs = [
        def("caller", vec![], bind(0, fresh(), call("inspect", vec![local(0)]))),
        def("inspect", vec![id(0)], crate::dup_(id(0), local(0))),
    ];
    let report = analyze_program_locality(&defs);
    assert_eq!(report.definitions[1].functions[0].sites[0].reason, Why::Input);
}

#[test]
fn unknown_callbacks_invalidate_their_arguments() {
    let call = e(
        K::App {
            func: Box::new(local(100)),
            args: vec![local(0)],
            reuse: Vec::new(),
            alloc: ClosureAlloc::Heap,
        },
        ty(),
    );
    let defs = [def("caller", vec![id(100)], bind(0, fresh(), bind(1, call, dup(0))))];
    assert_eq!(reasons(&defs), vec![Why::Call]);
}

#[test]
fn captured_values_cross_known_closure_call_boundaries() {
    let closure = e(
        K::MakeClosure { func: FnId(1), captures: vec![id(0)], alloc: ClosureAlloc::Heap },
        Ty::arrow(Ty::Unit, ty()),
    );
    let invoke = e(
        K::App {
            func: Box::new(e(K::Local(id(1)), closure.ty.clone())),
            args: vec![unit()],
            reuse: Vec::new(),
            alloc: ClosureAlloc::Heap,
        },
        ty(),
    );
    let mut caller =
        def("caller", vec![], bind(0, fresh(), bind(1, closure, bind(2, invoke, dup(2)))));
    caller.fns.push(CoreFn { captures: vec![id(100)], params: vec![id(101)], body: local(100) });
    assert_eq!(reasons(&[caller]), vec![Why::ProvenRoot]);
}

#[test]
fn unsupported_scalar_projection_does_not_claim_a_child_root() {
    let projection = e(
        K::DataField {
            base: Box::new(local(1)),
            index: FieldIndex::Const(0),
            scalar: true,
            niche: None,
        },
        Ty::Con(Con::Float),
    );
    let defs = [def(
        "caller",
        vec![],
        bind(0, fresh(), bind(1, data(local(0)), bind(2, projection, dup(2)))),
    )];
    assert_eq!(reasons(&defs), vec![Why::Representation]);
}

#[test]
fn recursive_input_publication_is_conservative() {
    let defs = [
        def("caller", vec![], bind(0, fresh(), bind(1, call("loop", vec![local(0)]), dup(0)))),
        def("loop", vec![id(0)], call("loop", vec![local(0)])),
    ];
    assert_eq!(reasons(&defs), vec![Why::Call]);
}

#[test]
fn nonreturning_branches_still_contribute_publication_effects() {
    let body = e(
        K::If {
            cond: Box::new(e(K::Lit(Lit::Bool(true)), Ty::bool())),
            then: Box::new(publish(local(0), e(K::Error, Ty::Error))),
            els: Box::new(unit()),
        },
        Ty::Unit,
    );
    let defs = [
        def("caller", vec![], bind(0, fresh(), bind(1, call("action", vec![local(0)]), dup(0)))),
        def("action", vec![id(0)], body),
    ];
    assert_eq!(reasons(&defs), vec![Why::Call]);
}

#[test]
fn analysis_preserves_every_input_definition() {
    let defs = [
        def("caller", vec![], bind(0, call("make", vec![unit()]), dup(0))),
        def("make", vec![id(0)], fresh()),
    ];
    let before = defs.clone();
    let first = analyze_program_locality(&defs);
    assert_eq!(defs, before);
    assert_eq!(first, analyze_program_locality(&defs));
}

#[test]
fn publication_through_one_aliased_argument_reaches_the_other() {
    let defs = [
        def(
            "caller",
            vec![],
            bind(0, fresh(), bind(1, call("send", vec![local(0), local(0)]), dup(0))),
        ),
        def("send", vec![id(0), id(1)], publish(local(1), local(0))),
    ];
    assert_eq!(reasons(&defs), vec![Why::Call]);
}

#[test]
fn unknown_calls_prevent_reusing_old_field_shapes() {
    let defs = [def(
        "caller",
        vec![],
        bind(
            0,
            fresh(),
            bind(
                1,
                data(local(0)),
                bind(2, call("missing", vec![local(1)]), bind(3, field(local(1), 0), dup(3))),
            ),
        ),
    )];
    assert_eq!(reasons(&defs), vec![Why::Projection]);
}

#[test]
fn unknown_partial_application_is_a_publication_barrier() {
    let defs = [
        def("caller", vec![], bind(0, fresh(), bind(1, call("two", vec![local(0)]), dup(0)))),
        def("two", vec![id(0), id(1)], local(0)),
    ];
    assert_eq!(reasons(&defs), vec![Why::Call]);
}

#[test]
fn nullary_initializers_use_their_result_summary() {
    let value = e(K::Global(name("value")), ty());
    let defs = [def("caller", vec![], bind(0, value, dup(0))), def("value", vec![], fresh())];
    assert_eq!(reasons(&defs), vec![Why::ProvenRoot]);
}

#[test]
fn repeated_loop_inputs_do_not_retain_fresh_first_iteration_proofs() {
    let body = e(K::Join { params: vec![id(0)], body: Box::new(dup(0)) }, Ty::Unit);
    let defs = [def("caller", vec![], bind(0, fresh(), body))];
    assert_eq!(reasons(&defs), vec![Why::Loop]);
}

#[test]
fn reset_ends_only_the_parent_identity() {
    let reset = e(
        K::Reset {
            value: Box::new(local(1)),
            token: id(2),
            body: Box::new(e(K::FreeReuse { token: id(2), body: Box::new(dup(0)) }, Ty::Unit)),
        },
        Ty::Unit,
    );
    let defs = [def("caller", vec![], bind(0, fresh(), bind(1, data(local(0)), reset)))];
    assert_eq!(reasons(&defs), vec![Why::ProvenRoot]);
}

#[test]
fn wide_result_graphs_fall_back_conservatively() {
    let args = (0..ROOTS + 1).map(|_| fresh()).collect::<Vec<_>>();
    let large = e(K::MakeData { tag: 0, args, reuse: None, scalars: 0, niche: None }, ty());
    let defs = [def("caller", vec![], bind(0, large, dup(0)))];
    let report = analyze_program_locality(&defs);
    assert!(report.summaries[0].limited);
    assert_eq!(reasons(&defs), vec![Why::Limit]);
}

#[test]
fn missing_callee_cannot_establish_result_freshness() {
    let defs = [def("caller", vec![], bind(0, call("missing", vec![unit()]), dup(0)))];
    assert_eq!(reasons(&defs), vec![Why::Call]);
}

#[test]
fn recursive_result_shapes_stabilize_without_exhausting_the_work_bound() {
    let body = e(
        K::If {
            cond: Box::new(e(K::Lit(Lit::Bool(true)), Ty::bool())),
            then: Box::new(data(local(0))),
            els: Box::new(data(call("tree", vec![local(0)]))),
        },
        ty(),
    );
    let defs = [def("tree", vec![id(0)], body)];
    let report = analyze_program_locality(&defs);
    assert!(report.converged, "{report:?}");
    assert!(!report.summaries[0].limited);
}

#[test]
fn abstracted_deep_fields_keep_aliases_to_exposed_siblings() {
    let nested = data(data(data(data(local(0)))));
    let outer = e(
        K::MakeData { tag: 0, args: vec![nested, local(0)], reuse: None, scalars: 0, niche: None },
        ty(),
    );
    let defs = [
        def(
            "caller",
            vec![],
            bind(
                0,
                call("make", vec![unit()]),
                bind(1, field(local(0), 0), bind(2, field(local(0), 1), publish(local(1), dup(2)))),
            ),
        ),
        def("make", vec![id(100)], bind(0, fresh(), outer)),
    ];
    assert_eq!(reasons(&defs), vec![Why::Published]);
}

#[test]
fn forwarding_a_known_closure_preserves_its_entry_and_capture() {
    let function_ty = Ty::arrow(Ty::Unit, ty());
    let closure = e(
        K::MakeClosure { func: FnId(1), captures: vec![id(0)], alloc: ClosureAlloc::Heap },
        function_ty.clone(),
    );
    let forwarded = e(
        K::App {
            func: Box::new(e(
                K::Global(name("identity")),
                Ty::arrow(function_ty.clone(), function_ty.clone()),
            )),
            args: vec![e(K::Local(id(1)), function_ty.clone())],
            reuse: Vec::new(),
            alloc: ClosureAlloc::Heap,
        },
        function_ty.clone(),
    );
    let invoke = e(
        K::App {
            func: Box::new(e(K::Local(id(2)), function_ty)),
            args: vec![unit()],
            reuse: Vec::new(),
            alloc: ClosureAlloc::Heap,
        },
        ty(),
    );
    let mut caller = def(
        "caller",
        vec![],
        bind(0, fresh(), bind(1, closure, bind(2, forwarded, bind(3, invoke, dup(3))))),
    );
    caller.fns.push(CoreFn { captures: vec![id(100)], params: vec![id(101)], body: local(100) });
    let defs = [caller, def("identity", vec![id(0)], local(0))];
    assert_eq!(reasons(&defs), vec![Why::ProvenRoot]);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn transitive_helper_summaries_preserve_publication(depth in 1usize..8, published in any::<bool>()) {
            let mut defs=vec![def("caller",vec![],bind(0,fresh(),bind(1,call("helper0",vec![local(0)]),dup(0))))];
            for index in 0..depth {
                let body=if index+1==depth { if published {publish(local(0),unit())} else {unit()} } else {call(&format!("helper{}",index+1),vec![local(0)])};
                defs.push(def(&format!("helper{index}"),vec![id(0)],body));
            }
            prop_assert_eq!(reasons(&defs),vec![if published {Why::Call} else {Why::ProvenRoot}]);
        }
    }
}
