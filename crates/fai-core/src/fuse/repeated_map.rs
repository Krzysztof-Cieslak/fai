//! Interchanges counted, independent scalar maps with per-element iteration.
//!
//! The unit-step counter cannot wrap before its invariant bound. A capture-free,
//! total numeric callback cannot observe reordering between elements, and its
//! resource-free state has no observable destruction. Float states up to four
//! components run in pairs to retain independent instruction streams; an odd
//! final element uses the single-state worker. Ordinary take/put ownership and
//! scalar replacement implement both paths.

use super::*;
use crate::ir::{FieldIndex, ffa_arity};
use fai_types::Scheme;

fn state_type(ty: &Ty) -> bool {
    matches!(ty, Ty::Con(Con::Int | Con::Float)) || ffa_arity(ty).is_some()
}

fn total_scalar(e: &CExpr) -> bool {
    match &e.kind {
        K::Lit(Lit::Int(_) | Lit::Float(_) | Lit::Bool(_) | Lit::Unit) => true,
        K::Local(_) => state_type(&e.ty) || matches!(e.ty, Ty::Con(Con::Bool) | Ty::Unit),
        K::Let { value, body, .. } => total_scalar(value) && total_scalar(body),
        K::If { cond, then, els } => total_scalar(cond) && total_scalar(then) && total_scalar(els),
        K::Prim {
            op:
                Prim::IntAdd
                | Prim::IntSub
                | Prim::IntMul
                | Prim::IntAnd
                | Prim::IntOr
                | Prim::IntXor
                | Prim::IntComplement
                | Prim::IntShl
                | Prim::IntShr
                | Prim::IntShrLogical
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
                | Prim::IntToFloat
                | Prim::FloatFromBits
                | Prim::FloatToBits
                | Prim::Sqrt
                | Prim::Eq
                | Prim::Compare
                | Prim::Not,
            args,
        } => args.iter().all(total_scalar),
        K::MakeData { args, niche: None, .. } => {
            ffa_arity(&e.ty).is_some() && args.iter().all(total_scalar)
        }
        K::DataField { base, index: FieldIndex::Const(index), scalar: true, niche: None } => {
            ffa_arity(&base.ty).is_some_and(|count| (*index as usize) < count) && total_scalar(base)
        }
        _ => false,
    }
}

fn resolve_local(e: &CExpr, aliases: &FxHashMap<LocalId, LocalId>) -> Option<LocalId> {
    let K::Local(mut local) = e.kind else { return None };
    for _ in 0..16 {
        match aliases.get(&local) {
            Some(next) => local = *next,
            None => return Some(local),
        }
    }
    None
}

