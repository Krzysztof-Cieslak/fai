//! Lends primitive-list roots through bounded, scalar-only tail traversals.

use fai_core::ir::{CExpr, CoreFn, ExprKind as K, FieldIndex, Lit, Prim};
use fai_db::Db;
use fai_resolve::{DefId, LocalId};
use fai_types::{Con, Ty};
use rustc_hash::FxHashSet;

/// The root owned by the caller and its nonescaping traversal aliases.
pub(crate) struct Plan {
    /// Input list kept live by the caller.
    pub(crate) root: LocalId,
    /// Local list tails and aliases that borrow that input.
    pub(crate) cursors: FxHashSet<LocalId>,
}

fn scalar(ty: &Ty) -> bool {
    matches!(ty, Ty::Unit | Ty::Con(Con::Int | Con::Float | Con::Bool | Con::Char))
}

/// Proves a scalar-only list scan without calls other than tail recursion.
pub(crate) fn plan(db: &dyn Db, def: DefId, function: &CoreFn) -> Option<Plan> {
    let scheme = fai_core::representation::definition_scheme(db, def)?;
    let mut ty = &scheme.ty;
    let mut root = None;
    for &param in &function.params {
        let Ty::Arrow(input, output, _) = ty else { return None };
        if let Ty::App(head, element) = input.as_ref()
            && matches!(head.as_ref(), Ty::Con(Con::List))
            && scalar(element)
        {
            if root.replace(param).is_some() {
                return None;
            }
        } else if !scalar(input) {
            return None;
        }
        ty = output;
    }
    if !scalar(ty) || !function.captures.is_empty() {
        return None;
    }
    let root = root?;
    let position = function.params.iter().position(|param| *param == root)?;
    let mut scan = Scan {
        def,
        position,
        arity: function.params.len(),
        cursors: FxHashSet::from_iter([root]),
        budget: 256,
        tail: false,
    };
    if !scan.walk(&function.body, true, false) || !scan.tail {
        return None;
    }
    Some(Plan { root, cursors: scan.cursors })
}

struct Scan {
    def: DefId,
    position: usize,
    arity: usize,
    cursors: FxHashSet<LocalId>,
    budget: usize,
    tail: bool,
}

impl Scan {
    fn cursor(&self, e: &CExpr) -> bool {
        match &e.kind {
            K::Local(local) => self.cursors.contains(local),
            K::DataField { base, index: FieldIndex::Const(1), niche: None, scalar: false } => {
                self.cursor(base)
            }
            _ => false,
        }
    }

    fn walk(&mut self, e: &CExpr, tail: bool, cursor: bool) -> bool {
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        match &e.kind {
            K::Local(local) => cursor || !self.cursors.contains(local),
            K::Lit(Lit::Int(_) | Lit::Float(_) | Lit::Bool(_) | Lit::Char(_) | Lit::Unit)
            | K::Error => true,
            K::Let { local, value, body } => {
                let derived = self.cursor(value);
                if !self.walk(value, false, derived) {
                    return false;
                }
                if derived {
                    self.cursors.insert(*local);
                }
                self.walk(body, tail, cursor)
            }
            K::If { cond, then, els } => {
                self.walk(cond, false, false)
                    && self.walk(then, tail, cursor)
                    && self.walk(els, tail, cursor)
            }
            K::DataTag { base, niche: None } => self.cursor(base),
            K::DataField { base, index: FieldIndex::Const(index), niche: None, .. } => {
                self.cursor(base) && ((*index == 0 && scalar(&e.ty)) || (*index == 1 && cursor))
            }
            K::Prim { op, args } => {
                numeric(*op) && args.iter().all(|arg| self.walk(arg, false, false))
            }
            K::App { func, args, reuse, .. }
                if tail
                    && args.len() == self.arity
                    && reuse.is_empty()
                    && matches!(func.kind, K::Global(target) if target == self.def) =>
            {
                self.tail = true;
                args.iter().enumerate().all(|(index, arg)| {
                    if index == self.position {
                        self.cursor(arg) && self.walk(arg, false, true)
                    } else {
                        self.walk(arg, false, false)
                    }
                })
            }
            _ => false,
        }
    }
}

fn numeric(op: Prim) -> bool {
    matches!(
        op,
        Prim::IntAdd
            | Prim::IntSub
            | Prim::IntMul
            | Prim::IntDiv
            | Prim::IntRem
            | Prim::IntAnd
            | Prim::IntOr
            | Prim::IntXor
            | Prim::IntShl
            | Prim::IntShr
            | Prim::IntShrLogical
            | Prim::IntComplement
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
            | Prim::Eq
            | Prim::Compare
            | Prim::IntToFloat
            | Prim::FloatToInt
            | Prim::Sqrt
            | Prim::FloatFromBits
            | Prim::FloatToBits
            | Prim::CharToCode
            | Prim::CharFromCode
            | Prim::IsValidCharCode
            | Prim::Not
    )
}

/// A list tail is always a uniform field. Keep scalar heads on the normal owned
/// projection path so full-width and Float conversions retain their boundaries.
pub(crate) fn rewrite(e: &mut CExpr, plan: &Plan) {
    if let K::Let { local, value, body } = &mut e.kind {
        if plan.cursors.contains(local)
            && let K::DataField { base, index: FieldIndex::Const(1), .. } = &value.kind
        {
            value.kind = K::Prim {
                op: Prim::DataPeek,
                args: vec![(**base).clone(), CExpr::new(K::Lit(Lit::Int(1)), Ty::int())],
            };
        }
        rewrite(value, plan);
        rewrite(body, plan);
    } else if let K::If { cond, then, els } = &mut e.kind {
        rewrite(cond, plan);
        rewrite(then, plan);
        rewrite(els, plan);
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::{borrow_sig, rc_checked};

    #[test]
    fn a_scalar_scan_borrows_its_tails_and_stays_a_loop() {
        let source = "module M\nlet count acc xs = match xs with | [] -> acc | x :: rest -> count (acc + x) rest\n";
        assert_eq!(borrow_sig(source, "count"), vec![false, true]);
        let body = rc_checked(source, "count");
        assert!(body.contains("dataPeek") && body.contains("recur"), "{body}");
    }

    #[test]
    fn an_escaping_tail_keeps_the_ordinary_ownership() {
        let source = "module M\nlet tail n xs = if n <= 0 then xs else match xs with | [] -> [] | _ :: rest -> tail (n - 1) rest\n";
        assert_eq!(borrow_sig(source, "tail"), vec![false, false]);
    }

    #[test]
    fn a_rebuilt_list_is_owned() {
        let source =
            "module M\nlet loop n xs = if n <= 0 then List.length xs else loop (n - 1) (n :: xs)\n";
        assert_eq!(borrow_sig(source, "loop"), vec![false, false]);
    }

    #[test]
    fn an_effectful_scan_keeps_ordered_releases() {
        let source = "module M\nlet visit xs = match xs with | [] -> () | x :: rest ->\n  let _ = stdConsole.writeLine (Int.toString x)\n  visit rest\n";
        assert_eq!(borrow_sig(source, "visit"), vec![false]);
    }

    #[test]
    fn an_unknown_payload_is_not_retained_by_the_scan() {
        let source = "module M\nlet count acc xs = match xs with | [] -> acc | _ :: rest -> count (acc + 1) rest\n";
        assert_eq!(borrow_sig(source, "count"), vec![false, false]);
    }
}
