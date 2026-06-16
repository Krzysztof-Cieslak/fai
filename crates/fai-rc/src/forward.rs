//! Inter-procedural reuse-token forwarding: the emit-ready lowering.
//!
//! [`crate::rc`] places reuse *within one function body* — a dead cell is reset
//! and recycled into a same-size construction on the same path, or freed where no
//! construction follows. [`rc_emit`] layers inter-procedural forwarding on top of
//! that baseline, producing the lowering code generation actually emits:
//!
//! * **Source side.** A freed reuse token (a [`K::FreeReuse`] the baseline emitted
//!   where it found no local construction) is **forwarded** into a saturated direct
//!   call on its path whose callee accepts a token (its
//!   [`crate::reuse_signature`]), by recording the token in that call's
//!   [`K::App::reuse`]. The callee recycles it; a path that reaches no such call
//!   keeps the free.
//! * **Sink side.** A function whose reuse signature is non-empty gets a
//!   token-taking specialized entry ([`LoweredDef::reuse_entry`]): the primary
//!   entry's body with leading reuse-token parameters threaded into its leftover
//!   sinks (constructions the function's own resets did not fill, or forwardable
//!   calls), freeing any token a path cannot place.
//!
//! Threading reuses the same first-sink-per-path discipline as the intra-function
//! reuse pass, generalized so a sink is *either* a construction *or* a forwardable
//! call. The result is verified by the same reference-count oracle (a forwarded
//! token is consumed by the call; an entry's token parameters are linear, consumed
//! once per path). Reading [`crate::reuse_signature`] makes this the firewall
//! seam: a caller's emitted code depends on its callees' reuse signatures.

use std::sync::Arc;

use fai_core::ir::{CExpr, CoreFn, ExprKind as K, LoweredDef};
use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId};
use fai_syntax::Symbol;

use crate::reuse_sig::forward_target;
use crate::{
    attach_reuse, free_reuse_, fresh, is_reuse_target, next_free_local, niche_wrapper_free,
    reuse_signature,
};

