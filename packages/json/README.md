# JSON for Fai

A native Fai JSON library. Include `src/*.fai` in your application's workspace;
modules resolve by name. From this repository:

```sh
fai run -C packages/json examples/JsonExample.fai
fai test -C packages/json
```

The complete package-local suite uses an **existing compiler binary**, with no
Cargo invocation or compiler rebuild:

```sh
python3 packages/json/test/run.py --fai /path/to/fai
```

It checks formatting and types, runs the Fai contracts, exercises JIT and AOT,
and compares seeded native numeric/JSON results with Python's standard library.
The package can move to another repository together with its tests and runner.

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
