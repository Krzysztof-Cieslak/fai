# TOML

A pure Fai TOML 1.1 library. The scalar foundation preserves temporal spellings
and precision, checks signed 64-bit integers, and rounds decimal floats using
the JSON package's checked binary64 converter. Explicit `inf` and `nan` tokens
remain distinct from finite overflow. String scanning supports TOML 1.1 escapes
and normalizes multiline line endings to LF while retaining original byte spans.

Run named tests and properties with `fai test -C packages toml`.

`Toml.parse`/`parseBytes` return located documents; `Toml.value` extracts the value
tree, and `Toml.find` retrieves a node using explicit `Key`/`Index` path segments.
Value constructors live in `TomlTypes`. Arrays preserve order; tables have sorted
case-sensitive keys. Definition tracking distinguishes dotted, implicit,
declared and sealed inline tables and the current element of an array of tables.

`Toml.toString` renders deterministic TOML 1.1 with sorted quoted keys and inline
containers. Comments/layout are not retained. Float values keep their numeric
kind and zero sign; NaN payloads normalize to signed canonical NaNs. Temporal
spelling and precision are retained. `TomlDecode` supplies strict scalar,
field, optional-field, collection and applicative decoding combinators.

Parser defaults are 4 MiB input, depth 128 and 100,000 nodes. `parseWith` and
`parseBytesWith` accept explicit limits. Rendering has separate limits (16 MiB
output by default). Container parsing, tree construction and value extraction
use explicit traversal frames. Error locations use original UTF-8 byte offsets
and one-based scalar line/columns, with both definitions shown for duplicates.

## Conformance

The Fai executable `examples/Conformance.fai` reads the upstream TOML 1.1 corpus,
checks all valid values against its JSON oracle, rejects invalid inputs, and
round-trips normalized output. The reviewed corpus is `toml-lang/toml-test` at
`ff49d109861c1ad25af53f687f2aef19ab650600` (712 TOML files).
The package's `ci.json` pins that fixture and declares the JIT/AOT entrypoint.
`python3 scripts/package_integrations.py toml --fai /path/to/fai` prepares it
and runs the conformance program plus selected dependent-package integrations.

```sh
fai run -C packages toml/examples/Conformance.fai -- /path/to/toml-test/tests
```
