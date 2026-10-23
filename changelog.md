# Changelog

## [Unreleased]

### Added
- Add read-only diagnostics for fresh-root locality and publication barriers in
  ownership IR, without changing reference counting or native code generation.
  Extend the diagnostics with bounded helper publication/result summaries and
  representation-aware field provenance.
- Add a standalone published-JSON example with a portable dependency lock,
  executable serialization contract, and README walkthrough.

### Changed
- Local cell reuse skips incompatible known field counts and selects nearby
  matching constructions while retaining death-point releases and runtime layout checks.
- Private temporary data cells can be removed after ownership insertion while
  preserving field acquisition, destruction order, and resource lifetimes.
- Borrowing of resource-free projected fields now extends through consecutive
  cleanup operations, avoiding redundant ownership while preserving resource release order.
- Encode ordinary HTTP response heads directly from ordered text pairs into one
  checked byte buffer, avoiding intermediate field-line Strings while preserving
  validation, duplicate order, framing and immutable input values.
- Buffer small pipelined HTTP server responses in a bounded per-connection writer.
  Handlers remain sequential, but later handlers may run before an earlier socket
  write completes or fails. Applications must not use handler invocation as proof
  that the previous response was delivered. Streaming, informational replies and
  large responses flush earlier output first; cancellation joins the writer.
- Transfer uniquely owned incoming HTTP heads into retained header text after
  byte parsing, avoiding a whole-head copy. UTF-8 validation and error order
  are preserved; retained headers may keep normal buffer growth capacity.
- Validate the variable reason in internally assembled HTTP response status
  lines directly, avoiding temporary slices while preserving wire bytes and
  validation before body evaluation.
- Accelerate long byte-class scans with per-call vector lookup tables on
  supported x86-64 and ARM64 processors, retaining scalar short-input and
  fallback paths, exact bounds, and arbitrary byte classes.
- Skip empty HTTP protocol-metadata transformations after head validation,
  preserving framing defaults, expectation handling, and error order.
- Keep spare `Bytes.concat` capacity below the 16 KiB independent-mapping
  boundary when the required data fits below it, then resume normal doubling
  sizes. Large buffers retain prompt final-drop release.
- Construct text-response headers directly, avoiding an intermediate empty
  header collection and replacement work.
- Scan incoming HTTP field values with a native byte-class span operation,
  reducing work on long values while preserving validation, whitespace trimming,
  field order and error precedence.
- Keep received HTTP headers in immutable text views, with selective lookup and
  bulk enumeration. Parsed and constructed headers retain the same equality,
  ordering, hashing, duplicate order and immutable-update behavior.
- Analyze ordinary HTTP response headers once per response, reusing validated
  classifications for connection policy, framing and rendering while preserving
  field order, errors and body evaluation.
- Preserve native Boolean predicates across known branch results, avoiding
  repeated tagging and testing when a Boolean is reused locally.
- Inline cached allocation lookups while keeping slab refill outside the hot path.
- Discard socket readiness that predates a locked `WouldBlock` probe before registering
  its waiter, avoiding a redundant retry while retaining lost-wakeup protection.
- Search byte patterns natively with `Bytes.find` and bounded `Bytes.findIn`.
  HTTP delimiter searches use the same borrowing search engine, and request
  metadata parsing rejects unrelated header names before lowercasing them.
- Pass opted-in native `Result` outputs as a tag and owned payload, avoiding
  temporary wrappers during immediate inspection. Checked UTF-8 slicing uses
  this convention while stored and first-class results keep their usual form.
- Lower immediately tested, single-use Boolean decisions through shared branch
  targets, avoiding intermediate tagged results while preserving short-circuit
  effects, traps and ownership releases.
- Borrow the source String for checked UTF-8 byte-range slicing, avoiding
  temporary reference-count operations in repeated extraction and parser loops.
- Track native primitive ownership per argument, allowing byte reads and checked
  text slices to borrow their buffer while consuming boxed scalar indices.
- Direct calls can return eligible `Result` values as a tag and owned components,
  eliminating temporary wrappers and small resource-free tuple payloads. Uniform
  first-class boundaries reconstruct ordinary values.
- Parse decimal integers directly from bytes with the existing wrapping rules.
- Add `String.splitOnce` and use first-separator searches in HTTP and URL parsing,
  avoiding full split lists and reconstructed suffixes.
