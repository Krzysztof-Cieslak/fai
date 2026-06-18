# Fai error-code catalog

<!-- Generated from each crate's `CODES` table by `crates/fai-tests/tests/catalog.rs`. Do not edit by hand; regenerate with `UPDATE_ERROR_CODES=1 cargo test -p fai-tests --test catalog`. -->

Every diagnostic Fai emits carries a stable `FAInnnn` code. Codes are a public, versioned API: they are never renumbered, and are allocated by compiler phase. Each entry below lists the code, its default severity, and what triggers it.

## FAI0xxx — Tooling, CLI & driver

### FAI0001 — command not implemented

**Severity:** error

The requested CLI command has no behavior in this build. It is a placeholder for a command that lands in a later release.

### FAI0002 — workspace or I/O error

**Severity:** error

The workspace could not be read: the root is not a directory, a file could not be read, or a path was not valid UTF-8. Check the path passed to `-C`/the entry file and filesystem permissions.

### FAI0003 — linker failed

**Severity:** error

The system linker returned an error while producing the native executable. The linker's own output accompanies this diagnostic; a missing toolchain or linker is the usual cause.

### FAI0004 — no entry point

**Severity:** error

`fai build`/`fai run` need an entry file defining `public main : Runtime -> Unit`, but none was found.

### FAI0005 — daemon unavailable; ran in-process

**Severity:** warning

The per-workspace daemon could not be reached, so the command ran in-process (correct, just without the warm-cache speedup). Run `fai daemon status` to investigate, or pass `--no-daemon` to silence it.

### FAI0006 — run timed out

**Severity:** error

A program under `fai run` exceeded its wall-clock limit and was terminated (exit 124). Raise `FAI_RUN_TIMEOUT_MS` for a longer-running program.

## FAI1xxx — Lexing & parsing

### FAI1001 — unexpected character

**Severity:** error

The lexer met a character that cannot begin any token. Remove or correct the stray character.

### FAI1002 — unterminated string literal

**Severity:** error

A string literal reached the end of the line or file without a closing double quote. Add the missing `"`.

### FAI1003 — unterminated block comment

**Severity:** error

A `(*` block comment was never closed. Add the matching `*)` (block comments nest, so each `(*` needs its own `*)`).

### FAI1004 — invalid character literal

**Severity:** error

A character literal is malformed — empty, multi-character, or missing its closing quote. A char literal holds exactly one character, e.g. `'a'`.

### FAI1005 — invalid numeric literal

**Severity:** error

A numeric literal has invalid digits for its base or a trailing identifier character. Check the digits and remove any stray suffix.

### FAI1006 — invalid escape sequence

**Severity:** error

A string or character literal contains an unrecognized `\` escape. The supported escapes are `\n \t \r \0 \\ \" \' \u{…}`.

### FAI1020 — syntax error

**Severity:** error

The parser found an unexpected token or a token it expected was missing. The message names what was expected; the parser recovers and continues so later errors are still reported.

### FAI1021 — layout/indentation error

**Severity:** error

Indentation does not fit the offside rule — typically a block body that is not indented past its enclosing block. `fai fmt` produces the canonical layout.

### FAI1022 — malformed module header

**Severity:** error

Every file must begin with a `module Name` header naming an upper-case module; it is missing or malformed.

### FAI1030 — construct not yet supported

**Severity:** error

A reserved construct that the parser recognizes but does not yet implement. It is rejected and recovered from until the feature lands.

## FAI2xxx — Name resolution & visibility

### FAI2001 — unbound name

**Severity:** error

A name could not be resolved to any local, this module's top level, or the auto-imported prelude. Check for a typo, a missing binding, or a needed qualified `Module.name`.

### FAI2002 — ambiguous name

**Severity:** error

A bare name resolves to more than one definition. Disambiguate it with a qualified `Module.name`.

### FAI2003 — reference to a private binding

**Severity:** error

A qualified reference names a member that is not `public` in the target module, so it is not visible across files. Mark the member `public` (and give it a signature), or move the caller into the same file.

### FAI2004 — duplicate definition

**Severity:** error

Two bindings in the same module scope share a name. Rename or remove one.

### FAI2005 — signature without a binding

**Severity:** error

A type signature has no matching `let` binding of the same name. Add the binding or remove the signature.

### FAI2006 — multiple signatures for one name

**Severity:** error

A name has more than one type signature in the same scope. Keep one.

### FAI2007 — duplicate module name

**Severity:** error

Two files declare the same top-level module name; module names must be unique across the workspace. The duplicated name is excluded from cross-module lookup until resolved.

### FAI2008 — unresolved module

**Severity:** error

A qualified path's leading segment names no module — neither a nested module in scope nor a workspace file module. Check the module name.

