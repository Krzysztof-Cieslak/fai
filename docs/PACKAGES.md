# Fai project, package, and dependency system — v1 specification

**Status:** Design specification; detailed completion choices remain open for review.

**Date:** 2026-10-12.

**Repository:** Fai compiler workspace.

**Scope:** Manifests, source scopes, imports, workspaces, dependency resolution,
locks, overrides, toolchains, platforms, fetching, vendoring, publishing, and tooling.

> **Implementation status:** `fai-package` provides the pure metadata foundation:
> canonical source identities, version requirements, located manifest validation,
> comment-preserving version edits, platform activation, and target source plans.
> The compiler also carries package environments and target-local source ownership,
> with scoped module lookup, internal visibility, and package-qualified native symbols.
> Explicit module imports and identity-preserving public re-exports are implemented
> in syntax, resolution, formatting, and IDE lookup for package-aware compiler inputs.
> The shared project loader now prepares local path dependencies, root path
> overrides, host source scopes, and native requirements for CLI, daemon, and LSP.
> Workspace/member and executable selection use those manifests. `init`, `add`,
> `remove`, `resolve`, `fetch`, `update`, and read-only `deps` are integrated with
> Git restoration, graph-local locks, and shared/personal baseline reconciliation.
> A provider-independent graph resolver implements conservative and eager policies,
> source-selector unification, and compiler/target filtering against supplied catalogs.
> Locked/offline/frozen commands verify exact sources and preserve valid selections.
> Vendoring supplies verified, cache-independent source snapshots with explicit
> bypass and refresh behavior. Publication validates isolated candidate graphs and
> journals version/commit/tag state before atomic pushes, with advisory API reports
> and exact resume. Installed compiler pins dispatch through an explicit local
> inventory, and standalone files retain isolated source scope. Editor restoration
> runs in the background with coalesced generations and authoritative source overlays.
> In-tree ecosystem migration remains in development.
> Native-only and unversioned directory sessions retain compatibility discovery.

This document consolidates the agreed product decisions and proposes the concrete
syntax and operational details needed to implement them. Section 23 explicitly
identifies completion choices that were not individually decided in discussion.
`MUST`, `MUST NOT`, `SHOULD`, and `MAY` describe the proposed v1 contract.
Examples use illustrative repositories, versions, and hashes. Implementation paths
are relative to the Fai repository above.

## Contents

1. Goals and agreed product model
2. Terminology and invariants
3. Project discovery and layout
4. Manifest schema
5. Sources, targets, and visibility
6. Imports and re-exports
7. Package identity and dependency sources
8. Version requirements and resolution
9. Compiler and platform policy
10. Overrides and personal development
11. Lockfile model and schema
12. Commands and state transitions
13. Git fetching and source integrity
14. Caching and offline operation
15. Vendoring
16. Publishing and release transactions
17. Daemon and language-server integration
18. Diagnostics and machine-readable output
19. End-to-end workflows
20. Compiler integration and migration
21. Implementation sequence
22. Acceptance scenarios
23. Completion choices requiring review

## 1. Goals and agreed product model

The system makes ordinary edit/check/test loops convenient while keeping source
bindings, selected dependency content, and compatibility claims explicit. It
requires no custom hosted registry or artifact service. Standard Git hosting,
including GitHub and private Git servers, supplies source storage and refs.

| Area | Agreed decision |
|---|---|
| Distribution | Git source packages, fetched through installed Git |
| Identity | Repository coordinates plus package subdirectory; dependency names are local aliases |
| Releases | Independent package versions with path-scoped tags |
| Root package tag | `v1.2.3` |
| Subdirectory package tag | `<subdir>/v1.2.3`, such as `packages/json/v1.2.3` |
| Selection | Newest compatible choices under a deterministic preference policy |
| Multiplicity | One selected instance per logical package in a resolution; independent roots/platforms may differ |
| Build/test consistency | A root package's production and tests share normal-dependency selections |
| Default add requirement | Caret range based on the selected version |
| Bare requirement | `1.2.3` means exactly `1.2.3` |
| Prereleases | Opt in explicitly for the corresponding major/minor/patch release |
| Shared lock | Commit exact selections and graph edges for applications and libraries |
| Lock repair | Automatic for affected graphs; locked mode forbids changes |
| Validation scope | Only graphs selected by the command must be valid |
| Add versus update | Add conservatively; targeted update prioritizes the requested package |
| Git selectors | Release ranges, full commits, and branches, including transitive branches |
| Unreleased substitution | A matching manifest version alone does not make an untagged commit satisfy a release range; use a root override |
| Library surface | Every shared library module's public members; no exported-module allowlist |
| Source scopes | Shared library modules, separate executable entries, separate test sources |
| Imports | Explicit file-local, module-only imports for external packages |
| Local/std modules | Automatically available according to source scope |
| Re-exports | `public import`, retaining original symbol/type identities |
| Name collisions | Require an explicit non-conflicting alias |
| Transitive APIs | Direct dependency or explicit public re-export |
| Internal access | Same package ownership, including its additional tests |
| Development dependencies | Separate test sources only; inline production contracts use normal dependencies |
| Check coverage | All targets of selected packages, including their closed examples |
| Dependency contracts | Executed only when explicitly selected |
| Workspace defaults | Root check/test covers all members; member-directory invocation covers that member |
| Executables | Libraries plus multiple binaries; conventional `src/Main.fai` with explicit overrides/additions |
| Standalone mode | A file outside a manifest project loads that file and std only |
| Optional integrations | Separate adapter packages instead of package feature flags |
| Overrides | Root-controlled, optionally member-scoped, with version constraints retained |
| Personal work | Automatic `fai.local.toml` and separate `fai.local.lock`, visibly active |
| Override conflicts | Local wins over shared; overlaps within one layer are errors |
| Baseline tracking | Local graphs reconcile corresponding shared-lock changes |
| Active context | Updates affect the active context; `--no-local` selects shared |
| Add in local context | Change the shared manifest and active local lock; affected shared graphs may remain stale |
| Compiler | Compatibility ranges plus optional exact root pin; select an installed matching compiler, install explicitly |
| Compiler mismatch | A valid lock fails clearly rather than silently downgrading |
| Platforms | Host compilation first; platform-specific sources, dependencies, and native settings |
| Lock coverage | Declared target matrix for updates; host default when absent |
| Platform alias conflicts | Reject duplicate active aliases |
| Native dependencies | System-library linking; no native source-build or binary-download subsystem in v1 |
| Offline | Explicit offline resolution; no automatic stale-metadata fallback after a required refresh fails |
| Vendoring | Included; original identities and integrity records retained |
| Editor | Background restoration using the CLI's context and lock rules |
| Publishing | Prepare versions/commit, validate, report API diff, tag, push |
| Release intent | Explicit per-package CLI bump or exact version |
| Release inputs | Clean checkout; application/library changes already committed |
| Validation | Fresh consumer-style validation of the exact release candidate |
| Batches | Selected same-repository packages, independent versions and individual tags |
| Batch dependencies | Keep authored compatibility requirements |
| Publication | Push branch and selected tags; optional plan-only mode |

## 2. Terminology and invariants

### 2.1 Terms

- **Package:** A manifest and owned source tree. It owns an `internal` visibility
  boundary and normal dependency declarations.
- **Workspace:** Explicit local membership plus shared root configuration/locks.
- **Root member:** The package whose own build/test environment is being resolved.
  A package can be a root in one graph and a dependency in another.
- **Target:** A library, executable, or additional-test source scope.
- **Target platform:** A canonical target triple and OS/architecture/environment
  descriptor, distinct from an executable or test target.
- **Context:** `shared`, or `local` when the personal overlay is active.
- **Resolution key:** `(context, root-member-path, target-triple)`.
- **Logical package key:** The identity named by declarations. For Git it is
  `(canonical repository locator, normalized package subdirectory)`.
- **Actual source:** The Git commit/subdirectory or local directory supplying an
  instance after root overrides.
- **Package instance:** A logical key bound to selected source and package metadata.
- **Resolution graph:** Selected instances and alias-labelled edges for one key,
  including the root's development environment.
- **Production projection:** The root package and closure of its normal edges.
- **Local input:** Mutable root/workspace/path source, whose current bytes are
  compiler inputs rather than immutable content promised by a lock.

### 2.2 Invariants

1. Each resolution has at most one instance per logical package key.
2. Different resolution keys may select different versions, commits, or sources.
3. A root's production/test targets share selections for normal dependencies.
4. A consumed dependency contributes normal dependencies, not its development
   dependencies, own lock decisions, toolchain pin, or root overrides.
5. Each package imports through its own aliases, not its consumer's aliases.
6. A valid lock is authoritative. A cache miss does not authorize changing versions.
7. Successful resolution satisfies all active version, compiler, platform, and
   source-selector constraints.
8. Network failure is not evidence that a candidate version is incompatible.
9. Missing content never causes silent substitution of another commit.
10. Semantic compiler queries consume immutable graph inputs; Git/network I/O is
    outside Salsa queries.
11. A resolution can be committed only while its relevant inputs and lock revision
    still match the observed snapshot.
12. Ownership is loader metadata. Paths, aliases, and names cannot grant std privileges.

## 3. Project discovery and layout

### 3.1 Files

| File/directory | Purpose |
|---|---|
| `fai.toml` | Package and/or workspace manifest |
| `fai.lock` | Shared graphs; committed |
| `fai.local.toml` | Root-only personal overrides; normally ignored by Git |
| `fai.local.lock` | Personal graphs and shared-baseline records; normally ignored |
| `src/` | Default shared source root |
| `src/Main.fai` | Conventional executable entry |
| `tests/` | Default additional test root |
| `vendor/` | Default vendored Git source storage |
| `.fai/` | Generated transactions, outputs, and release receipts; ignored |

`fai init` supplies ignore entries for personal files and generated state. Vendor
storage is intended to be committable.

### 3.2 Discovery

For a path-oriented command, start at the selected file/directory; otherwise use
the current directory. Find the nearest containing package manifest and an
ancestor workspace explicitly including it. The active workspace root owns locks
and root settings.

`--project <dir>` / `-C <dir>` explicitly selects the discovery base. Paths supplied
with it are relative to that project. An outside source is an error, not an
implicit extra package. A Git repository boundary alone is not a v1 Fai project.

Outside a manifest project, file invocations of `run`, `build`, `check`, and `test`
create a standalone scope containing that file and embedded std. They do not scan
siblings. Multiple local files or external dependencies require a manifest.
Formatting may walk explicitly selected files/directories without creating a
semantic package.

### 3.3 Workspace membership

```toml
manifest-version = 1

[workspace]
members = ["apps/*", "packages/*"]
```

