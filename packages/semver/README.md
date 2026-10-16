# Semantic versions

Version **0.1.0** · Requires **Fai 0.1.x** · [Changelog](changelog.md)

```sh
fai add https://github.com/Krzysztof-Cieslak/fai.git --subdir packages/semver --as semver --version '^0.1.0'
```

Pure Fai SemVer 2.0 values, explicit version requirements, stable selection and
compiler-package compatibility checks. Declare a `semver` dependency in your
application's `fai.toml`, then import the modules you use:

```fai
import semver.Semver
import semver.SemverRange
import semver.SemverFai
```

The library and its examples use only the standard library. The repository
workspace provides a local replacement for the portable Git dependency on
`packages/semver`.

## Version values

`Semver.Version` is opaque and immutable. `Semver.parse` accepts exactly one
complete SemVer version, including optional prerelease and build identifiers:

```fai
Semver.parse "1.2.3-rc.1+commit.abc"
Semver.parseTag "v1.2.3"
```

`parse` requires all three numeric components and rejects surrounding whitespace,
prefixes, empty identifiers, non-ASCII identifier characters and prohibited
leading zeros. Core numbers and numeric prerelease identifiers cannot have
leading zeros; build identifiers may (`1.2.3+001` is valid).

`parseTag` accepts either a strict version or one prefixed with a single lowercase
`v`. It does not trim whitespace or fill missing components. Both parsers have
`With` variants taking `Semver.Options`:

```fai
{ maxBytes = 4096, maxIdentifiers = 256 }
```

These are the defaults. The identifier budget counts prerelease and build
identifiers together. The byte budget for `parseTagWith` includes its `v` prefix.

Numeric components are exact decimal strings with no machine-integer ceiling.
Input budgets bound parsing work; a caller can explicitly raise them.
`toString` preserves valid identifier spelling and case, including build metadata.

### Construction and inspection

`parts version` returns a `Semver.Parts` record:

```fai
{
  major = "1",
  minor = "2",
  patch = "3",
  prerelease = ["rc", "1"],
  build = ["commit", "abc"]
}
```

`fromParts` / `fromPartsWith` validate every field independently. A list entry
containing a dot is invalid rather than silently becoming multiple identifiers.
Budgets count the final rendered length, including separators.

`fromInts major minor patch` constructs a stable version from nonnegative Fai
Ints. `toInts version` returns a checked `{ major : Int, minor : Int, patch : Int }`
record, failing with `OutOfRange` if any component exceeds the signed Int range.

### Identity and precedence

Ordinary equality compares the complete version, including build metadata.
`Semver.compare` implements precedence and ignores build metadata:

```text
1.2.3+linux <> 1.2.3+windows       (different identities)
compare those versions = 0        (equal precedence)
```

Use `Semver.compare` for version ordering. Fai's built-in ordering is structural.
The comparison function returns a negative value, zero or a positive value and
can be passed directly to `List.sortBy`. `samePrecedence` is its equality test.

Precedence compares the three numeric components, then prerelease identifiers.
Numeric identifiers compare numerically and precede textual identifiers; text
compares by ASCII spelling. A shorter equal-prefix prerelease list comes first.
A stable release follows every prerelease of the same core.

### Explicit release operations

| Operation on `1.2.3-rc.1+abc` | Result |
|---|---|
| `bumpMajor` | `2.0.0` |
| `bumpMinor` | `1.3.0` |
| `bumpPatch` | `1.2.4` |
| `release` | `1.2.3` |

Bumps always increment the named component, reset lower components and clear
both metadata sections. `release` keeps the core and clears both metadata
sections. They operate exactly on validated values, including arbitrarily large
decimals, without imposing a new parse budget. Use a suitably sized `parseWith`
when reading back a very large result.

## Version requirements

`SemverRange.parse` / `parseWith` use Fai's explicit requirement dialect over the
library's unbounded numeric domain:

