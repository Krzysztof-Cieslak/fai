# Glob and Git ignore matching

Pure Fai byte-oriented path matchers, compiled pattern sets, Git ignore rules,
and scoped filesystem discovery. Declare a `glob` dependency in `fai.toml` and
import the modules you use:

```fai
import glob.Glob
import glob.GlobSet
import glob.GlobWalk
import glob.Ignore
```

The library has no source-package dependencies. The `glob-examples` package
adds the process package for its independent Git compatibility fixture.

## Patterns and prepared candidates

```fai
match (Glob.compile "{src,test}/**/*.{fai,toml}",
       Glob.candidateBytes (Bytes.fromString "src/deep/Main.fai")) with
| (Ok pattern, Ok candidate) -> Glob.isMatch pattern candidate
| _ -> false
```

`Glob.compile` accepts String patterns; `compileBytes` accepts arbitrary bytes.
Their `With` variants take `Glob.Options`. `Pattern` and `Candidate` are opaque,
immutable reusable values. `patternBytes` retains the original pattern.

- `*` matches zero or more bytes within one component; `?` matches one byte.
- `**/` matches zero or more directory components, including the root.
- A trailing `/**` matches descendants, and `**` alone matches everything.
- `**` occupies a whole component; other multi-star forms fail compilation.
- `[abc]`, `[a-z]`, `[!abc]` and `[^abc]` select or negate byte classes. ASCII
  POSIX classes such as `[[:digit:]]` are supported. Classes never match `/`.
- `{a,b}` selects alternatives. Empty alternatives are allowed; braces do not
  nest. Alternatives compile as branches, without combinatorial text expansion.