Members are root-relative package directories. Globs expand deterministically;
duplicate paths are deduplicated. Nested workspaces are separate ownership domains
and cannot simultaneously be members of an enclosing workspace. A root with both
`[workspace]` and `[package]` includes `.` as a member.

Member paths are stable root identifiers. Package names are convenient CLI
selectors; ambiguous names require explicit relative paths.

Only the active root's toolchain, target matrix, overrides, and vendor settings
participate. A member's standalone root settings are inactive while it belongs to
that workspace, just as a dependency's root settings are inactive when consumed.

## 4. Manifest schema

### 4.1 General rules

- Syntax: TOML, with `manifest-version = 1` in new manifests.
- At least `[package]` or `[workspace]` is required.
- Unknown fields are errors except inside free-form `[metadata]`.
- Invalid combinations, duplicate aliases, and duplicate target names have located
  diagnostics.
- Paths are relative to the declaring manifest unless specified otherwise.
- Commands preserve comments when editing user manifests.
- Dependency declarations are data, not executable build scripts.

### 4.2 Package example

```toml
manifest-version = 1
lock-targets = ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]

[package]
name = "example-app"
version = "0.1.0"
fai-version = ">=0.5.0, <0.6.0"
source-dir = "src"
test-dir = "tests"
auto-main = true
default-run = "example-app"
description = "An illustrative Fai application"
license = "MIT"

[toolchain]
version = "0.5.3"

[dependencies]
json = { git = "https://github.com/example/libs.git", subdir = "packages/json", version = "^1.4.0" }

[dev-dependencies]
fixtures = { git = "https://github.com/example/fixtures.git", version = "^0.2.0" }

[[bin]]
name = "migrate"
path = "tools/Migrate.fai"

[native]
library-dirs = ["native"]
libraries = ["example_native"]
objects = []

[[platform]]
when = { os = "windows" }
source-dirs = ["platform/windows/src"]
test-source-dirs = ["platform/windows/tests"]

[platform.dependencies]
windows = { git = "https://github.com/example/windows-support.git", version = "^1.0.0" }

[platform.native]
libraries = ["ws2_32"]

[[platform]]
when = { os = "linux" }
source-dirs = ["platform/linux/src"]
test-source-dirs = ["platform/linux/tests"]

[metadata]
owner = "example-team"
```

### 4.3 Root fields

| Field | Type | Meaning |
|---|---|---|
| `manifest-version` | integer `1` | Schema version |
| `package` | table | Package metadata and source conventions |
| `workspace` | table | Explicit local membership |
| `dependencies` | alias → dependency | Normal dependencies |
| `dev-dependencies` | alias → dependency | Root package's development dependencies |
| `bin` | array of tables | Explicit executable entries |
| `native` | table | Common native link/load requirements |
| `platform` | array of tables | Conditional sources, dependencies, native requirements |
| `toolchain` | table | Active-root compiler pin |
| `lock-targets` | array of target strings | Active-root metadata update/preparation matrix |
| `overrides` | array of tables | Active-root shared replacements |
| `vendor` | table | Active-root vendor materialization configuration |
| `metadata` | table | Tool-owned metadata with no compiler/resolver semantics |

Package-specific fields require `[package]`. Workspace settings do not implicitly
inject normal dependencies or native requirements into members.

### 4.4 Package fields

| Field | Requirement/default | Meaning |
|---|---|---|
| `name` | required | Display/local CLI name, not global identity |
| `version` | required | Canonical package version |
| `fai-version` | local default `*`; explicit for publishing | Supported compiler releases |
| `source-dir` | `src` | Common library root |
| `test-dir` | `tests` | Additional test root |
| `auto-main` | `true` | Infer `<source-dir>/Main.fai` as an executable |
| `default-run` | absent | Preferred executable when `--bin` is absent |
| `supported-platforms` | absent means unrestricted | OR-list of platform predicates |
| `description`, `license`, `repository` | optional strings | Descriptive metadata; repository metadata does not replace source identity |

Package names have a portable lower-case hyphenated spelling, e.g. `http-client`.
Dependency aliases are valid non-keyword Fai identifiers, conventionally lower
case. The default CLI alias replaces name hyphens with underscores. A collision
or keyword requires `--as`.

### 4.5 Executable fields

Each `[[bin]]` has a unique `name` and a package-relative entry-file `path`.
An explicit declaration replaces an inferred target with the same name, or claims
the same entry path without creating a duplicate inferred target.

`default-run` must identify an existing executable. Selection is:

1. Explicit `--bin`.
2. `package.default-run`.
3. The only executable, when exactly one exists.
4. Otherwise a located ambiguity/no-executable error with available choices.

### 4.6 Dependency fields

A Git dependency has `git`, optional `subdir` (default `.`), and exactly one of:

| Field | Meaning |
|---|---|
| `version` | Requirement over canonical release tags |
| `rev` | Full commit ID; no abbreviations or revision expressions |
| `branch` | Branch name resolved to a commit retained in the lock |

A local dependency has `path` and optionally `version`, which checks its manifest
version. `path` and `git` are mutually exclusive. Paths identify package directories,
not source files. Published normal/development declarations use portable Git
locations; local paths are for development or root overrides.

```toml
[dependencies]
json = { git = "https://github.com/example/json.git", version = "^1.4.0" }
preview = { git = "https://github.com/example/preview.git", branch = "main" }
fixed = { git = "https://github.com/example/fixed.git", rev = "1111111111111111111111111111111111111111" }
local_tools = { path = "../tools", version = "^0.1.0" }
```

An alias cannot occur in both active normal and development tables. Multiple
aliases may intentionally identify one logical package, but their constraints
must be met by the same selected instance.

### 4.7 Platform fields

Each `[[platform]]` has:

- `when`: a nonempty conjunction of exact `os`, `arch`, and/or `env` strings.
- Optional `source-dirs` and `test-source-dirs` arrays.
- Optional `dependencies`, `dev-dependencies`, and `native` tables.

OS names include `linux`, `macos`, and `windows`; other fields follow canonical
target descriptors exposed by the toolchain. Unknown predicate fields/targets are
errors. Matching entries are additive, together with common configuration.

There is no most-specific-wins rule for dependency aliases. Duplicate active
aliases are errors even if their current values match. Express alternatives as
disjoint cases.

### 4.8 Native fields

Preserve `library-dirs`, `libraries`, and `objects` as ordered arrays. A proposed
`frameworks` array supports Apple frameworks. Relative files resolve against the
owning package; system library names remain environment requirements.

Common arrays precede active platform additions in declaration order. Propagate
native requirements through the selected production/test projection. Emit package
requirements in stable dependent-before-dependency topological order and retain
each package's authored order, including intentionally repeated linker inputs.

AOT links declared objects/libraries. JIT loads dynamic libraries/frameworks.
Objects-only declarations do not create a JIT implementation; preserve a clear
error when a required run-path native dependency cannot be loaded.

## 5. Sources, targets, and visibility

### 5.1 Source ownership

Fai source roots and executable entry paths must remain inside the package
directory. An outside local dependency is a separate package, not an extra source
root of this one.

Classify sources in this order:

1. Known executable entries are target-local, including the conventional Main
   path while `auto-main` is enabled.
2. Additional test roots supply test-local modules.
3. Active platform source roots supply shared library modules.
4. Common sources supply remaining shared library modules.

Common scanning prunes test roots, every declared platform source root (including
inactive roots), nested package boundaries, and known executable entries. A more
specific reserved root owns its subtree: inactive sources cannot leak through a
broader active scan. Conflicting declarations of the same root role are errors.

Scans find `.fai` files recursively in stable order. Generated state, `.git`, and
configured vendor storage are excluded. Source membership does not depend on a
developer's global Git ignore rules. Filesystem paths are not module names;
module headers retain their existing meaning.

### 5.2 Target scopes

| Source scope | Automatically available package modules | External aliases |
|---|---|---|
| Library | Shared library modules | Normal dependencies |
| Executable | Shared library modules and its own entry | Normal dependencies |
| Additional tests | Shared library and additional test modules | Normal + development dependencies |
| Consumer | Its own package modules | Its own direct dependencies |

The initial executable model is one entry file plus shared library modules.
Private/nested helpers can be in the entry; reusable multi-file logic belongs in
shared modules. Different executable entries may each declare `module Main`.
An entry or test module cannot silently shadow a shared module in its own scope.

Production files retain production lookup rules during tests. Loading test source
must not make test-only modules or development aliases available to production
files.

### 5.3 Visibility

- `public`: available through the public surface of a shared library module.
- `internal`: available to scopes owned by the same selected package instance,
  including its executables and additional tests.
- `private`: file-local under existing lexical/nested-module rules.

Workspace peers are separate packages. Std retains its separate trusted origin.
Every shared library module's public surface is importable; executable and test
sources do not become a dependency's library API. Existing signature, opacity,
and public-surface leak rules apply through aliases/re-exports.

### 5.4 Checking and contracts

`check` covers all targets of selected packages and retains closed-example
execution and `--no-examples` semantics. `test` runs their examples/properties.
Shared library contracts execute once per resolution, not once per binary.
Executable/test contracts run in their own source contexts. Existing worker
isolation, effect restrictions, and resource controls apply.

Dependency code receives normal name/type validation, including its contract
declarations, but dependency contracts do not execute implicitly. Explicit
selection of dependency contracts executes inline library contracts against the
consumer's current graph, without importing the dependency's development environment.
Testing that dependency's full development suite selects it as its own root.

## 6. Imports and re-exports

### 6.1 Syntax

Imports follow the module header and precede ordinary declarations:

```fai
module Web

import http.HttpClient as Client
public import json.Json
```

```text
import-declaration := [ "public" ] "import" import-path [ "as" module-name ]
import-path        := dependency-alias "." module-name ( "." module-name )*
```

Imports bind modules only. There are no selective value/type imports or wildcard
opens. They are file-level declarations, not declarations inside functions or
nested modules.

The first component is an active direct dependency alias. The next is one of that
dependency's shared library modules. Further components traverse public nested
modules or explicit public module re-exports. Without `as`, the final component
supplies the local name.

### 6.2 Bindings and collisions

An ordinary import is file-local. Collision with an automatically available
local/std module, another import, or a declaration in the same module namespace
is an error with an alias suggestion. Import order is not precedence.

The same identity-based resolution applies to values, constructors, types,
interfaces, and effects. Dotted paths are not implemented by textual substitution.

### 6.3 Re-exports

`public import json.Json` in `module Web` publishes the alias `Web.Json`. It exposes
the original public surface and retains original symbol/type/interface/effect
identities and signatures.

```fai
module App

import web.Web
import web.Web.Json
```

Only `web` must be a direct dependency. Traversing an arbitrary private dependency
of `web` is prohibited. Direct and re-exported access to the same selected package
denotes the same nominal types. A separately declared fork is a different logical
package; a root override retains the original logical key.

