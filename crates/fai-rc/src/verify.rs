//! An abstract reference-count interpreter that checks soundness of a
//! reference-counted definition.
//!
//! [`check_rc`] walks each function on every path, modeling ownership
//! (owned-live / consumed / dropped), borrowing (projection bases and offset
//! evidence read without consuming), captures (borrowed, never dropped), and the
//! reuse token threaded by `Reset`/`MakeData`. It verifies that every owned
//! binding is consumed-or-dropped exactly once per path, that no value is used
//! after release or dropped twice, that captures are never dropped, and that the
//! two arms of an `if` leave a consistent reference state. It returns the first
//! violation it finds as a human-readable message rather than panicking, so it is
//! usable both as a test oracle here and over generated/end-to-end programs.

use std::collections::HashMap;

use fai_core::ir::{CExpr, ExprKind, FieldIndex, LoweredDef};
use fai_resolve::{DefId, LocalId};

/// Verifies the reference-count soundness invariants on every function of `def`.
///
/// `arg_borrows(callee, nargs)` reports the callee's per-argument borrow flags for
/// a saturated direct call, mirroring what reference counting used to decide
/// whether each argument is consumed (owned) or merely lent (borrowed).
///
/// Returns `Ok(())` when sound, or `Err(message)` describing the first violation.
pub fn check_rc(
    def: &LoweredDef,
    arg_borrows: &dyn Fn(DefId, usize) -> Vec<bool>,
) -> Result<(), String> {
    for (i, f) in def.fns.iter().enumerate() {
        // Borrowed slots (captures, and the entry's borrowed parameters) are read
        // but never owned/dropped by this function.
        let mut captures: std::collections::HashSet<LocalId> = f.captures.iter().copied().collect();
        let mut refs: HashMap<LocalId, i64> = HashMap::new();
        for (pos, &p) in f.params.iter().enumerate() {
            if i == 0 && def.entry_param_borrowed(pos) {
                captures.insert(p); // borrowed: lent by the caller, not owned here
            } else {
                refs.insert(p, 1);
            }
        }
        let mut ck = Checker { captures: &captures, fn_index: i, arg_borrows };
        ck.eval(&f.body, &mut refs)?;
        for (l, n) in &refs {
            if *n != 0 {
                return Err(format!("fn{i}: local %{} left with {n} refs at exit", l.index()));
            }
        }
    }
    Ok(())
}

struct Checker<'a> {
    captures: &'a std::collections::HashSet<LocalId>,
    fn_index: usize,
    arg_borrows: &'a dyn Fn(DefId, usize) -> Vec<bool>,
}

