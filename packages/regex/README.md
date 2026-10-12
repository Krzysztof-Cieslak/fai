# Regular expressions

`regex` provides pure, reusable compiled patterns over Unicode text (`Regex`) and
arbitrary binary buffers (`RegexBytes`). The package uses the standard library's
typed `RegexEngine` bridge to the Rust regex family.

```fai
match Regex.compile "(?<word>[a-z]+)" with
| Err error -> Err error
| Ok pattern -> Regex.find pattern "123 hello"
```

Compilation and searching are pure. Keep a successfully compiled value in a local
binding or application state and pass it to helpers. Fai top-level value bindings
are re-evaluated when read; they are not an implicit compilation cache.

`compileWith` accepts explicit flags and resource limits. `compileWhole` anchors
the parsed expression to the whole input, preserving alternatives and verbose
comments. Text patterns default to Unicode; byte patterns default to byte mode.
Patterns use ordinary Fai String literals, so a regex `\d` is written `"\\d"`.
Rust regex syntax supports Unicode classes, captures, greedy/lazy repetition and
inline flags. Look-around and backreferences return located compilation errors.

`find`, `isMatch` and `captures` have `WithLimits` counterparts. No match is `Ok
None` or `Ok false`; invalid options, input limits and offsets are errors. Text
`findFromChar` and `findFromByte` preserve the original input's anchor context.
Byte offsets must be at UTF-8 boundaries. Text spans have explicit `startChar`,
`endChar`, `startByte`, `endByte` fields; byte spans only have byte fields. Ranges
are half-open and characters are Unicode scalars, not graphemes or display cells.

`group` and `groupNamed` distinguish unknown groups (Err), nonparticipation (Ok
None), and successful empty captures. Group zero is the whole match; captureCount
includes it. `text`/`bytes` extract a copy; `detach` returns that copy and the
original coordinates, letting the match and its retained input go out of scope.
Compiled values compare and hash by their complete immutable description,
including options, independently of native addresses and search cache state.

`cursor`/`next` and `captureCursor`/`nextCaptures` provide immutable iteration:
advancing a cursor returns a match and a new cursor without changing any alias.
`findAll`/`capturesAll` collect arrays; their WithLimits forms check result and
capture-slot budgets. Empty matches follow the Rust iterator's non-overlapping
policy, advancing text at scalar boundaries and binary input one byte at a time.
Scalar checkpoints advance with each match, so position conversion does not
rescan all preceding text on every iteration.

`fold`/`foldWithLimits` and `foldCapturesWithLimits` thread an accumulator through
callbacks returning `(state, Continue/Stop)`. Callback effects are forwarded.
Their result retains the last committed state, completed callback count and
search/callback failure. Stop is successful early termination; a resource limit
fails only when another result would exceed it. A single search is O(m*n); full
iteration retains the engine's possible O(m*n*n) worst case.

Replacement strings are literal: `replace` changes the first match, `replaceAll`
changes every match, and `replaceN` stops intentionally after its count. Their
bounded forms enforce output size before assembling the final value. `replaceWith`
passes captures to a callback returning Result and forwards its effect; failures
retain the current byte span and number of prior completed replacements. A
callback that returns oversized output has already run and is never replayed.

`RegexTemplate.compile regex source` validates `$1`, `${1}`, `${name}` and `$$`
against that regex's capture schema. Unknown groups and numeric overflow fail
before input searching. Nonparticipating groups expand to empty text. Templates
retain their regex, and `RegexTemplate.replace`/`replaceAll`/`replaceN` apply them.
`compileBytes` accepts arbitrary template bytes; `replaceBytes`/`replaceAllBytes`
preserve them. Text template locations count Unicode scalar columns; non-UTF-8
template locations count byte columns. Template limits default to 64 KiB and
4096 parts. `RegexTemplate.escape` quotes dollars for literal template text.

```sh
fai fmt --check -C packages regex
fai check -C packages regex
fai test -C packages regex
```