A public signature may refer through a file-private import binding to an
underlying public type. Exported interfaces retain the resolved identity, not a
requirement that consumers can see that private alias. Consumers may infer and
forward such values; naming the type explicitly requires a direct dependency or
public re-export. The existing leak rule still rejects genuinely private/internal
underlying types in a public signature.

Canonical formatting groups imports after the header and may sort them while
preserving attached comments. Sorting cannot change resolution.

## 7. Package identity and dependency sources

### 7.1 Git identity

```text
GitPackageKey = (canonical-repository-locator, normalized-subdirectory)
```

Names, aliases, and selectors do not create independent package identities.
Two subdirectories are separate packages even at one commit. One logical Git
package can be selected at different revisions in independent graphs.

The CLI expands `github:owner/repository` to a canonical HTTPS locator. Known
GitHub HTTPS/SSH equivalents normalize together, including owner/repository case
and an optional `.git` suffix. Generic-host normalization is conservative: retain
semantically significant paths, ports, and SSH user components. Do not equate
arbitrary SSH and HTTPS paths merely because their strings resemble each other.

Git transport rewrites, credentials, or mirrors can change the retrieval location
without changing the declaration's identity. Credentials never enter lockfiles,
cache names, or diagnostic URLs.

Support HTTPS and SSH, including normalized SCP-style SSH. Local `file:` Git
locations are permitted for development/tests but not published declarations.
Arbitrary external-helper strings and revision expressions are not package URLs.

### 7.2 Package paths and tags

Subdirectories normalize to repository-relative `/` paths without `.` or `..`
components and cannot escape the repository. A published path must also be a valid
Git ref prefix so its canonical tag can exist.

```text
root package       -> v1.4.0
packages/json      -> packages/json/v1.4.0
```

Tags identify whole-repository commits; Fai selects the relevant directory from
that snapshot. Web may come from commit A and JSON from commit B in one graph.
One commit may carry several package release tags.

The package manifest at the selected commit must exist and declare the tagged
version. Its source paths are relative to that package directory.

### 7.3 Local identities and replacements

Root/workspace/path packages have local identities derived from canonical package
directories; shared serialization uses root-relative paths where possible. Local
source edits do not generate a new compiler owner identity on every keystroke.

An override binds an original logical Git key to a path or another Git source.
The lock records the original key and actual source separately.

If an explicit override maps a dependency key to the selected root's own package
directory, root and dependency refer to one owner. This enables helpers that depend
on the library under test. Assigning multiple logical identities to that same root
owner in one graph is an ambiguity error. Equality of paths alone does not merge
unrelated Git identities.

### 7.4 Combining selectors

For one logical key:

- Release-range edges intersect over canonical tagged releases.
- Exact-commit selectors must agree on the commit.
- Branch selectors pin commits; different branches can agree at one commit.
- A release range and commit/branch can agree when that commit is the corresponding
  valid tagged release satisfying the range.
- An untagged commit's matching manifest version is insufficient to satisfy a
  release-range edge. Substitution requires an explicit root override.

An override replaces original branch/commit provenance while retaining every
active version-range constraint. Its declared version and compiler/platform
support must meet those constraints. It never creates two instances of the
original logical key in one graph.

## 8. Version requirements and resolution

### 8.1 Concrete version grammar

Use the SemVer major/minor/patch core and optional prerelease identifiers. This
draft excludes `+build` metadata from package versions/requirements; Git commits
and compiler build IDs carry build identity. This is a completion choice in §23.

All three numeric components are required. Partial forms (`1`, `1.2`, `1.x`) are
rejected with a full-range suggestion. SemVer leading-zero and prerelease ordering
rules apply.

```text
requirement := conjunction ( "||" conjunction )*
conjunction := comparator ( "," comparator )*
comparator  := "*" | [ "=" | "^" | "~" | ">=" | ">" | "<=" | "<" ] version
```

Whitespace can surround operators/separators. Commas intersect and `||` unions.
A bare full version is exact. Canonical formatting uses full versions,
comma-space intersections, and space-delimited `||`.

| Requirement | Stable-version meaning |
|---|---|
| `1.2.3` or `=1.2.3` | exactly `1.2.3` |
| `^1.2.3` | `>=1.2.3, <2.0.0` |
| `^0.2.3` | `>=0.2.3, <0.3.0` |
| `^0.0.3` | `>=0.0.3, <0.0.4` |
| `~1.2.3` | `>=1.2.3, <1.3.0` |
| `*` | any stable version |

### 8.2 Prereleases

A prerelease satisfies a conjunction only when it meets the ordering comparisons
and that conjunction explicitly mentions a prerelease with the same
major/minor/patch core. Union arms are evaluated independently.

`^2.0.0-rc.1` therefore admits further 2.0.0 previews, 2.0.0, and compatible stable
2.x releases, but not 2.1.0-beta.1. Sorting below a stable upper bound alone never
opts a prerelease in.

### 8.3 Graph construction

Resolve the root's normal and development declarations together. Normal edges
originate at package nodes; development edges originate at a synthetic root
context, not the package's exported dependency node. This allows a development
helper to refer back to the root library without introducing a false production
cycle. Actual normal package dependency cycles are errors.

A dependency contributes only normal edges. Graph membership follows active
manifest declarations rather than currently reachable imports. An unused declared
dependency can constrain resolution; dead-code elimination is a compiler concern.

Filter candidates by requirements, source selectors, compiler compatibility, and
platform support. Read each candidate manifest from its selected snapshot before
accepting its outgoing edges.

### 8.4 Preference policies

There is not necessarily a graph containing every package's independently newest
version. Define the outcome through deterministic preferences.

**Valid lock:** ordinary commands do no version selection. Validate eligibility
and restore exact content as needed.

**Conservative resolution** (`add`, `remove`, ordinary stale repair, `resolve`,
editor restoration):

1. First try retaining all still-relevant locked selections.
2. Choose newest eligible candidates for new/unlocked packages.
3. If no solution exists with those pins, relax implicated pins deterministically
   while preferring the remaining selections.
4. Drop unreachable packages; obsolete lock entries do not keep packages alive.

**Targeted update:**

1. Prioritize named logical keys in stable order.
2. Try their candidates newest-first.
3. For each candidate combination, retain other pins where possible, then relax
   them as necessary before choosing an older requested candidate.
4. Do not change authored requirements to obtain a newer version.

**Fresh/full update:** release relevant pins and use newest-first search. Prefer
root normal dependencies, then root development dependencies, then other required
packages. Canonical logical keys break ties. Manifest table order, hash-map order,
and asynchronous fetch completion cannot affect the result.

These are preferences over valid graphs, not a guarantee to minimize the global
number of changed packages. PubGrub-style propagation/conflict reporting is an
appropriate implementation, provided optimizations preserve observable policy.

### 8.5 Add without a requirement

Resolve the source under conservative-add policy before deriving its default
requirement. Write `^<selected-version>`. Thus an existing graph may cause an
unqualified add to choose an older compatible release. Report that selection and
any newer release held back by existing pins. An explicit range or later targeted
update expresses stronger intent.

Preserve user-supplied requirements semantically, with canonical formatting.
Explicit branches/commits retain their selector instead of inventing a range.

### 8.6 Mutable refs and failures

Conservative resolution keeps unchanged branch bindings pinned. Refresh a branch
when explicitly updating its logical package, performing a full update, or
resolving a new/changed branch selector. Ordinary builds do not advance branches.

Required metadata network failures and source-integrity failures are fatal, not
candidate incompatibilities. A malformed/version-mismatched release manifest is
reported as an invalid candidate and can be excluded while searching valid
releases. An observed conflicting mapping for a release being used is an integrity
error, not an invitation to choose the changed content.

Unsatisfiable resolution reports causal dependency chains, source identities,
platform/context, and manifest locations. It must not silently drop dependencies,
relax ranges, opt into previews, or install multiple instances of a logical key.

## 9. Compiler and platform policy

### 9.1 Compiler selection

Every selected package must support the compiler release. The active root may
pin an exact release:

```toml
[toolchain]
version = "0.5.3"
```

Select it before starting a daemon or LSP. Dispatch to an already-installed
matching host compiler, or report installation/selection instructions. Never
automatically download or substitute a compatible patch release.

Without a pin, use the invoked compatible compiler. Release compatibility differs
from artifact compatibility: object caches also use the actual compiler build ID
reported by `fai build-info`.

A valid lock incompatible with the selected compiler fails ordinary commands.
An explicit update/resolve can construct another graph. A compatible compiler
change alone does not upgrade a valid dependency selection.

Installation inventory details are a completion choice. They must distinguish
release, host, executable, and build identity; selection cannot depend on arbitrary
directory iteration. The invoked binary can be retained when it already matches.

### 9.2 Platforms

Evaluate predicates against the target descriptor. v1 compilation is host-only;
metadata resolution can prepare other known targets without their linker/SDK.
Inactive aliases and source roots do not enter that target's environment.

`supported-platforms` supplies explicit eligibility for platform-limited packages.
Bulk matrix preparation reports unsupported root/platform pairs as not applicable;
an explicitly requested unsupported pair is an error. Unsupported dependency
candidates are filtered by the solver.

### 9.3 Matrix

```toml
lock-targets = [
  "x86_64-unknown-linux-gnu",
  "aarch64-apple-darwin",
  "x86_64-pc-windows-msvc",
]
```

This is the default matrix for explicit update/lock preparation and vendoring.
Absent a list, use the host. Explicit metadata-command `--target` selections
replace that default.

Build/check/run/test uses the host graph and validates selected roots only.
Matrix resolution is not evidence that native builds/contracts ran on every
platform. System libraries, SDKs, and linkers are external build inputs; a source
lock does not reproduce those installations.

## 10. Overrides and personal development

### 10.1 Shape and scope

```toml
[[overrides]]
replace = { git = "https://github.com/example/libs.git", subdir = "packages/json" }
with = { path = "../json" }
members = ["apps/new-app"]
```

`replace` is a logical Git key without a version selector. `with` is a path or a
Git location with a version/branch/full-revision selector. `members` contains
exact workspace-relative member paths; it selects graphs rooted at those members,
not only edges declared by them. It includes their production/test environment
and selected target platforms. Omission means all roots in the active workspace.

### 10.2 Precedence and conflicts

1. Only active-root configuration supplies overrides.
2. Two shared entries matching the same key/root are an error.
3. Two personal entries matching the same key/root are an error.
4. A matching personal entry replaces a shared entry as a whole.
5. File order never breaks a tie.

A global and member-specific entry for the same key overlap within a layer and
therefore conflict. Disjoint member scopes are legal.

### 10.3 Personal context

