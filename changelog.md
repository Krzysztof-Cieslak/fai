# Changelog

## [Unreleased]

### Fixed
- Keep socket readiness delivery atomic with failed I/O probes and waiter
  registration, preventing concurrent operations from losing an edge and hanging.
- Restore usable Rust dependency caches in CI instead of reusing empty legacy
  entries, and report compiled cache contents separately from cache-key hits.
- Optimize CI test builds while retaining debug assertions, overflow checks,
  runtime leak guards, and the existing test selections.
- Limit package CI for workspace membership, overrides, and lock updates to affected
  packages and dependents while retaining whole-workspace locked validation.
- Discover authenticated draft releases before publication and resume a fully
  qualified release with its original tagged source and build artifacts.

## [0.0.1] - 2026-10-13

- Initial release.
