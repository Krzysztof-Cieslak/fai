# Configuration

Pure sourced configuration trees, explicit loading, and typed application settings.
The package uses the `toml` and `dotenv` source libraries; examples use `cli`.

`Config.fromValue` builds a typed layer with an explicit origin, `Config.toml`
parses a located layer, and `ConfigSource.snapshot` maps supplied name/value
pairs through explicit `{ name, path }` bindings. Paths are lists of literal key
segments: `["a.b"]` differs from `["a", "b"]`. Text inputs are preserved as text
until decoding. Missing names contribute no override; empty strings are present.

`Config.merge` overlays layers in application-selected order, with later values
winning. Tables merge recursively; arrays and scalars replace as a whole.
`Config.origin`/`explain` report the winning source; merged tables have composite
origins while their children retain individual provenance. No overridden history
is retained. TOML source spans are resolved lazily when inspecting an origin.

`ConfigSource.load capabilities limits base sources` accepts `Prepared`, `Toml`,
`Dotenv` and `Environment` sources. It requires an explicit absolute base Path and
uses existing FileSystem/Env capabilities. The complete plan is validated before
I/O. Optional files skip only NotFound; parse, permission and encoding failures
remain errors. Every selected file is parsed, even when its values are overridden.
Each distinct requested environment name is read once per Environment source.
No enumeration, interpolation or process-environment mutation occurs.

Defaults: 4 MiB source/bound-text bytes, 100,000 tree nodes, depth 128, 64 layers,
1,024 bindings and 64 decode errors. Limits are explicit and configurable.
Duplicate target paths and parent/child conflicts in a binding set fail. One
variable may map to several distinct paths; each mapped value counts toward the
bound-text budget. Pure merge validates each layer and each intermediate result.

Run native contracts with `fai test -C packages config`.

## Typed decoding

Compose `ConfigDecode.field`, scalar/collection readers and `map2`/`map3`/`andMap`
into application records, then call `ConfigDecode.decode`. Independent fields
accumulate errors in declaration order up to `maxErrors`. Objects reject unknown
fields by default, including nested records. `allowUnknown` relaxes just the
wrapped boundary; dictionary readers deliberately consume arbitrary keys.

Native TOML scalars retain their kind: a TOML string `"42"` is not an integer.
Environment and textual CLI values are parsed by the selected scalar reader.
Integers use checked decimal syntax (optional sign, leading zeros permitted;
no whitespace, underscores or radix prefixes). Booleans use exact `true`/`false`.
Float readers accept decimal text, with non-finite values only through `floatAll`.
`fromText` explicitly opts into a custom string parser, including structured text.

`optionalField`, `optional` and `withDefault` act only on absence: invalid present
values remain errors. `refine`, `range`, `choice` and `andThen` support field and
whole-record validation. `oneOf` tries complete alternatives and retains bounded
failure details. Errors include the configuration path and winning origin without
embedding raw values in generated error messages.

`path` performs checked native-path conversion. `pathRelativeTo base` resolves
against an explicit absolute base; `pathFromSource fallback` prefers the winning
file's directory and uses the explicit fallback for environment/CLI/defaults.
Neither operation reads the working directory. Date/time readers preserve TOML
kind and reject precision/offset values unrepresentable by the standard types.
