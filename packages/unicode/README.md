# Unicode text

Pure Fai Unicode **18.0.0** segmentation, case folding, normalization, and byte
provenance. The package has no dependencies and does not change the compiler's
`String`, regular expression, or terminal text behavior.

```fai
import unicode.UnicodeCase
import unicode.UnicodeGrapheme
import unicode.UnicodeNormalize

example: UnicodeCase.fold "Straße" = Ok "strasse"
example: UnicodeGrapheme.segments "a\u{301}👩‍💻" = Ok [| "a\u{301}", "👩‍💻" |]
example: UnicodeNormalize.normalize UnicodeNormalize.NFC "a\u{301}" = Ok "á"
```

## Segmentation

`UnicodeGrapheme` implements the default extended grapheme cluster boundaries in
UAX #29, including Unicode 18's updated Indic linker rule. `UnicodeWord`
implements default word boundaries, without dictionary or locale tailoring.
Word segments include whitespace, punctuation and symbols, not just letter-like
words. Concatenating either module's segments reconstructs the input exactly.

Both modules provide:

| Function | Result |
|---|---|
| `segments text` | `Result (Array String) Unicode.Error` |
| `spans text` | `Result (Array Unicode.Span) Unicode.Error` |
| `cursor text` | `Result Cursor Unicode.Error` |
| `next cursor` | `Result (Option (Unicode.Span * Cursor)) Unicode.Error` |

Construction and collection have `With` counterparts taking `Unicode.Limits`
first. Cursors retain immutable input bytes and tables, not a materialized array
of scalars or boundaries. Aliases advance independently. Each successor retains
its cumulative work, scalar and segment counts; exhausted cursors return `None`
without requiring another segment budget. Scanning is iterative and has bounded
look-ahead state. A word punctuation rule may scan ignored characters ahead,
which is charged against the same work budget.

Spans are zero-based, half-open **UTF-8 byte** ranges. They are not character,
grapheme, or terminal-cell indexes. Empty input has no segments. `Unicode.slice`
checks that a range is in bounds and contains whole scalars before copying it.
Ranges do not retain their input. Segment Strings own their bytes.

## Case folding

`UnicodeCase.fold` is full, default, locale-independent case folding. It supports
multi-scalar mappings such as `ß → ss`. It does not perform Unicode normalization
and is not locale-sensitive lowercasing. For example, `İ` folds to `i` followed by
a combining dot, while dotless `ı` stays distinct. Turkic tailoring and general
upper/lower/title casing are not exposed by this package.

`foldWith limits text` selects budgets; `foldMapped` and `foldMappedWith` also
return exact original-byte provenance.

## Normalization

`UnicodeNormalize.normalize form text` supports:

- `NFC`: canonical decomposition and composition.
- `NFD`: canonical decomposition.
- `NFKC`: compatibility decomposition and composition.
- `NFKD`: compatibility decomposition.

Compatibility forms can discard distinctions such as ligatures and width. They
are never applied implicitly. Hangul normalization is algorithmic; canonical
ordering is stable, including equal combining classes and composition blocking.
Ordering long combining sequences uses a bounded sort rather than repeated
insertion into a growing sequence.

`normalizeWith` takes explicit limits. `isNormalized`/`isNormalizedWith` compare
the input to that transformation and use the same budgets. `normalizeMapped`
and `normalizeMappedWith` preserve provenance as described below.

## Source mappings

Mapped operations return:

```text
{ text : String, mappings : Array Mapping }
Mapping = { output : Span, sources : Array Span }
```

There is one mapping per output scalar. Its source ranges identify the actual
contributing original scalars; adjacent or overlapping ranges are coalesced.
Mappings are not one-to-one: a folded `ß` gives two output scalars sharing one
source range; a composed character may have multiple noncontiguous sources.
Canonical reordering can make source positions nonmonotonic. Mappings cover the
output in order and retain no original String. Plain transformations do not
construct provenance lists.

`Unicode.isWhitespace` uses this version's Unicode `White_Space` property. It does
not classify zero-width space or BOM as whitespace. `Unicode.version` exposes the
data/algorithm release independently of the installed compiler's Unicode tables.

## Limits and failures

The shared defaults are:

| Limit | Default |
|---|---:|
| Input UTF-8 bytes | 16 MiB |
| Output UTF-8 bytes | 16 MiB |
| Input/intermediate/output scalars | 4,000,000 |
| Collected or traversed segments | 1,000,000 |
| Nonstarters in a normalization combining run | 65,536 |
| Mapped output entries plus source ranges | 4,000,000 |
| Algorithmic work units | 1,000,000,000 |

Limits are independent and nonnegative. They do not truncate text, insert
replacement characters or silently return partial success. Work units charge
decoding, table lookups, decomposition, ordering, composition and rendering;
they are deterministic estimates, not elapsed time or exact CPU instructions.
Unicode tables themselves have a fixed, release-dependent footprint. Mapping
provenance and scalar intermediates are bounded separately from final output.

Errors expose a stable `kind`, message and optional original byte offset.
`Unicode.errorToString` renders them. Final output size and mapping limits are
checked incrementally before materialization; intermediate expansion is bounded
by the scalar limit. No operation reads a clock, environment or global cache.

## Data and verification

`tools/data.json` pins every input by SHA-256. `tools/generate.py` produces
`src/UnicodeTables.fai` and complete compact test inputs under `data/`, using no
host Unicode database. The derived data is licensed under `UNICODE-LICENSE.txt`.

```sh
python3 packages/unicode/tools/generate.py --download
python3 packages/unicode/tools/generate.py --check
fai test -C packages --package unicode --seed 42 --count 128
python3 scripts/package_integrations.py unicode --no-dependents --fai /path/to/fai
```

The native conformance program checks every pinned grapheme, word, folding and
normalization row, including all twenty required normalization relationships.
It rejects missing coverage. JIT/AOT fixtures also exercise long canonical
ordering, cursor aliases, repeated transformations, mapped composition and
parallel immutable use. Normalization/folding idempotence, reconstruction,
byte boundaries and limit failures have named Fai tests and properties.
