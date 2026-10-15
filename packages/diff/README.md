# Diff and text patches

Pure Fai sequence/text diffing, review views, and exact unified patches. The only
dependency is `unicode`; CLI, TUI and process dependencies belong to the separate
`diff-examples` package in `examples/`.

```fai
import diff.DiffText
import diff.UnifiedDiff

let patch old new =
  match DiffText.compare old new with
  | Err error -> Err (DiffText.errorToString error)
  | Ok comparison ->
    match UnifiedDiff.fromComparison (Some "file.txt") (Some "file.txt") comparison with
    | Err error -> Err (UnifiedDiff.errorToString error)
    | Ok patch -> Result.mapError UnifiedDiff.errorToString (UnifiedDiff.toString patch)
```

## Sequence engine

`Diff.diff oldArray newArray` returns a `Script` containing coalesced `edits`
and `quality`. Each edit has a `kind` (Match/Delete/Insert) and zero-based,
half-open `old` and `new` ranges. Arrays are not retained by the script.

`diffWith limits options` separates algorithm policy from budgets:

- **Myers**, the default, computes a shortest insertion/deletion script with
  linear frontier storage. On equal frontier choices it takes the deletion
  route. Results are deterministic, not necessarily identical to other diff
  implementations' tie choices.
- **Patience** aligns unique shared keys through a longest increasing sequence,
  then recursively compares gaps, using Myers where no anchors exist. Quality
  is `Anchored`, not a claim of global minimality.
- Default exhaustion policy is `Fail`. Explicit `ReplaceRemainder` replaces
  unresolved regions wholesale and reports `Coarse`; it still reconstructs
  the complete target. Only search-work exhaustion falls back. Input, scratch
  and edit-run limits remain errors.

`diffBy`/`diffByWith` compute one pure key per input element. A key match aligns
values but does not assert that their original fields are equal. Key functions
must be pure; their own cost or possible traps are not controlled by the engine.
Options/size budgets are validated before they run. The `With` variant charges
key preparation and retains its key arrays within the scratch budget.

Default sequence limits: two million items per side, eight million scratch
slots, one million coalesced edits, and five million search-work units. Work is
charged across prefix/suffix scans, patience gaps and Myers subdivisions—not
reset on each recursive subproblem. User payload hashing/equality costs are
separate from this algorithm-step accounting.

## Text snapshots and review comparison

`DiffText.compare` compares lines exactly; `compareWith` accepts text limits and
options. A `Comparison` retains both original snapshots, indexes and changes.
`oldText`, `newText`, `oldLines`, `newLines`, `changes` and `quality` inspect it.

`Line.span` and `Line.content` are half-open byte offsets; `Change.old` and
`Change.new` are line-index ranges. LF separates lines, CRLF retains its CR,
and lone CR is ordinary content. Empty input has no lines. A trailing LF does
not create a phantom extra line; missing final newlines remain significant.

Review options are explicit and disabled by default:

| Setting | Values |
|---|---|
| `case` | Sensitive, AsciiInsensitive, UnicodeInsensitive |
| `whitespace` | ExactWhitespace, IgnoreTrailing, CollapseRuns, IgnoreAll |
| `unicodeWhitespace` | false: ASCII space/tab; true: Unicode White_Space |
| `normalization` | None or Some NFC/NFD/NFKC/NFKD |

Unicode behavior follows the pinned `unicode` dependency. Normalization runs
before folding and again afterwards when selected, then whitespace is handled
within lines. CollapseRuns replaces each run with one ASCII space; it does not
trim leading/trailing runs. Line structure and LF/CRLF/missing-final-newline
differences remain observable.

Changes distinguish `Equal`, `Equivalent`, `Deleted`, and `Inserted`.
Equivalent means original text differs but matches under the review rules.
`hunks context includeEquivalent comparison` returns context-expanded hunks;
overlapping or adjacent context windows merge. Negative context is an error.
`detach comparison hunk` copies its old/new text, allowing the original snapshots
to be released. `detachWith` selects the combined output-byte budget. Supplied
hunk ranges and complete copy size are validated before slicing.

Text limits have separate `unicode`, `sequence`, and `maxOutputBytes` fields.
Unicode preprocessing has a fixed number of separately bounded passes, while
all sequence subproblems share one search budget. No clock or ambient
cancellation is used. `maxOutputBytes` bounds the combined detached text.

## Inline detail and provenance

