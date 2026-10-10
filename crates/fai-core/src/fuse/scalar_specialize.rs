//! Bounded value specialization of private scalar recursion control parameters.

use super::*;

fn supported(e: &CExpr, def: DefId, first: LocalId, arity: usize, calls: &mut usize) -> bool {
    match &e.kind {
        K::Local(_) | K::Lit(Lit::Int(_) | Lit::Bool(_) | Lit::Unit) | K::Error => true,
        K::Let { value, body, .. } => {
            supported(value, def, first, arity, calls) && supported(body, def, first, arity, calls)
        }
        K::If { cond, then, els } => {
            supported(cond, def, first, arity, calls)
                && supported(then, def, first, arity, calls)
                && supported(els, def, first, arity, calls)
        }
        K::Prim {
            op:
                Prim::IntAdd
                | Prim::IntSub
                | Prim::IntMul
                | Prim::IntAnd
                | Prim::IntOr
                | Prim::IntXor
                | Prim::IntLt
                | Prim::IntLe
                | Prim::IntGt
                | Prim::IntGe
                | Prim::Eq
                | Prim::Compare
                | Prim::Not,
            args,
        } => args.iter().all(|arg| supported(arg, def, first, arity, calls)),
        K::App { func, args, .. }
            if matches!(func.kind, K::Global(target) if target == def) && args.len() == arity =>
        {
            let control = &args[0];
            let descending = match &control.kind {
                K::Local(local) => *local == first,
                K::Prim { op: Prim::IntSub, args } => {
                    matches!(args.as_slice(), [CExpr { kind: K::Local(local), .. }, CExpr { kind: K::Lit(Lit::Int(step)), .. }] if *local == first && *step > 0)
                }
                _ => false,
            };
            *calls += 1;
            descending
                && *calls <= 4
                && args.iter().all(|arg| supported(arg, def, first, arity, calls))
        }
        _ => false,
    }
}

fn literal_prim(op: Prim, args: &[CExpr]) -> Option<Lit> {
    if let [CExpr { kind: K::Lit(Lit::Bool(value)), .. }] = args {
        return (op == Prim::Not).then_some(Lit::Bool(!value));
    }
    let [CExpr { kind: K::Lit(Lit::Int(a)), .. }, CExpr { kind: K::Lit(Lit::Int(b)), .. }] = args
    else {
        return None;
    };
    Some(match op {
        Prim::IntAdd => Lit::Int(a.wrapping_add(*b)),
        Prim::IntSub => Lit::Int(a.wrapping_sub(*b)),
        Prim::IntMul => Lit::Int(a.wrapping_mul(*b)),
        Prim::IntAnd => Lit::Int(a & b),
        Prim::IntOr => Lit::Int(a | b),
        Prim::IntXor => Lit::Int(a ^ b),
        Prim::IntLt => Lit::Bool(a < b),
        Prim::IntLe => Lit::Bool(a <= b),
        Prim::IntGt => Lit::Bool(a > b),
        Prim::IntGe => Lit::Bool(a >= b),
        Prim::Eq => Lit::Bool(a == b),
        Prim::Compare => Lit::Int(match a.cmp(b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }),
        _ => return None,
    })
}

fn fold(e: &CExpr, constants: &FxHashMap<LocalId, CExpr>) -> CExpr {
    let kind = match &e.kind {
        K::Local(local) => return constants.get(local).cloned().unwrap_or_else(|| e.clone()),
        K::Let { local, value, body } => {
            let value = fold(value, constants);
            if matches!(value.kind, K::Lit(_)) {
                let mut constants = constants.clone();
                constants.insert(*local, value);
                return fold(body, &constants);
            }
            K::Let { local: *local, value: Box::new(value), body: Box::new(fold(body, constants)) }
        }
        K::If { cond, then, els } => {
            let cond = fold(cond, constants);
            if let K::Lit(Lit::Bool(value)) = cond.kind {
                return fold(if value { then } else { els }, constants);
            }
            K::If {
                cond: Box::new(cond),
                then: Box::new(fold(then, constants)),
                els: Box::new(fold(els, constants)),
            }
        }
        K::Prim { op, args } => {
            let args: Vec<_> = args.iter().map(|arg| fold(arg, constants)).collect();
            if let Some(value) = literal_prim(*op, &args) {
                return CExpr::new(K::Lit(value), e.ty.clone());
            }
            K::Prim { op: *op, args }
        }
        K::App { func, args, reuse, alloc } => K::App {
            func: Box::new(fold(func, constants)),
            args: args.iter().map(|arg| fold(arg, constants)).collect(),
            reuse: reuse.clone(),
            alloc: *alloc,
        },
        _ => return e.clone(),
    };
    CExpr::new(kind, e.ty.clone())
}

