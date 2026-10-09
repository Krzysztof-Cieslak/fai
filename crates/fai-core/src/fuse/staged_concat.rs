//! Folds materialized chunks without allocating a concatenated list spine.
//! A concatMap still completes all producer callbacks before any fold callback.

use super::*;

impl Fuser<'_> {
    pub(super) fn staged_concat(&mut self, e: &CExpr, base_fns: &[CoreFn]) -> Option<CExpr> {
        if let K::Let { local: binding, value, body } = &e.kind
            && let Some(chunks) = self.concat_chunks(value)
            && seq_elem(&chunks.ty).is_some_and(|inner| is_seq(&inner, Con::List))
            && self.single_fold_use(body, *binding)
        {
            let chunks = self.rewrite(&chunks, base_fns);
            let slot = self.fresh_consuming();
            self.flat_sources.insert(*binding, local(slot, chunks.ty.clone()));
            let body = self.rewrite(body, base_fns);
            self.flat_sources.remove(binding);
            return Some(CExpr::new(
                K::Let { local: slot, value: Box::new(chunks), body: Box::new(body) },
                e.ty.clone(),
            ));
        }
        let (def, args) = call_target(e)?;
        if self.defs.lookup(def) != Some((SeqKind::List, Comb::Foldl)) || args.len() != 3 {
            return None;
        }
        let chunks = if let K::Local(binding) = args[2].kind {
            self.flat_sources.get(&binding)?.clone()
        } else {
            self.concat_chunks(&args[2])?
        };
        let list_ty = seq_elem(&chunks.ty)?;
        let elem_ty = seq_elem(&list_ty)?;

        // Function construction and the initial accumulator precede the source
        // in a direct fold call. Explicit lets retain that strict order.
        let mut binds = Vec::new();
        let callback = &args[0];
        let literal = matches!(callback.kind, K::MakeClosure { func, .. }
            if base_fns[func.index()].params.len() == 2 && !body_has_closure(&base_fns[func.index()].body));
        let function = if literal
            || matches!(callback.kind, K::Global(g) if crate::abi_of(self.db, g).register_abi)
        {
            callback.clone()
        } else {
            let id = self.fresh_consuming();
            binds.push((id, self.rewrite(callback, base_fns)));
            local(id, callback.ty.clone())
        };
        let initial = self.fresh_consuming();
        binds.push((initial, self.rewrite(&args[1], base_fns)));
        let source = self.fresh_consuming();
        binds.push((source, self.rewrite(&chunks, base_fns)));
        let callback = self.fn_arg(&function, 2, base_fns);
        let mut result = self.chunk_fold(
            local(source, chunks.ty),
            local(initial, e.ty.clone()),
            callback,
            elem_ty,
            e.ty.clone(),
        );
        for (id, value) in binds.into_iter().rev() {
            result = CExpr::new(
                K::Let { local: id, value: Box::new(value), body: Box::new(result) },
                e.ty.clone(),
            );
        }
        Some(result)
    }

    fn concat_chunks(&self, value: &CExpr) -> Option<CExpr> {
        let (def, args) = call_target(value)?;
        match self.defs.lookup(def)? {
            (SeqKind::List, Comb::Concat) if args.len() == 1 => Some(args[0].clone()),
            (SeqKind::List, Comb::ConcatMap) if args.len() == 2 => {
                let map = *self.defs.map.get(&(SeqKind::List, Comb::Map))?;
                Some(CExpr::new(
                    K::App {
                        func: Box::new(global(map)),
                        args: args.to_vec(),
                        reuse: Vec::new(),
                        alloc: ClosureAlloc::Heap,
                    },
                    Ty::list(value.ty.clone()),
                ))
            }
            _ => None,
        }
    }

    fn single_fold_use(&self, body: &CExpr, binding: LocalId) -> bool {
        let mut work = vec![body];
        let mut budget = 1024;
        let mut uses = 0;
        let mut fold = false;
        while let Some(node) = work.pop() {
            if budget == 0 {
                return false;
            }
            budget -= 1;
            if let Some((def, args)) = call_target(node)
                && self.defs.lookup(def) == Some((SeqKind::List, Comb::Foldl))
                && args.len() == 3
                && matches!(args[2].kind, K::Local(local) if local == binding)
            {
                fold = true;
            }
            match &node.kind {
                K::Local(local) => uses += usize::from(*local == binding),
                K::MakeClosure { captures, .. } => uses += usize::from(captures.contains(&binding)),
                K::App { func, args, .. } => {
                    if args.len() > budget {
                        return false;
                    }
                    work.push(func);
                    work.extend(args);
                }
                K::Prim { args, .. } | K::Foreign { args, .. } | K::MakeData { args, .. } => {
                    if args.len() > budget {
                        return false;
                    }
                    work.extend(args);
                }
                K::If { cond, then, els } => work.extend([cond.as_ref(), then, els]),
                K::Let { value, body, .. } => work.extend([value.as_ref(), body]),
                K::DataTag { base, .. } | K::DataField { base, .. } => work.push(base),
                K::Lit(_) | K::Global(_) | K::Error => {}
                _ => return false,
            }
            if uses > 1 {
                return false;
            }
        }
        uses == 1 && fold
    }

    fn chunk_fold(
        &mut self,
        chunks: CExpr,
        initial: CExpr,
        callback: FnArg,
        elem_ty: Ty,
        result_ty: Ty,
    ) -> CExpr {
        let def = DefId::new(
            self.source,
            Symbol::intern(&format!("fuse#{}#{}", self.consuming.as_str(), self.chain_index)),
        );
        self.chain_index += 1;
        let mut g = LoopGen::new(def, result_ty.clone());
        let list_ty = Ty::list(elem_ty.clone());
        let outer_ty = chunks.ty.clone();
        let outer = g.add_param(outer_ty.clone(), chunks);
        let inner = g.add_param(
            list_ty.clone(),
            CExpr::new(
                K::MakeData {
                    tag: NIL_TAG,
                    args: Vec::new(),
                    reuse: None,
                    scalars: 0,
                    niche: None,
                },
                list_ty.clone(),
            ),
        );
        let acc = g.add_param(result_ty.clone(), initial);
        g.iter_locals = vec![outer, inner];
        g.acc_local = Some(acc);
        let head = g.fresh();
        // Classifying the callback adds its captures before any back-edge is built.
        let applied = apply_fn(
            &mut g,
            &callback,
            vec![local(acc, result_ty.clone()), local(head, elem_ty.clone())],
            result_ty.clone(),
        );
        let step = g.recur(
            &[
                local(outer, outer_ty.clone()),
                data_field(local(inner, list_ty.clone()), 1, list_ty.clone()),
            ],
            Some(applied),
        );
        let step = CExpr::new(
            K::Let {
                local: head,
                value: Box::new(data_field(local(inner, list_ty.clone()), 0, elem_ty)),
                body: Box::new(step),
            },
            result_ty.clone(),
        );
        let next_chunk = g.recur(
            &[
                data_field(local(outer, outer_ty.clone()), 1, outer_ty.clone()),
                data_field(local(outer, outer_ty.clone()), 0, list_ty.clone()),
            ],
            Some(local(acc, result_ty.clone())),
        );
        let empty = |value: CExpr| {
            prim(
                Prim::Eq,
                vec![
                    CExpr::new(K::DataTag { base: Box::new(value), niche: None }, Ty::int()),
                    int_lit(0),
                ],
            )
        };
        let next = if_(
            empty(local(outer, outer_ty)),
            local(acc, result_ty.clone()),
            next_chunk,
            result_ty.clone(),
        );
        let body = if_(empty(local(inner, list_ty)), next, step, result_ty.clone());
        let abi = self.loop_abi(&g, &result_ty);
        let params: Vec<_> = g.params.iter().map(|(id, _)| *id).collect();
        let arity = params.len();
        let lowered = LoweredDef {
            def,
            fns: vec![CoreFn { params, captures: Vec::new(), body }],
            entry_borrowed: Vec::new(),
            reuse_entry: None,
            entry_spread_params: Vec::new(),
            data_shapes: Vec::new(),
        };
        self.loops.push(FusedLoop { lowered, abi, arity, result: crate::ResultSig::default() });
        CExpr::new(
            K::App {
                func: Box::new(global(def)),
                args: g.call_args,
                reuse: Vec::new(),
                alloc: ClosureAlloc::Heap,
            },
            result_ty,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fused(source: &str) -> Arc<FuseResult> {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        fuse_def(&db, db.source_file(id).unwrap(), Symbol::intern("run"))
    }

    #[test]
    fn a_single_use_flattened_binding_becomes_a_chunk_loop() {
        let result = fused(
            "module M\nlet run chunks =\n  let flat = List.concat chunks\n  List.foldl (fun acc x -> acc + x) 0 flat\n",
        );
        assert_eq!(result.loops.len(), 1);
        assert!(!crate::pretty_def(&result.body).contains("@concat"));
        assert!(!crate::pretty_def(&result.loops[0].lowered).contains("(data 1"));
    }

    #[test]
    fn sharing_the_flattened_value_keeps_its_construction() {
        let result = fused(
            "module M\nlet run chunks =\n  let flat = List.concat chunks\n  List.foldl (fun acc x -> acc + x) 0 flat + List.length flat\n",
        );
        assert!(crate::pretty_def(&result.body).contains("@concat"));
    }

    #[test]
    fn shadowed_concat_is_not_a_standard_flattening_operation() {
        let result = fused(
            "module M\nlet concat n xs = if n = 0 then [] else concat (n - 1) xs\nlet run chunks = List.foldl (fun acc x -> acc + x) 0 (concat 1 chunks)\n",
        );
        assert!(crate::pretty_def(&result.body).contains("@concat"));
    }
}
