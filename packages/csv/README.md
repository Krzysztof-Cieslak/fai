# CSV

Pure Fai CSV processing. A shared byte-oriented incremental parser accepts text
or arbitrary byte fields, retains source spans and checks configurable limits.
`CsvCore` is the parser foundation; `CsvTypes` defines dialects and diagnostics.

Default quoting follows doubled-quote CSV rules. Record endings accept CRLF, LF
or CR; `CrLfOnly` requires CRLF outside quoted fields. Quoted contents preserve
their line endings and whitespace. Delimiters and quotes are distinct ASCII
symbols (tab is also supported). A quote opens only at field start; unexpected
quotes and bytes after a closing quote are errors. There are no implicit comments
or backslash escapes.

Empty input has no records. A blank line is one empty field. A final terminator
does not create another record. Trailing delimiters create empty fields. Strict
width is the default, established by the first record; `Flexible` and `Exact n`
are explicit alternatives.

Positions contain a zero-based byte offset and one-based physical line and byte
column. Record and field indexes are separate and zero-based. Spans are half-open
and describe original field syntax, including quotes. Records own field bytes;
they do not retain the whole input. Text mode validates UTF-8 incrementally and
strips one initial UTF-8 BOM by default; byte mode preserves it by default.

Feed produces at most one record and reports the first unconsumed chunk offset.
Pure parser values are reusable snapshots. EOF is explicit through `finish`.
Chunk boundaries, including split UTF-8, BOM, CRLF and doubled quotes, do not
change results. Field assembly uses a growable byte-word buffer and one final
materialization rather than repeatedly concatenating prefixes.

Run contracts with `fai test -C packages csv`.
