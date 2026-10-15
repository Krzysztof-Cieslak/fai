# Encoding

Version **0.1.0** · Requires **Fai 0.1.x** · [Changelog](changelog.md)

```sh
fai add github:Krzysztof-Cieslak/fai --subdir packages/encoding --as encoding --version '^0.1.0'
```

Pure Fai binary-to-ASCII codecs following [RFC 4648](https://www.rfc-editor.org/rfc/rfc4648).
Declare `encoding` in your application's `fai.toml`, then import its modules.
For a sibling checkout:

```toml
[dependencies]
encoding = { path = "../encoding" }
```

```fai
import encoding.Base64
import encoding.Base64Url
import encoding.Encoding
import encoding.Hex
```

The repository workspace supplies a local replacement for the portable Git
dependency on `packages/encoding`. The library itself uses only the standard
library; all operations, including incremental feeds, are pure. Its manifest
declares the library, tests, and runnable examples.

| Module | Alphabet | Default output | Default input |
|---|---|---|---|
| `Hex` | `0-9a-f` | Lowercase, two digits per byte | Either letter case, complete pairs |
| `Base64` | `A-Za-z0-9+/` | Padded | Complete padding required when needed |
| `Base64Url` | `A-Za-z0-9-_` | Unpadded | Padding forbidden |
| `Base32` | `A-Z2-7` | Uppercase, padded | Uppercase, complete padding |
| `Base32Hex` | `0-9A-V` | Uppercase, padded | Uppercase, complete padding |

## Whole-buffer operations

Every module provides these functions (with its own option record types):

```fai
encode : Bytes -> String
encodeWith : EncodeOptions -> Bytes -> String
decode : String -> Result Bytes Encoding.Error
decodeWith : DecodeOptions -> String -> Result Bytes Encoding.Error
decodeBytes : Bytes -> Result Bytes Encoding.Error
decodeBytesWith : DecodeOptions -> Bytes -> Result Bytes Encoding.Error
encodedLength : Int -> Result Int Encoding.Error
encodedLengthWith : EncodeOptions -> Int -> Result Int Encoding.Error
```

```fai
Base64.encode (Bytes.fromString "hello") // "aGVsbG8="
Base64Url.encode (Bytes.fromList [251, 255]) // "-_8"
Hex.decode "00fF" // Ok (Bytes.fromList [0, 255])
```

Encoded output is ASCII. Decoded output is arbitrary binary data; use
`Bytes.toString` explicitly when it should be UTF-8. Empty input succeeds in every
format. Encoders never insert line breaks. Decoders reject whitespace, prefixes,
separators, foreign alphabets, malformed padding, and nonzero unused trailing bits.

Length helpers calculate exact output sizes without constructing the output.
Negative byte counts return `InvalidLength`; sizes exceeding signed `Int` return
`Overflow`. Ordinary output allocation follows the runtime's allocation limits.

## Explicit options

Each module exports `encodeOptions` and `decodeOptions` containing its defaults.
For Base64, Base64Url, Base32 and Base32Hex:

```fai
Base64.encodeWith { padding = Encoding.Unpadded } bytes
Base64Url.decodeWith { padding = Encoding.AllowPadding } text
Base32.decodeWith {
  caseInsensitive = true,
  padding = Encoding.ForbidPadding
} text
```

Encoding padding is `Encoding.Padded` or `Encoding.Unpadded`. Decoding padding is:

- `RequirePadding`: incomplete final groups require their exact `=` suffix.
- `AllowPadding`: accept exact padding or its complete omission.
- `ForbidPadding`: reject every `=`.

Full groups need no padding under any policy. Optional padding still rejects a
partial suffix, extra padding, and data after a padded group. Unused bits must
always be zero. Base32's `caseInsensitive` folds ASCII letters only, without
mapping visually similar digits to letters. Base64 remains case-sensitive.

Hex encoding takes `{ letterCase = Encoding.Uppercase }` or `Lowercase`. Its
decoder takes `{ letterCase = None }` for either case, or
`{ letterCase = Some Encoding.Lowercase }` / `Some Encoding.Uppercase` to restrict
letters. Hex accepts neither padding nor an odd number of digits.

## Errors

`Encoding.Error` is `{ kind : ErrorKind, message : String, offset : Option Int }`.
`Encoding.errorToString` renders the message with its optional byte location.

Kinds are `InvalidCharacter`, `InvalidLength`, `InvalidPadding`,
`NonCanonicalBits`, and `Overflow`. Input offsets are zero-based bytes in the
original input, including for String input (UTF-8) and across incremental feeds.
EOF errors use the input length; trailing-bit errors locate the last data digit.
Size-helper errors have no offset.

Scanning reports the first detected fault. A padded group is terminal, so any
later byte reports `InvalidPadding`. At EOF the checks are, in order: incomplete
padding, impossible group length, missing required padding, and unused bits.
This ordering and the reported location are independent of chunk boundaries.

## Incremental codecs

Every module creates the shared opaque states:

```fai
encoder : EncodeOptions -> Encoding.Encoder
decoder : DecodeOptions -> Encoding.Decoder
```

Drive them with:

```fai
Encoding.feedEncoder : Encoding.Encoder -> Bytes -> Encoding.EncodeStep
Encoding.finishEncoder : Encoding.Encoder -> String
Encoding.feedDecoder : Encoding.Decoder -> Bytes -> Encoding.DecodeStep
Encoding.finishDecoder : Encoding.Decoder -> Result Bytes Encoding.Error
Encoding.decoderOffset : Encoding.Decoder -> Int
```

`EncodeStep` contains `encoder` and `output : String`. Feed emits complete groups;
finish emits the remaining symbols and padding. For example:

```fai
let first = Encoding.feedEncoder (Base64.encoder Base64.encodeOptions) (Bytes.fromString "f")
let next = Encoding.feedEncoder first.encoder (Bytes.fromString "oo")
let output = first.output ++ next.output ++ Encoding.finishEncoder next.encoder
// "Zm9v"
```

`DecodeStep` contains `decoder`, `output : Bytes`, `error : Option Encoding.Error`,
and `consumed : Int`. Feed emits validated complete groups and retains the
unfinished group. On failure, it returns the validated prefix from that feed;
the returned state retains the error. Later feeds on a failed state consume
nothing and emit nothing. `consumed` is the first unconsumed index within the
supplied chunk. A trailing-bit error can point back to a digit consumed earlier,
even in a previous chunk.

Always finish after the last successful feed and append the returned tail once.
Only successful finalization validates the complete input. A whole-buffer decode
returns either all decoded bytes or an error; incremental callers can observe a
validated prefix before a later error.

States are immutable snapshots. Finalization observes a snapshot: calling it
again returns the same tail, and an older state can independently continue with
another suffix. Empty feeds preserve state. A state retains at most one group
as scalar counters/bits, plus its fixed alphabet and optional error, and does not
retain input chunks or emitted output. Per-feed output storage scales with that
feed's input; fixed-size chunks keep auxiliary storage bounded over long streams.

## Development

```sh
fai fmt --check -C packages --package encoding
fai check --no-examples -C packages --package encoding
fai test -C packages --package encoding --seed 42 --count 128
fai run -C packages encoding/examples/EncodingExample.fai
python3 scripts/package_integrations.py encoding --no-dependents --fai /path/to/fai
```

Contracts cover RFC vectors, independent Python reference vectors for every byte
value, exact errors, size overflow, arbitrary binary round trips, immutable state
branches, and chunk partitions of valid and malformed input. The `ci.json`
integration exercises large buffers and fixed-chunk streams through JIT and AOT.

PostgreSQL authentication, LLM image payloads, and TUI clipboard output use this
package through manifest dependencies and explicit module imports.
`PgAuth.base64Decode` adapts the structured error to its existing String
error channel. Interoperability properties also compare standard Base64 output
with the embedded HTTP helper.