impl Fuser<'_> {
    /// Transposes only total scalar state transitions, retaining the original
    /// empty-iteration branch and one ownership-aware traversal of the array.
    pub(super) fn repeated_map(&mut self, base: &LoweredDef) -> Option<CExpr> {
        let entry = base.entry();
        if base.fns.len() != 1 || entry.params.len() != 3 || !entry.captures.is_empty() {
            return None;
        }
        let K::If { cond, then, els } = &entry.body.kind else { return None };
        let K::Prim { op: Prim::IntGe, args } = &cond.kind else { return None };
        let [counter, limit] = args.as_slice() else { return None };
        let K::Local(counter) = counter.kind else { return None };
        let K::Local(limit) = limit.kind else { return None };
        let K::Local(array) = then.kind else { return None };
        if counter == limit || counter == array || limit == array || !is_seq(&then.ty, Con::Array) {
            return None;
        }
        let counter_pos = entry.params.iter().position(|param| *param == counter)?;
        let limit_pos = entry.params.iter().position(|param| *param == limit)?;
        let array_pos = entry.params.iter().position(|param| *param == array)?;
        let (target, args) = call_target(els)?;
        if target != base.def
            || args.len() != 3
            || !matches!(args[limit_pos].kind, K::Local(local) if local == limit)
        {
            return None;
        }
        let K::Prim { op: Prim::IntAdd, args: increment } = &args[counter_pos].kind else {
            return None;
        };
        if !matches!(increment.as_slice(), [CExpr { kind: K::Local(local), ty }, CExpr { kind: K::Lit(Lit::Int(1)), .. }] if *local == counter && *ty == Ty::int())
        {
            return None;
        }
        let element = seq_elem(&then.ty)?;
        if !state_type(&element) {
            return None;
        }
        let mut mapped = &args[array_pos];
        let mut aliases = FxHashMap::default();
        while let K::Let { local, value, body } = &mapped.kind {
            if aliases.len() >= 16 {
                return None;
            }
            aliases.insert(*local, resolve_local(value, &aliases)?);
            mapped = body;
        }
        let (map, map_args) = call_target(mapped)?;
        if !matches!(self.defs.lookup(map), Some((SeqKind::Array, Comb::Map)))
            || map_args.len() != 2
            || resolve_local(&map_args[1], &aliases) != Some(array)
        {
            return None;
        }
        let K::Global(callback) = map_args[0].kind else { return None };
        if callback.file != self.source {
            return None;
        }
        let file = self.db.source_file(self.source)?;
        let callback = helper_inlined(self.db, file, callback.name);
        if callback.fns.len() != 1
            || callback.entry().params.len() != 1
            || !callback.entry().captures.is_empty()
            || callback.entry().body.ty != element
            || crate::helper_inline::node_count(&callback.entry().body) > 64
            || !total_scalar(&callback.entry().body)
        {
            return None;
        }

        let worker = DefId::new(
            self.source,
            Symbol::intern(&format!(
                "fuse#{}#iterate{}",
                self.consuming.as_str(),
                self.chain_index
            )),
        );
        self.chain_index += 1;
        let i = LocalId::from_index(0);
        let n = LocalId::from_index(1);
        let value = LocalId::from_index(2);
        let mut next = 3;
        let changed = crate::helper_inline::build_inline(
            callback.entry(),
            vec![local(value, element.clone())],
            1,
            element.clone(),
            &mut next,
        );
        let again = CExpr::new(
            K::App {
                func: Box::new(global(worker)),
                args: vec![
                    prim(Prim::IntAdd, vec![local(i, Ty::int()), int_lit(1)]),
                    local(n, Ty::int()),
                    changed,
                ],
                reuse: Vec::new(),
                alloc: ClosureAlloc::Heap,
            },
            element.clone(),
        );
        let body = if_(
            prim(Prim::IntGe, vec![local(i, Ty::int()), local(n, Ty::int())]),
            local(value, element.clone()),
            again,
            element.clone(),
        );
        let scheme =
            Scheme::mono(Ty::arrows([Ty::int(), Ty::int(), element.clone()], element.clone()));
        let abi = FnAbi::from_scheme(&scheme, 3, &|_| None);
        let lowered = LoweredDef {
            def: worker,
            fns: vec![CoreFn { params: vec![i, n, value], captures: Vec::new(), body }],
            entry_borrowed: Vec::new(),
            reuse_entry: None,
            entry_spread_params: Vec::new(),
            data_shapes: Vec::new(),
        };
        self.loops.push(FusedLoop { lowered, abi, arity: 3, result: crate::ResultSig::default() });
        if let Some(width) = match element {
            Ty::Con(Con::Float) => Some(1),
            _ => ffa_arity(&element).filter(|width| *width <= 4),
        } {
            let mapped =
                self.paired_map(&callback, worker, width, &element, then, (counter, limit));
            self.changed = true;
            return Some(if_((**cond).clone(), (**then).clone(), mapped, then.ty.clone()));
        }
        let per_element = FnArg::Lambda {
            params: vec![value],
            caps: vec![(i, local(counter, Ty::int())), (n, local(limit, Ty::int()))],
            body: CExpr::new(
                K::App {
                    func: Box::new(global(worker)),
                    args: vec![
                        local(i, Ty::int()),
                        local(n, Ty::int()),
                        local(value, element.clone()),
                    ],
                    reuse: Vec::new(),
                    alloc: ClosureAlloc::Heap,
                },
                element.clone(),
            ),
        };
        let mapped = self.generate(
            Chain {
                source: Source::ArrayValue { seq: (**then).clone() },
                stages: Vec::new(),
                consumer: Consumer::Build { f: per_element, filter: false, seq: SeqKind::Array },
                source_elem_ty: element.clone(),
                consumer_elem_ty: element,
                result_ty: then.ty.clone(),
            },
            &base.fns,
        );
        self.changed = true;
        Some(if_((**cond).clone(), (**then).clone(), mapped, then.ty.clone()))
    }

    /// Builds a two-state worker and an array cursor that calls it once per pair.
    fn paired_map(
        &mut self,
        callback: &LoweredDef,
        single: DefId,
        width: usize,
        element: &Ty,
        array: &CExpr,
        counters: (LocalId, LocalId),
    ) -> CExpr {
        let (counter, limit) = counters;
        let pair = DefId::new(
            self.source,
            Symbol::intern(&format!("fuse#{}#paired{}", self.consuming.as_str(), self.chain_index)),
        );
        self.chain_index += 1;
        let pair_ty = Ty::Tuple(vec![Ty::Con(Con::Float); 2 * width]);
        let i = LocalId::from_index(0);
        let n = LocalId::from_index(1);
        let left = LocalId::from_index(2);
        let right = LocalId::from_index(3);
        let next_left = LocalId::from_index(4);
        let next_right = LocalId::from_index(5);
        let mut next = 6;
        let step_left = crate::helper_inline::build_inline(
            callback.entry(),
            vec![local(left, element.clone())],
            1,
            element.clone(),
            &mut next,
        );
        let step_right = crate::helper_inline::build_inline(
            callback.entry(),
            vec![local(right, element.clone())],
            1,
            element.clone(),
            &mut next,
        );
        let again = call(
            pair,
            vec![
                prim(Prim::IntAdd, vec![local(i, Ty::int()), int_lit(1)]),
                local(n, Ty::int()),
                local(next_left, element.clone()),
                local(next_right, element.clone()),
            ],
            pair_ty.clone(),
        );
        let changed = bind(next_left, step_left, bind(next_right, step_right, again));
        let fields = [left, right]
            .into_iter()
            .flat_map(|value| {
                (0..width).map(move |index| component(local(value, element.clone()), index))
            })
            .collect();
        let finished = data(fields, pair_ty.clone());
        let body = if_(
            prim(Prim::IntGe, vec![local(i, Ty::int()), local(n, Ty::int())]),
            finished,
            changed,
            pair_ty.clone(),
        );
        let scheme = Scheme::mono(Ty::arrows(
            [Ty::int(), Ty::int(), element.clone(), element.clone()],
            pair_ty.clone(),
        ));
        self.loops.push(FusedLoop {
            lowered: lowered(pair, vec![i, n, left, right], body),
            abi: FnAbi::from_scheme(&scheme, 4, &|_| None),
            arity: 4,
            result: crate::ResultSig::default(),
        });

        let map = DefId::new(
            self.source,
            Symbol::intern(&format!(
                "fuse#{}#pairedMap{}",
                self.consuming.as_str(),
                self.chain_index
            )),
        );
        self.chain_index += 1;
        let a = LocalId::from_index(0);
        let index = LocalId::from_index(1);
        let length = LocalId::from_index(2);
        let start = LocalId::from_index(3);
        let stop = LocalId::from_index(4);
        let first = LocalId::from_index(5);
        let second = LocalId::from_index(6);
        let result = LocalId::from_index(7);
        let updated = LocalId::from_index(8);
        let final_first = LocalId::from_index(9);
        let final_result = LocalId::from_index(10);
        let array_ty = array.ty.clone();
        let at = |local_id| local(local_id, array_ty.clone());
        let idx = local(index, Ty::int());
        let next_index = prim(Prim::IntAdd, vec![idx.clone(), int_lit(1)]);
        let take = |offset: CExpr| {
            CExpr::new(K::Prim { op: Prim::ArrayTake, args: vec![at(a), offset] }, element.clone())
        };
        let put = |array: CExpr, offset: CExpr, value: CExpr| {
            CExpr::new(
                K::Prim { op: Prim::ArrayPut, args: vec![array, offset, value] },
                array_ty.clone(),
            )
        };
        let unpack = |offset: usize| {
            let fields: Vec<_> = (offset..offset + width)
                .map(|index| component(local(result, pair_ty.clone()), index))
                .collect();
            if matches!(element, Ty::Con(Con::Float)) {
                fields[0].clone()
            } else {
                data(fields, element.clone())
            }
        };
        let paired = bind(
            first,
            take(idx.clone()),
            bind(
                second,
                take(next_index.clone()),
                bind(
                    result,
                    call(
                        pair,
                        vec![
                            local(start, Ty::int()),
                            local(stop, Ty::int()),
                            local(first, element.clone()),
                            local(second, element.clone()),
                        ],
                        pair_ty.clone(),
                    ),
                    bind(
                        updated,
                        put(at(a), idx.clone(), unpack(0)),
                        call(
                            map,
                            vec![
                                put(at(updated), next_index.clone(), unpack(width)),
                                prim(Prim::IntAdd, vec![idx.clone(), int_lit(2)]),
                                local(length, Ty::int()),
                                local(start, Ty::int()),
                                local(stop, Ty::int()),
                            ],
                            array_ty.clone(),
                        ),
                    ),
                ),
            ),
        );
        let final_one = bind(
            final_first,
            take(idx.clone()),
            bind(
                final_result,
                call(
                    single,
                    vec![
                        local(start, Ty::int()),
                        local(stop, Ty::int()),
                        local(final_first, element.clone()),
                    ],
                    element.clone(),
                ),
                put(at(a), idx.clone(), local(final_result, element.clone())),
            ),
        );
        let rest = if_(
            prim(Prim::IntLt, vec![next_index, local(length, Ty::int())]),
            paired,
            final_one,
            array_ty.clone(),
        );
        let body = if_(
            prim(Prim::IntGe, vec![idx, local(length, Ty::int())]),
            at(a),
            rest,
            array_ty.clone(),
        );
        let scheme = Scheme::mono(Ty::arrows(
            [array_ty.clone(), Ty::int(), Ty::int(), Ty::int(), Ty::int()],
            array_ty.clone(),
        ));
        self.loops.push(FusedLoop {
            lowered: lowered(map, vec![a, index, length, start, stop], body),
            abi: FnAbi::from_scheme(&scheme, 5, &|_| None),
            arity: 5,
            result: crate::ResultSig::default(),
        });
        let length = self.fresh_consuming();
        bind(
            length,
            CExpr::new(K::Prim { op: Prim::ArrayLength, args: vec![array.clone()] }, Ty::int()),
            call(
                map,
                vec![
                    CExpr::new(
                        K::Prim { op: Prim::ArrayUnique, args: vec![array.clone()] },
                        array_ty.clone(),
                    ),
                    int_lit(0),
                    local(length, Ty::int()),
                    local(counter, Ty::int()),
                    local(limit, Ty::int()),
                ],
                array_ty,
            ),
        )
    }
}

