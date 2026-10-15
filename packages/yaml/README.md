# YAML

Version **0.1.0** · Requires **Fai 0.1.x** · [Changelog](changelog.md)

```sh
fai add github:Krzysztof-Cieslak/fai --subdir packages/yaml --as yaml --version '^0.1.0'
```

A pure Fai YAML 1.2 data library: located document graphs, exact numeric values,
typed decoding/encoding, normalized rendering, and JSON conversion. Declare a
`yaml` dependency in your `fai.toml` and import the modules you use:

```fai
import yaml.Yaml
import yaml.YamlDecode
import yaml.YamlData
```

The source package depends on `json` for exact decimal conversion and quoted
string output. Parsing and conversion never perform I/O or instantiate native
objects from YAML tags.

## Parse documents, load application data

```fai
let document = Yaml.parse "name: Ada\nage: 37\n"
let user = YamlDecode.map2
  (fun name age -> { age = age, name = name })
  (YamlDecode.field "name" YamlDecode.string)
  (YamlDecode.field "age" YamlDecode.int)
let decoded = YamlDecode.fromString user "name: Ada\nage: 37\n"
```

`Yaml.parse`/`parseBytes` return one `YamlDocument.Document`; `parseAll`/
`parseAllBytes` return every document in a YAML stream. Empty/comment-only streams
have no documents. Single-document parsing supplies an empty/null root for such
input and rejects a second document. The `With` variants take `Yaml.Options`.

`parseBytes` detects UTF-8, UTF-16 LE/BE and UTF-32 LE/BE, including their BOMs and
YAML's zero-byte detection. Invalid encoding and disallowed raw control characters
are located errors. CRLF and CR line endings normalize to LF in scalar content;
locations still refer to original bytes. String entry points use UTF-8.

This is whole-input parsing, not a chunk-feed API. Defaults are 4 MiB input,
1024 documents, 100000 nodes per document, 1 MiB per decoded scalar, and depth
128. Parser nesting can be lowered but not raised beyond 128. Negative limits
and excessive depth settings are errors; limits never silently truncate data.

## Located document graphs

`YamlDocument.root`, `node`, `nodes`, `span`, `index` and `directives` expose an
immutable arena. Node IDs are local to their owning document. Nodes contain:

- Original source spans and optional anchor names.
- Expanded explicit tag URIs.
- Scalars with style and decoded text.
- Sequences of node IDs and ordered mappings of arbitrary key/value node IDs.
- Aliases with their name and resolved preceding target ID.

Aliases stay references during parsing. Shared values, cycles, complex keys and
duplicate mapping entries therefore have finite, inspectable representations.
An undefined alias is a parse error. Anchor names may be reused according to YAML
scoping; aliases bind the most recent preceding definition. Tag directives apply
only to their document. YAML 1.x version directives are accepted as syntax, while
application conversion consistently uses the configured YAML 1.2 Core policy.

`source` retains the original input buffer; `spelling` retrieves a node's raw
syntax bytes, including its properties. Documents from a stream share that whole
buffer until released. Comments and layout are not modeled for editing.
`YamlEvents.events`
provides a finite event view that does not follow aliases. Its diagnostic text
normalizes collection presentation and document-marker style.

## Application values and Core schema

`Yaml.value document` returns `Result YamlData.Value YamlTypes.Error`.
`Yaml.load`/`loadBytes` combine parsing and projection. The application value
constructors are:

```text
Null | Boolean Bool | String String
Integer YamlNumber.Integer | Float YamlNumber.Floating
Sequence (Array Value) | Mapping (Array (String * Value))
```

Mappings preserve insertion order and require string keys. Projection rejects
duplicate explicit keys throughout the tree, including unused fields, with both
definitions located. A key such as unquoted `true` resolves to a Boolean and is
rejected by this layer; quote it to obtain the string key `"true"`.

Core schema recognizes its null, boolean, integer and floating spellings. Quoted
strings remain strings unless explicitly tagged. `on`, `yes`, timestamps,
underscored numbers, binary integer syntax and sexagesimal values remain strings.
Scalar spelling remains separately available in the document.

`YamlProject.treeWith` exposes the located application tree used by typed
decoders; `treeValue`, `treeSpan`, `treeItems` and `treeFields` inspect it. Default
projection limits allow depth 128 and 100000 expanded node visits. Alias cycles
produce `CyclicAlias`; repeated aliases consume the budget on every expansion.

## Numbers

`YamlNumber.Integer` retains a normalized exact decimal integer, including
values beyond 64 bits. `parseInteger` accepts Core decimal, octal and hexadecimal
forms; `integerToString` renders decimal. `integerFromInt`, `integerToInt`, and
`integerToFloat` provide exact construction and checked machine conversion.

