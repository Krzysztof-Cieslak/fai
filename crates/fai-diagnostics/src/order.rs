//! Deterministic diagnostic ordering.
//!
//! Output order must be stable regardless of how diagnostics were collected
//! (`Agents.md` §8). Both renderers sort by `(file, byte_start, code)`.

use fai_span::SpanResolver;

use crate::diagnostic::Diagnostic;

/// Returns indices into `diags` ordered by `(file, byte_start, code)`.
///
/// Unresolved spans sort first (empty path) but remain deterministic.
pub(crate) fn sort_order(diags: &[Diagnostic], resolver: &dyn SpanResolver) -> Vec<usize> {
    let mut keyed: Vec<(String, u32, &'static str, usize)> = diags
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let (file, byte_start) = match resolver.resolve(d.primary) {
                Some(r) => (r.path.into_string(), r.byte_start),
                None => (String::new(), d.primary.start().raw()),
            };
            (file, byte_start, d.code.as_str(), i)
        })
        .collect();
    keyed.sort();
    keyed.into_iter().map(|(_, _, _, i)| i).collect()
}
