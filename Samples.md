# Fai — Language by Example

A tour of the whole language. These snippets are the source of truth for the
surface syntax; the test suite checks them (parse/typecheck, and run where
applicable), so they cannot drift from the implementation.

> **Status:** the compiler is not built yet (see `Plan.md`). This file specifies
> the intended language. Built-in names like `sqrt`, `intToString`,
> `floatToString`, `Console`, and `Runtime` denote the standard prelude and
> capability set that the runtime provides.

**Conventions used below**
- Indentation is significant (offside rule); canonical layout is 2 spaces, no
  tabs (pinned by `fai fmt`).
- `public` exports a binding; everything else is private to its module.
- Every `public` binding has an explicit signature on its own line above it.
- Type variables are F#-style: `'a`, `'k`, `'v`.
- Equality is `=`, inequality is `<>` (both structural).

---

## 1. Hello, world (the entry point & capabilities)

`main` receives a `Runtime` — a record of capability values supplied by the host.
There are no ambient side effects: printing requires the `Console` capability.

```fai
module Hello

public main : Runtime -> Unit
let main runtime =
  runtime.console.writeLine "Hello, Fai!"
```

`Runtime` is (roughly) the transparent alias:

```fai
type Runtime =
  { console : Console
  , clock   : Clock
  , random  : Random
  , fs      : FileSystem
  , env     : Env }
```

---

## 2. Comments

```fai
module Comments

// line comment

(* block comment,
   spans multiple lines *)

/// Doc comment — human prose only. Attached to the binding below and surfaced to
/// the LSP and to `--message-format=json`. (Checked examples/laws are separate
/// first-class declarations — see §12 — never comment text.)
public answer : Int
let answer = 42
```

---

## 3. Values, primitives & operators

```fai
module Basics

// Private bindings may omit signatures (types are inferred).
let count = 3                  // Int (64-bit)
let ratio = 3.0 / 2.0          // Float (64-bit); + - * / work on Float too
let name = "Fai"               // String (UTF-8)
let initial = 'F'              // Char  (note: 'F' is a char; 'a is a type var)
let yes = true                 // Bool
let nothing = ()               // Unit

// Operators: arithmetic (+ - * /) overloaded over Int and Float,
// compare (< <= > >=), equality (= <>), boolean (&& || not),
// string/list concat (++), pipe (|>), and composition (>>).
let shouted = "go" ++ "!"      // "go!"
let isEven = count % 2 = 0     // here `=` is the equality operator
```

> `=` is overloaded by position: as the binding form in `let x = …` and as the
> equality operator inside expressions, exactly as in F#. The parser
> disambiguates; agents rarely need to think about it.
>
> Arithmetic `+ - * /` is overloaded over `Int` and `Float`. An unconstrained
> numeric type defaults to `Int`, and there is **no implicit `Int`/`Float`
> coercion** — convert explicitly with `intToFloat` / `floatToInt`.

---

## 4. Functions, currying, pipes & composition

```fai
module Funcs

// All functions are curried. Lambdas use `fun`.
public add : Int -> Int -> Int
let add x y = x + y

public inc : Int -> Int
let inc = add 1                       // partial application

// `|>` pipes a value into a function; `>>` composes left-to-right.
public describe : Int -> String
let describe n =
  n
  |> inc
  |> intToString

public twice : ('a -> 'a) -> 'a -> 'a
let twice f = f >> f                  // composition
```

---

## 5. `if`, `let`, and local bindings

Local `let` bindings stack by layout; the final expression is the result. No
`in` keyword.

```fai
module Locals

public hypotenuse : Float -> Float -> Float
let hypotenuse a b =
  let a2 = a * a
  let b2 = b * b
  sqrt (a2 + b2)

public classify : Int -> String
let classify n =
  if n < 0 then "negative"
  else if n = 0 then "zero"
  else "positive"
```

---

## 6. Tuples (structural)

Tuples need no declaration. The type of `(a, b)` is `'a * 'b`; `*` binds tighter
than `->`, so `Int -> Int -> Int * Int` returns a pair.

```fai
module Tuples

/// Integer quotient and remainder.
public divMod : Int -> Int -> Int * Int
let divMod a b = (a / b, a % b)
example: divMod 7 3 = (2, 1)

public swap : 'a * 'b -> 'b * 'a
let swap pair =
  let (x, y) = pair          // destructuring
  (y, x)
```

---

## 7. Algebraic data types & pattern matching

Discriminated unions; `match` is checked for exhaustiveness.

```fai
module Shapes

type Shape =
  | Circle Float
  | Rect Float Float

public area : Shape -> Float
let area shape =
  match shape with
  | Circle r -> 3.14159 * r * r
  | Rect w h -> w * h
```

Generic unions — the prelude's `Option` and `Result`:

