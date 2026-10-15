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
change results. Field assembly uses reversed byte and field lists with one final
materialization, keeping each incremental append constant-time even with parser
snapshots sharing earlier input.

Run named tests and properties with `fai test -C packages csv`.

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

`CsvEncode.toString`/`toBytes` encode arrays of text records; `CsvBytesEncode`
encodes byte records. Options select minimal/always quoting, CRLF/LF/CR endings,
strict/flexible widths and optional BOM output. Empty documents emit a BOM only
when requested. Minimal text output quotes a literal BOM at the start of the first
field so the default reader preserves it. Every record receives a terminator.

`CsvEncode.schema` validates ordered named columns once. A `column` combines a
name, a scalar encoder and a projection from the application value. `encode`
produces a record and `header` its ordered header record. Scalar encoders include
strings, exact integers, booleans, finite Floats and explicit Option tokens.
Custom conversion failures retain column names/indexes and accumulate up to the
configured error limit. Schema/limit validation precedes column evaluation.

`CsvWrite` is the shared immutable writer state: `push` validates and assembles
one complete record, and `finish` completes an empty BOM-enabled document.
Per-record limits include quoting/delimiters but exclude the terminator and BOM;
the optional total-output limit includes both. Whole-document encoding has a
separate 32 MiB default buffer limit. It converts one input record at a time.

## Streaming

`CsvStream.reader options headerMode chunks` accepts `Stream Bytes 'e` and
returns a checked text reader. `byteReader` is the raw-byte counterpart.
`fromPull`/`bytesFromPull` accept an explicit state and a typed pull function,
retaining its original error type. Empty chunks are allowed; only source EOF
finishes parsing. `Csv.strictOptions` and `CsvBytes.strictOptions` require CRLF
record endings outside quoted fields.

`next`/`nextBytes` return `(optionalRow, nextReader)`, including the final reader
at EOF so header-only files still expose their headers. Each row contains its
record, current headers and ending source position. Advance each continuation
once when its source performs I/O; pure parser snapshots remain reusable.

```fai
let countRows reader =
  CsvStream.fold CsvStream.next reader 0 (fun count row ->
    Ok (count + 1, CsvTypes.Continue))
```

The explicit step function lets the source and callback arrows forward their
effects together. A fold retains application state, parsed data-row count,
completed callback count, last committed position and an outcome. `Stop` is a
successful early return. A callback failure leaves its previous state committed;
source, syntax and callback failures stay distinct. No subsequent input is
parsed or pulled after Stop, though the current chunk may contain unread bytes.
Header records are excluded from fold callback counts.

`collect`/`collectBytes` return a table and enforce collection limits for records
consumed during that call, including an input header. Ordinary streaming folds
have no implicit total-input or total-record cap. To quarantine schema failures,
decode each well-formed row in the callback and choose how to advance. Flexible
width mode supports the same approach for ragged records. Syntax/UTF-8 failures
remain terminal; malformed quoted input cannot be reliably resynchronized.

`CsvStream.write options read state sink` and `writeBytes` take explicit source
and sink functions with typed errors. `writeEncoded` converts application values;
`writeEncodedWithHeader` emits the encoder's header first. Confirmed record/byte
counts are retained on failure; `uncertainRecord` identifies a record whose sink
write failed and may have partially completed. Counts include emitted headers
and BOM bytes. They describe completed writes, not durability.

## Files and limits

`CsvIO.reader`/`byteReader` and `sink` borrow existing file handles. `withFile`/
`withBytesFile`, `foldFile`/`foldBytesFile`, and `writeFile`/`writeBytesFile` own
scoped handles. They validate options before opening files, close under masked
cancellation, and retain a `closeError` alongside the callback/fold/write result.
After scope exit, retained aliases cannot perform further file reads. File
writers take an explicit `FileSystem.WriteMode`. Existing atomic-writer scopes
can use `CsvIO.writeWriter` when replacement needs atomic commit semantics.

| Limit | Default |
|---|---:|
| Decoded field bytes | 1 MiB |
| Raw/encoded record bytes, excluding terminator | 8 MiB |
| Fields per record | 4,096 |
| Decoder/typed-encoder errors per row | 64 |
| Collected source records | 100,000 |
| Collected field-content bytes | 32 MiB |
| Whole-document encoded buffer | 32 MiB |
| File read chunk | 64 KiB |
| Total input bytes / source records / output bytes | Unlimited unless configured |

All budgets are configurable and failures are explicit. Streaming working
storage depends on the current record, header and input chunk rather than total
file size. A caller retaining old parser snapshots or result rows retains those
values normally. Parsing and field assembly are linear in input size.

## Verification

Named Fai tests cover byte/text round trips, every two-chunk split of
generated inputs, generated multi-chunk partitions, locations, headers, quotas,
typed failures and writer progress. The executable fixtures exercise 20,000-row
streams, multi-chunk fields, actual files, expired handles, early stop, typed
headers, and mixed source/callback effects through JIT and AOT:

```sh
fai test -C packages csv
fai run -C packages csv/examples/RuntimeChecks.fai
fai run -C packages csv/examples/StreamChecks.fai
fai run -C packages csv/examples/StressChecks.fai
```

The package and its test logic are Fai. CI invokes these same programs on Linux,
macOS and Windows; no compiler or standard-library extension is needed.
The programs are declared in `examples/ci.json`; run them with
`python3 scripts/package_integrations.py csv --fai /path/to/fai`.