The allowed semantic fields of `fai.local.toml` are `manifest-version` and
`overrides`. It cannot redefine package metadata, dependencies, source roots,
compiler pinning, or the target matrix.

File presence activates the local context automatically. Invalid local
configuration fails rather than falling back. `--no-local` ignores the local file,
even if malformed, and selects `fai.lock`. Otherwise use `fai.local.lock`, including
for a selected member with no applicable replacement. Output reports the context
and actual applicable overrides.

### 10.4 Baseline reconciliation

Each local graph records its corresponding shared baseline and baseline package
selections. Initial local resolution is seeded from that baseline if available;
a valid shared solution is not a prerequisite for personal development.

When the corresponding shared graph changes:

- Prefer newly changed shared selections where local constraints permit.
- Keep existing local choices when that key's shared selection did not change,
  including intentional local-only upgrades.
- Apply local replacements and retain all compatibility checks.

Store per-root/platform baseline identities, not a whole-lockfile timestamp/hash.
An unrelated shared graph change must not invalidate every local graph. Locked
mode reports required reconciliation rather than performing it.

## 11. Lockfile model and schema

### 11.1 Ownership and structure

One shared lock and one optional local lock belong to the active root. Each can
contain multiple root/platform graphs. Consumers ignore dependency-owned locks.
The root's normal/development union is resolved once; production uses a projection.

A lock contains:

1. Format and resolver-policy versions.
2. Context.
3. Selected package-instance descriptions.
4. Per-root/platform graph records and exact alias-labelled edges.
5. Per-graph input fingerprints and branch bindings.
6. For local graphs, corresponding shared baseline identity/selections.

**Edges belong to graphs, not globally to package records.** One package revision
may have different dependency bindings in different root/platform graphs.

| Record | Fields |
|---|---|
| File | `lock-version: 1`, `resolver-version: 1`, `context: shared|local`, `packages[]`, `graphs[]` |
| Package | `id`, `logical`, `version`, `source`, `manifest-fingerprint`, optional immutable `checksum`, and `released` evidence for accepted canonical tags |
| Git logical locator | `git`, `subdir` |
| Actual Git source | `git`, `subdir`, `rev` including Git object-format prefix |
| Local logical/source locator | `path`, relative to active root where possible |
| Graph | `id`, `root`, `target`, `root-package`, `input-fingerprint`, `packages[]`, `edges[]`, `branches[]`, informational `resolved-with` |
| Local graph additions | `base-graph-hash`, `base-packages[]` |
| Edge | `from`, `alias`, `kind: normal|development`, `to` |
| Branch binding | `logical-package`, `branch`, `rev` |

`$root` in an edge is the synthetic development environment. Normal root edges
originate at `root-package`. A development edge originates at `$root`; outgoing
edges from its selected dependency are ordinary normal edges.

IDs are opaque to readers and derived deterministically by writers from canonical
instance/graph descriptors. They cannot depend on process-local interned IDs or
checkout absolute paths. Local manifest changes can produce new lock descriptors
while the compiler retains a stable path-owned source identity for incremental
edits. Old descriptors remain while a retained graph still references them.

### 11.2 Structural example

Symbolic IDs and synthetic hashes below illustrate relationships rather than a
real repository's object database.

```toml
lock-version = 1
resolver-version = 1
context = "shared"

[[packages]]
id = "p-root"
logical = { path = "apps/server" }
version = "0.1.0"
source = { path = "apps/server" }
manifest-fingerprint = "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

[[packages]]
id = "p-json"
logical = { git = "https://github.com/example/libs.git", subdir = "packages/json" }
version = "1.5.0"
source = { git = "https://github.com/example/libs.git", subdir = "packages/json", rev = "sha1:1111111111111111111111111111111111111111" }
manifest-fingerprint = "blake3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
checksum = "blake3:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"

[[graphs]]
id = "g-server-linux"
root = "apps/server"
target = "x86_64-unknown-linux-gnu"
root-package = "p-root"
input-fingerprint = "blake3:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
packages = ["p-root", "p-json"]
resolved-with = "0.5.3"

[[graphs.edges]]
from = "p-root"
alias = "json"
kind = "normal"
to = "p-json"
```

Store full Git object IDs with `sha1:` or `sha256:`. Mutable paths do not receive
an immutable source-content promise merely because they appear in a lock.
A local graph's baseline package descriptors are retained for three-way
reconciliation even when they differ from its active selections.

### 11.3 Fingerprints and validity

Include normalized resolution inputs: active normal/development declarations,
compiler/platform requirements, relevant local package metadata, applicable
overrides for reachable keys, target, and resolver-policy version.

A package descriptor's manifest fingerprint includes its normal dependency
metadata, version, compiler/platform requirements, and active normal platform
configuration. Its development declarations contribute only when it is the
selected root, through that graph's input fingerprint. Editing a path dependency's
own dev configuration must not invalidate its consumers' normal graphs.

Exclude comments, descriptive metadata, source bodies, timestamps, generated
output locations, and unrelated graphs. Source membership and actual compiler
build identity are separate compiler/cache inputs.

Canonical descriptor hashing uses a versioned domain and canonical JSON: object
keys sorted by UTF-8 byte order, no insignificant whitespace, preserved semantic
array order, UTF-8 strings with standard JSON control-character escaping, and
integers in decimal. Semantically unordered sets are sorted before encoding.
Floating-point values are not part of these descriptors. Use distinct domains
`fai.instance.v1`, `fai.graph-key.v1`, `fai.graph-input.v1`, and
`fai.graph-content.v1`, each followed by one NUL byte. Graph IDs derive from
context/root/target; graph-content hashes describe selections and edges and omit
informational `resolved-with` history.

Check compatibility with the currently selected compiler independently.
`resolved-with` is history, not an exact pin. A still-compatible compiler change
does not force a dependency update.

Validate graph contents, not just claimed hashes: reachability, complete edges,
constraints, selector bindings, one instance per key, package metadata, and
fingerprints must agree. A fingerprint alone cannot bless malformed lock data.

Only selected graphs must be current. A stale graph can remain while another is
updated. Preserve unselected selections and all records they reference.

### 11.4 Artifact identity

An immutable source checksum describes the canonical package tree, not a
GitHub-generated archive. A compiled unit additionally depends on its exact
resolved bindings, source scope, target, compiler build identity, ABI/native
inputs where relevant, and instrumentation flags. Repository/version alone is
not a sufficient cache key.

### 11.5 Writes and recovery

Serialize deterministically without timestamps. Use temporary siblings and atomic
replacement for individual files.

Manifest-plus-lock commands use a short root-level commit transaction with
expected-input checks and a recoverable journal. Several renames are not one
filesystem-atomic operation; detect/recover interrupted tool-owned writes before
using them. Recovery must preserve files changed independently by the user.

Resolve/fetch outside the write critical section. Recheck relevant manifest,
overlay, compiler/target settings, and lock revision immediately before committing.
An observed concurrent change causes retry or a diagnostic, never a blind overwrite.

## 12. Commands and state transitions

### 12.1 Selection and flags

At a workspace root, build/check/test select all members by default. In a member
directory they select that member. `--package <name-or-relative-path>` replaces
that default and is repeatable where multiple roots make sense.

`run` requires one selected package; it uses the target rules in §4.5. A virtual
workspace with several members requires an explicit package selection. Existing
file/directory selections remain available inside the selected package's source
scopes. An explicit entry file can form an ad-hoc executable scope using that
package's normal environment; it does not add a published library module.

| Flag | Contract |
|---|---|
| `--locked` | Selected graph(s) must already be valid; no lock mutation; exact-source fetching allowed |
| `--offline` | No network, including implicit Git lazy fetch; use local metadata/content only |
| `--frozen` | `--locked` plus `--offline` |
| `--no-local` | Use shared configuration/lock; ignore the personal overlay |
| `--target <triple>` | Select metadata-resolution platform(s); cross-host compilation errors in v1 |
| `--no-vendor` | Explicitly bypass configured vendor materialization |
| `--message-format=json` | Versioned structured results/diagnostics |
| `--no-daemon` | Same semantics in-process |

Changing flags cannot bypass compatibility or integrity checks. Meaningless flag
combinations, such as an update that requires lock changes under `--locked`, fail
with a clear usage/state diagnostic.

### 12.2 Command matrix

The concrete command set below completes the user-facing workflow. New command
spellings and defaults are listed for review in §23.

| Command | Roots/platforms by default | Resolution behavior | Persistent effect |
|---|---|---|---|
| `init [path] [--lib]` | New/current project | None required | Manifest, starter source, ignore entries |
| `add <source> ...` | One member, host | Conservative | Member manifest + active lock |
| `remove <alias> [--dev]` | One member, host | Conservative | Member manifest + active lock |
| `resolve` | Selected roots, lock matrix | Conservative repair | Active lock |
| `fetch` | Selected roots, lock matrix | Conservative repair if needed, then materialize | Active lock/cache |
| `update [package-selector ...]` | Selected roots, lock matrix | Targeted eager or full update | Active lock |
| `build` | Selected roots, host | Reuse/repair conservatively | Active lock, build/cache outputs |
| `run` | One root/entry, host | Reuse/repair conservatively | Active lock/cache; run worker |
| `check` | All targets of selected roots, host | Reuse/repair conservatively | Active lock/cache, diagnostics |
| `test` | Selected roots' contracts, host | Reuse/repair conservatively | Active lock/cache, test results |
| `vendor` | Selected roots, lock matrix | Conservative repair, materialize | Active lock, vendor index/tree/config |
| `deps` / `deps why <selector>` | Selected roots, host | Inspect available graph; report stale state | No implicit updates |
| `publish` | Explicit selected packages | Isolated candidate validation | Version changes, release commit/tags/push/receipt |
| `publish --plan` | Same | Same planning/validation inputs | No working-tree, branch, or tag mutation |

An explicit target list replaces a metadata command's default matrix. Add/remove
repair the host graph unless other targets are explicitly selected, and report
other affected graphs as stale. This avoids silently turning a local edit into
an all-platform transaction. Explicit `resolve`/`update` prepares the matrix.

### 12.3 Add/remove

Proposed options include `--as`, `--version`, `--branch`, `--rev`, `--subdir`,
`--path`, `--dev`, and `--package`. Exactly one source selector is effective.
A GitHub shorthand is expanded to the canonical manifest locator.

Determine the candidate edit and solve before committing CLI-authored changes.
Resolution failure leaves the original manifest/lock intact. This does not undo
manual edits made before the command.

In local context, add/remove edits the shared member manifest and only the active
local lock. Affected shared graphs become stale. Report them explicitly; do not
require a shared solution to permit a working personal graph. Shared preparation
can later use `fai --no-local resolve` or `update`.

Removing a normal dependency does not silently remove a different development
entry or rewrite source imports. Unreachable graph nodes are pruned from selected
graphs, while records used by other graphs remain.