```text
requirement := conjunction ( "||" conjunction )*
conjunction := comparator ( "," comparator )*
comparator  := "*" | [ "=" | "^" | "~" | ">=" | ">" | "<=" | "<" ] version
```

A bare full version is exact. All endpoints require a complete core and reject
build metadata. Partial versions, x-ranges, hyphen ranges, empty requirements,
empty alternatives and trailing commas are errors. Comparator tokens allow
surrounding Unicode whitespace; spacing between an operator and its version is
ASCII space, matching the compiler's parser.

```fai
SemverRange.parse ">=1.2.3, <2.0.0 || ^3.0.0"
```

Caret compatibility preserves the first nonzero core component: `^1.2.3` stays
within major 1, `^0.2.3` within minor 2, and `^0.0.3` within core 0.0.3. Tilde
preserves major and minor. Lower bounds compare full SemVer precedence.

**Prerelease eligibility belongs to each alternative.** Besides satisfying its
comparisons, a prerelease needs a comparator in that same alternative that names
a prerelease with the same major/minor/patch core. Thus `^2.0.0-rc.1` accepts
`2.0.0-rc.2` and stable `2.1.0`, but not `2.1.0-beta.1`. `*` admits stable versions
only. An unrelated alternative cannot opt a prerelease in.

`toString` sorts and deduplicates comparators and alternatives, writes exact
versions with `=`, and uses comma-space / space-delimited `||`. It preserves
alternative boundaries and does not merge intervals or absorb a prerelease arm
into `*`. Contradictory requirements are valid and match nothing.

`SemverRange.Options` defaults to 65536 bytes, 256 alternatives, 1024 comparators,
and `Semver.defaults` for individual endpoints. Limits count written tokens
before deduplication; each `*` counts as a comparator.

### Matching and selection

```text
matches        : Requirement -> Semver.Version -> Bool
filter         : Requirement -> List Semver.Version -> List Semver.Version
maxSatisfying  : Requirement -> List Semver.Version -> Option Semver.Version
minSatisfying  : Requirement -> List Semver.Version -> Option Semver.Version
```

Matching ignores a candidate version's build metadata. Filtering retains input
order. Selection preserves the original version and returns the first input on
equal-precedence ties, including distinct build variants. An empty or nonmatching
input returns `None`. Inputs are already parsed values, so malformed strings are
never silently skipped.

## Fai package compatibility

`SemverFai` checks the compiler's narrower accepted domain:

- `parseVersion` / `parseVersionWith`: strict versions with no build metadata and
  core components in `0..18446744073709551615`.
- `parseRequirement` / `parseRequirementWith`: the same requirement dialect with
  every endpoint restricted to that unsigned 64-bit domain.
- `validateVersion` / `validateRequirement`: check already parsed general values.

Parsing retains offsets in the original input, including requirement endpoints
before sorting. Validation of existing values identifies components without
inventing source offsets. Numeric prerelease identifiers remain unbounded.

## Errors and verification

`Semver.Error` carries `kind`, `message`, optional `component`, optional zero-based
`identifier`, and optional zero-based UTF-8 byte `offset`. EOF errors use input
length. Constructed values have component/index information without a source
offset. `Semver.errorToString` renders that context. Resource-limit failures and
out-of-range machine conversions are distinct from syntax failures.

```sh
fai test -C packages --package semver --seed 42 --count 128
python3 scripts/package_integrations.py semver --no-dependents --fai /path/to/fai
fai run -C packages semver/examples/SelectVersion.fai -- '^1.2.0' v1.2.0 1.4.0+first 1.4.0+second 2.0.0
```

The selection example prints `1.4.0+first`. Contracts cover the official SemVer
precedence sequence, malformed input and locations, Fai requirements, unbounded
values, conversion limits, canonicalization and comparison laws. Native JIT/AOT
fixtures exercise long decimal carry chains, large identifier/comparator lists,
stable selection and compiler compatibility boundaries.
