//! Unrolls directly consumed literal-list spines while keeping strict evaluation.

use super::*;
use rustc_hash::FxHashSet;

/// A bounded same-file producer whose result is a complete literal after its
/// straight-line bindings. A rejected body is a stable early-cutoff result.
#[salsa::tracked]
fn literal_producer(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<Arc<CoreFn>> {
    let def = DefId::new(file.source(db), name);
    if fai_resolve::recursive_defs(db, file).contains(&def) || !crate::abi_of(db, def).register_abi
    {
        return None;
    }
    let source = helper_inlined(db, file, name);
    if source.fns.len() != 1
        || !source.entry().captures.is_empty()
        || node_count(&source.entry().body) > 128
    {
        return None;
    }
    let mut tail = &source.entry().body;
    while let K::Let { body, .. } = &tail.kind {
        tail = body;
    }
    let elements = literal_elems(SeqKind::List, tail)?;
    (1..=16).contains(&elements.len()).then(|| Arc::new(source.entry().clone()))
}

fn bind(local: LocalId, value: CExpr, body: CExpr) -> CExpr {
    let ty = body.ty.clone();
    CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(body) }, ty)
}

impl Fuser<'_> {
    pub(super) fn ordered_literal_fold(
        &mut self,
        expression: &CExpr,
        base_fns: &[CoreFn],
    ) -> Option<CExpr> {
        let (def, args) = call_target(expression)?;
        let (SeqKind::List, kind @ (Comb::Foldl | Comb::Foldr)) = self.defs.lookup(def)? else {
            return None;
        };
        let [callback, initial, source] = args else { return None };
        let element_type = seq_elem(&source.ty)?;
        if !local_constructors::resource_free(
            self.db,
            &element_type,
            &mut FxHashSet::default(),
            &mut 128,
        ) {
            return None;
        }
        // Keep the existing pure-literal path, which can inline its element lambda.
        if let Some(elements) = literal_elems(SeqKind::List, source)
            && self.arg_reorderable(callback, 2, base_fns)
            && self.expr_reorderable(initial)
            && elements.iter().all(|element| self.expr_reorderable(element))
        {
            return None;
        }

        let mut expanded = source.clone();
        if let Some((producer, arguments)) = call_target(source) {
            if producer.file != self.source
                || !arguments.iter().all(|argument| {
                    local_constructors::resource_free(
                        self.db,
                        &argument.ty,
                        &mut FxHashSet::default(),
                        &mut 128,
                    )
                })
            {
                return None;
            }
            let file = self.db.source_file(self.source)?;
            let template = literal_producer(self.db, file, producer.name)?;
            if template.params.len() != arguments.len() {
                return None;
            }
            expanded = crate::helper_inline::build_inline(
                &template,
                arguments.to_vec(),
                arguments.len(),
                source.ty.clone(),
                &mut self.next_local,
            );
        }
        let mut prefix = Vec::new();
        let mut tail = &expanded;
        while let K::Let { local, value, body } = &tail.kind {
            prefix.push((*local, (**value).clone()));
            tail = body;
        }
        let elements = literal_elems(SeqKind::List, tail)?;
        if !(1..=16).contains(&elements.len()) {
            return None;
        }

        let mut bindings = Vec::new();
        let function = if matches!(callback.kind, K::Global(def) if crate::abi_of(self.db, def).register_abi)
        {
            callback.clone()
        } else {
            let slot = self.fresh_consuming();
            bindings.push((slot, self.rewrite(callback, base_fns)));
            local(slot, callback.ty.clone())
        };
        let accumulator = self.fresh_consuming();
        bindings.push((accumulator, self.rewrite(initial, base_fns)));
        for (slot, value) in prefix {
            bindings.push((slot, self.rewrite(&value, base_fns)));
        }
        let mut values = Vec::new();
        for element in elements {
            let slot = self.fresh_consuming();
            values.push(local(slot, element.ty.clone()));
            bindings.push((slot, self.rewrite(&element, base_fns)));
        }
        let step = FnArg::Value(function);
        let init = local(accumulator, initial.ty.clone());
        let consumer = if kind == Comb::Foldl {
            Consumer::Foldl { step, init }
        } else {
            Consumer::Foldr { step, init }
        };
        let mut result = self.try_unroll(&values, &[], &consumer, &expression.ty, base_fns)?;
        for (slot, value) in bindings.into_iter().rev() {
            result = bind(slot, value, result);
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_db::FaiDatabase;

    fn output(source: &str) -> String {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        let file = db.source_file(id).unwrap();
        let diagnostics = fai_types::check_file::accumulated::<fai_db::Diag>(&db, file);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        crate::pretty_def(&fuse_def(&db, file, Symbol::intern("run")).body)
    }

    #[test]
    fn a_non_total_callback_can_consume_a_literal_without_its_spine() {
        let body = output(
            "module M\npublic run : Int -> Int\nlet run n = List.foldl (fun acc item -> acc / item) n [1, 2]\n",
        );
        assert!(!body.contains("@foldl") && !body.contains("(data "), "{body}");
    }

    #[test]
    fn a_shared_literal_keeps_its_materialized_spine() {
        let body = output(
            "module M\npublic run : Int -> Int\nlet run n =\n  let items = [1, 2]\n  List.foldl (fun acc item -> acc / item) n items + List.length items\n",
        );
        assert!(body.contains("@foldl") && body.contains("(data "), "{body}");
    }

    #[test]
    fn a_dynamic_list_tail_is_not_unrolled() {
        let body = output(
            "module M\npublic run : List Int -> Int\nlet run items = List.foldl (fun acc item -> acc / item) 10 (1 :: items)\n",
        );
        assert!(body.contains("@foldl"), "{body}");
    }

    #[test]
    fn closure_elements_keep_their_original_lifetimes() {
        let body = output(
            "module M\npublic run : Int -> Int\nlet run n = List.foldl (fun acc f -> acc + f ()) 0 [fun _ -> n, fun _ -> n + 1]\n",
        );
        assert!(body.contains("@foldl"), "{body}");
    }

    #[test]
    fn oversized_literals_keep_the_library_fold() {
        let items = (1..=17).map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
        let body = output(&format!(
            "module M\npublic run : Int -> Int\nlet run n = List.foldl (fun acc item -> acc / item) n [{items}]\n"
        ));
        assert!(body.contains("@foldl"), "{body}");
    }

    #[test]
    fn a_bounded_neighbour_producer_unrolls_at_its_consumer() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source(
            "GameOfLife.fai".into(),
            include_str!("../../../../samples/algorithms/GameOfLife.fai").into(),
        );
        let body = crate::pretty_def(
            &fuse_def(&db, db.source_file(id).unwrap(), Symbol::intern("countNeighbors")).body,
        );
        assert!(!body.contains("@neighbors"), "{body}");
        assert_eq!(body.matches("@bump").count(), 8, "{body}");
    }

    #[test]
    fn a_cross_file_producer_stays_behind_its_signature() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        db.add_source(
            "Other.fai".into(),
            "module Other\npublic items : Int -> List Int\nlet items n = [n, n + 1]\n".into(),
        );
        let id = db.add_source("M.fai".into(), "module M\npublic run : Int -> Int\nlet run n = List.foldl (fun acc item -> acc / item) 100 (Other.items n)\n".into());
        let body = crate::pretty_def(
            &fuse_def(&db, db.source_file(id).unwrap(), Symbol::intern("run")).body,
        );
        assert!(body.contains("@items") && body.contains("@foldl"), "{body}");
    }

    #[test]
    fn producer_resource_arguments_keep_their_call_lifetime() {
        let items = (1..=16).map(|n| format!("n + {n}")).collect::<Vec<_>>().join(", ");
        let source = format!(
            "module M\nitems : Console -> Int -> List Int / {{ Console }}\nlet items console n =\n  let _ = console.writeLine \"source\"\n  [{items}]\npublic run : Console -> Int -> Int / {{ Console }}\nlet run console n = List.foldl (fun acc item -> acc / item) 100 (items console n)\n"
        );
        let body = output(&source);
        assert!(body.contains("@items") && body.contains("@foldl"), "{body}");
    }
}