### 12.4 Ordinary repair

For each selected graph:

1. Load active declarations and root settings.
2. Validate the existing graph and selected compiler.
3. If valid, retain selections and restore missing content.
4. If stale/missing and locked, report the reason without writing.
5. Otherwise conservatively solve the active context, verify content, and commit
   only if expected input/lock revisions still match.

A compiler incompatible with an otherwise valid lock is not automatic-repair
permission: report the compiler requirement. Explicit resolve/update is required
for a deliberate different selection.

### 12.5 Update transactions

All requested graphs must resolve successfully before replacing the active lock.
Do not leave half of a requested matrix update installed. Unrequested stale
graphs do not block the operation and are not changed.

Targeted updates can downgrade other packages within their requirements when
necessary to obtain a newer requested package. The result lists every addition,
removal, upgrade, downgrade, source replacement, and branch movement with its
reason and affected graphs.

### 12.6 Build products

A library build validates backend-reachable exported/internal code and warms the
content-addressed cache. v1 source packages do not promise a separately distributed
stable native library ABI. Executable outputs use a target/profile/package-specific
location under `.fai/build/`; explicit `--out` keeps its existing role.

Compilation must not execute dependency contracts implicitly. Check/test keep the
selected-contract rules in §5.4. Native requirements are selected from the actual
production/test projection.

## 13. Git fetching and source integrity

### 13.1 Git as the transport

Use installed Git and normal user-owned SSH, credential helper, proxy, and URL
rewrite configuration. Invoke commands with structured argument arrays rather
than constructing shell commands from manifests.

Release discovery uses refs, not GitHub Releases APIs. Read canonical package tag
names, peel annotated tags to commit IDs, and ignore unrelated tags. One command
captures one consistent ref snapshot per repository for the refs it needs.
A new tag appearing later is considered by a subsequent refresh, not spliced into
an in-progress solve.

A valid locked build retrieves the exact recorded commit without needing fresh
version discovery. Git transport may exchange ref advertisements internally, but
those advertisements cannot change the locked selection.

### 13.2 Materialization

Fetch sufficient Git objects to read the selected package tree. Full working
clones/history are not required. One shared repository object store can contain
multiple selected commits; packages must not assume one checkout per repository.

Materialize raw committed content, independent of checkout filters or host newline
conversion. Read metadata and source from the same selected object graph. Verify
objects, normalized package paths, and package source checksum before publishing
an immutable cache entry.

Submodules and Git LFS expansion are not implicit v1 package dependencies. A
package requiring unmaterialized external content receives an explicit unsupported
source-layout diagnostic. Model such source as an ordinary declared dependency or
commit the required package content. This is a completion choice in §23.

### 13.3 Canonical source checksum

The v1 digest is BLAKE3 over the following binary encoding:

1. UTF-8 bytes of `fai.package-tree.v1` followed by one NUL byte.
2. Number of tracked file/symlink entries as an unsigned 64-bit little-endian integer.
3. For each entry in UTF-8 relative-path byte order:
   - Kind as one byte: `0` for a regular/executable file, `1` for a symlink.
   - Canonical Git mode as an unsigned 32-bit little-endian integer: octal
     `100644`, `100755`, or `120000` respectively.
   - Path byte length as unsigned 64-bit little-endian, then path bytes.
   - Payload length as unsigned 64-bit little-endian, then raw payload bytes.

Paths use `/`, have no leading `./`, and preserve Unicode spelling without Unicode
normalization. Symlink payload is its committed target bytes. Directories are
implicit; Git has no tracked empty-directory content to include. V1 package paths
must be representable as UTF-8. This encoding must have checked-in golden vectors.
Do not hash archive timestamps, compression, owner IDs, local absolute paths, or
host checkout transformations.

Package paths must materialize safely inside their package root. Reject escaping
paths, filesystem-ambiguous collisions for the target materialization, and escaping
symlinks. Symlink traversal must not grant another package's ownership or include
untracked outside content. Unsupported Git entry kinds are explicit errors.

### 13.4 Known release mappings

When a release is locked/accepted, remember `(logical key, version) -> commit`.
If a later use observes that same release at a different commit, report an
integrity conflict. Do not silently accept the replacement or change the lock.
Unrelated package tags do not affect this package's version discovery.

A cache/lock can detect changes to previously observed mappings. With no custom
immutable registry, it cannot prove an unseen tag has never moved. Reproducibility
comes from the recorded commit and verified package bytes. If those bytes are
unavailable upstream and absent locally, restoration fails rather than choosing
a substitute release.

## 14. Caching and offline operation

### 14.1 Cache layers

Extend the existing cache selected through `FAI_CACHE_DIR` with logically distinct
stores for repository objects, ref snapshots, immutable package trees, and package
metadata. Keep existing content-addressed backend artifacts separately keyed by
actual compiler/build inputs.

Publish cache entries atomically after validation. Concurrent processes coordinate
per repository/materialization key; cache locks do not hold the compiler database
write lock across network operations. An interrupted fetch cannot expose a partial
package as a complete entry.

Cached Git sources are treated as immutable. Editing a dependency uses a path or
fork override. Cache cleanup can discard derived entries; vendor trees and user
checkouts are not cache garbage-collection targets.

### 14.2 Network policy

- Normal locked use can work without network when content is present.
- A required metadata refresh must succeed online; failure is not automatic
  permission to use stale metadata.
- `--offline` forbids all network activity, including Git promisor/lazy fetch.
- `--locked` forbids selected-graph changes but permits fetching exact content.
- `--frozen` combines both restrictions.

For new/stale offline resolution, candidates must have sufficient locally available
metadata and source content to complete the requested graph. Choose the newest
compatible locally available graph under the same ordering policy and report that
it was resolved offline.

For an already valid lock, missing offline content is an error. Do not downgrade
to an available older version. Offline locks remain ordinary locks afterward;
coming online does not automatically upgrade them.

### 14.3 Freshness

Version/branch inventories are refreshed when a selected operation needs fresh
unlocked candidates or explicitly updates refs. Existing valid pins do not expire
on a timer. Cache age never turns a build into an automatic update.

A corrupt derived cache entry can be discarded and refetched online, but content
that still disagrees with the locked identity fails integrity checks. Vendor
corruption is reported as a repository materialization problem rather than hidden
by an automatic remote fallback.

## 15. Vendoring

### 15.1 Contract

`fai vendor` is a materialization operation for exact locked Git package instances.
It does not change logical package identity, source selection, or dependency edges.
The shared-snapshot workflow is:

```sh
fai --no-local vendor
fai --no-local --frozen check
```

It selects roots and the lock target matrix, conservatively prepares missing/stale
active graphs unless locked, and copies verified package trees. To regenerate a
stale vendor tree, the command may use the cache/Git rather than being blocked by
its existing vendor configuration.

### 15.2 Layout and index

```text
vendor/
  fai-vendor.toml
  sources/
    <package-tree-digest>/
      fai.toml
      src/...
```

The root manifest receives an explicit configuration:

```toml
[vendor]
directory = "vendor"
```

The index has `vendor-version = 1` and entries mapping locked instance descriptors
to source paths and checksums. It records original logical identity, actual Git
locator/subdirectory/commit, package version, and source-tree digest. Multiple
entries can share one identical materialized tree. A representative index is:

```toml
vendor-version = 1

[[packages]]
id = "p-json"
logical = { git = "https://github.com/example/libs.git", subdir = "packages/json" }
source = { git = "https://github.com/example/libs.git", subdir = "packages/json", rev = "sha1:1111111111111111111111111111111111111111" }
version = "1.5.0"
checksum = "blake3:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
directory = "sources/cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"

[[coverage]]
context = "shared"
root = "apps/server"
target = "x86_64-unknown-linux-gnu"
graph-hash = "blake3:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
packages = ["p-json"]
```

Directory paths stay inside the configured vendor root. Match the complete locked
descriptor and checksum, not just an unchecked instance ID from the index.

Graph coverage records identify the context/root/target snapshots materialized.
They are inspection receipts; actual use validates each required Git instance
against the current lock rather than invalidating identical content merely because
an unrelated graph receipt changed.

### 15.3 Use and refresh

When vendor mode is configured, each required Git instance must have a matching,
verified entry. Missing or altered required entries produce an actionable error;
do not silently fall through to the network. `--no-vendor` explicitly bypasses
vendor materialization. Ordinary resolution may still need online metadata when
changing versions; vendoring is not a global package-version index.

Path/workspace packages remain mutable project inputs. For the proposed
self-contained snapshot command, a selected external path outside the active
project is an error with guidance to use shared context, include that source in
the project, or select an immutable Git source. Paths already inside the project
are not duplicated. Ordinary builds with personal path overrides can still use
vendor entries for their remaining Git dependencies.

Prepare a new index/tree set before committing the vendor snapshot. Preserve user
files outside tool-owned vendor paths. An interrupted refresh must be detectable
and recoverable. The index format and external-path policy are completion choices
for review.

## 16. Publishing and release transactions

### 16.1 CLI and version intent

Proposed spelling:

```sh
fai publish json=minor web=patch
fai publish json=1.5.0 web=0.8.1
fai publish json=minor web=patch --plan
```

In a single selected package, `fai publish patch` or `fai publish 1.5.0` is shorthand.
A workspace batch uses package names or explicit member paths. Intent is required;
there is no inferred default bump based solely on code or API changes.

For a stable current version, patch/minor/major increment the corresponding core
and reset lower components. Prerelease transitions use an exact version in v1.
Exact current-version publication is allowed when the author has already prepared
that version and no conflicting release tag exists. A maintenance branch can
publish a lower release line than the repository's globally highest tag.

### 16.2 Clean inputs

Normal publication starts with a clean tracked working tree/index. Application
and library changes are already committed. Untracked, unignored files relevant to
selected package sources must be handled explicitly rather than silently included
in a release. Unrelated generated/ignored files do not become release content.

All selected packages belong to the same Git repository. The tool edits selected
package versions, not authored dependency ranges. Package paths, version syntax,
explicit compiler requirements, and portable declared sources must be publishable.
A release cannot rely on local/workspace overrides, a vendor mapping, or the
author's dependency locks for consumer validity.

### 16.3 Candidate validation

1. Capture the base commit and expected relevant working-tree state.
2. Calculate requested versions and canonical tag names.
3. Build an immutable candidate snapshot containing the version edits.
4. Introduce an in-memory candidate release provider mapping planned package tags
   to package trees in that snapshot.
5. For each selected package, resolve a synthetic external-consumer graph that
   depends on that candidate and includes only its normal dependency edges. Use
   neither development overrides nor existing lock decisions.
6. Validate its production library/code in that normal consumer graph. Separately
   resolve its own development root, including its dev declarations, and check/test
   all its targets in the candidate snapshot on the host. Both validations must
   succeed: a test helper must not mask a consumer failure by changing selections.