```fai
module Prelude

type Option 'a =
  | None
  | Some 'a

type Result 'ok 'err =
  | Ok 'ok
  | Err 'err

public mapOption : ('a -> 'b) -> Option 'a -> Option 'b
let mapOption f opt =
  match opt with
  | None -> None
  | Some x -> Some (f x)
```

---

## 8. Lists & recursion

Module-level bindings are **mutually recursive** — there is no `rec` keyword.
Lists use `[ ... ]`, cons is `::`, and the type is `List 'a`.

```fai
module Lists

/// Apply f to every element, preserving order.
public map : ('a -> 'b) -> List 'a -> List 'b
let map f xs =
  match xs with
  | [] -> []
  | x :: rest -> f x :: map f rest
example: map (fun x -> x + 1) [1, 2, 3] = [2, 3, 4]
forall xs: map (fun x -> x) xs = xs

public foldl : ('acc -> 'a -> 'acc) -> 'acc -> List 'a -> 'acc
let foldl f acc xs =
  match xs with
  | [] -> acc
  | x :: rest -> foldl f (f acc x) rest

// `reverse` can refer to `foldl` above freely (mutual recursion, no `rec`).
public reverse : List 'a -> List 'a
let reverse xs =
  foldl (fun acc x -> x :: acc) [] xs
forall xs: reverse (reverse xs) = xs
```

---

## 9. Structural records with row polymorphism

Records are **structural**: a record type *is* its set of fields. No declaration
is required, though you may name common shapes with a **transparent alias**.

```fai
module Geometry

// Used structurally — no declaration needed.
public length : { x : Float, y : Float } -> Float
let length v = sqrt (v.x * v.x + v.y * v.y)

// A transparent alias: `Vec2` and `{ x : Float, y : Float }` are interchangeable.
type Vec2 = { x : Float, y : Float }

public origin : Vec2
let origin = { x = 0.0, y = 0.0 }

// Immutable copy-and-update with `{ r with ... }` (Perceus may do it in place).
public scale : Float -> Vec2 -> Vec2
let scale k v = { v with x = v.x * k, y = v.y * k }
```

**Anonymous records need no declaration.** Build one and its type is inferred.
Private bindings can omit the signature; a `public` binding must *write* the
(structural) type it infers — no nominal name required:

```fai
module Anon

// Private: type fully inferred, no signature needed.
let center = { x = 0, y = 0 }            // : { x : Int, y : Int }
let pair a b = { x = a, y = b }          // : 'a -> 'b -> { x : 'a, y : 'b }

// Public: inference still works, but the signature is required (and checked).
public makePoint : Int -> Int -> { x : Int, y : Int }
let makePoint x y = { x = x, y = y }
```

A record literal infers a **closed** type (exactly its fields), so every branch
of a function must produce the same field set.

**Record types are closed by default**, with a `|` tail to open them:

- `{ x : 'a }` — closed: *exactly* an `x` field.
- `{ x : 'a | _ }` — open with an **anonymous** tail: "has `x`, plus any other
  fields, which it ignores". This is the common case.
- `{ x : 'a | 'r }` — open with a **named** tail: use `'r` only when the result
  must carry the same other fields through (see `setX` below).

```fai
module Rows

// Accepts any record that has at least an `x` field (others ignored).
public getX : { x : 'a | _ } -> 'a
let getX r = r.x

// Works on Vec2, Vec3, or any record with an x:
//   getX { x = 1.0, y = 2.0 }        = 1.0
//   getX { x = 7, y = 0, z = 9 }     = 7

// A NAMED tail is needed only to thread the other fields into the result:
public setX : 'a -> { x : 'b | 'r } -> { x : 'a | 'r }
let setX v r = { r with x = v }
```

Records in patterns, including **field punning** (`{ x, y }` binds `x` and `y`).
Patterns mirror the type-level openness rule: a bare `{ … }` is **closed** (it
names *all* of the record's fields); a `| _` tail makes it **open** (match these
fields, ignore the rest).

```fai
module RecordMatch

type Vec2 = { x : Float, y : Float }

public describe : Vec2 -> String
let describe v =
  match v with
  | { x = 0.0, y = 0.0 } -> "origin"      // closed: names every field
  | { x = 0.0 | _ } -> "on the y-axis"    // open: ignore the rest explicitly
  | { x, y } -> floatToString x ++ ", " ++ floatToString y
```

Matching a **row-polymorphic** record *requires* `| _`, because the rest of the
record (`'r`) is abstract and therefore unmatchable:

```fai
module Status

public classify : { status : Int | _ } -> String
let classify r =
  match r with
  | { status = 0 | _ } -> "ok"
  | { status | _ } -> "error " ++ intToString status   // irrefutable → exhaustive
```

> No duplicate labels are allowed (lacks constraints). In v1 a pattern tail may
> only be `_` (ignore the rest); *binding* the rest as a record (`{ x | rest }`)
> is record restriction, deferred to v2. v1 has literals, dot access, `{ with }`
> update, row-polymorphic access, and aliases.

---

## 10. Interfaces & instances

