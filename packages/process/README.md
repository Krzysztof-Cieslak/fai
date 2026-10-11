# Managed native processes

`process` supplies pure command builders and scoped native execution. Add
`ProcessHost.native` explicitly to your runtime as `process`, together with
`stdConcurrency` as `concurrency`.

```fai
let request = Process.command "git" ["status", "--short"]
  |> Process.workingDirectory repository
let result = Process.run runtime Process.defaults request
```

The result contains binary stdout, one stderr buffer and optional exit status per
stage, and a stopped flag. Nonzero exit codes remain data; `expectSuccess` checks
them explicitly. `stdoutText` validates UTF-8. Failures retain bounded partial
output, native error information and separate cleanup failures.

Command builders are pure: `argument`, `arguments`, `workingDirectory`,
`inheritEnvironment`, `clearEnvironment`, `setEnvironment`, `unsetEnvironment`,
`stdinBytes`, and `stdinText`. `describe` exposes their immutable description.
Environment edits apply in order; unset removes a name. Native execution resolves
paths and the effective PATH. Windows names compare case-insensitively. Shell
programs are explicit executables; Windows batch scripts require an explicit
command interpreter.

`run` feeds stdin concurrently with both output pipes. Every operation owns a
native supervisor; its pipes are nonblocking and never occupy the general
blocking pool. Monotonic deadlines are enforced by that supervisor, so expiry can
stop the external process even while application code handles output. Managed
execution carries `{ Concurrency, ProcessHost.Process }`.

Options default to a 1 MiB streaming buffer, 64 KiB chunks, a 16 MiB total capture
limit, a 1-second shutdown grace, a 5-second post-exit drain deadline, and 10 ms
exit polling. Overall execution has no default deadline. All are configurable.
Sizes are checked before starting; native buffers are limited to 1 GiB, polling
to 1..1000 ms, and pipeline length to at most 1024 stages (package default: 64).

Closing ends input, then terminates remaining process groups/Jobs after the grace
interval and reaps direct children. A deadline starts cleanup; returning waits
for native cleanup. Unix descendants that detach from the group are outside that
group's ownership. External code executes with its operating-system permissions.

`Runner 'e` is a small effect-carrying captured-execution record. `runner`
constructs a native implementation; a scripted `Runner {}` is usable in pure
contracts.

## Streaming and manual sessions

`withOutput runtime options command consume` feeds preset input and delivers
`Chunk { bytes, stage, stderr }` values sequentially. The callback returns
`Ok Continue`, `Ok Stop`, or `Err error`. Its effects are forwarded. Returned
output contains a bounded preview (default 8 KiB), with `truncated = true` when
longer. Streaming itself is independent of the capture limit. Each pipe keeps its
byte order; ordering between different pipes is observation order.

`communicate runtime options command produce consume` runs a custom input
producer concurrently with output consumption. Its two callbacks may perform
different effects; their effects union through the call. The producer receives
a scoped `Child`; returning closes stdin. Both callbacks use `Process.Error` for
application failures. Use a command without preset input with this operation.

`withChild runtime options command work` gives direct control of a live child,
returning `Completed { value, report }`. Use `write`, `writeStream`, `endInput`,
`next`, `status`, and `stop` inside the callback. Input begins open, and must be
written/closed explicitly. Read concurrently with large writes. There is one
active output reader and one active native write; overlapping operations report
`Busy`. `status` observes exit without draining output. Callback return closes
the session; retained aliases fail rather than extending its lifetime.

`ProcessText.decoder maxLineBytes`, `feed`, and `finish` provide a pure incremental
UTF-8 LF/CRLF decoder. Keep a separate decoder per output pipe. Split code points
remain buffered; invalid UTF-8 and oversized lines are explicit errors. A final
unterminated line is emitted at EOF.

```sh
fai fmt --check -C packages process
fai check -C packages process
fai test -C packages process
```
