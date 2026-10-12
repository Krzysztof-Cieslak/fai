# Structured logging

`logging` is a source package depending on `json`. Its record model, field merging,
filtering and human/JSON Lines formatting are pure Fai. Copy/include package sources
under the same workspace root as the application.

`LogTypes` defines levels, records, checked delivery receipts and errors.
`LogField` builds typed fields; later fields override earlier ones, and final keys
are sorted. Field encoding failures remain data until an enabled event is prepared.
`LogFilter` supports a minimum level (None disables logging) and longest dotted
target-prefix overrides. `LogFormat` emits a versioned JSON envelope or a quoted
human-readable line. Lines exclude their final newline; outputs own framing.

Limits bound the complete encoded record, merged fields, value depth and value
nodes. Validation counts escaped UTF-8 bytes before JSON rendering and rejects
duplicate nested JSON keys. Integers retain their full signed range; non-finite
floats fail instead of silently changing type.

`Log.create Log.defaults sink` constructs a checked logger. `withTarget` and
`withFields` derive immutable child loggers; event fields override child and parent
context. `info`, `debug`, `warn`, `error`, `trace` and `fatal` return checked
receipts. `writeLazy`/`debugLazy` defer the whole `Log.entry`, including fields,
until filtering accepts it. `withClock` adds the explicit Clock effect; custom
`withTimestamp` providers are read once, and the most recently attached provider
wins even when it returns None. `Log.prepare` is the pure preparation entry point.

`LogSink.formatted name limits format output` composes rendering and a line
output. `combine` fans out in registration order, attempting every destination
and retaining successful receipts alongside failures. Names must be distinct.
`LogBestEffort.fromChecked logger onFailure` supplies Unit-returning calls; its
handler runs once per failed operation, after checked operations release guards.
The Fatal level does not terminate the application.

`LogSink.stdout` and `stderr` use one Console call per line. Console does not expose
OS write/flush failures, so a completed receipt means its API returned, not that
the operating system confirmed delivery. File outputs use one native `writeChunk`
per complete line; concurrent writes through the same Writer retain record
boundaries. `LogSink.writer fs writer EachRecord` borrows a writer, while
`LogSink.withFile env options path work` owns open/flush/close and reports callback
and cleanup failures separately. File options default to append and EachRecord
flushing; OnFlush buffers until an explicit flush or close. Neither implies fsync.
Retained aliases return Closed after the scope. `LogSink.serialized` supplies a
shared gate for custom outputs that need compound-operation serialization.

## Background delivery

`LogQueue.withOutput env options output work` borrows a line output and gives the
callback an `Output { Concurrency }`. `withQueue` additionally exposes a live
snapshot of outstanding records/bytes and summary counters. The owner carries
Clock, Concurrency and the underlying output/callback effects; a queued log call
itself performs Concurrency. Filtering and formatting happen before admission.

An Accepted receipt is returned only after a masked, bounded reservation and
transfer commit. It is never inferred from Channel.send alone. Record slots and
UTF-8 byte budgets include in-flight output. FIFO flush barriers also consume a
slot, but no record bytes. Full queues block before admission; cancellation there
returns a checked error without accepting the record. Callers blocked before
admission retain their own line buffer outside the queue's accepted-byte budget.

Normal scope exit closes admission, drains, flushes and joins. Cancellation and
the optional drain deadline abandon pending records and join in-flight work;
an already executing output is allowed to settle, so the deadline is not a hard
bound on a blocked OS operation. Failure closes admission, wakes producers and
barriers, and retains the first error plus completed/failed/abandoned counts.
No write is automatically replayed. Retained endpoints report Closed or the
stored failure. The result always retains the shutdown summary, including when
the callback also failed. Admission-wait counts are saturating retry counters.

Defaults are 1024 outstanding slots, 8 MiB of accepted UTF-8 record bytes,
256 KiB per record (including newline), and no drain deadline. Limits and positive
optional deadlines are validated before the worker starts. Close the file owner
outside the queue scope so its Writer survives draining.

```sh
fai fmt --check -C packages logging
fai check -C packages logging
fai test -C packages logging
```