/// The emit-ready lowering of `name`: the reference-counted body with reuse tokens
/// forwarded into accepting callees, plus a token-taking specialized entry when
/// `name` accepts forwarded tokens. Code generation consumes this rather than the
/// intra-function [`crate::rc`] baseline.
#[salsa::tracked]
pub fn rc_emit(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<LoweredDef> {
    let base = crate::rc(db, file, name);
    let self_def = base.def;
    let mut next = next_free_local(&base);

    // Source side: forward each function's freed tokens into accepting calls.
    let fns: Vec<CoreFn> = base
        .fns
        .iter()
        .map(|f| CoreFn {
            params: f.params.clone(),
            captures: f.captures.clone(),
            body: forward_pass(db, self_def, f.body.clone()),
        })
        .collect();

    // Sink side: a token-taking entry when this function accepts forwarded tokens.
    let sig = reuse_signature(db, file, name);
    let reuse_entry =
        (!sig.is_empty()).then(|| build_reuse_entry(db, self_def, &fns[0], sig.len(), &mut next));

    if fns == base.fns && reuse_entry.is_none() {
        // Nothing forwarded and no specialized entry: reuse the baseline `Arc` so
        // salsa's pointer-equality fast path gives O(1) early cutoff.
        return base;
    }
    Arc::new(LoweredDef {
        def: base.def,
        fns,
        entry_borrowed: base.entry_borrowed.clone(),
        reuse_entry,
        entry_spread_params: base.entry_spread_params.clone(),
    })
}

/// Rewrites a body so every freed reuse token that can reach an accepting call on
/// its path is forwarded there instead of freed.
fn forward_pass(db: &dyn Db, self_def: DefId, e: CExpr) -> CExpr {
    let CExpr { kind, ty } = e;
    let sub = |e: CExpr| forward_pass(db, self_def, e);
    match kind {
        // A freed token: thread it into the (already forward-passed) continuation,
        // landing it in an accepting call if one is reachable, else re-freeing it.
        K::FreeReuse { token, body } => thread_token(db, self_def, sub(*body), token),
        K::Let { local, value, body } => CExpr::new(
            K::Let { local, value: Box::new(sub(*value)), body: Box::new(sub(*body)) },
            ty,
        ),
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond: Box::new(sub(*cond)),
                then: Box::new(sub(*then)),
                els: Box::new(sub(*els)),
            },
            ty,
        ),
        K::Reset { value, token, body } => CExpr::new(
            K::Reset { value: Box::new(sub(*value)), token, body: Box::new(sub(*body)) },
            ty,
        ),
        K::Dup { local, body } => CExpr::new(K::Dup { local, body: Box::new(sub(*body)) }, ty),
        K::Drop { local, body } => CExpr::new(K::Drop { local, body: Box::new(sub(*body)) }, ty),
        K::Prim { op, args } => {
            CExpr::new(K::Prim { op, args: args.into_iter().map(sub).collect() }, ty)
        }
        K::Foreign { symbol, args, marshalled } => CExpr::new(
            K::Foreign { symbol, args: args.into_iter().map(sub).collect(), marshalled },
            ty,
        ),
        K::MakeData { tag, args, reuse, scalars, niche } => CExpr::new(
            K::MakeData { tag, args: args.into_iter().map(sub).collect(), reuse, scalars, niche },
            ty,
        ),
        K::App { func, args, reuse, alloc } => CExpr::new(
            K::App {
                func: Box::new(sub(*func)),
                args: args.into_iter().map(sub).collect(),
                reuse,
                alloc,
            },
            ty,
        ),
        K::DataTag { base, niche } => {
            CExpr::new(K::DataTag { base: Box::new(sub(*base)), niche }, ty)
        }
        K::DataField { base, index, scalar, niche } => {
            CExpr::new(K::DataField { base: Box::new(sub(*base)), index, scalar, niche }, ty)
        }
        // Spread/LetMany carry no reuse tokens; rebuild with forwarded children.
        K::Spread { components } => {
            CExpr::new(K::Spread { components: components.into_iter().map(sub).collect() }, ty)
        }
        K::LetMany { locals, value, body } => CExpr::new(
            K::LetMany { locals, value: Box::new(sub(*value)), body: Box::new(sub(*body)) },
            ty,
        ),
        K::Join { params, body } => CExpr::new(K::Join { params, body: Box::new(sub(*body)) }, ty),
        K::Recur { args } => CExpr::new(K::Recur { args: args.into_iter().map(sub).collect() }, ty),
        K::HoleStart { hole, body } => {
            CExpr::new(K::HoleStart { hole, body: Box::new(sub(*body)) }, ty)
        }
        K::HoleFill { hole, cell, field } => {
            CExpr::new(K::HoleFill { hole, cell: Box::new(sub(*cell)), field }, ty)
        }
        K::HoleClose { hole, base } => {
            CExpr::new(K::HoleClose { hole, base: Box::new(sub(*base)) }, ty)
        }
        kind @ (K::Local(_) | K::Lit(_) | K::Global(_) | K::MakeClosure { .. } | K::Error) => {
            CExpr::new(kind, ty)
        }
    }
}

