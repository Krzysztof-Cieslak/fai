# Fai — Language by Example

The `.fai` files in this directory are the canonical tour of the whole language
and the source of truth for the surface syntax. The test suite verifies them
(parse and format now; typecheck and run as later milestones land), so they
cannot drift from the implementation.

Each file is one self-contained module. Files that use only already-implemented
surface are parsed, formatted, and round-tripped by the test suite; files that
exercise not-yet-implemented surface are recognized as such and skipped until
their feature lands.

> **Status:** the compiler is still being built (see `docs/PLAN.md`). Built-in
> names like `sqrt`, `intToString`, `floatToString`, `Console`, and `Runtime`
> denote the standard prelude and capability set that the runtime provides.

## Conventions

- Indentation is significant (offside rule); canonical layout is 2 spaces, no
  tabs (pinned by `fai fmt`).
- `public` exports a binding; everything else is private to its module.
- Every `public` binding has an explicit signature on its own line above it.
- Type variables are F#-style: `'a`, `'k`, `'v`.
- Equality is `=`, inequality is `<>` (both structural).

## Canonical formatting (what `fai fmt` enforces)

- 2-space indentation, no tabs; one statement/branch per line.
- `match` arms align with the `match` keyword; each arm starts with `| `.
- Multi-line record/list elements use a leading-comma layout.
- A binding groups with the `example`/`forall` declarations directly beneath it
  (no blank line within the group); exactly one blank line separates groups; the
  file ends with a newline.
- `fmt` is idempotent: formatting already-formatted code is a no-op.

Because there is one canonical layout, generated code is low-entropy: there is
essentially one correct way to write a given program.
