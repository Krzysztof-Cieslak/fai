# Decimal arithmetic

Version **0.1.0** · Requires **Fai 0.1.x** · [Changelog](changelog.md)

```sh
fai add github:Krzysztof-Cieslak/fai --subdir packages/decimal --as decimal --version '^0.1.0'
```

Pure Fai arbitrary-precision decimal arithmetic with explicit rounding and
resource limits. The core package has no dependencies. Declare it in `fai.toml`
and import `decimal.Decimal`:

```fai
import decimal.Decimal

let result =
  match (Decimal.parse "0.1", Decimal.parse "0.2") with
  | (Ok a, Ok b) -> Result.map Decimal.toString (Decimal.add a b)
  | (Err error, _) -> Err error
  | (_, Err error) -> Err error
// Ok "0.3"
```

## Values and identity

`Decimal.Decimal` is opaque and immutable. Finite values use normalized
arbitrary-precision integer coefficients and checked decimal exponents. Leading
and trailing zeros do not affect identity: `1`, `1.0`, and `1.00` are the same
value. All zero spellings, including negative zero, normalize to `Decimal.zero`.

The constants `zero`, `one`, `nan`, `positiveInfinity`, and `negativeInfinity`
are available. NaN is a single canonical value, equal to itself and hashable.
`classify` returns `FiniteValue`, `NegativeInfinity`, `PositiveInfinity`, or `NaN`;
`isFinite`, `isInfinite`, `isNaN`, and `isZero` provide simple predicates.

`Decimal.compare` defines numerical ordering:

```text
-Infinity < finite values < +Infinity < NaN
```

Use this function for numeric sorting. Ordinary Fai equality and hashing follow
numeric identity, but the built-in structural ordering is not decimal precedence.
There are no implicit conversions or overloaded numeric literals.

## Parsing and construction

`parse : String -> Result Decimal Error` accepts ordinary ASCII decimals,
optional signs, decimal points and base-ten exponents: `.5`, `1.`, `+001.2300`,
and `1.23e-4`. It does not trim whitespace or accept separators, underscores,
hexadecimal, currency symbols, or locale-specific spellings.

Special spellings are `NaN`, `Infinity`, `+Infinity`, and `-Infinity`.
Other case variants and signed NaNs are rejected.

`fromInt` constructs every signed 64-bit value exactly. `parts` returns
`Some { negative, coefficient, exponent }` for finite values, where the coefficient
is canonical unsigned decimal text and the value is sign × coefficient × 10^exponent.
It returns `None` for special values. `fromParts` validates those fields and
normalizes them. `parseWith` and `fromPartsWith` take explicit `Limits` first.

`toString` emits a deterministic exact spelling, using scientific notation for
large positive powers and very small magnitudes instead of expanding implied
zeros. It does not preserve input spelling or scale. `toStringWith` checks the
supplied operand and output limits.

## Exact arithmetic

```text
add          : Decimal -> Decimal -> Result Decimal Error
subtract     : Decimal -> Decimal -> Result Decimal Error
multiply     : Decimal -> Decimal -> Result Decimal Error
divideExact  : Decimal -> Decimal -> Result Decimal Error
negate       : Decimal -> Decimal
abs          : Decimal -> Decimal
```

Addition, subtraction and multiplication never silently round. Exact division
reduces the rational quotient and succeeds only when its decimal expansion
terminates, otherwise returning `NonTerminating`. `1 / 8` is `0.125`; `1 / 3`
does not have an exact finite result. Each fallible arithmetic function has a
`With` variant taking `Limits` first.

Special operands behave explicitly:

- Existing NaN operands propagate canonical NaN.
- Compatible infinity arithmetic retains its sign.
- Adding opposite infinities, subtracting equal infinities, multiplying infinity
  by zero, or dividing infinity by infinity returns `InvalidOperation`.
- Division by zero, including `0 / 0`, returns `DivisionByZero`.
- A finite value divided by infinity is canonical zero.

Options and operand budgets are validated before arithmetic. Resource exhaustion
returns an error, never a rounded replacement or infinity.

## Rounding and division

All precision reduction is explicit. The `Rounding` cases are:

| Mode | Behavior |
|---|---|
| `HalfEven` | Nearest; an exact tie chooses an even retained digit |
| `HalfAwayFromZero` | Nearest; ties go away from zero (Python ROUND_HALF_UP) |
| `HalfTowardZero` | Nearest; ties go toward zero (Python ROUND_HALF_DOWN) |
| `TowardZero` | Truncate toward zero |
| `AwayFromZero` | Always move away when nonzero digits are discarded |
| `Floor` | Toward negative infinity |
| `Ceiling` | Toward positive infinity |
| `Exact` | Return `Inexact` if any nonzero information would be lost |

