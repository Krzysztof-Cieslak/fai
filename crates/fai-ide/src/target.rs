//! Resolving a `fai query` target string to a definition.
//!
//! A target is either a name path (`Module.name` or a bare, unique `name`) or a
//! `file:line:col` position. Both resolve to a located definition in some file.

use fai_db::{Db, SourceFile};
use fai_resolve::{ModuleName, module_file, module_name};
use fai_span::ByteOffset;
use fai_syntax::Symbol;

/// A resolved query target: a definition in a file.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedTarget {
    /// The file the definition lives in.
    pub file: SourceFile,
    /// The definition's name.
    pub name: Symbol,
}

/// Resolves a target string against the workspace.
#[must_use]
pub fn resolve_target(db: &dyn Db, target: &str) -> Option<ResolvedTarget> {
    if let Some(pos) = parse_position(target) {
        return resolve_position(db, &pos.0, pos.1, pos.2);
    }
    if let Some((module, member)) = target.split_once('.') {
        // `File.member` or `File.Nested.member`: the first segment names a file
        // module; the rest is the member's (possibly nested) qualified name.
        if let Some(file) = module_file(db, ModuleName(Symbol::intern(module))) {
            let sym = Symbol::intern(member);
            if has_binding(db, file, sym) {
                return Some(ResolvedTarget { file, name: sym });
            }
        }
        // Otherwise the whole dotted string may be a qualified name unique across
        // files (e.g. addressing a nested member without naming its file).
    }
    // Bare or fully-qualified name: unique across user modules.
    resolve_bare(db, Symbol::intern(target))
}

/// Whether `file` defines a binding named `name`.
fn has_binding(db: &dyn Db, file: SourceFile, name: Symbol) -> bool {
    fai_resolve::module_defs(db, file).get(name).is_some()
}

fn resolve_bare(db: &dyn Db, name: Symbol) -> Option<ResolvedTarget> {
    let mut found = None;
    for file in db.all_source_files() {
        if has_binding(db, file, name) {
            if found.is_some() {
                return None; // ambiguous
            }
            found = Some(ResolvedTarget { file, name });
        }
    }
    found
}

/// Parses a `file:line:col` target. Lines/cols are 1-based.
fn parse_position(target: &str) -> Option<(String, u32, u32)> {
    let parts: Vec<&str> = target.rsplitn(3, ':').collect();
    if parts.len() != 3 {
        return None;
    }
    // rsplitn yields reversed: [col, line, file]
    let col: u32 = parts[0].parse().ok()?;
    let line: u32 = parts[1].parse().ok()?;
    let file = parts[2].to_owned();
    Some((file, line, col))
}

/// Resolves a `file:line:col` position to the enclosing definition (at any
/// nesting depth), returning its qualified name.
fn resolve_position(db: &dyn Db, path: &str, line: u32, col: u32) -> Option<ResolvedTarget> {
    let file = db.all_source_files().into_iter().find(|f| f.path(db) == path)?;
    let offset = line_col_to_offset(file.text(db), line, col)?;
    let parsed = fai_syntax::parse(db, file);
    let defs = fai_resolve::module_defs(db, file);
    // Find the smallest enclosing definition (its binding or signature item),
    // keyed by the definition's qualified name.
    let mut best: Option<(u32, Symbol)> = None;
    for d in &defs.defs {
        for item in [Some(d.binding), d.signature].into_iter().flatten() {
            let r = parsed.module.items[item.index()].span;
            if r.start().raw() <= offset && offset < r.end().raw() {
                let width = r.end().raw() - r.start().raw();
                if best.as_ref().is_none_or(|(w, _)| width < *w) {
                    best = Some((width, d.name));
                }
            }
        }
    }
    best.map(|(_, name)| ResolvedTarget { file, name })
}

fn line_col_to_offset(text: &str, line: u32, col: u32) -> Option<u32> {
    let mut idx = 0usize;
    for (i, l) in text.split_inclusive('\n').enumerate() {
        let cur_line = i as u32 + 1;
        if cur_line == line {
            let col_off = col.saturating_sub(1) as usize;
            let bytes = l.len().min(col_off);
            return Some(ByteOffset::from_usize(idx + bytes).raw());
        }
        idx += l.len();
    }
    None
}

/// The module name (header) of a file, or its path stem as a fallback.
#[must_use]
pub fn module_label(db: &dyn Db, file: SourceFile) -> String {
    if let Some(ModuleName(name)) = module_name(db, file) {
        name.as_str().to_owned()
    } else {
        file.path(db).as_str().to_owned()
    }
}
