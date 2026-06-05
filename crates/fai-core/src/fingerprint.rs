//! A portable, deterministic content fingerprint of a lowered definition.
//!
//! Unlike [`crate::pretty`] (a debugging aid), this rendering is **complete** and
//! **portable**: every [`ExprKind::Global`] is rendered through the caller's
//! namer (so a callee is identified by its module-qualified backend symbol, not a
//! process-local [`DefId`]), each referenced definition's arity is included, and
//! every node carries its canonical type. Hashing the result yields a key that
//! changes exactly when the emitted object could change, and is stable across
//! processes and runs (no interner ids, no file indices). The driver uses it as
//! the content key of the on-disk artifact cache.

use std::fmt::Write as _;

use fai_resolve::DefId;
use fai_types::render_canonical;

use crate::ir::{CExpr, ExprKind, Lit, LoweredDef, Prim};

/// Builds a portable, deterministic fingerprint string for `def`.
///
/// `namer` maps a definition to its backend symbol (module-qualified) and
/// `arity_of` to its parameter count — the same information code generation emits
/// into the object, so the fingerprint tracks exactly what the object depends on.
#[must_use]
pub fn fingerprint_def(
    def: &LoweredDef,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "def {}/{}", namer(def.def), arity_of(def.def));
    for (i, f) in def.fns.iter().enumerate() {
        let params: Vec<String> = f.params.iter().map(|p| format!("%{}", p.index())).collect();
        let caps: Vec<String> = f.captures.iter().map(|c| format!("%{}", c.index())).collect();
        let _ = write!(out, "fn{i}({})[{}] = ", params.join(","), caps.join(","));
        write_expr(&mut out, &f.body, namer, arity_of);
        out.push('\n');
    }
    out
}

fn write_expr(
    out: &mut String,
    e: &CExpr,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
) {
    match &e.kind {
        ExprKind::Lit(Lit::Int(n)) => {
            let _ = write!(out, "i{n}");
        }
        ExprKind::Lit(Lit::Bool(b)) => {
            let _ = write!(out, "b{b}");
        }
        ExprKind::Lit(Lit::Str(bytes)) => {
            let _ = write!(out, "s{bytes:?}");
        }
        ExprKind::Lit(Lit::Unit) => out.push('u'),
        ExprKind::Local(id) => {
            let _ = write!(out, "%{}", id.index());
        }
        ExprKind::Global(def) => {
            // Module-qualified symbol + arity: exactly what codegen emits for a
            // call, so the key is stable across processes (no DefId/SourceId).
            let _ = write!(out, "@{}/{}", namer(*def), arity_of(*def));
        }
        ExprKind::Prim { op, args } => {
            let _ = write!(out, "(p:{}", prim_tag(*op));
            for a in args {
                out.push(' ');
                write_expr(out, a, namer, arity_of);
            }
            out.push(')');
        }
        ExprKind::App { func, args } => {
            out.push_str("(app ");
            write_expr(out, func, namer, arity_of);
            for a in args {
                out.push(' ');
                write_expr(out, a, namer, arity_of);
            }
            out.push(')');
        }
        ExprKind::If { cond, then, els } => {
            out.push_str("(if ");
            write_expr(out, cond, namer, arity_of);
            out.push(' ');
            write_expr(out, then, namer, arity_of);
            out.push(' ');
            write_expr(out, els, namer, arity_of);
            out.push(')');
        }
        ExprKind::Let { local, value, body } => {
            let _ = write!(out, "(let %{} ", local.index());
            write_expr(out, value, namer, arity_of);
            out.push(' ');
            write_expr(out, body, namer, arity_of);
            out.push(')');
        }
        ExprKind::MakeClosure { func, captures } => {
            let caps: Vec<String> = captures.iter().map(|c| format!("%{}", c.index())).collect();
            let _ = write!(out, "(clo fn{} [{}])", func.index(), caps.join(","));
        }
        ExprKind::Dup { local, body } => {
            let _ = write!(out, "(dup %{} ", local.index());
            write_expr(out, body, namer, arity_of);
            out.push(')');
        }
        ExprKind::Drop { local, body } => {
            let _ = write!(out, "(drop %{} ", local.index());
            write_expr(out, body, namer, arity_of);
            out.push(')');
        }
        ExprKind::Error => out.push_str("<err>"),
    }
    // Each node carries its canonical type. Codegen ignores types in the current
    // subset, but including them keeps the key correct as later phases derive
    // layout (e.g. record field offsets) from types.
    let _ = write!(out, ":{}", render_canonical(&e.ty));
}

/// A stable, semantic tag for a primitive (its runtime symbol — never reordered).
fn prim_tag(op: Prim) -> &'static str {
    op.runtime_symbol()
}

#[cfg(test)]
mod tests {
    use fai_db::{Db, FaiDatabase};
    use fai_resolve::module_name;
    use fai_syntax::Symbol;

    use super::*;
    use crate::core;

    /// A namer mirroring the backend's `symbol_base`, so the fingerprint test is
    /// self-contained.
    fn namer(db: &FaiDatabase, def: DefId) -> String {
        let label = db
            .source_file(def.file)
            .and_then(|f| module_name(db, f))
            .map_or_else(|| "M".to_owned(), |m| m.0.as_str().to_owned());
        let sanitized: String =
            label.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        format!("fai_{sanitized}_{}", def.name)
    }

    fn fingerprint(src: &str, name: &str) -> String {
        let mut db = FaiDatabase::new();
        fai_types::prelude::load_prelude(&mut db);
        let id = db.add_source("M.fai".into(), src.to_owned());
        let file = db.source_file(id).unwrap();
        let lowered = core(&db, file, Symbol::intern(name));
        fingerprint_def(&lowered, &|d| namer(&db, d), &|_| 0)
    }

    #[test]
    fn distinguishes_different_bodies() {
        let a = fingerprint("module M\n\nlet f x = x + 1\n", "f");
        let b = fingerprint("module M\n\nlet f x = x + 2\n", "f");
        assert_ne!(a, b);
    }

    #[test]
    fn stable_for_identical_bodies() {
        let a = fingerprint("module M\n\nlet f x = x + 1\n", "f");
        let b = fingerprint("module M\n\nlet f x = x + 1\n", "f");
        assert_eq!(a, b);
    }
}