An **interface** is a named set of related function signatures — the only
OO-flavored feature. An **instance** is built with `{ InterfaceName with <methods> }`
(no `new`), where each method uses the ML form `name args = body`. It's the *only*
way to construct one: a dictionary of functions whose state is captured in the
closures (so it's an existential value). The braces mirror record update
`{ r with ... }` — the head (an interface name vs a record value) selects which.

```fai
module Logging

interface Logger =
  log : String -> Unit

// Build a Logger from another Logger by prefixing every message.
public withPrefix : String -> Logger -> Logger
let withPrefix prefix inner =
  { Logger with
      log msg = inner.log (prefix ++ msg) }

// Adapt a Console capability into a Logger.
public consoleLogger : Console -> Logger
let consoleLogger console =
  { Logger with
      log msg = console.writeLine msg }
```

---

## 11. Capabilities & least authority

Effects are values. A function can only perform an effect if it is *given* the
corresponding capability, and capabilities only originate from `main`'s
`Runtime`. Thanks to row polymorphism, a function requests **exactly** the
capabilities it needs and accepts any larger runtime — least authority by
construction.

```fai
module Greet

// Needs ONLY a console; accepts any runtime that has at least one.
public greet : { console : Console | _ } -> String -> Unit
let greet env name =
  env.console.writeLine ("Hello, " ++ name ++ "!")

public main : Runtime -> Unit
let main runtime =
  greet runtime "Fai"          // a full Runtime satisfies { console : Console | _ }
```

```fai
module Capabilities

interface Console =
  writeLine : String -> Unit

interface Clock =
  now : Unit -> Int            // milliseconds since epoch (a capability, so it is
                               // honestly effectful, not ambient)

interface Random =
  nextInt : Int -> Int         // pseudo-random in [0, n)
```

---

## 12. Contracts: examples & properties

`example` and `forall` are **first-class declarations** (peers of `let`/`type`),
placed right after the binding they describe. They are real, type-checked
expressions resolved in module scope — *not* comment text — so names inside them
resolve normally (an out-of-scope name is a real error, you get hover/go-to-def,
etc.). `fai test` evaluates each `example` once and checks each `forall` law with
generated inputs (shrinking counterexamples on failure).

```fai
module Math

/// Absolute value.
public abs : Int -> Int
let abs n =
  if n < 0 then 0 - n else n
example: abs (-3) = 3
example: abs 3 = 3
forall n: abs n >= 0

/// `clamp lo hi x` constrains x to the range [lo, hi].
public clamp : Int -> Int -> Int -> Int
let clamp lo hi x =
  if x < lo then lo
  else if x > hi then hi
  else x
example: clamp 0 10 15 = 10
forall lo x: clamp lo (lo + 100) x >= lo
```

Because contracts are ordinary module-scoped code, a **law may relate several
functions** — there is no single signature to bury it under:

```fai
module Algebra

forall xs ys: length (append xs ys) = length xs + length ys
forall xs: reverse (reverse xs) = xs
```

---

## 13. Modules & visibility (nesting)

One **top-level** module per file (declared with `module Name` on the first
line). Modules may **nest** with `module Name =` and an indented body. Names are
private unless marked `public`; refer to nested members by qualification.

```fai
module Geometry

// Nested module — private helpers grouped together.
module Internal =
  let pi = 3.14159
  let square x = x * x

public circleArea : Float -> Float
let circleArea r =
  Internal.pi * Internal.square r

public perimeter : Float -> Float
let perimeter r =
  2.0 * Internal.pi * r
```

---

## 14. A small end-to-end program

Ties together capabilities, an ADT, records, pattern matching, a list, and a
contract.

```fai
module Cart

type Item =
  { name : String, price : Float, qty : Int }

/// Line-item total (price × quantity).
public subtotal : Item -> Float
let subtotal item =
  item.price * intToFloat item.qty
example: subtotal { name = "pen", price = 1.5, qty = 2 } = 3.0

public total : List Item -> Float
let total items =
  items
  |> map subtotal
  |> foldl (fun acc x -> acc + x) 0.0

public main : Runtime -> Unit
let main runtime =
  let cart =
    [ { name = "pen", price = 1.5, qty = 2 }
    , { name = "pad", price = 3.0, qty = 1 } ]
  let due = total cart
  runtime.console.writeLine ("Total: " ++ floatToString due)
```

---

## 15. Canonical formatting (what `fai fmt` enforces)

- 2-space indentation, no tabs; one statement/branch per line.
- `match` arms align with the `match` keyword; each arm starts with `| `.
- Multi-line record/list elements use a leading-comma layout (as in §14).
- A binding groups with the `example`/`forall` declarations directly beneath it
  (no blank line within the group); exactly one blank line separates groups; the
  file ends with a newline.
- `fmt` is idempotent: formatting already-formatted code is a no-op.

Because there is one canonical layout, generated code is low-entropy: there is
essentially one correct way to write a given program.