fn component(base: CExpr, index: usize) -> CExpr {
    if base.ty == Ty::Con(Con::Float) {
        return base;
    }
    CExpr::new(
        K::DataField {
            base: Box::new(base),
            index: FieldIndex::Const(index as u32),
            scalar: true,
            niche: None,
        },
        Ty::Con(Con::Float),
    )
}

fn data(args: Vec<CExpr>, ty: Ty) -> CExpr {
    let scalars = (1u64 << args.len()) - 1;
    CExpr::new(K::MakeData { tag: 0, args, reuse: None, scalars, niche: None }, ty)
}

fn bind(local: LocalId, value: CExpr, body: CExpr) -> CExpr {
    let ty = body.ty.clone();
    CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(body) }, ty)
}

fn call(def: DefId, args: Vec<CExpr>, ty: Ty) -> CExpr {
    CExpr::new(
        K::App { func: Box::new(global(def)), args, reuse: Vec::new(), alloc: ClosureAlloc::Heap },
        ty,
    )
}

fn lowered(def: DefId, params: Vec<LocalId>, body: CExpr) -> LoweredDef {
    LoweredDef {
        def,
        fns: vec![CoreFn { params, captures: Vec::new(), body }],
        entry_borrowed: Vec::new(),
        reuse_entry: None,
        entry_spread_params: Vec::new(),
        data_shapes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_db::{Db, FaiDatabase};

    fn transformed(source: &str) -> bool {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        let result = crate::fuse_def(&db, db.source_file(id).unwrap(), Symbol::intern("loop"));
        result.loops.iter().any(|function| function.lowered.def.name.as_str().contains("#iterate"))
    }

    #[test]
    fn a_pure_numeric_map_can_run_its_iterations_per_element() {
        assert!(transformed(
            "module M\nlet step x = x * 0.5\nlet loop i n xs = if i >= n then xs else loop (i + 1) n (Array.map step xs)\n"
        ));
    }

    #[test]
    fn reordered_parameters_keep_their_roles() {
        assert!(transformed(
            "module M\nlet step x = x + 1\nlet loop n xs i = if i >= n then xs else loop n (Array.map step xs) (i + 1)\n"
        ));
    }

    #[test]
    fn a_potentially_wrapping_counter_step_is_not_interchanged() {
        assert!(!transformed(
            "module M\nlet step x = x + 1\nlet loop i n xs = if i >= n then xs else loop (i + 2) n (Array.map step xs)\n"
        ));
    }

    #[test]
    fn effectful_callbacks_keep_iteration_major_order() {
        assert!(!transformed(
            "module M\nlet step x =\n  let _ = stdConsole.writeLine (Int.toString x)\n  x + 1\nlet loop i n xs = if i >= n then xs else loop (i + 1) n (Array.map step xs)\n"
        ));
    }

    #[test]
    fn possible_callback_traps_keep_iteration_major_order() {
        assert!(!transformed(
            "module M\nlet step x = 100 / x\nlet loop i n xs = if i >= n then xs else loop (i + 1) n (Array.map step xs)\n"
        ));
    }

    #[test]
    fn callbacks_depending_on_the_iteration_are_not_interchanged() {
        assert!(!transformed(
            "module M\nlet step i x = x + i\nlet loop i n xs = if i >= n then xs else loop (i + 1) n (Array.map (step i) xs)\n"
        ));
    }

    #[test]
    fn nonnumeric_payloads_keep_their_original_lifetimes() {
        assert!(!transformed(
            "module M\nlet step x = x ++ \"a\"\nlet loop i n xs = if i >= n then xs else loop (i + 1) n (Array.map step xs)\n"
        ));
    }

    #[test]
    fn cross_file_callbacks_keep_the_signature_firewall() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        db.add_source(
            "A.fai".into(),
            "module A\npublic step : Float -> Float\nlet step x = x * 0.5\n".into(),
        );
        let id = db.add_source("M.fai".into(), "module M\nlet loop i n xs = if i >= n then xs else loop (i + 1) n (Array.map A.step xs)\n".into());
        let result = crate::fuse_def(&db, db.source_file(id).unwrap(), Symbol::intern("loop"));
        assert!(
            !result.loops.iter().any(|function| function
                .lowered
                .def
                .name
                .as_str()
                .contains("#iterate"))
        );
    }

    #[test]
    fn paired_workers_expose_the_same_abi_to_ownership_and_codegen() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let source = "module M\ntype V = { x : Float, y : Float }\nstep : V -> V\nlet step v = { x = v.x * 0.5, y = v.y + 1.0 }\nlet loop i n xs = if i >= n then xs else loop (i + 1) n (Array.map step xs)\n";
        let id = db.add_source("M.fai".into(), source.into());
        let result = crate::fuse_def(&db, db.source_file(id).unwrap(), Symbol::intern("loop"));
        let paired = result
            .loops
            .iter()
            .find(|function| {
                function.lowered.def.name.as_str().contains("#paired0")
                    || function.lowered.def.name.as_str().contains("#paired1")
            })
            .unwrap();
        assert_eq!(*crate::abi_of(&db, paired.lowered.def), paired.abi);
        assert!(paired.abi.spread_param(2).is_some());
    }
}