impl Checker<'_> {
    fn owned(&self, x: LocalId) -> bool {
        !self.captures.contains(&x)
    }

    /// Consumes one owned reference of `x` (no-op for a borrowed capture).
    fn consume(&self, x: LocalId, refs: &mut HashMap<LocalId, i64>) -> Result<(), String> {
        if !self.owned(x) {
            return Ok(());
        }
        let Some(n) = refs.get_mut(&x) else {
            return Err(format!("fn{}: consume of unbound/owned %{}", self.fn_index, x.index()));
        };
        if *n < 1 {
            return Err(format!("fn{}: use of released %{}", self.fn_index, x.index()));
        }
        *n -= 1;
        Ok(())
    }

    /// An operation operand: a borrowed atom is read; otherwise it is consumed.
    fn operand(
        &mut self,
        a: &CExpr,
        is_borrow: bool,
        refs: &mut HashMap<LocalId, i64>,
    ) -> Result<(), String> {
        if is_borrow && let ExprKind::Local(x) = a.kind {
            self.borrow(x, refs)
        } else {
            self.eval(a, refs)
        }
    }

    /// Reads `x` without consuming it (borrow); the value must still be alive.
    fn borrow(&self, x: LocalId, refs: &HashMap<LocalId, i64>) -> Result<(), String> {
        if !self.owned(x) {
            return Ok(());
        }
        let n = refs.get(&x).copied().unwrap_or(0);
        if n < 1 {
            return Err(format!("fn{}: borrow of released/unbound %{}", self.fn_index, x.index()));
        }
        Ok(())
    }

    fn eval(&mut self, e: &CExpr, refs: &mut HashMap<LocalId, i64>) -> Result<(), String> {
        match &e.kind {
            ExprKind::Lit(_) | ExprKind::Global(_) | ExprKind::Error => {}
            ExprKind::Local(x) => self.consume(*x, refs)?,
            ExprKind::Prim { op, args } => {
                let borrows = crate::prim_borrows(*op, args);
                for (i, a) in args.iter().enumerate() {
                    self.operand(a, borrows.get(i).copied().unwrap_or(false), refs)?;
                }
            }
            ExprKind::MakeData { args, reuse, .. } => {
                for a in args {
                    self.eval(a, refs)?;
                }
                if let Some(t) = reuse {
                    self.consume(*t, refs)?; // the reuse token is consumed here
                }
            }
            ExprKind::App { func, args } => {
                self.eval(func, refs)?;
                let borrows = match &func.kind {
                    ExprKind::Global(def) => (self.arg_borrows)(*def, args.len()),
                    _ => Vec::new(),
                };
                for (i, a) in args.iter().enumerate() {
                    self.operand(a, borrows.get(i).copied().unwrap_or(false), refs)?;
                }
            }
            ExprKind::MakeClosure { captures, .. } => {
                for &c in captures {
                    self.consume(c, refs)?;
                }
            }
            ExprKind::DataTag(base) => self.borrow_atom(base, refs)?,
            ExprKind::DataField { base, index, .. } => {
                self.borrow_atom(base, refs)?;
                if let FieldIndex::Dyn { evidence, .. } = index {
                    self.borrow(*evidence, refs)?;
                }
            }
            ExprKind::If { cond, then, els } => {
                // The condition (an immediate Bool) is consumed by the test.
                self.eval(cond, refs)?;
                let mut t = refs.clone();
                let mut e2 = refs.clone();
                self.eval(then, &mut t)?;
                self.eval(els, &mut e2)?;
                if t != e2 {
                    return Err(format!(
                        "fn{}: branches leave inconsistent reference state",
                        self.fn_index
                    ));
                }
                *refs = t;
            }
            ExprKind::Let { local, value, body } => {
                self.eval(value, refs)?;
                if refs.insert(*local, 1).is_some() {
                    return Err(format!("fn{}: rebound %{}", self.fn_index, local.index()));
                }
                self.eval(body, refs)?;
                let n = refs.remove(local).unwrap_or(0);
                if n != 0 {
                    return Err(format!(
                        "fn{}: let %{} left with {n} refs",
                        self.fn_index,
                        local.index()
                    ));
                }
            }
            ExprKind::Dup { local, body } => {
                if self.owned(*local) {
                    let Some(n) = refs.get_mut(local) else {
                        return Err(format!(
                            "fn{}: dup of unbound %{}",
                            self.fn_index,
                            local.index()
                        ));
                    };
                    if *n < 1 {
                        return Err(format!(
                            "fn{}: dup of released %{}",
                            self.fn_index,
                            local.index()
                        ));
                    }
                    *n += 1;
                }
                self.eval(body, refs)?;
            }
            ExprKind::Drop { local, body } => {
                if !self.owned(*local) {
                    return Err(format!(
                        "fn{}: drop of captured %{}",
                        self.fn_index,
                        local.index()
                    ));
                }
                self.consume(*local, refs)?;
                self.eval(body, refs)?;
            }
            ExprKind::Reset { value, token, body } => {
                self.eval(value, refs)?;
                if refs.insert(*token, 1).is_some() {
                    return Err(format!(
                        "fn{}: rebound reuse token %{}",
                        self.fn_index,
                        token.index()
                    ));
                }
                self.eval(body, refs)?;
                let n = refs.remove(token).unwrap_or(0);
                if n != 0 {
                    return Err(format!(
                        "fn{}: reuse token %{} left with {n} refs",
                        self.fn_index,
                        token.index()
                    ));
                }
            }
            // Freeing a token consumes its one reference (like a `MakeData` that
            // reuses it would), so a path that frees and a path that reuses leave a
            // consistent state.
            ExprKind::FreeReuse { token, body } => {
                self.consume(*token, refs)?;
                self.eval(body, refs)?;
            }
            // The loop's carried locals are already bound (function parameters and,
            // for a destination-passing loop, the hole). Each `Recur`/`HoleClose`
            // along a path consumes them, so loop balance falls out of the existing
            // per-path consistency check; the body is evaluated once here.
            ExprKind::Join { body, .. } => self.eval(body, refs)?,
            // A tail back-edge consumes the new loop-carried values (the next
            // iteration's parameters). It is terminal.
            ExprKind::Recur { args } => {
                for a in args {
                    self.eval(a, refs)?;
                }
            }
            // The hole is a linear token: born here (like a reuse token), advanced
            // by `HoleFill`, consumed by `HoleClose` — exactly once per path.
            ExprKind::HoleStart { hole, body } => {
                if refs.insert(*hole, 1).is_some() {
                    return Err(format!(
                        "fn{}: rebound hole token %{}",
                        self.fn_index,
                        hole.index()
                    ));
                }
                self.eval(body, refs)?;
                let n = refs.remove(hole).unwrap_or(0);
                if n != 0 {
                    return Err(format!(
                        "fn{}: hole token %{} left with {n} refs",
                        self.fn_index,
                        hole.index()
                    ));
                }
            }
            // Linking a cell consumes the current hole token and the cell; the new
            // token it yields is bound by the enclosing `let`.
            ExprKind::HoleFill { hole, cell, .. } => {
                self.eval(cell, refs)?;
                self.consume(*hole, refs)?;
            }
            // Closing the spine consumes the hole token and the base value.
            ExprKind::HoleClose { hole, base } => {
                self.eval(base, refs)?;
                self.consume(*hole, refs)?;
            }
        }
        Ok(())
    }

    /// A projection base is an atom after A-normal form; borrow it.
    fn borrow_atom(&self, base: &CExpr, refs: &HashMap<LocalId, i64>) -> Result<(), String> {
        if let ExprKind::Local(x) = base.kind {
            self.borrow(x, refs)
        } else {
            // A non-atom base would be an owned temporary; A-normal form prevents
            // this, so its appearance is a lowering invariant violation.
            Err(format!("fn{}: projection base is not an atom", self.fn_index))
        }
    }
}
