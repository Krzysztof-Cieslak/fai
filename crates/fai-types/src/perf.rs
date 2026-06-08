//! Always-on instrumentation counters for the inference solver.
//!
//! These thread-local counters tally the structural work the solver performs —
//! representative-resolution clones, occurs-check node visits, and
//! free-variable collection visits — so benchmarks and the deterministic
//! anti-quadratic guards can observe the solver's asymptotic complexity
//! directly, without depending on wall-clock time. Inference runs
//! single-threaded per definition, so the counters are thread-local `Cell`s and
//! each increment is a plain (non-atomic) cell update.

use std::cell::Cell;

thread_local! {
    static RESOLVE_CLONES: Cell<u64> = const { Cell::new(0) };
    static OCCURS_VISITS: Cell<u64> = const { Cell::new(0) };
    static FREE_VAR_VISITS: Cell<u64> = const { Cell::new(0) };
}

/// A snapshot of the solver-work counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// Solver-type clones made while resolving a variable to its representative.
    pub resolve_clones: u64,
    /// Nodes visited by the occurs check.
    pub occurs_visits: u64,
    /// Nodes visited while collecting a type's free variables.
    pub free_var_visits: u64,
}

/// Records one representative-resolution clone.
#[inline]
pub(crate) fn bump_resolve_clone() {
    RESOLVE_CLONES.with(|c| c.set(c.get() + 1));
}

/// Records one occurs-check node visit.
#[inline]
pub(crate) fn bump_occurs_visit() {
    OCCURS_VISITS.with(|c| c.set(c.get() + 1));
}

/// Records one free-variable-collection node visit.
#[inline]
pub(crate) fn bump_free_var_visit() {
    FREE_VAR_VISITS.with(|c| c.set(c.get() + 1));
}

/// Resets every solver-work counter to zero. Call before a measured run.
pub fn reset() {
    RESOLVE_CLONES.with(|c| c.set(0));
    OCCURS_VISITS.with(|c| c.set(0));
    FREE_VAR_VISITS.with(|c| c.set(0));
}

/// Returns a snapshot of the current solver-work counters.
#[must_use]
pub fn snapshot() -> Counters {
    Counters {
        resolve_clones: RESOLVE_CLONES.with(Cell::get),
        occurs_visits: OCCURS_VISITS.with(Cell::get),
        free_var_visits: FREE_VAR_VISITS.with(Cell::get),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_reset_and_accumulate() {
        reset();
        assert_eq!(snapshot(), Counters::default());
        bump_resolve_clone();
        bump_occurs_visit();
        bump_occurs_visit();
        bump_free_var_visit();
        let s = snapshot();
        assert_eq!(s.resolve_clones, 1);
        assert_eq!(s.occurs_visits, 2);
        assert_eq!(s.free_var_visits, 1);
        reset();
        assert_eq!(snapshot(), Counters::default());
    }
}