7. Resolve metadata for applicable declared target-matrix entries; this does not
   claim cross-host executable/contract validation.
8. Produce the public API diff and a deterministic release plan.

The candidate provider lets Web require a new JSON release in the same batch before
remote tags exist. It supplies only actually planned versions; it must not act as
a version-bypassing source override.

Each selected package must support the current host for local check/test validation.
A batch with no common validation host needs cross-host orchestration; its receipt
protocol is a review item, not an implicit promise of host-only v1 validation.

### 16.4 Authored requirements

Batching coordinates publication, not dependency upgrades. Publishing JSON 2.0.0
alongside Web whose manifest still requires JSON `^1.5.0` is valid: Web uses an
eligible 1.x release. To ship Web against 2.x, author that requirement before
publishing. If source now needs a newer API than its requirement can supply,
candidate validation fails with that dependency context.

### 16.5 API report

Report added, removed, and changed public library API, including public re-exports,
with target/compiler context and machine-readable results. Prefer the current
manifest release when it is a reachable prior tag; otherwise select an appropriate
reachable prior release below the candidate. First releases have no baseline.

The report is advisory. Structural analysis is not a proof of behavioral SemVer
compatibility, and incomplete comparisons are labelled rather than presented as
complete. API differences do not automatically choose or reject a version bump.

### 16.6 Commit and push

After successful validation and a final input-state check:

1. Journal the candidate plan before the first local mutation, then apply generated
   version edits and create one release commit. If the candidate tree already
   equals the base tree, reuse that commit instead of creating an empty commit.
2. Verify its content matches the validated candidate tree.
3. Create one canonical tag per selected package at that commit.
4. Persist the completed local states in a recoverable publication receipt.
5. Atomically push the destination branch and selected tags.

The journal is updated as local states complete, so a crash between commit and
receipt finalization can recover the prepared identity rather than incrementing
versions again. Existing matching tags may be recognized as already completed;
a tag at another commit is a conflict. Hooks or external edits that alter the
candidate tree require revalidation before any remote publication.

Never force-update a release tag or bypass the remote's branch rules. If atomic
push is unavailable, fail with the prepared plan/receipt rather than silently
publishing part of the batch. A non-fast-forward branch or tag conflict fails.

Choose the explicit remote when supplied; otherwise use an unambiguous upstream
remote/destination. Ambiguous or detached-HEAD cases require explicit destination
options. Do not guess among unrelated remotes.

### 16.7 Plan and retry

`--plan` performs planning and candidate validation and prints the same proposed
versions, edits, graphs, API report, tags, and destinations. It does not mutate
working-tree files, branches, or tags. Download/build cache population is allowed.

A failed push retains the prepared release identity. Proposed `publish --resume`
uses that receipt, commit, and tag set without another bump. Verify remote refs
after uncertain transport outcomes: already-published matching refs count as
success; conflicting refs stop the operation. A changed source candidate requires
a new plan rather than silently reusing old validation.

### 16.8 Avoiding commit self-reference

Candidate validation graphs are temporary. They cannot serialize the future release
commit ID into a lockfile that is itself part of that commit. Final Git IDs become
consumer lock data after publication.

For this draft, publishing commits version edits and retains existing development
locks as development records; it reports affected stale graphs for subsequent
normal/shared preparation. It does not rewrite them with fictitious future Git
IDs. A more elaborate automatic post-release lock commit is not assumed. This
completion choice is explicitly listed for review.

## 17. Daemon and language-server integration

### 17.1 Shared orchestration

CLI, daemon, LSP, and `--no-daemon` use one manifest/selection/resolution pipeline.
Choose the compiler before daemon selection, using the existing compiler-build
identity separation. A daemon database can contain several graph contexts, but a
request identifies its root/platform/context and target source scope.

Git/network work does not hold the semantic database write lock. On completion,
verify source integrity and expected inputs, then publish the new immutable graph
snapshot as tracked compiler inputs. The existing cancellation model applies to
read requests whose graph/source inputs changed.

### 17.2 Editor restoration

Opening a project and saving valid dependency/configuration changes triggers
background conservative restoration. Unsaved manifest buffers can receive syntax
feedback, but do not publish lockfiles from transient partial edits.

Show progress, selected context, applicable overrides, and restoration failures.
Respect offline/locked settings. Authentication failure produces an actionable
state rather than blocking the editor on terminal input.

A task that started before a newer manifest, overlay, pin, target selection, or
lock update cannot overwrite that newer state. Check lock revisions as well as
manifest hashes; an explicit concurrent update must not be undone by a background
conservative solve.

### 17.3 IDE source identity

Navigation, completion, references, and diagnostics use package/graph/source-scope
identity. Same-spelled modules in different packages or executable scopes remain
distinct. Package internals appear only to eligible same-owner scopes.

Git-cache sources are immutable navigation targets. Local overrides point tooling
to the actual editable checkout. Inactive platform sources are identified as
inactive for the selected target rather than receiving misleading missing-import
diagnostics from another target's environment.

The protocol must carry enough graph context to express these requests and events;
version/feature negotiation must reject incompatible peers rather than lose it.

## 18. Diagnostics and machine-readable output

### 18.1 Requirements

Every failure identifies the root, target, context, logical source, and relevant
manifest location where available. Resolver conflicts explain dependency chains
and source-selector distinctions, not just intersected version strings.

Reuse Fai diagnostics, byte spans, secondary labels, help text, and machine-applicable
suggestions. Manifest text participates in the source/span registry. Preserve
existing diagnostic meanings and numeric codes.

The following currently unused tooling range is proposed for new package errors;
allocation must be rechecked and entered in the generated error catalog during
implementation.

| Proposed code | Meaning |
|---|---|
| `FAI0800` | Invalid package/workspace manifest |
| `FAI0801` | Unsupported manifest format |
| `FAI0802` | Invalid source locator or package path |
| `FAI0803` | Invalid version requirement |
| `FAI0804` | Unsatisfiable dependency graph |
| `FAI0805` | Normal package dependency cycle |
| `FAI0806` | Missing/stale selected locked graph |
| `FAI0807` | Unsupported lock or resolver policy version |
| `FAI0808` | Invalid lock structure or inconsistent graph |
| `FAI0809` | Required compiler unavailable/incompatible |
| `FAI0810` | Unsupported target/platform |
| `FAI0811` | Git/source retrieval failure |
| `FAI0812` | Authentication required or rejected |
| `FAI0813` | Required offline metadata/content unavailable |
| `FAI0814` | Source integrity mismatch |
| `FAI0815` | Previously accepted release mapping changed |
| `FAI0816` | Overlapping root overrides |
| `FAI0817` | Replacement violates compatibility requirements |
| `FAI0818` | Duplicate active dependency alias |
| `FAI0819` | Invalid target/source scope |
| `FAI0820` | Missing/stale vendor materialization |
| `FAI0821` | Invalid publication inputs/version intent |
| `FAI0822` | Candidate release validation failure |
| `FAI0823` | Remote publication conflict/failure or unsupported atomic push |
| `FAI0824` | Pending publication needs resume/replan |
| `FAI0825` | Inputs changed during a mutating operation |
| `FAI0826` | Ambiguous package/executable selection |
| `FAI0827` | Unsupported package source layout |

Import syntax and source-language name/visibility errors use the appropriate
existing/new `FAI1xxx`/`FAI2xxx` catalog entries rather than misclassifying them as
network or solver failures. Report underlying type/contract diagnostics alongside
release-validation context instead of replacing them with one generic error.

### 18.2 Package command result

New package commands return a versioned result envelope. Existing build/check/test
result fields retain their meaning; dependency context can be additive metadata.
Do not mix human progress text into machine-result stdout.

```json
{
  "schemaVersion": 1,
  "command": "update",
  "ok": true,
  "context": "shared",
  "lockfile": "fai.lock",
  "roots": ["apps/server"],
  "targets": ["x86_64-unknown-linux-gnu"],
  "changes": [
    {
      "kind": "upgrade",
      "package": {
        "git": "https://github.com/example/libs.git",
        "subdir": "packages/json"
      },
      "before": { "version": "1.4.0" },
      "after": { "version": "1.5.0" },
      "reason": "requested-update",
      "graphs": [
        { "root": "apps/server", "target": "x86_64-unknown-linux-gnu" }
      ]
    }
  ],
  "staleGraphs": [],
  "diagnostics": []
}
```

Actual `before`/`after` records also carry the selected actual source and full Git
revision where applicable. Change kinds include add, remove, upgrade, downgrade,
source-replacement, and branch-move. `staleGraphs` identifies affected unprepared
graphs and reasons, including shared graphs left stale by local-context edits.

`deps` exposes exact edges, selector bindings, active replacements, compiler
requirements, and provenance sufficient to answer “why this version?” and “which
file is being compiled?”. Canonical ordering is observable API behavior.

### 18.3 Exit behavior

Preserve Fai's established categories: success `0`; completed semantic/validation
failure `1`; invalid CLI usage `2`; I/O/network/tool availability `3`; internal bug
`4`; existing timeout/cancellation statuses where applicable. New diagnostics
must not turn malformed user input into an internal crash. Existing command
compatibility is covered by CLI tests and updated reference documentation.

## 19. End-to-end workflows

These workflows are executable acceptance narratives, not merely syntax examples.
Command spellings are the proposed interface in §12.

### 19.1 Create, add, import, run

```sh
fai init hello
```

Creates a v1 package and `src/Main.fai`. A library is `fai init json --lib` and has
no conventional Main entry. Initial creation does not need a network resolution. The generated manifest
records a conservative compiler compatibility range beginning at the creating
compiler release; authors can deliberately widen it after validation.

```sh
fai add github:example/libs --subdir packages/json --as json --version '^1.4.0'
fai check
fai run
```

The dependency declaration uses the expanded Git locator. Code imports explicitly:

```fai
module Main

import json.Json

public main : Runtime -> Unit / { Console }
let main runtime =
  runtime.console.writeLine "Hello"
```

The example intentionally only establishes the binding; using `Json.parse` then
resolves to the selected library rather than any globally discovered Json module.
The first semantic command creates/repairs the active lock. Subsequent valid
locked runs use the same source selection.

### 19.2 Multiple executables

```toml
manifest-version = 1

[package]
name = "tools"
version = "0.1.0"
fai-version = "^0.5.3"
default-run = "serve"

[[bin]]
name = "serve"
path = "commands/Serve.fai"

[[bin]]
name = "migrate"
path = "commands/Migrate.fai"
```

Both entries may declare `module Main`; each gets shared library modules and its
own entry. `fai run` chooses `serve`; `fai run --bin migrate` chooses the other.
The migration entry does not become an importable library module.

