//! Bounded reduction of known interface fields and finite method choices.

use fai_db::{Db, Diag, SourceFile};
use fai_resolve::{DefId, LocalId, recursive_defs};
use fai_syntax::Symbol;
use fai_types::Ty;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{CAF_NODE_BUDGET, Simplifier, apply, ground_type, node_count, total_nodes};
use crate::inline::{fresh_local, remap_expr};
use crate::ir::{CExpr, ExprKind as K, FieldIndex};

const MAX_CHOICES: usize = 4;
const MAX_CONTINUATION: usize = 256;

fn interface(ty: &Ty) -> bool {
    match ty {
        Ty::Interface(_) => true,
        Ty::App(head, _) => interface(head),
        _ => false,
    }
}

/// Counts finite return choices, resolving lexical aliases without executing any
/// initializer. Conditions and leading lets keep their original strict order.
fn choices(
    e: &CExpr,
    env: &mut FxHashMap<LocalId, usize>,
    leaf: &impl Fn(&CExpr) -> bool,
) -> Option<usize> {
    let count = match &e.kind {
        K::Error => 0,
        K::If { then, els, .. } => choices(then, env, leaf)? + choices(els, env, leaf)?,
        K::Let { local, value, body } => {
            let old = env.remove(local);
            if let Some(n) = choices(value, env, leaf) {
                env.insert(*local, n);
            }
            let result = choices(body, env, leaf);
            env.remove(local);
            if let Some(n) = old {
                env.insert(*local, n);
            }
            return result;
        }
        K::Local(local) => env.get(local).copied().or_else(|| leaf(e).then_some(1))?,
        _ if leaf(e) => 1,
        _ => return None,
    };
    (count <= MAX_CHOICES).then_some(count)
}

/// Body edits of an ineligible factory stop at this small summary. Factories are
/// same-file, acyclic, non-row-polymorphic and return a concrete interface shape.
#[salsa::tracked]
fn receiver_summary(db: &dyn Db, file: SourceFile, name: Symbol) -> Option<usize> {
    let def = DefId::new(file.source(db), name);
    if recursive_defs(db, file).contains(&def) {
        return None;
    }
    let body = super::simplified(db, file, name);
    if !interface(&body.entry().body.ty)
        || !ground_type(&body.entry().body.ty)
        || total_nodes(&body) > CAF_NODE_BUDGET
        || !body.entry().captures.is_empty()
        || crate::representation::definition_scheme(db, def)
            .is_some_and(|s| fai_types::evidence_count(&s) != 0)
        || !crate::core::accumulated::<Diag>(db, file, name).is_empty()
    {
        return None;
    }
    let n = choices(&body.entry().body, &mut FxHashMap::default(), &|e| {
        matches!(e.kind, K::MakeData { .. }) && interface(&e.ty)
    })?;
    (n > 0).then_some(body.entry().params.len())
}

