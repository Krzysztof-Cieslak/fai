//! A compact textual rendering of Core IR, for tests and debugging.

use std::fmt::Write as _;

use crate::ir::{CExpr, ExprKind, Lit, LoweredDef, Prim};

/// Renders a lowered definition as a compact, deterministic string.
#[must_use]
pub fn pretty_def(def: &LoweredDef) -> String {
    let mut out = String::new();
    for (i, f) in def.fns.iter().enumerate() {
        let params: Vec<String> = f.params.iter().map(|p| format!("%{}", p.index())).collect();
        let caps: Vec<String> = f.captures.iter().map(|c| format!("%{}", c.index())).collect();
        let _ = write!(out, "fn{i}({})", params.join(", "));
        if !caps.is_empty() {
            let _ = write!(out, " [caps {}]", caps.join(", "));
        }
        let _ = write!(out, " = ");
        write_expr(&mut out, &f.body);
        out.push('\n');
    }
    out
}

fn prim_name(op: Prim) -> &'static str {
    match op {
        Prim::IntAdd => "+",
        Prim::IntSub => "-",
        Prim::IntMul => "*",
        Prim::IntDiv => "/",
        Prim::IntRem => "%",
        Prim::IntLt => "<",
        Prim::IntLe => "<=",
        Prim::IntGt => ">",
        Prim::IntGe => ">=",
        Prim::Eq => "=",
        Prim::StrConcat => "++",
        Prim::IntToString => "intToString",
        Prim::Not => "not",
        Prim::ConsoleWriteLine => "writeLine",
    }
}

fn write_expr(out: &mut String, e: &CExpr) {
    match &e.kind {
        ExprKind::Lit(Lit::Int(n)) => {
            let _ = write!(out, "{n}");
        }
        ExprKind::Lit(Lit::Bool(b)) => {
            let _ = write!(out, "{b}");
        }
        ExprKind::Lit(Lit::Str(bytes)) => {
            let _ = write!(out, "{:?}", String::from_utf8_lossy(bytes));
        }
        ExprKind::Lit(Lit::Unit) => out.push_str("()"),
        ExprKind::Local(id) => {
            let _ = write!(out, "%{}", id.index());
        }
        ExprKind::Global(def) => {
            let _ = write!(out, "@{}", def.name);
        }
        ExprKind::Prim { op, args } => {
            let _ = write!(out, "({}", prim_name(*op));
            write_args(out, args);
            out.push(')');
        }
        ExprKind::App { func, args } => {
            out.push_str("(app ");
            write_expr(out, func);
            write_args(out, args);
            out.push(')');
        }
        ExprKind::If { cond, then, els } => {
            out.push_str("(if ");
            write_expr(out, cond);
            out.push(' ');
            write_expr(out, then);
            out.push(' ');
            write_expr(out, els);
            out.push(')');
        }
        ExprKind::Let { local, value, body } => {
            let _ = write!(out, "(let %{} = ", local.index());
            write_expr(out, value);
            out.push_str("; ");
            write_expr(out, body);
            out.push(')');
        }
        ExprKind::MakeClosure { func, captures } => {
            let caps: Vec<String> = captures.iter().map(|c| format!("%{}", c.index())).collect();
            let _ = write!(out, "(closure fn{} [{}])", func.index(), caps.join(", "));
        }
        ExprKind::Dup { local, body } => {
            let _ = write!(out, "(dup %{}; ", local.index());
            write_expr(out, body);
            out.push(')');
        }
        ExprKind::Drop { local, body } => {
            let _ = write!(out, "(drop %{}; ", local.index());
            write_expr(out, body);
            out.push(')');
        }
        ExprKind::Error => out.push_str("<error>"),
    }
}

fn write_args(out: &mut String, args: &[CExpr]) {
    for a in args {
        out.push(' ');
        write_expr(out, a);
    }
}
