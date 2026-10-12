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

```sh
fai fmt --check -C packages logging
fai check -C packages logging
fai test -C packages logging
```