impl Simplifier<'_> {
    fn stable(&self, e: &CExpr) -> bool {
        match &e.kind {
            K::Lit(_) | K::Local(_) | K::MakeClosure { .. } => true,
            K::MakeData { args, .. } => args.iter().all(|e| self.stable(e)),
            K::Global(def) => crate::abi_of(self.db, *def).register_abi,
            _ => false,
        }
    }

    pub(super) fn known_binding(&self, e: &CExpr) -> Option<CExpr> {
        let known = if let K::Local(local) = &e.kind { self.known_values.get(local)? } else { e };
        let candidate = matches!(known.kind, K::MakeClosure { .. } | K::Global(_))
            || (matches!(known.kind, K::MakeData { .. }) && interface(&known.ty));
        if !candidate {
            return None;
        }
        if known.ty != e.ty || !ground_type(&e.ty) {
            return None;
        }
        match &known.kind {
            K::MakeClosure { .. } => Some(known.clone()),
            K::MakeData { .. } if interface(&known.ty) && self.stable(known) => Some(known.clone()),
            K::Global(def) if crate::abi_of(self.db, *def).register_abi => Some(known.clone()),
            _ => None,
        }
    }

    fn spend(&mut self, nodes: usize) -> bool {
        if nodes > self.choice_growth {
            return false;
        }
        self.choice_growth -= nodes;
        true
    }

    /// Removing a closure must not move the last use of a captured resource
    /// earlier. Inlined methods keep their captures live in the continuation;
    /// otherwise retain the construction/drop at its original evaluation point.
    fn discardable(known: &CExpr, continuation: &CExpr) -> bool {
        match &known.kind {
            K::MakeClosure { captures, .. } => captures.iter().all(|&l| uses(continuation, l)),
            K::MakeData { args, .. } => args.iter().all(|a| Self::discardable(a, continuation)),
            K::Global(_) | K::Lit(_) => true,
            K::Local(l) => uses(continuation, *l),
            _ => false,
        }
    }

    fn receiver_count(&self, e: &CExpr) -> Option<usize> {
        choices(e, &mut FxHashMap::default(), &|e| {
            matches!(e.kind, K::MakeData { .. }) && interface(&e.ty)
                || self.known_binding(e).is_some_and(|v| matches!(v.kind, K::MakeData { .. }))
        })
    }

    fn callable_count(&self, e: &CExpr) -> Option<usize> {
        choices(e, &mut FxHashMap::default(), &|e| {
            self.known_binding(e).is_some_and(|v| self.method_value(&v))
        })
    }

    fn method_value(&self, value: &CExpr) -> bool {
        match value.kind {
            K::MakeClosure { func, .. } => self.method_functions.contains(&func),
            K::Global(_) => true,
            _ => false,
        }
    }

    fn expand_receiver(&mut self, e: &CExpr) -> Option<CExpr> {
        if !interface(&e.ty) || !ground_type(&e.ty) {
            return None;
        }
        let (def, args) = match &e.kind {
            K::Global(def) => (*def, &[][..]),
            K::App { func, args, .. } => match func.kind {
                K::Global(def) => (def, args.as_slice()),
                _ => return None,
            },
            _ => return None,
        };
        if def.file != self.source || receiver_summary(self.db, self.file, def.name)? != args.len()
        {
            return None;
        }
        let callee = super::simplified(self.db, self.file, def.name);
        if callee.entry().body.ty != e.ty || !self.spend(total_nodes(&callee)) {
            return None;
        }
        Some(self.relocated_body(&callee, args))
    }

    fn project(&mut self, base: &CExpr, index: u32, scalar: bool, ty: &Ty) -> Option<CExpr> {
        let projection = |base: CExpr| {
            CExpr::new(
                K::DataField {
                    base: Box::new(base),
                    index: FieldIndex::Const(index),
                    scalar,
                    niche: None,
                },
                ty.clone(),
            )
        };
        if let Some(expanded) = self.expand_receiver(base) {
            return Some(projection(expanded));
        }
        match &base.kind {
            K::Local(local) => {
                let known = self.known_values.get(local)?.clone();
                (known.ty == base.ty).then_some(())?;
                self.project(&known, index, scalar, ty)
            }
            K::MakeData { args, .. } => {
                let selected = args.get(index as usize)?;
                // Instance method closures have erased slot types in Core; the
                // resolved interface projection supplies their logical arrow.
                if selected.ty != *ty
                    && !(selected.ty == Ty::Error && matches!(selected.kind, K::MakeClosure { .. }))
                {
                    return None;
                }
                if !self.spend(node_count(base)) {
                    return None;
                }
                for arg in args {
                    if let K::MakeClosure { func, .. } = arg.kind {
                        self.method_functions.insert(func);
                    }
                }
                let locals: Vec<_> = args.iter().map(|_| fresh_local(&mut self.next)).collect();
                let mut result = CExpr::new(K::Local(locals[index as usize]), ty.clone());
                for (position, (&local, value)) in locals.iter().zip(args).enumerate().rev() {
                    let mut value = value.clone();
                    if position == index as usize {
                        value.ty = ty.clone();
                    }
                    result = CExpr::new(
                        K::Let { local, value: Box::new(value), body: Box::new(result) },
                        ty.clone(),
                    );
                }
                Some(result)
            }
            K::Let { local, value, body } if self.receiver_count(base).is_some() => {
                Some(CExpr::new(
                    K::Let {
                        local: *local,
                        value: value.clone(),
                        body: Box::new(projection((**body).clone())),
                    },
                    ty.clone(),
                ))
            }
            K::If { cond, then, els } if self.receiver_count(base).is_some() && self.spend(2) => {
                Some(CExpr::new(
                    K::If {
                        cond: cond.clone(),
                        then: Box::new(projection((**then).clone())),
                        els: Box::new(projection((**els).clone())),
                    },
                    ty.clone(),
                ))
            }
            _ => None,
        }
    }

    pub(super) fn contract_choice(&mut self, e: &CExpr) -> Option<CExpr> {
        match &e.kind {
            K::DataField { base, .. } if matches!(base.kind, K::Error) => {
                Some(CExpr::new(K::Error, e.ty.clone()))
            }
            K::DataField { base, index: FieldIndex::Const(index), scalar, niche: None }
                if interface(&base.ty) && ground_type(&base.ty) =>
            {
                self.project(base, *index, *scalar, &e.ty)
            }
            K::App { func, args, reuse, alloc } if ground_type(&func.ty) => {
                if let K::Local(_) = func.kind
                    && let Some(known) = self.known_binding(func)
                    && self.method_value(&known)
                {
                    let cost = match &known.kind {
                        K::MakeClosure { func, .. } => node_count(&self.fns[func.index()].body),
                        K::Global(_) => 1,
                        _ => return None,
                    };
                    if cost <= CAF_NODE_BUDGET && self.spend(cost + args.len()) {
                        return Some(apply(known, args.clone(), e.ty.clone()));
                    }
                }
                match &func.kind {
                    K::Let { local, value, body } => Some(CExpr::new(
                        K::Let {
                            local: *local,
                            value: value.clone(),
                            body: Box::new(CExpr::new(
                                K::App {
                                    func: body.clone(),
                                    args: args.clone(),
                                    reuse: reuse.clone(),
                                    alloc: *alloc,
                                },
                                e.ty.clone(),
                            )),
                        },
                        e.ty.clone(),
                    )),
                    K::If { cond, then, els } if self.callable_count(func).is_some() => {
                        let cost: usize = args.iter().map(node_count).sum();
                        if cost > MAX_CONTINUATION || !self.spend(cost) {
                            return None;
                        }
                        let left = args.iter().map(|a| self.fresh_copy(a, None)).collect();
                        let right = args.iter().map(|a| self.fresh_copy(a, None)).collect();
                        Some(CExpr::new(
                            K::If {
                                cond: cond.clone(),
                                then: Box::new(apply((**then).clone(), left, e.ty.clone())),
                                els: Box::new(apply((**els).clone(), right, e.ty.clone())),
                            },
                            e.ty.clone(),
                        ))
                    }
                    _ => None,
                }
            }
            K::Let { local, value, body } => {
                if !uses(body, *local) {
                    let known = self
                        .known_binding(value)
                        .or_else(|| self.method_value(value).then(|| (**value).clone()));
                    if known.as_ref().is_some_and(|known| {
                        (!matches!(known.kind, K::MakeClosure { .. }) || self.method_value(known))
                            && Self::discardable(known, body)
                    }) {
                        return Some((**body).clone());
                    }
                }
                let usage = if interface(&value.ty) { Usage::Project } else { Usage::Call };
                if usage == Usage::Call && !matches!(value.ty, Ty::Arrow(..)) {
                    return None;
                }
                if !ground_type(&value.ty)
                    || !only_uses(body, &FxHashSet::from_iter([*local]), usage)
                {
                    return None;
                }
                if usage == Usage::Project
                    && let Some(expanded) = self.expand_receiver(value)
                {
                    return Some(CExpr::new(
                        K::Let { local: *local, value: Box::new(expanded), body: body.clone() },
                        e.ty.clone(),
                    ));
                }
                let count = if usage == Usage::Project {
                    self.receiver_count(value)
                } else {
                    self.callable_count(value)
                }?;
                if count == 0 {
                    return None;
                }
                match &value.kind {
                    K::Let { local: inner, value: init, body: result } => Some(CExpr::new(
                        K::Let {
                            local: *inner,
                            value: init.clone(),
                            body: Box::new(CExpr::new(
                                K::Let { local: *local, value: result.clone(), body: body.clone() },
                                e.ty.clone(),
                            )),
                        },
                        e.ty.clone(),
                    )),
                    K::If { cond, then, els } => {
                        let cost = node_count(body);
                        if cost > MAX_CONTINUATION || !self.spend(cost) {
                            return None;
                        }
                        let left = fresh_local(&mut self.next);
                        let right = fresh_local(&mut self.next);
                        let left_body = self.fresh_copy(body, Some((*local, left)));
                        let right_body = self.fresh_copy(body, Some((*local, right)));
                        Some(CExpr::new(
                            K::If {
                                cond: cond.clone(),
                                then: Box::new(CExpr::new(
                                    K::Let {
                                        local: left,
                                        value: then.clone(),
                                        body: Box::new(left_body),
                                    },
                                    e.ty.clone(),
                                )),
                                els: Box::new(CExpr::new(
                                    K::Let {
                                        local: right,
                                        value: els.clone(),
                                        body: Box::new(right_body),
                                    },
                                    e.ty.clone(),
                                )),
                            },
                            e.ty.clone(),
                        ))
                    }
                    K::MakeData { tag, args, reuse, scalars, niche } if !self.stable(value) => {
                        let locals: Vec<_> =
                            args.iter().map(|_| fresh_local(&mut self.next)).collect();
                        let fields = locals
                            .iter()
                            .zip(args)
                            .map(|(&l, a)| CExpr::new(K::Local(l), a.ty.clone()))
                            .collect();
                        let value = CExpr::new(
                            K::MakeData {
                                tag: *tag,
                                args: fields,
                                reuse: *reuse,
                                scalars: *scalars,
                                niche: *niche,
                            },
                            value.ty.clone(),
                        );
                        let mut result = CExpr::new(
                            K::Let { local: *local, value: Box::new(value), body: body.clone() },
                            e.ty.clone(),
                        );
                        for (&local, value) in locals.iter().zip(args).rev() {
                            result = CExpr::new(
                                K::Let {
                                    local,
                                    value: Box::new(value.clone()),
                                    body: Box::new(result),
                                },
                                e.ty.clone(),
                            );
                        }
                        Some(result)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Freshen bound locals in a duplicated continuation, preserving free values
    /// and the identities of its lifted functions (capture operands are remapped).
    fn fresh_copy(&mut self, e: &CExpr, replacement: Option<(LocalId, LocalId)>) -> CExpr {
        let mut locals = FxHashMap::default();
        let mut bound = FxHashSet::default();
        walk(e, &mut |e| match &e.kind {
            K::Local(l) => {
                locals.entry(*l).or_insert(*l);
            }
            K::MakeClosure { captures, .. } => {
                for &l in captures {
                    locals.entry(l).or_insert(l);
                }
            }
            K::Let { local, .. } => {
                bound.insert(*local);
            }
            K::LetMany { locals: ls, .. } => {
                bound.extend(ls);
            }
            K::DataField { index: FieldIndex::Dyn { evidence, .. }, .. } => {
                locals.entry(*evidence).or_insert(*evidence);
            }
            _ => {}
        });
        // Deterministic local allocation, independent of hash-table iteration.
        let mut bound: Vec<_> = bound.into_iter().collect();
        bound.sort_by_key(|l| l.index());
        for l in bound {
            locals.insert(l, fresh_local(&mut self.next));
        }
        if let Some((from, to)) = replacement {
            locals.insert(from, to);
        }
        remap_expr(e, &mut locals, &FxHashMap::default(), &mut self.next)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Usage {
    Project,
    Call,
}

fn only_uses(e: &CExpr, aliases: &FxHashSet<LocalId>, usage: Usage) -> bool {
    match &e.kind {
        K::Local(l) => return !aliases.contains(l),
        K::MakeClosure { captures, .. } => return captures.iter().all(|l| !aliases.contains(l)),
        K::DataField { base, index: FieldIndex::Const(_), niche: None, .. }
            if usage == Usage::Project
                && matches!(base.kind, K::Local(l) if aliases.contains(&l)) =>
        {
            return true;
        }
        K::App { func, args, .. }
            if usage == Usage::Call && matches!(func.kind, K::Local(l) if aliases.contains(&l)) =>
        {
            return args.iter().all(|a| only_uses(a, aliases, usage));
        }
        K::Let { local, value, body } if matches!(value.kind, K::Local(l) if aliases.contains(&l)) =>
        {
            let mut aliases = aliases.clone();
            aliases.insert(*local);
            return only_uses(body, &aliases, usage);
        }
        _ => {}
    }
    let mut valid = true;
    children(e, &mut |child| {
        if valid {
            valid = only_uses(child, aliases, usage);
        }
    });
    valid
}

fn uses(e: &CExpr, local: LocalId) -> bool {
    let mut found = false;
    walk(e, &mut |e| {
        found |= match &e.kind {
            K::Local(l) => *l == local,
            K::MakeClosure { captures, .. } => captures.contains(&local),
            K::DataField { index: FieldIndex::Dyn { evidence, .. }, .. } => *evidence == local,
            _ => false,
        }
    });
    found
}

fn walk(e: &CExpr, visit: &mut impl FnMut(&CExpr)) {
    visit(e);
    children(e, &mut |child| walk(child, visit));
}

fn children(e: &CExpr, visit: &mut impl FnMut(&CExpr)) {
    match &e.kind {
        K::Prim { args, .. }
        | K::Foreign { args, .. }
        | K::MakeData { args, .. }
        | K::Recur { args }
        | K::Spread { components: args } => args.iter().for_each(visit),
        K::App { func, args, .. } => {
            visit(func);
            args.iter().for_each(visit);
        }
        K::If { cond, then, els } => {
            visit(cond);
            visit(then);
            visit(els);
        }
        K::Let { value, body, .. }
        | K::LetMany { value, body, .. }
        | K::Reset { value, body, .. } => {
            visit(value);
            visit(body);
        }
        K::DataField { base, .. } | K::DataTag { base, .. } | K::HoleClose { base, .. } => {
            visit(base)
        }
        K::FreeReuse { body, .. }
        | K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::Join { body, .. }
        | K::HoleStart { body, .. } => visit(body),
        K::HoleFill { cell, .. } => visit(cell),
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(source: &str) -> (fai_db::FaiDatabase, SourceFile) {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source.into());
        let file = db.source_file(id).unwrap();
        assert!(fai_types::check_file::accumulated::<Diag>(&db, file).is_empty());
        (db, file)
    }

    fn reduced(source: &str) -> String {
        let (db, file) = database(source);
        crate::pretty_def(&super::super::simplified(&db, file, Symbol::intern("run")))
    }

    #[test]
    fn scorer_choice_reduces_to_direct_arithmetic() {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source(
            "InterfaceDispatch.fai".into(),
            include_str!("../../../samples/algorithms/InterfaceDispatch.fai").into(),
        );
        let result =
            super::super::simplified(&db, db.source_file(id).unwrap(), Symbol::intern("loop"));
        let text = crate::pretty_def(&result);
        assert!(
            !text.contains("@pick") && !text.contains("(field ") && !text.contains("closure"),
            "{text}"
        );
    }

    #[test]
    fn local_interface_aliases_share_the_captured_value() {
        let text = reduced(
            "module M\ninterface S = score : Int -> Int\nlet run x =\n  let d = { S with score y = x + y }\n  let alias = d\n  d.score x + alias.score (x + 1)\n",
        );
        assert!(
            !text.contains("(field ") && !text.contains("(data ") && !text.contains("closure"),
            "{text}"
        );
    }

    #[test]
    fn unknown_dictionary_stays_dynamic() {
        let text = reduced(
            "module M\ninterface S = score : Int -> Int\nrun : S -> Int -> Int\nlet run d x = d.score x\n",
        );
        assert!(text.contains("(field "), "{text}");
    }

    #[test]
    fn same_spelled_record_field_is_not_an_interface_method() {
        let text =
            reduced("module M\nlet run x =\n  let d = { score = fun y -> x + y }\n  d.score x\n");
        assert!(text.contains("(field "), "{text}");
    }

    #[test]
    fn generic_dictionary_keeps_its_representation_boundary() {
        let text = reduced(
            "module M\ninterface Box 'a = get : Unit -> 'a\nlet run value =\n  let box = { Box with get u = value }\n  box.get ()\n",
        );
        assert!(text.contains("(field "), "{text}");
    }

    #[test]
    fn escaping_choice_stays_materialized() {
        let text = reduced(
            "module M\ninterface S = score : Int -> Int\nlet run x =\n  let d = if x = 0 then { S with score y = y + 1 } else { S with score y = y - 1 }\n  (d, d.score x)\n",
        );
        assert!(text.contains("(data ") && text.contains("(field "), "{text}");
    }

    #[test]
    fn too_many_choices_keep_the_shared_factory() {
        let source = "module M\ninterface S = score : Int -> Int\nlet pick x =\n  if x = 0 then { S with score y = y }\n  else if x = 1 then { S with score y = y + 1 }\n  else if x = 2 then { S with score y = y + 2 }\n  else if x = 3 then { S with score y = y + 3 }\n  else { S with score y = y + 4 }\nlet run x = (pick x).score x\n";
        assert!(reduced(source).contains("@pick"));
    }

    #[test]
    fn large_continuation_is_not_duplicated() {
        let uses = (0..80).map(|i| format!("d.score {i}")).collect::<Vec<_>>().join(", ");
        let text = reduced(&format!(
            "module M\ninterface S = score : Int -> Int\nlet run x =\n  let d = if x = 0 then {{ S with score y = y + 1 }} else {{ S with score y = y - 1 }}\n  ({uses})\n"
        ));
        assert!(text.contains("(field "), "{text}");
    }

    #[test]
    fn an_ineligible_factory_body_edit_cuts_off_before_its_caller() {
        let source = "module M\ninterface S = score : Int -> Int\nkeep : S -> S\nlet keep s = s\nrun : S -> Int -> Int\nlet run s x = (keep s).score x\n";
        let (mut db, file) = database(source);
        let before = super::super::simplified(&db, file, Symbol::intern("run"));
        db.enable_event_log();
        db.add_source(
            "M.fai".into(),
            source.replace("let keep s = s", "let keep s = if true then s else s"),
        );
        assert_eq!(before, super::super::simplified(&db, file, Symbol::intern("run")));
        let events = db.take_events();
        assert_eq!(events.iter().filter(|e| e.contains("simplified")).count(), 1, "{events:?}");
    }

    #[test]
    fn unused_captured_closure_retains_its_capture_at_the_binding() {
        let text = reduced(
            "module M\nrun : String -> Int\nlet run resource =\n  let unused = fun x -> resource\n  1\n",
        );
        assert!(text.contains("closure"), "the last capture use must not move earlier: {text}");
    }

    #[test]
    fn cross_file_factory_bodies_remain_firewalled() {
        let factory = "module Factory\npublic interface S = score : Int -> Int\npublic make : Int -> S\nlet make n = { S with score x = x + n + 1 }\n";
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        db.add_source("Factory.fai".into(), factory.into());
        let id = db
            .add_source("M.fai".into(), "module M\nlet run x = (Factory.make x).score x\n".into());
        let file = db.source_file(id).unwrap();
        let before = super::super::simplified(&db, file, Symbol::intern("run"));
        assert!(crate::pretty_def(&before).contains("(field "));
        db.enable_event_log();
        db.add_source("Factory.fai".into(), factory.replace("n + 1", "n + 2"));
        assert_eq!(before, super::super::simplified(&db, file, Symbol::intern("run")));
        assert!(!db.take_events().iter().any(|e| e.contains("simplified")));
    }
}
