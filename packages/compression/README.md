# Compression

Version **0.1.0** · Requires **Fai 0.1.x** · [Changelog](changelog.md)

```sh
fai add github:Krzysztof-Cieslak/fai --subdir packages/compression --as compression --version '^0.1.0'
```

Bounded, pure gzip, zlib and raw DEFLATE codecs, with immutable incremental
snapshots and effect-forwarding streaming adapters. This package requires a
compiler containing the `CompressionEngine` standard bridge, available from
Fai 0.1.0; the original 0.0.1 binary does not contain it.

```fai
import compression.Gzip

let packed = Gzip.compress (Bytes.fromString "hello")
let original = match packed with
| Ok bytes -> Gzip.decompress bytes
| Err error -> Err error
```

`Gzip`, `Zlib`, and `Deflate` expose `compress`, `decompress`, their `With`
variants, and `encoder`/`decoder` constructors. `Compression` supplies the same
operations with an explicit `Format` (`Gzip`, `Zlib`, `RawDeflate`). Inputs and
outputs are `Bytes`; no text conversion or I/O occurs implicitly.

## Formats and validation

Gzip encoding emits one member with zero timestamp, no filename/comment, and
OS 255. Decoding accepts concatenated members and checks every header, optional
header checksum, payload checksum and modulo-2^32 length. Header extensions are
bounded; names and comments are discarded rather than accumulated. Reserved
flags and unsupported methods fail. Zlib checks its header and Adler-32 trailer;
preset dictionaries return `DictionaryRequired`. Raw DEFLATE has no checksum.

Whole-buffer decoding requires complete input consumption and rejects trailing
data. Empty compressed input is truncated, while an encoded empty value is valid.
Formats are explicit: there is no sniffing or fallback from zlib to raw DEFLATE.

The encoder uses canonical 64 KiB blocks with independent compression histories.
Input chunk sizes do not create flush boundaries. This bounds comparable native
snapshots without retaining the input history; compression across block boundaries
can be less effective than a continuous-history encoder. Encoded bytes are
deterministic for a compiler version, not a compatibility promise across releases.

## Options and limits

`Compression.encodeOptions` has `level = 6` (accepted levels 0–9) and `limits`.
`Compression.decodeOptions` contains `limits`. Defaults are:

| Limit | Default |
|---|---:|
| `maxInputBytes` | 64 MiB |
| `maxOutputBytes` | 64 MiB |
| `chunkBytes` | 64 KiB |
| `maxHeaderBytes` | 64 KiB per gzip member |
| `maxMembers` | 1,024 |

Totals cover the complete operation, including every concatenated member.
Callers can raise total limits for large streams. `chunkBytes` must be in
1..1048576; other limits are nonnegative. Input and output limits are independent:
incompressible input can exceed the output limit during compression. Invalid
options fail before reading a source. A ratio limit never replaces an absolute
output bound.

`Error` contains `kind`, `format`, `message`, optional zero-based input `offset`
and `member`, and cumulative `inputBytes`/`outputBytes`. Offsets report detection
points, not an invented precise malformed-bit position. Kinds distinguish invalid
options/headers, corrupt or truncated data, checksum failures, dictionaries,
trailing input, resource limits, counter overflow and internal engine failures.

## Incremental snapshots

`feedEncoder encoder bytes offset` and `feedDecoder decoder bytes offset` return
a `Step` containing `state`, `output`, `consumed`, `status`, cumulative byte counts
and completed members. Advance the input offset by exactly `consumed`.

Statuses are `Progress`, `NeedInput`, `Flushed`, `MemberEnd`, `Finished`, and
`Failed error`. Each call emits at most `chunkBytes`; `Progress` can require
another call on the same input remainder, including an empty remainder while
pending output drains. `NeedInput` requests another chunk. `MemberEnd` validates
one gzip member, not the entire input. Low-level raw/zlib decoders stop precisely
at their boundary; callers retain and interpret the unconsumed suffix.

`flushEncoder` performs an explicit sync flush without finishing the stream.
Call until `Flushed`. `finishEncoder`/`finishDecoder` declare EOF and must be
repeated until `Finished` or `Failed`. They may emit output. No input can be added
after EOF. Failure is sticky. A step can return an emitted prefix with its error;
whole-buffer helpers instead return only `Err`.

States are immutable values: retained snapshots can branch and be shared across
tasks. Native storage updates only when uniquely owned. Comparisons and hashing
use deterministic state content, never pointer identity; they can be expensive.
Snapshots are not a persistent storage or cross-version serialization format.

## Streaming

`CompressionStream.encoder` / `decoder` take a format, options, and
`Stream Bytes 'e`, returning a typed `Reader String 'e`. `encoderFromPull` and
`decoderFromPull` accept custom state-threading byte sources with typed errors.

`next` returns an output chunk and continuation, a typed source/codec failure, or
`Ok (None, reader)` only after complete validation. `fold` threads application
state through callbacks, preserving source and callback errors separately.
Its result retains committed state, delivered bytes, chunk count, outcome and
`complete`; early `Stop` leaves `complete = false`. Callback effects propagate.

`toStream` explicitly renders errors into Stream's String error channel.
`CompressionStream.compress` / `decompress` combine construction and that adapter.
No source is read merely by constructing an adapter. Resource ownership follows
the underlying source; drop releases the remaining source and codec snapshot.
Consume I/O-backed reader continuations once, just like an I/O-backed `Stream`.
Branchable codec snapshots do not make external input effects replayable.

**Emitted decompression output is provisional until complete validation.**
Checksums can fail after bytes were delivered. Early consumers do not validate
the remainder, and checksums are not authentication. Compose file writes with
`File.withAtomicWriter` when failed validation must not publish a partial file.

## Verification

```sh
fai fmt --check -C packages compression
fai test -C packages compression
python3 scripts/package_integrations.py compression --no-dependents --fai /path/to/fai
```

Native engine tests cover independent zlib-rs interoperability, arbitrary binary
inputs/chunking, shared snapshots, header/trailer failures and bounded storage.
Named package tests and JIT/AOT fixtures cover the public APIs and streamed effects.
Archives, HTTP content negotiation, Brotli and Zstandard are separate future work.