impl Fuser<'_> {
    pub(super) fn scalar_specialized(&mut self, e: &CExpr, base_fns: &[CoreFn]) -> Option<CExpr> {
        let (def, args) = call_target(e)?;
        if def.file != self.source || args.len() < 2 {
            return None;
        }
        let first = fold(&args[0], &FxHashMap::default());
        let K::Lit(Lit::Int(control)) = first.kind else { return None };
        if !(0..=8).contains(&control) {
            return None;
        }
        let file = self.db.source_file(def.file)?;
        let defs = fai_resolve::module_defs(self.db, file);
        if defs.get(def.name)?.visibility != fai_syntax::ast::Visibility::Private {
            return None;
        }
        if !fai_resolve::recursive_defs(self.db, file).contains(&def) {
            return None;
        }
        let abi = crate::abi_of(self.db, def);
        if !abi.register_abi
            || abi.ret != Repr::ScalarInt
            || abi.params.len() != args.len()
            || args.len() > 4
            || abi.params.iter().any(|p| *p != Repr::ScalarInt)
        {
            return None;
        }
        let template = crate::helper_inlined(self.db, file, def.name);
        if template.fns.len() != 1
            || template.entry().params.len() != args.len()
            || !template.entry().captures.is_empty()
            || crate::helper_inline::node_count(&template.entry().body) > 64
        {
            return None;
        }
        let mut calls = 0;
        if !supported(
            &template.entry().body,
            def,
            template.entry().params[0],
            args.len(),
            &mut calls,
        ) || calls == 0
        {
            return None;
        }
        if let Some(target) = self.scalar_variants.get(&(def, control)).copied() {
            let arguments = args[1..].iter().map(|arg| self.rewrite(arg, base_fns)).collect();
            return Some(CExpr::new(
                K::App {
                    func: Box::new(global(target)),
                    args: arguments,
                    reuse: Vec::new(),
                    alloc: ClosureAlloc::Heap,
                },
                e.ty.clone(),
            ));
        }
        let constants = FxHashMap::from_iter([(template.entry().params[0], int_lit(control))]);
        let body = fold(&template.entry().body, &constants);
        let entry =
            CoreFn { params: template.entry().params[1..].to_vec(), captures: Vec::new(), body };
        let mut referenced = false;
        walk_pre(&entry.body, &mut |e| {
            referenced |= matches!(e.kind, K::Global(target) if target == def);
        });
        if !referenced {
            let arguments = args[1..].iter().map(|arg| self.rewrite(arg, base_fns)).collect();
            return Some(crate::helper_inline::build_inline(
                &entry,
                arguments,
                entry.params.len(),
                e.ty.clone(),
                &mut self.next_local,
            ));
        }
        if self.scalar_variants.len() >= 8 {
            return None;
        }
        let target = DefId::new(
            self.source,
            Symbol::intern(&format!("fuse#{}#scalar{}", self.consuming.as_str(), self.chain_index)),
        );
        self.chain_index += 1;
        self.scalar_variants.insert((def, control), target);
        let arguments = args[1..].iter().map(|arg| self.rewrite(arg, base_fns)).collect();
        let saved_next = self.next_local;
        self.next_local = crate::inline::next_free_local(&template);
        let body = self.rewrite(&entry.body, &template.fns);
        self.next_local = saved_next;
        let mut variant_abi = (*abi).clone();
        variant_abi.params.remove(0);
        let arity = entry.params.len();
        let body = if crate::helper_inline::node_count(&body) <= 512 {
            body
        } else {
            let mut args = vec![int_lit(control)];
            args.extend(entry.params.iter().map(|p| local(*p, Ty::int())));
            CExpr::new(
                K::App {
                    func: Box::new(global(def)),
                    args,
                    reuse: Vec::new(),
                    alloc: ClosureAlloc::Heap,
                },
                Ty::int(),
            )
        };
        let lowered = LoweredDef {
            def: target,
            fns: vec![CoreFn { body, ..entry }],
            entry_borrowed: Vec::new(),
            reuse_entry: None,
            entry_spread_params: Vec::new(),
            data_shapes: Vec::new(),
        };
        self.loops.push(FusedLoop {
            lowered,
            arity,
            abi: variant_abi,
            result: crate::ResultSig::default(),
        });
        Some(CExpr::new(
            K::App {
                func: Box::new(global(target)),
                args: arguments,
                reuse: Vec::new(),
                alloc: ClosureAlloc::Heap,
            },
            e.ty.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_db::{Db, FaiDatabase};

    fn fused(source: &str) -> Arc<FuseResult> {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        crate::fuse_def(&db, db.source_file(id).unwrap(), Symbol::intern("run"))
    }

    const LEVELS: &str = "module M\nlet descend level n = if level = 0 then n else if n = 0 then descend (level - 1) 1 else descend level (n - 1)\n";

    #[test]
    fn the_number_of_variants_is_bounded() {
        let result = fused(&format!("{LEVELS}let run n = descend 8 n\n"));
        assert_eq!(result.loops.len(), 8);
    }

    #[test]
    fn variable_control_arguments_keep_the_shared_entry() {
        let result = fused(&format!("{LEVELS}let run level n = descend level n\n"));
        assert!(result.loops.is_empty());
    }

    #[test]
    fn large_constants_keep_the_shared_entry() {
        let result = fused(&format!("{LEVELS}let run n = descend 9 n\n"));
        assert!(result.loops.is_empty());
    }

    #[test]
    fn negative_constants_keep_the_shared_entry() {
        let result = fused(&format!("{LEVELS}let run n = descend (-1) n\n"));
        assert!(result.loops.is_empty());
    }

    #[test]
    fn exported_functions_keep_the_shared_entry() {
        let source = "module M\npublic descend : Int -> Int -> Int\nlet descend level n = if level = 0 then n else descend (level - 1) (n + 1)\nlet run n = descend 3 n\n";
        assert!(fused(source).loops.is_empty());
    }

    #[test]
    fn increasing_controls_are_not_specialized() {
        let source = "module M\nlet ascend level n = if level >= 10 then n else ascend (level + 1) (n + 1)\nlet run n = ascend 3 n\n";
        assert!(fused(source).loops.is_empty());
    }

    #[test]
    fn effectful_scalar_functions_are_not_specialized() {
        let source = "module M\nlet descend level n =\n  let _ = stdConsole.writeLine \"step\"\n  if level = 0 then n else descend (level - 1) (n + 1)\nlet run n = descend 3 n\n";
        assert!(fused(source).loops.is_empty());
    }

    #[test]
    fn constant_scalar_levels_generate_bounded_variants() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let source = "module M\nlet ack m n = if m = 0 then n + 1 else if n = 0 then ack (m - 1) 1 else ack (m - 1) (ack m (n - 1))\nlet run n = ack 3 n\n";
        let id = db.add_source("M.fai".into(), source.into());
        let file = db.source_file(id).unwrap();
        let result = crate::fuse_def(&db, file, Symbol::intern("run"));
        let base = crate::helper_inlined(&db, file, Symbol::intern("ack"));
        let mut calls = 0;
        assert!(
            !result.loops.is_empty(),
            "{}\nsize={} supported={} abi={:?}",
            crate::pretty_def(&base),
            crate::helper_inline::node_count(&base.entry().body),
            supported(&base.entry().body, base.def, base.entry().params[0], 2, &mut calls),
            crate::abi_of(&db, base.def)
        );
    }
}