/// Threads `token` to the first sink on every path of `e` — a construction with no
/// reuse token yet (filled in place), or a forwardable saturated direct call with
/// a free token slot (recorded in its [`K::App::reuse`]) — and frees it on any
/// path that reaches no sink. Generalizes the intra-function reuse pass's
/// `thread_or_free` so a sink may be a call as well as a construction.
fn thread_token(db: &dyn Db, self_def: DefId, e: CExpr, token: LocalId) -> CExpr {
    let CExpr { kind, ty } = e;
    let rebuild_ty = ty.clone();
    match kind {
        // A construction with no token yet: recycle it in place. A wrapper-free
        // niche `Some` allocates no cell, so it is not a sink (mirrors the
        // intra-function `is_reuse_target`); the token frees on this path instead.
        K::MakeData { tag, args, reuse: None, scalars, niche }
            if !args.is_empty() && !niche_wrapper_free(niche) =>
        {
            CExpr::new(K::MakeData { tag, args, reuse: Some(token), scalars, niche }, ty)
        }
        // A forwardable call with a free slot: forward the token into it. The reuse
        // list has one entry per callee token slot (`None` = a null-token pad); the
        // token lands in the first free slot, leaving the rest padded.
        K::App { func, args, mut reuse, alloc } => {
            let slots = forward_target(db, self_def, &func, args.len()).map_or(0, |s| s.len());
            if reuse.is_empty() {
                reuse = vec![None; slots];
            }
            match reuse.iter().position(Option::is_none) {
                Some(slot) => {
                    reuse[slot] = Some(token);
                    CExpr::new(K::App { func, args, reuse, alloc }, ty)
                }
                None => free_reuse_(token, CExpr::new(K::App { func, args, reuse, alloc }, ty)),
            }
        }
        K::Let { local, value, body } => {
            // A reuse-target construction (or forwardable call) bound in a `let` is
            // a sink; otherwise thread on to the body.
            if is_reuse_target(&value) {
                let value = Box::new(attach_reuse(*value, token));
                CExpr::new(K::Let { local, value, body }, ty)
            } else if is_forwardable_call(db, self_def, &value) {
                let value = Box::new(thread_token(db, self_def, *value, token));
                CExpr::new(K::Let { local, value, body }, ty)
            } else {
                let body = Box::new(thread_token(db, self_def, *body, token));
                CExpr::new(K::Let { local, value, body }, ty)
            }
        }
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond,
                then: Box::new(thread_token(db, self_def, *then, token)),
                els: Box::new(thread_token(db, self_def, *els, token)),
            },
            ty,
        ),
        K::Dup { local, body } => CExpr::new(
            K::Dup { local, body: Box::new(thread_token(db, self_def, *body, token)) },
            ty,
        ),
        K::Drop { local, body } => CExpr::new(
            K::Drop { local, body: Box::new(thread_token(db, self_def, *body, token)) },
            ty,
        ),
        K::Reset { value, token: tok, body } => CExpr::new(
            K::Reset {
                value,
                token: tok,
                body: Box::new(thread_token(db, self_def, *body, token)),
            },
            ty,
        ),
        // A leaf that is no sink: free the token here.
        other => free_reuse_(token, CExpr::new(other, rebuild_ty)),
    }
}

/// Whether `e` is a saturated direct call to a callee that accepts a token.
fn is_forwardable_call(db: &dyn Db, self_def: DefId, e: &CExpr) -> bool {
    if let K::App { func, args, reuse, .. } = &e.kind {
        let slots = forward_target(db, self_def, func, args.len()).map_or(0, |s| s.len());
        return reuse.len() < slots;
    }
    false
}

/// Builds the token-taking specialized entry: the primary entry's body with `k`
/// fresh leading reuse-token parameters, each threaded into a leftover sink (or
/// freed where a path has none).
fn build_reuse_entry(
    db: &dyn Db,
    self_def: DefId,
    primary: &CoreFn,
    k: usize,
    next: &mut usize,
) -> CoreFn {
    let tokens: Vec<LocalId> = (0..k).map(|_| fresh(next)).collect();
    let mut body = primary.body.clone();
    for &t in &tokens {
        body = thread_token(db, self_def, body, t);
    }
    // The reuse tokens are leading parameters (not reference-counted); the source
    // parameters follow, unchanged.
    let mut params = tokens;
    params.extend(primary.params.iter().copied());
    CoreFn { params, captures: primary.captures.clone(), body }
}