### 19.3 Multi-package development

```toml
manifest-version = 1
lock-targets = ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]

[workspace]
members = ["apps/server", "packages/json", "packages/web"]

[toolchain]
version = "0.5.3"

[[overrides]]
replace = { git = "https://github.com/example/libs.git", subdir = "packages/json" }
with = { path = "packages/json" }

[[overrides]]
replace = { git = "https://github.com/example/libs.git", subdir = "packages/web" }
with = { path = "packages/web" }
```

Package manifests retain portable Git requirements. The workspace explicitly
substitutes local members. Root `fai check` checks all applicable members on the
host; `fai test --package json` selects JSON's own development environment.

Server and JSON-root graphs are distinct. Server does not inherit JSON's dev
requirements merely because both are workspace members.

### 19.4 Personal fork while retaining shared development

Root `fai.local.toml`:

```toml
manifest-version = 1

[[overrides]]
replace = { git = "https://github.com/example/libs.git", subdir = "packages/json" }
with = { git = "https://github.com/me/libs.git", subdir = "packages/json", branch = "fix-parser" }
members = ["apps/server"]
```

This personal entry wins for server-root graphs; the shared entry still applies
to other roots. The fork's declared version must satisfy all server graph ranges.

```sh
fai check --package server
fai update json --package server
fai --no-local --locked check --package server
```

The first two commands operate on `fai.local.lock`. The third validates the shared
selection independently. A branch move does not change ordinary builds between
explicit updates.

### 19.5 Conservative add versus eager update

Assume a root declares A and later adds B, both with `^1.0.0`:

| Candidate | Requirement on Shared |
|---|---|
| A 1.9.0 | `^1.0.0` |
| A 1.8.0 | `^2.0.0` |
| B 1.9.0 | `^2.0.0` |
| B 1.8.0 | `^1.0.0` |

With A 1.9.0 already locked, adding B preserves A and selects B 1.8.0. Explicitly
updating B prefers B 1.9.0 and can select A 1.8.0 plus Shared 2.x. Every chosen
version still satisfies the root's authored ranges.

### 19.6 Re-exported API

Web declares JSON as a normal direct dependency:

```fai
module Web

public import json.Json
```

A consumer declaring only Web can use:

```fai
module App

import web.Web.Json
```

It cannot traverse Web's other private dependency aliases. If it also declares
JSON directly, the shared logical key resolves once and both access paths denote
the same types. A conflicting direct JSON range is a solver conflict, not another
hidden copy.

### 19.7 Platform implementations

```text
src/Main.fai
src/Application.fai
platform/linux/Platform.fai
platform/windows/Platform.fai
```

Platform-selected roots each provide `module Platform`. Shared code addresses
`Platform` as an ordinary same-package module. Windows-only imports appear in the
Windows source set and refer to aliases active only on that target.

An update prepares declared target graphs. Linux and Windows can select different
versions where their constraints require it. Each target's own build and tests
share that target's normal selections.

### 19.8 Offline shared build

```sh
fai --no-local update
fai --no-local vendor
fai --no-local --frozen check
```

After committing the shared manifests, lock, vendor configuration/index, and source
materializations, another checkout can resolve those locked Git inputs locally.
The compiler and native system prerequisites remain explicit external inputs.
A missing required vendor entry fails instead of fetching silently.

### 19.9 Coordinated release

The author commits source/API changes and any intended compatibility-range edits,
then runs:

```sh
fai publish json=minor web=patch --plan
fai publish json=minor web=patch
```

The plan is optional. Both paths use the same candidate preparation and validation
rules. If the new Web declaration admits the new JSON version, its fresh validation
can select the planned JSON release before remote tags exist.

The resulting tags might be:

```text
packages/json/v1.5.0 -> release commit C
packages/web/v0.8.1  -> release commit C
```

A later consumer can combine Web from C with a newer compatible JSON commit D.
Sharing a release commit never forces permanent lockstep.

## 20. Compiler integration and migration

### 20.1 Current implementation seams

The repository currently has a native-only manifest reader in
`crates/fai-driver/src/manifest.rs`. Project collection is directory-oriented;
module lookup is global within the loaded workspace. `SourceOrigin` in
`crates/fai-db/src/lib.rs` distinguishes `User` and `StandardLibrary`.

Existing in-tree packages under `packages/` describe CI dependencies in `ci.json`.
Those files are useful migration input, not a second long-term package resolver.
Examples include Web depending on JSON/SSE and larger packages depending on many
other source packages.

### 20.2 Package loader and compiler input model

Introduce explicit models for:

- Root/workspace membership and manifest metadata.
- Logical source keys, actual sources, and selected instances.
- Root/platform/context graph bindings.
- Source ownership and target-local module scopes.
- Per-package normal dependency environments and root-only dev environments.

Replace global module-name lookup with lookup in an explicit package/source
context. Preserve the trusted embedded std loader and its separate source identity
registry. A package called `std`, or a path resembling an embedded path, must not
acquire std privileges.

A text file and its semantic instantiation are distinct concepts. The same package
revision can be present in several graphs with different dependency bindings.
Queries must carry sufficient context instead of attaching one global resolution
to a physical filesystem path.

### 20.3 Incrementality and type identity

Keep local owner/source identities stable across ordinary edits. Position-free
item trees, late span resolution, and public-signature firewalls remain central.
Import declarations contribute explicit name-binding dependencies.

Use a per-package semantic instantiation identity based on selected source,
active source set/target, and its normal dependency bindings. Do not inject the
entire workspace lock hash into every semantic query or object cache key: an
unrelated root/dev dependency edit must not invalidate all production modules.

Definition/type identities and native symbols must distinguish incompatible
package instantiations. Library identities are shared by that root's executable
and test scopes; private entry modules have target-local identities. Re-exporting
an existing symbol retains its identity rather than constructing a new nominal
type or function definition.

### 20.4 Integration responsibilities

| Component | Responsibility |
|---|---|
| Package tooling layer | Manifests, ranges, graph solver, Git/cache providers, locks, overrides, vendor and release transactions |
| `fai-driver` | Unified project preparation; pass resolved inputs to all build/check/run/test paths; native requirement projections |
| `fai-db` | Stable owners, source registries, graph/context inputs and projections |
| `fai-syntax` / `fai-fmt` | Import/re-export grammar, item-tree representation, canonical formatting |
| `fai-resolve` / `fai-types` | Scoped module/alias lookup, visibility, identity-preserving re-exports, public-signature resolution |
| `fai-core` / `fai-codegen` / wire formats | Context-correct definition IDs, symbols, ABI/cache boundaries |
| `fai-cli` | Consistent root/target/context selection, commands, transactions, structured results |
| `fai-server` / `fai-lsp` / `fai-ide` | Snapshot publication, async restore, context-aware navigation/completion/diagnostics |
| CI/package scripts | Read manifests for dependency-aware selection; keep CI-only settings separate |

A new package-tooling crate is a reasonable organizational boundary. Its I/O must
not become callable from semantic queries. If shared ID models would create a
crate cycle, extract only the pure model rather than giving the DB a transport
or process dependency.

### 20.5 Migration

1. Introduce v1 manifests for in-tree packages, matching current dependencies.
2. Establish a workspace and explicit local overrides for portable Git declarations.
3. Add imports for cross-package module uses; same-package/std access stays automatic.
4. Audit currently public helpers that should become `internal` at package scope.
5. Adapt samples/tests relying on directory-wide implicit source discovery.
6. Route all CLI, daemon, JIT, AOT, contract, and IDE paths through the same loader.
7. Make CI dependency selection consume package manifests, avoiding drift with
   `ci.json`; retain only genuinely CI-specific settings elsewhere.
8. Update `AGENTS.md`, `docs/MEMORY.md`, `docs/CLI.md`, `llms.txt`, samples, and the
   generated diagnostic catalog with the implementation.

For transition, a manifest containing only the legacy `[native]` section can be
recognized as a native-only legacy project with its current explicit-entry and
directory-source behavior. New manifests are explicitly v1. A directory with no
manifest follows the newly agreed single-file semantic mode. The duration and
exact shape of the native-only compatibility path require review.

## 21. Implementation sequence

Each step should be independently reviewable and verified through focused tests.
The order establishes language/package identity before remote-source complexity.

### A. Pure models and manifest parsing

Implement canonical package/root/target IDs, schema validation with spans,
requirements, platform activation, source classification, and deterministic
serialization. Add fixtures for the actual in-tree package shapes. Keep network
providers out of these models.

### B. Local package ownership and imports

Implement package-aware DB inputs, source scopes, import/re-export syntax, alias
lookup, internal visibility, and context-aware IDE operations using local fixtures.
Exercise multiple same-named modules and entry points before adding remote Git.

### C. Resolver and graph semantics

Implement a provider-independent solver against an in-memory immutable version
catalog. Verify policy against a small exhaustive oracle. Add root normal/dev
union, graph projections, source-selector conflicts, compiler/platform filtering,
overrides, and per-root/platform independence.

### D. Locks, transactions, and selection

Implement graph-local validity, selected-graph operations, conservative/eager
policies, local baseline reconciliation, deterministic diffs, and recoverable
manifest/lock writes. Integrate both CLI and daemon execution paths.

### E. Git, materialization, and offline operation

Add system-Git providers, ref snapshots, exact-commit fetching, checksum validation,
immutable caches, network/offline policy, and failure diagnostics. Test against
local bare repositories and controlled transport failures rather than live GitHub.

### F. Platform/native and toolchain integration

Add installed-compiler dispatch, target matrix preparation, active platform source
sets, and native dependency projection. Verify host behavior on supported OSes.

### G. Editor restoration and vendoring

Publish complete graph snapshots asynchronously, guard against stale writers,
finish source-aware navigation, and add vendor transactions/index verification.
Measure warm-cache and unrelated-root edit behavior.

### H. Publishing

Implement candidate release plans, isolated consumer validation, API reports,
version-only preparation, publication receipts, atomic push, and idempotent retry.
Exercise server rejection and unknown-outcome recovery.

### I. Ecosystem migration and documentation

Migrate source packages, sample workflows, CI selection, and user/agent docs.
Release the manifest/lock/protocol changes together with clear compatibility notes.
Run only affected local compiler/integration suites; full platform/CI coverage
remains the CI gate, consistent with the repository contributor guide.

## 22. Acceptance scenarios

These are behavioral requirements, not tests mirroring implementation functions.
Use one focused named test per concrete case and generative tests for invariants.

### 22.1 Manifests and source scopes