`DiffInline.compare old new` returns exact grapheme-level detail.
`compareWith limits options granularity old new` additionally selects Unicode
`Words`, `Graphemes`, or `Scalars` and the same review options. Word segments
include punctuation and whitespace. Refine selected replacement regions instead
of eagerly processing an entire large file.

The result is `{ changes, quality }`. Each change refers to arrays of original
byte ranges, not indexes in normalized text. Ranges can be noncontiguous, and
can overlap between changes when one original scalar expands to several
transformed tokens. Case-folded `ß` and `ss`, or composed/decomposed accents,
therefore align without inventing source positions. Removed whitespace is
attributed to neighboring surviving text where possible. An entirely ignored
comparison can still report one Equivalent change for differing originals.

## Unified patch interchange

`UnifiedDiff.parse` reads an entire UTF-8 patch into file patches and hunks.
`toString` validates and emits canonical unified text. Their `With` variants
take `UnifiedDiff.Limits` first. `reverse` swaps paths, coordinates, additions
and removals.

Supported features:
- Standard unified hunks and multi-file documents.
- Common Git text headers, quoted UTF-8 filenames and octal byte escapes.
- Ordinary file creation/deletion using `/dev/null`, including empty files.
- Exact CRLF payloads and `\\ No newline at end of file` markers.
- Descriptive index headers and ordinary `100644` creation/deletion metadata.

Binary, rename/copy, combined and mode-changing patches return explicit errors.
Creation/deletion of non-ordinary modes is unsupported. Paths containing NUL,
LF or CR are rejected. Unknown content, malformed quoting, invalid/overflowing
counts, overlapping hunks and invalid no-newline markers are never skipped.
Patch input must end with LF framing; the parser preserves CR in payloads
instead of guessing at transport line-ending normalization. The canonical
renderer always quotes paths and writes explicit hunk counts. Incidental Git
metadata is not retained, so round trips preserve the supported patch model,
not the original header spelling.

`fromComparison` exports with three context lines; `fromComparisonWith` accepts
explicit context and patch limits. **Export always includes exact original
changes, even those hidden by review options.** It never converts an
Equivalent line into unchanged patch context.

Default patch limits are 16 MiB input/output, 10,000 file sections, 100,000 hunks
and one million payload lines. Rendering checks complete size before building
output. Errors carry a kind, message, and one-based patch line/byte column;
unlocated model errors use location zero.

## Applying patches

`Patch.applyFile filePatch original` is strict. `original : Option String`
distinguishes an absent file from an existing empty file. It returns
`{ text : Option String, relocations }`; None means deletion.

`applyFileWith limits mode` supports `Relocate radius` explicitly:
- Try the declared position first.
- Otherwise search only within the bounded line radius, in the original snapshot.
- Match all context and removed text exactly; never drop context or ignore case.
- Reject ambiguous matches, out-of-order placements and overlaps.
- Report declared/actual line indexes for each relocated hunk.

Context-free insertions have no relocation anchor. Their declared position must
be valid. Strict matching checks the patch's old-side text, not unrelated file
content. No-newline payloads cannot be followed by more output.

`applyBatch patch snapshots` uses literal path identifiers.
`applyBatchWith limits mode mapPath patch snapshots` accepts a pure explicit
path mapper, for example to remove Git's `a/` and `b/` prefixes. The library
never treats patch paths as filesystem paths. Old/new names must map to one
identifier for modifications; duplicate snapshots, collisions, renames and
conflicting sections fail. Only after the whole batch succeeds is its array of
proposed updates returned. Applications own actual file resolution and writes;
this API provides no filesystem transaction claim.

Application defaults bound source bytes per file and output to 16 MiB, with five
million comparison steps. Batch output and search work share those budgets
across the batch. Parser/model limits apply before application.

## Verification and examples

```sh
fai test -C packages --package diff --seed 42 --count 256
fai test -C packages --package diff-examples
python3 scripts/package_integrations.py diff-examples --no-dependents --fai /path/to/fai
fai run -C packages diff/examples/Cli.fai -- old.txt new.txt
fai run -C packages diff/examples/Viewer.fai -- old.txt new.txt
```

Contracts test independent small-sequence minimality, reconstruction, patience,
review rules, exact export, failure modes and resource limits. Native JIT/AOT
fixtures cover exhaustive small inputs, large sparse changes, shared execution,
Git-generated patches and Git application of generated patches. CLI and viewer
snapshot fixtures compile the real examples without requiring an interactive
terminal in CI.
