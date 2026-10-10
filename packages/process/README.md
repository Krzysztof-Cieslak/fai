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

```sh
fai fmt --check -C packages process
fai check -C packages process
fai test -C packages process
```