- `/` is the pattern separator on every platform; `\` escapes the following byte.
- Dots are ordinary bytes. `*.fai` matches `Main.fai` but not `src/Main.fai`;
  `**/*.fai` matches both. Case sensitivity is identical on every platform.

Wildcards count encoded bytes: `?` does not match `é`, while `??` does. Unicode
literals match their complete encoded spelling. `caseInsensitive = true` enables
ASCII-only case folding for ordinary globs.

`Glob.candidate path` prepares a native root-relative `Path`.
`candidateBytes bytes` prepares portable slash-separated bytes. Both normalize
redundant separators and `.` components and reject parent components and NULs.
Native roots/prefixes and a leading portable slash are rejected. `Glob.root` is
the empty relative candidate. Patterns themselves are not path-normalized and
match the candidate's complete normalized spelling.

Unix filenames retain arbitrary native bytes. Windows UTF-16 filenames convert
losslessly to UTF-8/WTF-8, including unpaired surrogates. `candidateSpelling` is
the matching representation, while filesystem results retain native `Path`
values. Diagnostic display strings are never used for matching or I/O.

`matchesPath pattern path` combines preparation and matching. Preparation and
compilation return `Result`; matching a prepared candidate returns `Bool`.
`Glob.Error` has a kind, message, optional zero-based byte offset and optional
zero-based pattern-set index. `errorToString` renders its location.

The defaults allow 4096 pattern bytes and 16384 compiled states. Negative byte
limits and nonpositive state limits fail before compilation. Matching uses an
iterative automaton with work proportional to candidate bytes times compiled
states, and bounded active-state storage; it has no recursive backtracking.

## Sets

`GlobSet.compile ["**/*.fai", "src/**"]` builds a shared matcher.
`compileWith` takes `Glob.Options`, with `maxStates` bounding the entire set.
`ofPatterns maxStates patterns` combines already compiled patterns, retaining
their individual case policies.

`matches set candidate` returns every matching input index in declaration order.
Duplicate patterns retain separate indices. `isMatch` tests whether any matches;
`matchesPath` prepares a native path first. `empty` matches nothing.

`mayMatchDescendant` on a pattern or set is conservative: false proves that a
directory can be pruned, while true can include impossible suffixes.

## Git ignore rules

```fai
let source = { base = Glob.root, name = ".gitignore" }
let parsed = Ignore.parse source (Bytes.fromString "build/\n*.tmp\n!keep.tmp\n")
```

A `Source` contains a prepared base directory and a caller-defined name.
`parse`/`parseWith` return an opaque `File`. `append file rules` and
`ofFiles files` build an opaque `Rules` value in increasing precedence order;
their `With` variants take explicit limits. `Ignore.empty` admits everything.

Parsing follows Git's byte rules: comments, negation, backslash escapes,
unescaped trailing spaces, UTF-8 BOM, LF/CRLF, leading-slash anchors, basename
patterns, trailing-slash directory rules, recursive stars and POSIX classes.
Braces remain literal. Malformed Git patterns never match and are counted by
`ignoredPatternCount`; blank lines and comments are not counted. Size/compilation
limits return located errors instead of silently dropping rules.

`decide rules candidate kind` returns `NoMatch`, `Included Match`, or
`Ignored Match`. It checks ancestors first: `build/` followed by
`!build/keep.txt` still excludes the file. Directory-only rules use the actual
entry kind, so a directory symlink is not itself a directory match.
`decideWithOverrides rules overrides candidate kind` evaluates the overrides
at higher precedence for both ancestors and the final entry.

`Match` retains the original line bytes, source identity, one-based line number,
zero-based start/finish byte offsets, matched path and optional blocked parent.
`isIgnored` extracts the exclusion decision. Caller-supplied source order controls
precedence; automatic discovery adds deeper `.gitignore` files after outer files.

`Ignore.Options` contains the glob compiler options plus per-file bytes, total
active bytes, rules and compiled-state limits. Defaults are 1 MiB per file,
4 MiB active source bytes, 10000 rules and 65536 total states. Case-insensitive
rules preserve Git's byte-class and escape behavior; this includes Git's distinct
treatment of literal uppercase class members versus ranges.

Compatibility follows Git 2.55's corrected component semantics. Older Git releases had a
literal-prefix optimization bug for patterns such as `a**/b`; contracts pin the
corrected behavior, while the live comparison uses equivalent class-prefixed
forms for that edge. Git repository configuration and tracked-file state remain
application inputs; the library evaluates the supplied rule sources.

## Filesystem discovery

`GlobWalk.fold env options root patterns initial callback` visits matching
descendants. `env` supplies `fs` and `concurrency`; callbacks forward their own
effects and return `Result (state * Directory.Decision) callbackError`.
`Continue`, `SkipChildren` and `Stop` keep their usual traversal meaning.

`collect env options maxResults root patterns` returns bounded collected entries
in traversal order. A further match reports `ResultLimit` and retains the prefix.
An empty set performs no traversal. The root itself is not emitted.

Defaults use sorted depth-first traversal, include ordinary dotfiles, skip `.git`,
read nested `.gitignore` files, return all entry kinds, and do not follow symlinks.
Ordering is by native filename spelling within each directory, rather than a
global sort. Configure `entries` as `FilesOnly`, `DirectoriesOnly`, or `LinksOnly`
to filter emissions without losing traversal through necessary directories.
`includeHidden`, `skipGit`, and `respectIgnore` are independent explicit options;
`respectIgnore` controls file discovery, while supplied rules always apply.

The explicit root is the ignore-discovery boundary. Excluded directories are
pruned before loading their rule files. Discovered `.gitignore` symlinks are
skipped. `baseRules` precede discovered files; `overrideRules` take precedence over
all of them. In `SkipErrors` mode an unreadable ignore file retains inherited
rules and increments the error count. Cancellation and resource-limit failures
are terminal. Filesystem enumeration and metadata are point-in-time observations.

Each `Entry` contains its native `path`, native `relative` path, actual `kind`,
depth, and matching pattern indices. `WalkResult` retains committed callback
state, outcome, visited/matched/ignored counts, skipped errors, invalid ignore
pattern count, and a separate cleanup error. A directory callback's successful
state update survives a later failure while preparing that directory's rules.
Directory cursors close through the standard cancellation-masked cleanup path.

## Verification and examples

```sh
fai test -C packages --package glob --seed 42 --count 128
python3 scripts/package_integrations.py glob-examples --no-dependents --fai /path/to/fai
```

The native fixtures exercise temporary trees, scope precedence, pruning, symlinks,
lossless names, result/resource limits, callback failure and cancellation through
JIT and AOT. The Git fixture compares thousands of decisions with `git check-ignore`
in an isolated temporary repository, with explicit case policy and configuration.
Pure contracts additionally compare the automaton against an independent small
wildcard interpreter and cover alternatives without exponential expansion.

The coding-assistant example uses `find_files` to search root-relative patterns
with Git ignore rules. Results expose checked Unicode paths plus display labels,
and explicitly report a bounded result prefix with `truncated = true`.
