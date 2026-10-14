# Changelog

## [Unreleased]

### Changed
- Add `fai test --junit <path>` for atomic JUnit XML artifacts alongside human or
  JSON output, including diagnostics and preparation errors. Native package CI
  publishes these reports and summaries on success or failure.
- Give `fai test` Rust-style live results with source locations, grouped failure
  details, reproduction settings and timed summaries. Distinguish assertion
  failures, crashes, timeouts and blocked execution; add compatible JSON summary,
  configuration and location fields while preserving existing event identities.

### Fixed
- Scope independent release, performance and JUnit tooling changes to their
  validation lanes, and avoid native-runner queues for documentation-only CI.
- Keep malformed JUnit timings and invalid report roots from crashing the CI
  summary renderer.
- Validate isolated VS Code extension changes with TypeScript, grammar and VSIX
  checks instead of the Rust suite; retain editor checks for full compiler changes.
- Reserve daemon-test fixture directories atomically so reused Windows process
  IDs cannot reopen stale Git repositories from earlier tests.
- Keep root and package documentation checks lightweight, and scope package
  test/integration edits without retesting unrelated production consumers.
- Compile only the Rust packages and target names needed by cross-platform CI,
  preserving its test policy and checking exact coverage against the full build.
- Reuse the compiler produced by CI's Rust test build for package and relocation
  checks, with profile-specific bundle identities instead of a second compiler build.
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
