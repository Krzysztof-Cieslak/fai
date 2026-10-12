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

```sh
fai fmt --check -C packages logging
fai check -C packages logging
fai test -C packages logging
```