```text
roundToScale      : Int -> Rounding -> Decimal -> Result Rounded Error
roundToPrecision  : Int -> Rounding -> Decimal -> Result Rounded Error
divideToScale     : Int -> Rounding -> Decimal -> Decimal -> Result Rounded Error
divideToPrecision : Int -> Rounding -> Decimal -> Decimal -> Result Rounded Error
```

`Rounded` is `{ value : Decimal, inexact : Bool }`. Discarding only zeros does not
set `inexact`; numeric identity still discards presentation scale.

Scale is decimal places: scale 2 means hundredths; scale -2 means hundreds.
Precision is positive significant digits and cannot exceed the coefficient
digit budget. Division operands are dividend then divisor. `With` variants take
`Limits` before the arguments above.

Examples: `1 / 3` at scale 2 yields `{ value = 0.33, inexact = true }`.
Rounding `1250` to scale -2 with `HalfEven` yields `1200`; `1350` yields `1400`.
Rounding `-0.01` to scale 0 with `Ceiling` yields zero, not negative zero.

## Presentation and native conversions

`toFixed places mode value` returns `Result (String * Bool) Error`: the rendered
text and whether rounding was inexact. Places must be nonnegative; zeros are
included to obtain exactly that many decimal places. `1.2` at two places becomes
`"1.20"` with false. `toScientific precision mode value` similarly emits one
leading digit, exactly the requested significant digits, and an exponent.
Both have `With` variants and check complete output length before padding.

`toIntExact` rejects fractions, special values and signed 64-bit overflow.
Float conversions distinguish two intentions:

- `fromFloatExact` preserves the actual binary64 mathematical value. Float `0.1`
  becomes `0.1000000000000000055511151231257827021181583404541015625`.
- `fromFloatShortest` parses Float's shortest round-trip spelling, so Float `0.1`
  becomes decimal `0.1`.
- `toFloat` rounds to nearest binary64, ties to even, using exact midpoint
  comparisons. Finite overflow is `OutOfRange`; tiny finite values may underflow
  to a signed Float zero. Explicit NaN/infinity map to IEEE values.

The Float functions have `With` variants. Converting Float negative zero into
Decimal loses the sign by design; all NaN signs and payloads become canonical NaN.

## Resource limits and errors

Defaults:

```fai
{
  maxDigits = 10000,
  maxExponent = 1000000,
  maxInputBytes = 65536,
  maxIntermediateDigits = 20000,
  maxOutputBytes = 1048576,
  maxWork = 50000000
}
```

Precision is not fixed: callers can raise coefficient limits. These are resource
budgets, not rounding contexts. `maxExponent` bounds the normalized stored
exponent; its configurable ceiling is 10^9. The parser accepts written exponents
up to magnitude 2×10^9 to allow scientific spellings whose fractional digits
normalize into the stored range. Intermediate digits have a configurable ceiling
of 10^7. Alignment checks avoid allocating enormous runs of zeros; magnitude
comparison and rounding very small values do not need those runs.

`maxWork` bounds conservative limb-operation estimates, including Euclidean
reduction and exact-decimal expansion loops. It is not a wall-clock timeout.
Native Float conversion counts scratch work and intermediate coefficients too.
The same limits apply to operands supplied to a smaller-budget operation.

`Error` is `{ kind, message, offset : Option Int }`. Parse syntax errors retain
zero-based byte locations; arithmetic/resource failures have no source offset.
Kinds include `InvalidSyntax`, `InvalidOptions`, `DivisionByZero`,
`InvalidOperation`, `NonTerminating`, `Inexact`, `OutOfRange`, and `ResourceLimit`.

## Development

```sh
fai test -C packages --package decimal --seed 42 --count 128
python3 scripts/package_integrations.py decimal --no-dependents --fai /path/to/fai
```

Contracts cover exact arithmetic, all rounding modes, exceptional values,
machine boundaries, and comparison/round-trip laws. Independent Python decimal
vectors exercise multi-limb arithmetic and division. Native JIT/AOT fixtures
exercise large carry chains, repeated arithmetic and shared use across tasks.

The optional sibling `decimal-codecs` integrates JSON, YAML and SQL without
adding dependencies to this arithmetic package.
