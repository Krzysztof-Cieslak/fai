//! A compact textual rendering of Core IR, for tests and debugging.

use std::fmt::Write as _;

use crate::ir::{CExpr, ClosureAlloc, ExprKind, FieldIndex, Lit, LoweredDef, Prim};
use crate::niche::NicheKind;

/// The niche-scheme suffix shown on a data node (empty for a standard node).
fn niche_suffix(niche: Option<NicheKind>) -> &'static str {
    match niche {
        None => "",
        Some(NicheKind::A) => "~a",
        Some(NicheKind::B) => "~b",
    }
}

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
        Prim::IntAnd => "intAnd",
        Prim::IntOr => "intOr",
        Prim::IntXor => "intXor",
        Prim::IntShl => "intShiftLeft",
        Prim::IntShr => "intShiftRight",
        Prim::IntShrLogical => "intShiftRightLogical",
        Prim::IntComplement => "intComplement",
        Prim::IntLt => "<",
        Prim::IntLe => "<=",
        Prim::IntGt => ">",
        Prim::IntGe => ">=",
        Prim::FloatAdd => "+.",
        Prim::FloatSub => "-.",
        Prim::FloatMul => "*.",
        Prim::FloatDiv => "/.",
        Prim::FloatLt => "<.",
        Prim::FloatLe => "<=.",
        Prim::FloatGt => ">.",
        Prim::FloatGe => ">=.",
        Prim::Compare => "compare",
        Prim::Hash => "hash",
        Prim::Eq => "=",
        Prim::StrConcat => "++",
        Prim::IntToString => "intToString",
        Prim::FloatToString => "floatToString",
        Prim::IntToFloat => "intToFloat",
        Prim::FloatToInt => "floatToInt",
        Prim::Sqrt => "sqrt",
        Prim::FloatFromBits => "floatFromBits",
        Prim::FloatToBits => "floatToBits",
        Prim::CharToString => "charToString",
        Prim::CharToCode => "charToCode",
        Prim::CharFromCode => "charFromCode",
        Prim::IsValidCharCode => "isValidCharCode",
        Prim::StringLength => "stringLength",
        Prim::ToUpper => "toUpper",
        Prim::ToLower => "toLower",
        Prim::Trim => "trim",
        Prim::StringContains => "stringContains",
        Prim::StringSplit => "split",
        Prim::StringJoin => "join",
        Prim::StringSubstring => "substring",
        Prim::StringTake => "take",
        Prim::StringDrop => "drop",
        Prim::Not => "not",
        Prim::ConsoleWriteLine => "consoleWriteLine",
        Prim::ClockNow => "clockNow",
        Prim::RandomNextInt => "randomNextInt",
        Prim::FileRead => "fileRead",
        Prim::FileWrite => "fileWrite",
        Prim::EnvGet => "envGet",
        Prim::EnvArgs => "envArgs",
        Prim::RecordUpdate => "recordUpdate",
        Prim::ArrayWithCapacity => "arrayWithCapacity",
        Prim::ArrayLength => "arrayLength",
        Prim::ArrayGet => "arrayGet",
        Prim::ArraySet => "arraySet",
        Prim::ArrayPush => "arrayPush",
        Prim::ArraySplit => "arraySplit",
        Prim::ArrayJoin => "arrayJoin",
    }
}

fn write_expr(out: &mut String, e: &CExpr) {
    match &e.kind {
        ExprKind::Lit(Lit::Int(n)) => {
            let _ = write!(out, "{n}");
        }
        ExprKind::Lit(Lit::Float(bits)) => {
            let _ = write!(out, "{}", f64::from_bits(*bits));
        }
        ExprKind::Lit(Lit::Char(c)) => {
            let _ = write!(out, "{c:?}");
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
        ExprKind::App { func, args, reuse, alloc } => {
            out.push_str("(app ");
            // Only a stack-allocated partial application is annotated, so an ordinary
            // call prints unchanged.
            if matches!(alloc, ClosureAlloc::Stack) {
                out.push_str("stack ");
            }
            write_expr(out, func);
            write_args(out, args);
            for t in reuse {
                match t {
                    Some(l) => {
                        let _ = write!(out, " @%{}", l.index());
                    }
                    None => out.push_str(" @_"),
                }
            }
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
        ExprKind::MakeClosure { func, captures, alloc } => {
            let caps: Vec<String> = captures.iter().map(|c| format!("%{}", c.index())).collect();
            let kind = match alloc {
                ClosureAlloc::Static => "static",
                ClosureAlloc::Stack => "stack",
                ClosureAlloc::Heap => "heap",
            };
            let _ = write!(out, "(closure/{kind} fn{} [{}])", func.index(), caps.join(", "));
        }
        ExprKind::MakeData { tag, args, reuse, scalars, niche } => {
            match reuse {
                Some(t) => {
                    let _ = write!(out, "(data@%{} {tag}", t.index());
                }
                None => {
                    let _ = write!(out, "(data {tag}");
                }
            }
            // Show the scalar (`f64`) field bitmap only when non-empty.
            if *scalars != 0 {
                let _ = write!(out, "!{scalars}");
            }
            out.push_str(niche_suffix(*niche));
            write_args(out, args);
            out.push(')');
        }
        ExprKind::DataTag { base, niche } => {
            let _ = write!(out, "(tag{} ", niche_suffix(*niche));
            write_expr(out, base);
            out.push(')');
        }
        ExprKind::DataField { base, index, scalar, niche } => {
            // A scalar (unboxed `f64`) slot reads as `fieldf`; a niche `Option`
            // projection appends its scheme.
            let kw = if *scalar { "fieldf" } else { "field" };
            let nq = niche_suffix(*niche);
            match index {
                FieldIndex::Const(n) => {
                    let _ = write!(out, "({kw}{nq} {n} ");
                }
                FieldIndex::Dyn { base: off, evidence } => {
                    let _ = write!(out, "({kw}{nq} {off}+%{} ", evidence.index());
                }
            }
            write_expr(out, base);
            out.push(')');
        }
        ExprKind::Reset { value, token, body } => {
            let _ = write!(out, "(reset %{} = ", token.index());
            write_expr(out, value);
            out.push_str("; ");
            write_expr(out, body);
            out.push(')');
        }
        ExprKind::FreeReuse { token, body } => {
            let _ = write!(out, "(free-reuse %{}; ", token.index());
            write_expr(out, body);
            out.push(')');
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
        ExprKind::Join { params, body } => {
            let ps: Vec<String> = params.iter().map(|p| format!("%{}", p.index())).collect();
            let _ = write!(out, "(join [{}] ", ps.join(", "));
            write_expr(out, body);
            out.push(')');
        }
        ExprKind::Recur { args } => {
            out.push_str("(recur");
            write_args(out, args);
            out.push(')');
        }
        ExprKind::HoleStart { hole, body } => {
            let _ = write!(out, "(holestart %{}; ", hole.index());
            write_expr(out, body);
            out.push(')');
        }
        ExprKind::HoleFill { hole, cell, field } => {
            let _ = write!(out, "(holefill %{} {field} ", hole.index());
            write_expr(out, cell);
            out.push(')');
        }
        ExprKind::HoleClose { hole, base } => {
            let _ = write!(out, "(holeclose %{} ", hole.index());
            write_expr(out, base);
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