- Reuse unchanged lowercase/trimmed/split strings and one immortal empty Bytes
  buffer. Integer formatting writes directly from bounded stack storage.
- Eliminate reference-count operations on proven permanent String literals and
  capture-free closures, including in concurrent code.
- Resume incomplete HTTP header and framing-line searches at their unexamined
  suffix, avoiding quadratic rescanning under highly fragmented input.
- Inline read-only Array length and element access in concurrent programs,
  preserving shared ownership, raw Float slots and bounds diagnostics.
- Receive large TCP payloads into their final byte allocation, compacting short
  reads and releasing private buffers on failure.
- Add `Net.sendMany` for bounded vectored writes and use it for managed plain-HTTP
  buffered responses. Custom `Net` instances must implement the new method; a
  fallback may pass `Bytes.concatMany chunks` to their existing send operation.
  Bundle transport tests and benchmarks use the production deep-bundle decoder
  to cover the larger capability dictionary.
- Transfer uniquely owned inline buffers between String and Bytes without
  copying, while preserving UTF-8 validation, substring views and shared aliases.
- Read buffered HTTP request bodies in bounded 15 KiB batches, reducing transient
  page mappings while retaining normal large-body reclamation and streaming
  response chunk sizes.
- Add `Bytes.concatMany` for one-allocation chunk collection and use it when
  buffering HTTP bodies. Buffered sends join the head and body chunks once,
  preserving stream failures, checked lengths and chunk order.
- Compile byte lengths and indexed reads to native loads, including concurrent
  code, while preserving bounds faults and full-width integer arguments.
- Borrow byte buffers for length, indexing and UTF-8 validation, avoiding
  temporary reference-count traffic in reader functions and loops.
- Transfer full-range Bytes slices without copying their buffer, retaining
  clamped indexing, immutable aliases and prompt release of large buffers.
- Let repository documentation checks accept package Unreleased entries during
  development; prepared release candidates still require promoted release notes.
- Append into uniquely owned byte buffers with geometric growth, avoiding
  repeated prefix copying when collecting large HTTP bodies. Shared byte values
  keep their contents; concatenation with an empty buffer returns the other value.
- Collect request framing, expectation and connection metadata during the
  validated field scan, avoiding repeated header lookups and case conversion.
- Render computed HTTP Content-Length fields directly without constructing a
  temporary replacement header collection, preserving validation and wire order.
- Concatenate Bytes directly into one final buffer with checked allocation size,
  avoiding an intermediate native vector and an extra full-payload copy.
- Parse HTTP request fields in one validation/whitespace scan, materializing the
  final names and values without split/rejoin buffers and preserving field order.
- Use bounded coroutine-stack scratch buffers for TCP reads up to 4 KiB,
  avoiding a temporary native heap allocation while returning independent Bytes.
- Drive network readiness and timers directly from an idle single scheduler
  worker, avoiding reactor-thread handoffs. Multi-worker execution retains its
  dedicated reactor; cancellation and off-thread work still wake idle workers.
- Preserve URL components without percent escapes directly, avoiding replacement
  buffers while keeping Unicode, literal plus signs, and malformed escapes intact.
- Avoid temporary punctuation lists and repeated length reads during HTTP header
  validation; validate outgoing response heads once before framing their bodies.
- Keep HTTP/1.1 server connections alive across sequential and pipelined requests,
  preserving buffered bytes and scoped response-writer lifetimes. Send
  `Connection: close` in a request or response to retain one-request behavior;
  HTTP/1.0 remains close-by-default. Server termination also releases idle connections.
  Request bodies remain fully read but may contain multiple independent chunks;
  use `Http.bodyBytes` or `Http.bodyText` when a combined payload is required.

### Fixed
- Do not forward reuse tokens to mutual-tail-recursion members whose standalone
  entries are replaced by combined-loop wrappers.
- Keep split-Result tag comparisons and tuple reconstruction in A-normal form,
  preserving ownership when result components are inspected, boxed and reused.
- Disconnected peers during vectored TCP sends now return an error instead of
  raising `SIGPIPE`, including native executables with default Unix signal
  handling. Custom process signal handlers are preserved.
- Keep timers and network events progressing under a continuously runnable
  single-worker queue by polling nonblocking between bounded task batches.
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