`YamlNumber.Floating` is `Finite JsonNumber.Number`, `PositiveInfinity`,
`NegativeInfinity`, or `NaN`. `parseFloat` and `floatToString` preserve exact finite
decimal information rather than rounding while loading. `floatToFloat` rounds
finite data to binary64 with ties to even and rejects finite overflow. Explicit
infinities/NaN map to their native values. `floatFromFloat` preserves finite
binary64 values and classifies non-finite values.

## Merge keys and custom tags

Default conversion treats `<<` as an ordinary string key. Enable the legacy merge
extension explicitly:

```fai
let options = { YamlProject.defaults with mergeKeys = true }
let result = Yaml.loadWith Yaml.defaults options source
```

An unquoted `<<` or explicitly tagged merge scalar accepts a mapping or sequence
of mappings. Explicit entries override inherited ones regardless of position;
earlier mappings in a merge sequence win. Duplicate explicit keys still fail.
Projection emits explicit fields in source order followed by unoverridden merged
fields in merge order. Quoted `"<<"` is not special. Cyclic merges and invalid
operands fail without unbounded expansion.

Custom tags remain visible in documents. Ordinary value projection fails with
`UnknownTag` unless a pure handler is registered:

```fai
let upper = {
  tag = "!upper",
  run = fun content ->
    match content with
    | YamlProject.Scalar _ text -> Ok (YamlData.String (String.toUpper text))
    | _ -> Err "expected a scalar"
}
let options = { YamlProject.defaults with handlers = [upper] }
```

Handlers receive scalar style/text or converted sequence/mapping children. A
custom mapping handler can interpret non-string keys explicitly. Handler outputs
are validated, duplicate handler registrations fail, and Core tags cannot be
overridden. Errors retain the tagged node's source location. No runtime class
loading, environment access or file access occurs through tags.

## Typed codecs and JSON

`YamlDecode` offers `string`, `bool`, `integer`, checked `int`/`float`, `null`,
`nullable`, `field`, `optionalField`, `index`, `list`, `array`, `keyValuePairs`,
`dict`, `map`, `map2`, `andMap`, `andThen`, `oneOf`, `lazy`, and custom `fromFunction`.
Missing fields and explicit null remain distinct. `decodeDocumentWith` accepts
projection options; `fromString`/`fromBytes` load with defaults, and `decode` accepts
an existing application value. Failures retain field/index paths and available
source spans; alternatives retain every branch's error.

`YamlEncode` uses ordinary pure encoder functions returning `Result Value Error`.
Primitive encoders, lists/arrays, nullable values, mapping builders and `contramap`
compose; `object` rejects duplicate names. `toString` validates and renders.

`YamlJson.toJson` and `fromJson` preserve exact finite numeric values and mapping
order, reject duplicates, and reject YAML infinity/NaN when converting to JSON.
Integer spellings normalize, including integer negative zero; original spelling
belongs to the YAML document or original JSON tree, not the application value.

## Rendering

`Yaml.toString` / `toStringWith` validate application data and emit UTF-8 YAML
with two-space block indentation and a final LF. String values and mapping keys
are quoted, avoiding implicit type changes. Default mapping order is insertion
order; `sortKeys = true` sorts keys explicitly. Output limits are independent of
input limits: 16 MiB output and depth 128 by default.

`documentToString` / `YamlRender.documentWith` emit normalized flow-style graphs,
retaining tags, anchors, complex keys and aliases without expanding them.
`toStringAll` and `documentsToString` and their `With` variants share one output
budget across a stream, emitting explicit `---` and `...` markers.

## Diagnostics and verification

`YamlTypes.Error` carries kind, message, zero-based document index, application
path, optional primary span and optional secondary span. Positions contain the
original zero-based byte offset and one-based Unicode-scalar line/column. These
coordinates refer to the original input even for UTF-16/UTF-32 and CRLF input.

```sh
fai fmt --check -C packages --package yaml
fai test -C packages --package yaml --seed 42 --count 128
python3 scripts/package_integrations.py yaml --no-dependents --fai /path/to/fai
```

The conformance executable accounts for all **402** cases in the published
`yaml-test-suite` **data-2022-01-17** release, pinned to commit
`6e6c296ae9c9d2d5c4134b4b64d01b29ac19ff6f`. It compares normalized parser events,
rejects every invalid case, reparses rendered graphs, and compares application
values where projection applies. Event comparison intentionally ignores flow
versus block collection style and explicit document-marker presentation.
Both conformance and large-input/alias/runtime fixtures run through JIT and AOT.
Named tests independently cover Core resolution, Unicode encodings and byte
locations, typed codecs, merges, tag handlers, cycles and configured limits.
