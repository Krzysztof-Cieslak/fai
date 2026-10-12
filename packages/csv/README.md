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

`Csv.parse` reads String input; `parseBytes` accepts UTF-8 bytes. `CsvBytes.parse`
reads arbitrary bytes. All return checked arrays of opaque records; `fields`,
`field` and `inspect` expose values and metadata. `record`/`recordWith` construct
checked records from arrays and reject zero-field records. `Csv.fromBytes`
explicitly converts a byte record to text, retaining its field metadata.

`parserWith`/`feedFrom`/`finish` expose incremental parsing on both modules.
`Feed.next` is an absolute offset in the supplied chunk, not a count of records.
`NeedInput` requests another chunk; `RecordReady` carries one row. Empty chunks
are not EOF. The parser must receive `finish` when the source actually ends.

`parseTable` consumes the first record as headers. `parseTableWith` selects
`NoHeader`, `FirstRecord` or `Supplied headers`. Strict width follows header width;
flexible mode preserves ragged rows. Headers reject empty and duplicate names;
`headersWith` normalizes names explicitly and checks collisions afterwards.
`headerRecord` retains original spellings/locations, while `headerNames` exposes
normalized names. Text matching is exact and case-sensitive; bytes match exactly.

Whole-input collection budgets include an input header record. They bound both
record count and total decoded field-content bytes. Streaming does not use these
collection budgets. No whole-file byte limit is imposed unless configured.

`CsvDecode.Value 'a` decodes one String field, and `CsvDecode.Decoder 'a` composes
fields into a row. Use `index` or `field`, then `map2`/`map3`/`andMap`. Pass an
optional header to `decode`; named access requires one. Independent failures
accumulate in declaration order (64 by default, configurable with `decodeWith`).
Errors retain field index, header bytes and source span. Extra columns are allowed
unless the row decoder is wrapped in `exactColumns`.

`int` is checked decimal (optional sign, leading zeroes allowed); `float` is
checked finite decimal binary64; `bool` accepts exactly true/false. No reader
trims implicitly: wrap it in `trim` if desired. `custom`/`fromText`, `mapValue`
and `refineValue` define field conversions; `refine` validates the whole row.
Missing columns, empty fields and invalid values are distinct. `optionalField`
and `optionalIndex` handle absence, `emptyAsNone`/`nullToken` give explicit null
policies, and `withDefault` handles a single absent column without hiding invalid
present values. `CsvBytesDecode` supplies the byte-record counterpart, including
raw `bytes` and explicit `utf8` conversion through a text value decoder.

See `examples/People.fai` for a typed row decoder and `fai run -C packages
csv/examples/People.fai` to execute it.