| ID | Scenario and expected result |
|---|---|
| M01 | Minimal application infers its conventional Main executable |
| M02 | Library without Main exposes all public library modules |
| M03 | Explicit name/path overrides inference without duplicate executable |
| M04 | `auto-main = false` permits that file to remain an ordinary library source |
| M05 | Two executable entries named Main compile independently |
| M06 | A dependency cannot import an executable/test-only module |
| M07 | Additional tests see library internal members but not file-private members |
| M08 | Production source cannot gain test-module/dev-alias visibility under `check` |
| M09 | Unknown fields, duplicate active aliases, invalid paths and malformed ranges have exact spans |
| M10 | Native-only legacy fixtures behave according to the documented migration rule |
| M11 | A standalone file does not discover a neighboring Fai module |
| M12 | Inactive platform subtrees do not leak through a broader common source scan |
| M13 | Nested packages retain separate ownership and source collection |
| M14 | Formatter changes preserve TOML comments and import parse/format equivalence |

### 22.2 Imports and identities

| ID | Scenario and expected result |
|---|---|
| I01 | Import alias binds exactly one declared direct dependency module |
| I02 | Adding an unrelated dependency does not change existing import bindings |
| I03 | Collision with an automatic local/std module requires `as` |
| I04 | Public re-export can be traversed through the consuming direct dependency |
| I05 | Private dependency aliases cannot be traversed transitively |
| I06 | Direct and re-exported use of one selected type have identical nominal identity |
| I07 | A separately declared fork has distinct identity; an override preserves the original logical key |
| I08 | Internal visibility is package-scoped, not workspace-scoped |
| I09 | Reserved/trusted std behavior cannot be obtained through names or paths |
| I10 | Same source revision with different dependency bindings has context-correct types and native symbols |
| I11 | Import identities remain position-independent through comments/formatting edits |

### 22.3 Resolution

| ID | Scenario and expected result |
|---|---|
| R01 | Caret, tilde, exact, intersections, unions, and zero-major boundaries match their specified sets |
| R02 | Bare full version is exact, not caret-compatible |
| R03 | Prerelease eligibility is limited to an explicitly mentioned release core in the satisfied union arm |
| R04 | Fresh resolution chooses the policy-preferred valid graph, not merely a per-edge maximum |
| R05 | Conservative add preserves the A/B fixture's existing A pin |
| R06 | Targeted B update can downgrade A within its range to obtain newer B |
| R07 | Full update is deterministic under permutations of provider responses/table order |
| R08 | Unsatisfiable ranges produce a causal chain with manifest locations |
| R09 | One graph never has two instances of one logical key |
| R10 | Independent workspace roots/platforms may select different versions |
| R11 | Build/test of one root/platform share normal selections influenced by its dev constraints |
| R12 | Dependency-owned dev requirements are not inherited |
| R13 | Normal dependency cycles fail; a synthetic dev-root/helper back-reference can be valid |
| R14 | Untagged matching manifest versions do not satisfy release edges without an override |
| R15 | Different branch names can unify at one commit; incompatible commits conflict |
| R16 | Branch bindings remain pinned until the appropriate explicit update/new selector |
| R17 | Compiler/platform filtering participates in fresh resolution |
| R18 | Invalid release metadata is distinguished from network and integrity failures |

Property tests should generate small finite version catalogs and compare solver
validity/preferences with an exhaustive oracle, check requirement normalization,
and vary input order without changing a deterministic result.

### 22.4 Locks, contexts, and concurrency

| ID | Scenario and expected result |
|---|---|
| L01 | Repeated valid locked builds do not update versions or need version discovery |
| L02 | Tampered references, duplicate logical instances and invalid edges are rejected even with plausible hashes |
| L03 | Updating one graph preserves unrelated graph selections |
| L04 | Stale App B does not block a locked App A command |
| L05 | Local add changes the manifest/local lock and reports affected shared graphs stale |
| L06 | Local/shared override conflicts follow layer and overlap rules |
| L07 | Scoped overrides affect every matching root edge, including transitive uses |
| L08 | An incompatible replacement is rejected despite explicit selection |
| L09 | Shared baseline changes reconcile affected local graphs but not unrelated ones |
| L10 | Three-way reconciliation retains a local-only update when that key's shared selection did not change |
| L11 | A stale background resolver cannot overwrite an explicit newer update |
| L12 | Interrupted manifest/lock replacement is recoverable without blindly reverting independent user edits |
| L13 | Compatible compiler changes retain pins; an incompatible valid lock reports the compiler requirement |
| L14 | Body/comment edits do not cause lock writes |
| L15 | CLI, daemon, and no-daemon agree on graph selection and diagnostics |

### 22.5 Git, offline, and vendoring

| ID | Scenario and expected result |
|---|---|
| G01 | Lightweight/annotated package tags peel to the same selected source semantics |
| G02 | Repository-wide unrelated tags are ignored for a subdirectory package |
| G03 | Two subdirectory packages can use different commits of one repository |
| G04 | Ref movement during a fetch cannot mix metadata from one commit with source from another |
| G05 | A known changed release mapping fails without silently accepting new content |
| G06 | Unavailable locked commit fails even when a newer tag exists |
| G07 | Required refresh failure does not silently fall back to cached versions |
| G08 | Explicit offline fresh resolution can select the policy-preferred fully local graph |
| G09 | Offline use of a valid lock fails for missing content rather than downgrading |
| G10 | Offline mode prevents Git lazy/promisor fetch as well as explicit fetch commands |
| G11 | Shared caches survive concurrent interrupted fetches without exposing partial trees |
| G12 | Source checksum is independent of archive timestamps, checkout newline settings, and directory location |
| G13 | Escaping paths/symlinks and unsupported external-content layouts fail predictably |
| G14 | Vendor sources retain original identity and satisfy frozen builds without cache/network |
| G15 | Missing/altered required vendor entries fail rather than fetching silently |
| G16 | Vendor refresh changes only tool-owned materializations and recovers from interruption |

### 22.6 Publication

| ID | Scenario and expected result |
|---|---|
| P01 | Plan mode reports the same requested changes without modifying working-tree files/refs |
| P02 | A normal release automatically prepares selected versions from clean committed code |
| P03 | Dirty relevant inputs fail before publishing unintended content |
| P04 | Validation uses the candidate snapshot, not a local override or existing developer lock |
| P04a | A dev helper that forces an older dependency cannot hide failure in the independent normal consumer graph |
| P05 | A same-repository batch can validate against its planned new dependency release |
| P06 | Authored ranges remain unchanged; an independently released new major is not forced into a dependent |
| P07 | API changes are reported but do not themselves reject the chosen bump |
| P08 | All selected tags identify the validated release commit |
| P09 | Atomic push rejection leaves no partial remote batch |
| P10 | Unknown transport outcome is reconciled against remote refs before retry |
| P11 | Resume reuses the prepared commit/tags and does not bump again |
| P11a | A crash between local commit/tag steps recovers through the journal without inventing another release |
| P12 | Conflicting existing tags are never force-moved |
| P13 | Candidate validation cannot introduce a self-referential commit hash into its own tree |
| P14 | Publication reports host validation scope separately from other-target metadata resolution |

### 22.7 Performance and compiler correctness

- Extend incremental-versus-clean checks to import edits, graph changes, and
  source-scope changes.
- Preserve private-body/signature firewalls across package boundaries.
- An unrelated root or test-helper edit must not invalidate every production unit.
- Exercise JIT, AOT, contract workers, wire serialization, daemon snapshots, and
  IDE queries against duplicate module names in different contexts.
- Measure warm locked command and editor restoration paths separately from cold
  network fetching. No wall-clock network thresholds belong in deterministic CI.
- Add golden checksum/fingerprint vectors and round-trip format tests.

## 23. Completion choices requiring review

The product decisions in §1 are agreed. The following are explicit specification
completions, not silently attributed to the user. They can be adjusted before the
specification is accepted without changing the core architecture.

| ID | Proposed completion | Why review it |
|---|---|---|
| C01 | Exact TOML field names, required package name/version, local default compiler range, source/test roots, `auto-main`, and metadata table | Concrete schema spelling/defaults were not exhaustively selected in conversation |
| C02 | Full three-component requirements; comma intersections; `||` unions; no partial versions or `+build` metadata | Closes the requirement grammar and equal-precedence release ambiguity |
| C03 | `supported-platforms`, bulk unsupported-root filtering, platform test-source/dev tables, Apple `frameworks` field | Completes platform eligibility and test/native integration |
| C04 | Root/package discovery details, ambiguous-name path selectors, ad-hoc entry invocation, and native-only legacy mode | Needed to migrate actual current CLI behavior |
| C05 | Source-path containment, reserved subtree scanning, entry-only binary scopes, and top-level import placement | Makes file ownership and module lookup unambiguous |
| C06 | CLI `resolve`/`fetch`/`deps` commands; host-only add/remove repair; matrix defaults for explicit preparation | Completes command scope and offline-preparation ergonomics |
| C07 | Per-graph three-way local baseline reconciliation and lock record/ID encoding | Makes the accepted baseline-tracking rule precise without unrelated churn |
| C08 | Strict release-candidate metadata versus retrieval/integrity failure treatment | Prevents network errors from masquerading as version incompatibility |
| C09 | Vendor index/config, `--no-vendor`, Git-only snapshots, and rejection of selected outside-project mutable paths | Completes vendoring without pretending mutable local inputs are locked Git content |
| C10 | Submodule/LFS external content excluded from implicit materialization | Bounds v1 Git source semantics; these could later become explicit source features |
| C11 | Stable-version bump rules, exact prerelease intent, publication destination/resume options | Completes publishing CLI and recovery mechanics |
| C12 | Publishing leaves development locks for separate preparation and reports staleness | Avoids self-referential hashes; automatic post-publication lock commits would be a separate policy |
| C13 | Host-only release validation and handling batches with no common validation host | Cross-host validation receipt/orchestration is not yet a defined v1 guarantee |
| C14 | Installed-toolchain inventory/registration interface | Selection behavior is agreed; concrete installation layout/commands need finalization |
| C15 | Proposed FAI0800–FAI0827 allocation and additive JSON metadata | Recheck availability and compatibility when implementation starts |
| C16 | Library build warms backend artifacts without promising a portable binary-library distribution format | Completes source-package build output semantics |

### Specification acceptance checklist

Before implementation is treated as conforming:

1. Review completion choices C01–C16, especially publication locks and cross-host
   validation, and record any accepted amendments.
2. Freeze manifest/lock/vendor versions and canonical hashing vectors.
3. Map diagnostics into the repository catalog and finalize new command schemas.
4. Turn the acceptance scenarios into targeted tests alongside each implementation
   step rather than postponing validation until the whole manager exists.
5. Keep the user-facing and agent-facing documentation synchronized with the same
   source of truth as the implementation.

The specification is deliberately a draft until that review. It now contains the
agreed architecture, concrete proposed formats, lifecycle rules, implementation
seams, and behavioral acceptance criteria needed for that review and implementation.
