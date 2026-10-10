# JSON for Fai

A native Fai JSON library. Include `src/*.fai` in your application's workspace;
modules resolve by name. From this repository:

```sh
fai run -C packages/json examples/JsonExample.fai
fai test -C packages/json
```

The package tests are Fai `example`/`forall` declarations and run directly with
`fai test`. From the shared source-package workspace, use
`fai test -C packages json`; repeated calls reuse the warm daemon. The package
can move to another repository together with its `.fai` tests. Repository CI
dependency metadata lives in `ci.json` and is not needed by the test runner.

## Values and documents

`Json.Value` has `Null`, `Boolean Bool`, `Number JsonNumber.Number`,
`String String`, `Array (Array Json.Value)`, and
`Object (Array (String * Json.Value))` cases. Constructors are qualified.

```fai
let value = Json.Object [|
  ("name", Json.String "Fai"),
  ("version", Json.Number (JsonNumber.fromInt 1))
|]
let document = Json.toString value
let again = Json.parse document
```

`Json.parse : String -> Result Json.Value Json.ParseError` and `parseBytes`
read one complete document. Surrounding space, tab, CR and LF are accepted.
Comments, trailing commas, leading byte-order marks, invalid UTF-8, unescaped
control characters and lone UTF-16 surrogate escapes are rejected. A valid
surrogate pair becomes one Unicode scalar. Errors carry a zero-based byte
`offset`, one-based `line` and Unicode-scalar `column`, and a `message`.
`Json.errorToString` renders them for display; LF starts a new line.

`Json.parseWith` and `parseBytesWith` take `{ maxDepth : Int }` first. The
default is 256 open containers. Zero allows scalar documents only; negative
limits return an error. Parsing and rendering use explicit heap frames, so
raising this limit does not consume a native stack frame per container.

`Json.toString` renders compact JSON. `Json.toPrettyString` uses two-space
indentation and no final newline. Both preserve object member order, duplicate
names, and number spellings. String escaping is normalized; non-ASCII Unicode
is emitted directly. This is deterministic rendering, not canonical JSON for
cryptographic signing. Object order and numeric spelling participate in the
ordinary structural equality of a value tree.

## Exact numbers

`JsonNumber.Number` is opaque and retains validated JSON number text. It accepts
arbitrarily long integers and decimal exponents without converting them to a
machine number. `fromString` validates a complete number without whitespace;
`toString` returns its exact spelling. Thus `1`, `1.0` and `1e0` remain distinct.

- `fromInt : Int -> Number` is exact over all signed 64-bit values.
- `toInt : Number -> Result Int String` accepts exactly integral, in-range
  values, including `1.00e3`. Fractional values and overflow are errors.
- `fromFloat : Float -> Result Number String` rejects NaN and infinity and
  preserves finite values, including negative zero, through a Float round trip.
- `toFloat : Number -> Result Float String` rounds to nearest binary64, ties
  to even. Overflow is an error; underflow produces a subnormal or signed zero.
  Exact decimal comparisons against rounding boundaries avoid double rounding
  even for long significands. The conversion's private integer work buffers
  are bounded by binary64's exponent range.

All APIs are pure. The implementation uses ordinary public Fai library APIs.

## Typed decoding

`JsonDecode.Decoder 'a` is an opaque, reusable, pure decoder. `decode decoder
value` returns `Result 'a JsonDecode.Error`. `fromString decoder text` and
`fromBytes decoder bytes` additionally parse a document and return
`Result 'a JsonDecode.ReadError`: `Syntax Json.ParseError` or `Data Error`.

```fai
type User = { id : Int, name : String }

let userDecoder = JsonDecode.map2
  (fun id name -> { id = id, name = name })
  (JsonDecode.field "id" JsonDecode.int)
  (JsonDecode.field "name" JsonDecode.string)
```

Primitive decoders are `value`, `string`, `bool`, `number`, `int`, `float`, and
`null result`. `array decoder` and `list decoder` decode JSON arrays;
`keyValuePairs decoder` preserves object member order, while `dict decoder`
builds a `HashDict String 'a`.

- `field name decoder` requires a field. Unknown fields are accepted.
- `optionalField name decoder` maps **absence** to `None`; a present field must
  satisfy its decoder, even if null. `nullable decoder` separately maps **null**
  to `None`. Combining them distinguishes missing, null, and a decoded value.
- `index position decoder` selects an array element; `at names decoder` follows
  a sequence of object fields.
- `map` transforms a decoded value. `map2` combines two decoders over the same
  input. For larger records, use `succeed constructor |> andMap fieldDecoder`
  repeatedly; the first failure follows field declaration order.
- `andThen choose decoder` selects a decoder based on a decoded value. The
  selected decoder sees the **original input**, useful for tagged unions.
- `oneOf decoders` tries alternatives in order and retains their failures.
- `lazy (fun _ -> decoder)` supports recursive types. `fail message` and
  `fromFunction` support custom validation.

Every public decoding entry point rejects duplicate object names **throughout
the tree**, including unknown fields, before running the decoder. Keys are
compared after escape decoding, so `"a"` and `"\u0061"` collide. Validation runs
once; child combinators do not repeat the complete-tree scan.

Errors are `Failure path message` or `Alternatives errors`. Paths contain
`Field name` / `Index position` segments. `errorToString` and `readErrorToString`
render unambiguous paths, for example `$["users"][2]["age"]`.

## Typed encoding

`JsonEncode.Encoder 'a` is the function type
`'a -> Result Json.Value JsonEncode.Error`. Define encoders as ordinary functions:

```fai
encoder : JsonEncode.Encoder User
let encoder user = JsonEncode.object [
  ("id", JsonEncode.int user.id),
  ("name", JsonEncode.string user.name)
]
```

Primitive encoders are `value`, `string`, `bool`, `number`, `int`, `float`, and
`null` (for Unit). `array encoder`, `list encoder`, `nullable encoder`, and
`dict encoder` compose them. `dict` sorts keys for stable output. `object` takes
already encoded field results, preserves their order, and rejects duplicate
names. `value` explicitly passes an existing raw tree through, including any
duplicates. To omit an optional field, omit its entry from the field list;
`nullable` encodes `None` as null.

`contramap projection encoder` adapts an encoder to a larger application type.
`toString encoder value` and `toPrettyString encoder value` encode and render.
Failures carry `path` and `message`; `errorToString` renders them. Encoding a
non-finite Float is an error, never a null or a nonstandard numeric token.

Run the complete record example with:

```sh
fai run -C packages/json examples/CodecExample.fai
```
