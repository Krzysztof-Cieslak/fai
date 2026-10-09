//! Excludes cells that a live alias or projection parent keeps shared.

use fai_core::ir::{CExpr, ExprKind as K, Prim};
use fai_resolve::LocalId;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{Locals, reuse_sig::e_children};

/// Candidate cells, simple pointer aliases, and the parents of projections.
pub(crate) struct ReuseCandidates {
    data: Locals,
    roots: FxHashMap<LocalId, LocalId>,
    aliased: FxHashSet<LocalId>,
    owners: FxHashMap<LocalId, LocalId>,
}

impl ReuseCandidates {
    pub(crate) fn new(body: &CExpr, data: Locals) -> Self {
        let mut result = Self {
            data,
            roots: FxHashMap::default(),
            aliased: FxHashSet::default(),
            owners: FxHashMap::default(),
        };
        result.collect(body);
        result
    }

    fn root(&self, local: LocalId) -> LocalId {
        self.roots.get(&local).copied().unwrap_or(local)
    }

    fn collect(&mut self, expression: &CExpr) {
        if let K::Let { local, value, body } = &expression.kind {
            self.collect(value);
            let mut value = value.as_ref();
            while let K::Dup { body, .. } | K::Drop { body, .. } = &value.kind {
                value = body;
            }
            match &value.kind {
                K::Local(other) => {
                    let root = self.root(*other);
                    self.roots.insert(*local, root);
                    self.aliased.insert(root);
                }
                K::DataField { base, .. } => {
                    if let K::Local(parent) = base.kind {
                        self.owners.insert(*local, self.root(parent));
                    }
                }
                K::Prim { op: Prim::ArrayGet, args } => {
                    if let Some(K::Local(parent)) = args.first().map(|arg| &arg.kind) {
                        self.owners.insert(*local, self.root(*parent));
                    }
                }
                _ => {}
            }
            self.collect(body);
        } else {
            e_children(expression, &mut |child| self.collect(child));
        }
    }

    pub(crate) fn can_reuse(&self, local: LocalId, continuation: &CExpr) -> bool {
        if !self.data.contains(&local) {
            return false;
        }
        let mut root = self.root(local);
        // A bounded scan adds constant work per candidate. If it cannot prove a
        // live retainer, keep the ordinary runtime uniqueness check.
        let mut budget = 512;
        if self.aliased.contains(&root) && self.mentions(continuation, root, &mut budget) {
            return false;
        }
        for _ in 0..32 {
            let Some(&parent) = self.owners.get(&root) else { break };
            if self.mentions(continuation, parent, &mut budget) {
                return false;
            }
            root = parent;
        }
        true
    }

    fn mentions(&self, expression: &CExpr, target: LocalId, budget: &mut usize) -> bool {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        match &expression.kind {
            K::Local(local) | K::Drop { local, .. } | K::Dup { local, .. }
                if self.root(*local) == target =>
            {
                return true;
            }
            K::MakeClosure { captures, .. }
                if captures.iter().any(|local| self.root(*local) == target) =>
            {
                return true;
            }
            _ => {}
        }
        let mut found = false;
        let mut stop = |child: &CExpr| {
            if *budget > 0 {
                found = self.mentions(child, target, budget);
            }
            found || *budget == 0
        };
        // Short-circuit sibling traversal as well as recursion, so one very
        // wide constructor cannot defeat the per-candidate work budget.
        match &expression.kind {
            K::Prim { args, .. }
            | K::Foreign { args, .. }
            | K::MakeData { args, .. }
            | K::Recur { args }
            | K::Spread { components: args } => {
                args.iter().any(&mut stop);
            }
            K::App { func, args, .. } => {
                if !stop(func) {
                    args.iter().any(&mut stop);
                }
            }
            K::If { cond, then, els } => {
                [cond.as_ref(), then, els].into_iter().any(&mut stop);
            }
            K::Let { value, body, .. }
            | K::Reset { value, body, .. }
            | K::LetMany { value, body, .. } => {
                [value.as_ref(), body].into_iter().any(&mut stop);
            }
            K::FreeReuse { body, .. }
            | K::Dup { body, .. }
            | K::Drop { body, .. }
            | K::Join { body, .. }
            | K::HoleStart { body, .. } => {
                stop(body);
            }
            K::DataTag { base, .. } | K::DataField { base, .. } | K::HoleClose { base, .. } => {
                stop(base);
            }
            K::HoleFill { cell, .. } => {
                stop(cell);
            }
            K::Local(_) | K::Lit(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fai_core::ir::{ClosureAlloc, FieldIndex, FnId, Lit};
    use fai_types::Ty;

    fn local(id: LocalId) -> CExpr {
        CExpr::new(K::Local(id), Ty::Error)
    }

    fn fixture() -> (ReuseCandidates, LocalId, LocalId, LocalId) {
        let parent = LocalId::from_index(0);
        let alias = LocalId::from_index(1);
        let child = LocalId::from_index(2);
        let projection = CExpr::new(
            K::DataField {
                base: Box::new(local(alias)),
                index: FieldIndex::Const(0),
                scalar: false,
                niche: None,
            },
            Ty::Error,
        );
        let body = CExpr::new(
            K::Let {
                local: alias,
                value: Box::new(local(parent)),
                body: Box::new(CExpr::new(
                    K::Let {
                        local: child,
                        value: Box::new(projection),
                        body: Box::new(local(child)),
                    },
                    Ty::Error,
                )),
            },
            Ty::Error,
        );
        (ReuseCandidates::new(&body, Locals::from_iter([child])), parent, alias, child)
    }

    #[test]
    fn a_live_parent_blocks_reuse_of_its_projected_child() {
        let (candidates, parent, _, child) = fixture();
        assert!(!candidates.can_reuse(child, &local(parent)));
    }

    #[test]
    fn a_live_parent_alias_also_blocks_child_reuse() {
        let (candidates, _, alias, child) = fixture();
        assert!(!candidates.can_reuse(child, &local(alias)));
    }

    #[test]
    fn a_dead_parent_leaves_its_child_eligible() {
        let (candidates, _, _, child) = fixture();
        assert!(candidates.can_reuse(child, &CExpr::new(K::Lit(Lit::Unit), Ty::Unit)));
    }

    #[test]
    fn a_future_capture_keeps_the_parent_alive() {
        let (candidates, parent, _, child) = fixture();
        let closure = CExpr::new(
            K::MakeClosure { func: FnId(1), captures: vec![parent], alloc: ClosureAlloc::Heap },
            Ty::Error,
        );
        assert!(!candidates.can_reuse(child, &closure));
    }

    #[test]
    fn an_alias_is_not_a_dead_cell_while_the_original_remains_live() {
        let (mut candidates, parent, alias, _) = fixture();
        candidates.data.insert(alias);
        assert!(!candidates.can_reuse(alias, &local(parent)));
    }
}
