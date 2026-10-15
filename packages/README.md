# Source packages

The initial package versions are **0.1.0**, requiring **Fai 0.1.x**. Their
READMEs contain installation commands and link to individual changelogs. Publish
the compiler before the initial package batch; repository source alone does not
make a version available to `fai add`.

Packages are released independently through the manual
[package release workflow](../docs/RELEASING_PACKAGES.md). A package tag looks like
`packages/json/v0.1.0`; the initial `all` batch includes nested example packages
needed by development dependencies.

After publication, add a library to an application using its repository and
subdirectory, rather than copying sources:

```sh
fai add github:Krzysztof-Cieslak/fai --subdir packages/json --as json --version '^0.1.0'
```

These libraries are ordinary Fai packages. Each `fai.toml` declares its public
source, dependencies, additional tests, and executable entries. Cross-package
modules use explicit imports, for example `import json.JsonDecode`.

`packages/fai.toml` declares the workspace and replaces this repository's Git
dependencies with the local checkout. The committed `fai.lock` records each
member's platform graphs. A published consumer uses the portable Git declarations;
it does not inherit this workspace's local replacements.

```sh
fai check -C packages --package web
fai test -C packages --package web
fai fmt -C packages --package web
fai run -C packages web/examples/JsonWebExample.fai
fai resolve -C packages
```

The `config`, `csv`, `llm`, and `postgres` examples share helper modules in nested
example packages. Select them by names such as `llm-examples`, or run an explicit
example file. Their parent libraries do not acquire example-only dependencies.

Repository CI reads normal and development dependencies directly from these
manifests. Inline `example` and `forall` declarations document nearby library
implementations. Dedicated test sources use named `Test.test` cases and
`Test.property` laws with explicit generators; `fai test` runs both forms.
Compiler-specific JIT/AOT, transport, and differential fixtures live in
`scripts/tests/`.
Optional `ci.json` files contain only integration-program and external-fixture
metadata; package identities and dependency edges come from `fai.toml`.
