# Changelog

## [Unreleased]

### Added
- Add a standalone published-JSON example with a portable dependency lock,
  executable serialization contract, and README walkthrough.

### Changed
- Drive network readiness and timers directly from an idle single scheduler
  worker, avoiding reactor-thread handoffs. Multi-worker execution retains its
  dedicated reactor; cancellation and off-thread work still wake idle workers.
- Avoid temporary punctuation lists and repeated length reads during HTTP header
  validation; validate outgoing response heads once before framing their bodies.
- Keep HTTP/1.1 server connections alive across sequential and pipelined requests,
  preserving buffered bytes and scoped response-writer lifetimes. Send
  `Connection: close` in a request or response to retain one-request behavior;
  HTTP/1.0 remains close-by-default. Server termination also releases idle connections.
  Request bodies remain fully read but may contain multiple independent chunks;
  use `Http.bodyBytes` or `Http.bodyText` when a combined payload is required.

### Fixed
- Use full HTTPS package URLs in onboarding examples so they work with the
  released 0.1.0 compiler.

## [0.1.0] - 2026-10-15

### Added
- Add `[package] publish = false` for private workspace and example projects;
  both release planning and publication reject them. Keep example-specific tests
  outside published libraries and exclude private projects from release batches.
- Add a manual source-package release workflow with exact published-compiler
  qualification on Linux, macOS, and Windows, atomic package tags, and immutable
  per-package release notes without changing the compiler's Latest channel.
- Add `Test.property` for named property-based tests with explicit generators,
  runner-controlled seed/count/size, shrinking, and located assertion details.
  Hand-built `Test.Context` values must now include `trials` and `maxSize`;
  ordinary runner-supplied contexts include them automatically.
- Add the pure `CompressionEngine` native bridge for bounded gzip, zlib and raw
  DEFLATE snapshots, backing the optional `compression` source package.

### Changed
- Declare MIT licensing consistently across the compiler workspace and VS Code extension.
- Migrate dedicated source-package tests to named cases and properties, keeping
  inline examples and laws beside library implementations. Runtime scenarios now
  run through package test discovery, and provisioned PostgreSQL suites publish
  named JUnit results alongside their JIT/AOT checks.
- Run logging, regex, JSON, web, LLM, filesystem and process runtime scenarios as
  named Fai tests. Keep Python coverage for compiler diagnostics, independent
  OS/oracle checks, executable backends, relocation and terminal behavior.
- Add named `Test.Suite` scenarios, structured `Expect` checks, scoped fixtures,
  focus/skip metadata, `fai test --list`, and the `--deny-focused` CI guard. Cases
  run in fresh supervised workers with explicit capabilities and temporary roots.
  Test JSON uses schema version 2 for named identities and new outcome states;
  existing contract identities and generator behavior are unchanged. JUnit now
  includes named failures, operation errors and intentional skips. `Expect` is
  now a reserved standard module name; rename any user module with that name.
  Imported cases report their declaration's file and line while keeping the
  selected root suite's stable identity, without duplicating JUnit diagnostics
  when native path separators differ from report paths.
- Add `fai test --junit <path>` for atomic JUnit XML artifacts alongside human or
  JSON output, including diagnostics and preparation errors. Native package CI
  publishes these reports and summaries on success or failure.
- Give `fai test` Rust-style live results with source locations, grouped failure
  details, reproduction settings and timed summaries. Distinguish assertion
  failures, crashes, timeouts and blocked execution; add compatible JSON summary,
  configuration and location fields while preserving existing event identities.

### Fixed
- Allow bounded cold Windows PowerShell startup in installer archive tests while
  keeping validation failures and process timeouts fatal.
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