### FAI2009 — visibility marker on a binding with a signature

**Severity:** error

Visibility lives on the signature, so a `let` binding may not carry `public` when a signature already exists. Move `public` to the signature.

### FAI2010 — binding shadows a prelude name

**Severity:** warning

A binding reuses a name auto-imported from the prelude, hiding it in this scope. Rename the binding if the prelude name was intended.

### FAI2011 — duplicate forall binder

**Severity:** error

A `forall` contract lists the same binder name twice. Give each binder a distinct name.

### FAI2012 — unbound constructor

**Severity:** error

An upper-case name in expression or pattern position is not a known data constructor. Check for a typo or a missing `type` declaration.

### FAI2013 — duplicate auto-imported export

**Severity:** warning

More than one auto-imported module exports the same name; auto-imported modules must export disjoint names. (Contributor-facing: it concerns the standard library's own modules.)

### FAI2014 — intrinsics used outside the standard library

**Severity:** error

The prelude-private `Prim.*` intrinsics are reachable only from standard-library modules. Use the public wrapper (e.g. `Int.toString`) instead.

### FAI2015 — private type exposed by a public signature

**Severity:** error

A public surface (a signature, alias body, or constructor field) names a same-file type that is not itself cross-file-accessible. Make the type public, or make the surface private.

### FAI2016 — name already declared in this module

**Severity:** error

A nested module's name collides with another module, type, interface, or constructor in the same scope (they share the upper-case namespace). Rename one.

### FAI2017 — module name used as a value or type

**Severity:** error

A qualified path resolved to a module rather than a member. Name a member of the module (e.g. `Module.value`).

### FAI2018 — constructor of an opaque type

**Severity:** error

A constructor of an `opaque` type is referenced from another file. An opaque type exports its name but not its constructors, so it can only be built and matched through the functions its module provides. Use those operations instead of the constructor.

### FAI2019 — foreign declaration cannot be public

**Severity:** error

A `foreign` declaration binds a raw native function and is always module-private; it cannot be marked `public`. Expose its behavior through a capability interface (an instance whose methods call it) and make that public instead.

## FAI3xxx — Types & rows

### FAI3001 — type mismatch

**Severity:** error

Two types that had to be equal could not be unified (e.g. an `Int` used where a `String` was expected). The message shows the expected and actual types. Note there is no implicit `Int`/`Float` coercion — use `Int.toFloat`/`Float.toInt`.

### FAI3002 — infinite type (occurs check)

**Severity:** error

Unification would make a type contain itself (an infinite type), usually from a self-application or a mis-shaped recursive definition. Add a signature or fix the recursion.

### FAI3003 — missing public signature

**Severity:** error

Every `public` binding must carry an explicit type signature (so a module's API is readable from its signatures alone). Add the signature on the line above the binding.

### FAI3004 — signature disagrees with inferred type

**Severity:** error

A binding's declared signature does not match the type inferred from its body (signatures are checked, not trusted). Fix the body or the signature.

### FAI3005 — ambiguous type

**Severity:** error

Inference could not determine a type (e.g. an unresolved numeric or constrained variable that would escape without a signature). Add a type annotation or a conversion.

### FAI3006 — equality on a function type

**Severity:** error

`=`/`<>` (and ordering) are structural and undefined on function-typed values. Compare the results of applying the functions instead.

### FAI3007 — contract is not Bool

**Severity:** error

An `example`/`forall` contract body must have type `Bool`. Make the body a boolean expression (often an equality).

### FAI3008 — unknown type constructor

**Severity:** error

A type name in a signature or declaration is not a known built-in, in-scope, prelude, or qualified type. Check the spelling or qualify it.

### FAI3009 — record field access not supported yet

**Severity:** error

A record field access shape is not yet supported by the type checker. (Retired in current builds; kept reserved so the code is never reused.)

### FAI3010 — duplicate record field label

**Severity:** error

A record type or literal lists the same field label twice. Records have no duplicate labels; remove the repeat.

### FAI3011 — wrong number of constructor arguments

**Severity:** error

A data constructor was applied to the wrong number of arguments. Supply exactly the fields the constructor declares.

### FAI3012 — wrong number of type arguments

**Severity:** error

A type constructor or interface was applied to the wrong number of type arguments. Match the declared parameter count.

### FAI3013 — recursive type alias

**Severity:** error

A transparent `type` alias refers to itself (directly or indirectly); aliases must be acyclic. Use a discriminated union for a recursive type.

### FAI3014 — unknown interface method

**Severity:** error

An interface instance defines a method the interface does not declare. Match the interface's method set.

### FAI3015 — interface instance method set mismatch

**Severity:** error

An interface instance does not implement exactly the interface's methods (some missing or extra). Provide each declared method once.

### FAI3016 — not an interface

**Severity:** error

An instance `{ Name with … }` names something that is not an interface. Use a declared interface name.

### FAI3017 — sealed built-in interface cannot be instantiated

**Severity:** error

The operator interfaces (`Num`/`Eq`/`Ord`) are sealed to their built-in instances and cannot be instantiated by user code.

### FAI3018 — access to an opaque type's representation

**Severity:** error

An opaque type's representation (its record fields or alias body) is accessed from another file — a field access, record construction, or `{ r with … }` update. An opaque type exports its name but not its structure, so build and inspect its values through the functions its module provides.

### FAI3019 — parameter used as both a type and an effect

**Severity:** error

A type or interface parameter (`'a`) is used in type position and also as an effect row (after `/`). A parameter is one kind or the other — give the type use and the effect use separate parameters.

### FAI3020 — wrong kind of type argument

**Severity:** error

A type or interface argument has the wrong kind: an effect row (`{ Console }`) was supplied for a type parameter, a type for an effect parameter, or an effect row was written somewhere other than an argument position. Supply the kind the parameter expects.

## FAI4xxx — Exhaustiveness & patterns

### FAI4001 — non-exhaustive match

**Severity:** error

A `match` does not cover every possible value of the scrutinee. Add the missing arms, or a `_` catch-all.

### FAI4002 — unreachable match arm

**Severity:** error

A `match` arm can never be reached because earlier arms already cover its values. Remove or reorder it.

## FAI5xxx — Capabilities

### FAI5001 — effect disagrees with inferred effect

**Severity:** error

A binding's declared effect row (the capabilities after `/`) does not match the effect inferred from its body — it either performs a capability the signature omits, or declares one it never uses. Fix the body or the declared effect.

### FAI5002 — foreign declaration must name a capability

**Severity:** error

A `foreign` declaration calls native code, so its signature must name a capability in its effect row (e.g. `: String -> Unit / { Console }`). This keeps a function's reach visible: a caller of the foreign then surfaces that capability in its own effect, rather than laundering a native side effect as pure.

### FAI5003 — foreign type cannot be marshalled

**Severity:** error

A user `foreign` declaration's arguments and result are marshalled across a plain native ABI, so each must be `Int`, `Float`, `Bool`, or `String` (and the result may also be `Unit`). A richer type (a list, record, tuple, function, or type variable) cannot cross the boundary; wrap the native function so its signature uses only the supported types.

## FAI6xxx — Contracts

### FAI6001 — contract failed

**Severity:** error

An `example`/`forall` contract did not hold. `fai check` evaluates closed `example` contracts and reports a failing one here; `fai test` runs the rest (every `example` and `forall`), reporting a `forall` failure with the shrunk counterexample (binder names and rendered values) in the help.

### FAI6002 — contract cannot be run

**Severity:** error

A contract cannot be exercised because a binder's type has no value generator — a function-typed binder, a row-polymorphic (open) record, or too many binders.

### FAI6003 — contract aborted at runtime

**Severity:** error

The contract aborted while being checked: a generated input drove the body into a runtime trap (e.g. integer division by zero), or it did not finish within the time limit. Each contract runs in an isolated worker, so the abort fails only this contract — the rest of the run continues.

### FAI6004 — impure contract

**Severity:** error

An `example`/`forall` contract references a host capability — `Console`, `Clock`, `Random`, `FileSystem`, `Env`, or the `Runtime` that bundles them. Contracts are checked by `fai check` and run by `fai test`, so they must be deterministic and pure and cannot reach a capability. Express the law over pure values instead.

### FAI6005 — binder type has no finite value

**Severity:** error

A `forall` binder's type cannot be generated because it has no finite value: every constructor is recursive, with no base case to terminate generation (e.g. `type S = Cons Int S`, or a mutually-recursive group where no member bottoms out). Add a non-recursive constructor, or supply a custom `Arbitrary` for the type.

### FAI6006 — ambiguous custom generator

**Severity:** error

More than one top-level `Arbitrary` value matches a binder's type, so which one overrides the synthesized generator is ambiguous. Keep a single `Arbitrary` for the type in the contract's module.

## FAI7xxx — Native backend

### FAI7001 — construct not supported by the native backend yet

**Severity:** error

A definition reachable from `main` uses a construct the native backend does not lower yet. Reported only for reachable code, so unused unsupported constructs still type-check.

### FAI7002 — row-polymorphic record access not yet supported by the native backend

**Severity:** error

Reserved for a row-polymorphic record access or update the backend could not compile. Such access now lowers via offset-evidence passing, so this is kept reserved and not normally emitted.
