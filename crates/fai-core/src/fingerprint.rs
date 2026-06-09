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

use crate::ir::{CExpr, ExprKind, FieldIndex, Lit, LoweredDef, Prim};

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
    // Borrowing changes the calling convention (the owned-ABI wrapper and which
    // arguments direct callers transfer), so it is part of the cache key.
    if def.borrows_any() {
        let _ = writeln!(out, "borrow {:?}", def.entry_borrowed);
    }
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
        ExprKind::Lit(Lit::Float(bits)) => {
            let _ = write!(out, "f{bits}");
        }
        ExprKind::Lit(Lit::Char(c)) => {
            let _ = write!(out, "c{}", *c as u32);
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
        ExprKind::MakeData { tag, args, reuse } => {
            match reuse {
                Some(t) => {
                    let _ = write!(out, "(data@%{} {tag}", t.index());
                }
                None => {
                    let _ = write!(out, "(data {tag}");
                }
            }
            for a in args {
                out.push(' ');
                write_expr(out, a, namer, arity_of);
            }
            out.push(')');
        }
        ExprKind::DataTag(base) => {
            out.push_str("(tag ");
            write_expr(out, base, namer, arity_of);
            out.push(')');
        }
        ExprKind::DataField { base, index } => {
            match index {
                FieldIndex::Const(n) => {
                    let _ = write!(out, "(field {n} ");
                }
                FieldIndex::Dyn { base: off, evidence } => {
                    let _ = write!(out, "(field {off}+%{} ", evidence.index());
                }
            }
            write_expr(out, base, namer, arity_of);
            out.push(')');
        }
        ExprKind::Reset { value, token, body } => {
            let _ = write!(out, "(reset %{} ", token.index());
            write_expr(out, value, namer, arity_of);
            out.push(' ');
            write_expr(out, body, namer, arity_of);
            out.push(')');
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
        ExprKind::Join { params, body } => {
            let ps: Vec<String> = params.iter().map(|p| format!("%{}", p.index())).collect();
            let _ = write!(out, "(join [{}] ", ps.join(","));
            write_expr(out, body, namer, arity_of);
            out.push(')');
        }
        ExprKind::Recur { args } => {
            out.push_str("(recur");
            for a in args {
                out.push(' ');
                write_expr(out, a, namer, arity_of);
            }
            out.push(')');
        }
        ExprKind::HoleStart { hole, body } => {
            let _ = write!(out, "(holestart %{} ", hole.index());
            write_expr(out, body, namer, arity_of);
            out.push(')');
        }
        ExprKind::HoleFill { hole, cell, field } => {
            let _ = write!(out, "(holefill %{} {field} ", hole.index());
            write_expr(out, cell, namer, arity_of);
            out.push(')');
        }
        ExprKind::HoleClose { hole, base } => {
            let _ = write!(out, "(holeclose %{} ", hole.index());
            write_expr(out, base, namer, arity_of);
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
        fai_types::std_lib::load_std(&mut db);
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
    fn distinguishes_literal_kinds() {
        let int = fingerprint("module M\n\nlet f x = 1\n", "f");
        let string = fingerprint("module M\n\nlet f x = \"1\"\n", "f");
        let boolean = fingerprint("module M\n\nlet f x = true\n", "f");
        assert_ne!(int, string);
        assert_ne!(int, boolean);
        assert_ne!(string, boolean);
    }

    #[test]
    fn char_literal_is_distinct_from_other_literals() {
        // A char must hash differently from every other literal kind — in
        // particular from an `Int` of the same code point (`'a'` vs `97`), or a
        // stale cached object could be reused across a Char/Int edit.
        let chr = fingerprint("module M\n\nlet f x = 'a'\n", "f");
        let int = fingerprint("module M\n\nlet f x = 97\n", "f");
        let other_chr = fingerprint("module M\n\nlet f x = 'b'\n", "f");
        let string = fingerprint("module M\n\nlet f x = \"a\"\n", "f");
        assert_ne!(chr, int);
        assert_ne!(chr, other_chr);
        assert_ne!(chr, string);
    }

    #[test]
    fn stable_for_identical_bodies_across_databases() {
        // Two independent databases (fresh interners, fresh file ids) must yield
        // identical fingerprints — nothing process-local leaks into the key.
        let a = fingerprint("module M\n\nlet f x = x + 1\n", "f");
        let b = fingerprint("module M\n\nlet f x = x + 1\n", "f");
        assert_eq!(a, b);
    }

    #[test]
    fn type_distinguishes_structurally_identical_defs() {
        // Same name and structure (`f x = x`), different types: the annotated
        // `Int -> Int` and the inferred `'a -> 'a` must hash differently, because
        // every node carries its canonical type.
        let inferred = fingerprint("module M\n\nlet f x = x\n", "f");
        let annotated = fingerprint("module M\n\npublic f : Int -> Int\nlet f x = x\n", "f");
        assert_ne!(inferred, annotated);
    }

    /// Lowers a def that references another (so its body holds a `Global`).
    fn caller() -> (FaiDatabase, LoweredDef) {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source(
            "M.fai".into(),
            "module M\n\nlet helper x = x + 1\n\nlet g x = helper x\n".into(),
        );
        let file = db.source_file(id).unwrap();
        let lowered = (*core(&db, file, Symbol::intern("g"))).clone();
        (db, lowered)
    }

    #[test]
    fn module_naming_is_part_of_the_key() {
        // The same lowered def fingerprinted under two different module namings
        // differs, because every `Global` (and the def id) is rendered via the
        // namer — this is what `pretty_def` drops and the fingerprint must not.
        let (_db, g) = caller();
        let under_a = fingerprint_def(&g, &|d| format!("fai_A_{}", d.name), &|_| 1);
        let under_b = fingerprint_def(&g, &|d| format!("fai_B_{}", d.name), &|_| 1);
        assert_ne!(under_a, under_b);
    }

    #[test]
    fn callee_arity_is_part_of_the_key() {
        // A reference's arity decides whether codegen forces a value or passes a
        // closure, so it must be in the key.
        let (_db, g) = caller();
        let namer = |d: DefId| format!("fai_M_{}", d.name);
        let arity_one = fingerprint_def(&g, &namer, &|_| 1);
        let arity_two = fingerprint_def(&g, &namer, &|_| 2);
        assert_ne!(arity_one, arity_two);
    }
}
