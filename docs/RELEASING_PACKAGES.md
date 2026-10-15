# Releasing source packages

Source packages live in one repository but have independent versions, changelogs,
and path-scoped tags. The manual **Package release** workflow publishes prepared
versions with the existing `fai publish --plan` validator. It never edits source
files, creates a version-bump commit, pushes `main`, or force-moves a tag.

## Prepare a release

1. Update each selected package's `fai.toml` version, README, and `changelog.md` on
   a feature branch. Move its Unreleased entries into a nonempty, dated version
   section. The 27 public libraries start at **0.1.0** and require **Fai 0.1.x**.
   The six nested example/test projects set `[package] publish = false`; they
   remain workspace members but have no independent releases or changelogs.
2. Preserve authored dependency ranges. Include any as-yet-unpublished dependency
   versions in the same batch, including published development dependencies.
   Public packages must not depend on private projects: put tests needing example
   helpers in the example project's test directory instead.
   For the first release, select `all` to release all 27 public libraries together.
3. Run `python3 scripts/package_release.py check` to check prepared package
   metadata. Run the affected native package tests and integrations as described
   in [DEVELOPMENT.md](DEVELOPMENT.md).
4. Merge the preparation PR through the normal required checks. Record its exact
   merged commit. The workflow only accepts commits already on `main`.
5. Publish the chosen compiler first using [RELEASING.md](RELEASING.md). The initial
   package batch needs compiler **0.1.0**, not the previously published 0.0.1.

Package requirements describe compatibility; `compiler_version` identifies the
one exact published compiler used for qualification. The workflow verifies its
immutable release, checksums, and build information. It never substitutes a
source-built compiler or silently downloads a newer version.

## Validate without publishing

Use GitHub CLI from an authenticated maintainer session. Replace `COMMIT` with the
full merged source commit:

```sh
gh workflow run package-release.yml --ref main \
  -f packages=all -f compiler_version=0.1.0 -f source_ref=COMMIT \
  -F publish=false
```

For an individual package or a future mixed-version batch:

```sh
gh workflow run package-release.yml --ref main \
  -f 'packages={"json":"0.1.0","web":"0.1.0"}' \
  -f compiler_version=0.1.0 -f source_ref=COMMIT -F publish=false
```

`all` selects the exact versions already written in every publishable manifest. It
does not calculate bumps. Explicit JSON selections must match committed versions;
unknown/duplicate names, private projects, and unprepared changelogs are rejected.

Inspect the resulting run in Actions or use `gh run list --workflow
package-release.yml`, then `gh run watch RUN_ID --exit-status`. Avoid confusing
another maintainer's run with your dispatch: check the source commit and inputs.

## Qualification

The workflow runs natively on Linux x86-64, macOS ARM64, and Windows x86-64. Each
host installs the same exact published compiler and runs `fai publish --plan`
against the same source commit and version batch. The publisher checks:

- Fresh external-consumer graphs, without development locks, local overrides,
  or vendor substitutions.
- Production libraries separately from each package's development/test graph.
- Named tests and inline contracts, with committed focus markers rejected.
- Executable preparation and the declared platform metadata graphs.
- An advisory public API difference against previous releases, where available.

The first batch has no prior API baseline. The workflow is not a replacement
for provisioned database/terminal integration tests in normal CI, and does not
claim native execution on the three additional compiler release architectures.
Private example/test projects retain their own workspace checks and integration
programs in CI, but never receive package release tags.
Validation artifacts retain the request, compiler build information, complete
plans, and API differences. No publication job runs unless every host succeeds.

## Publish and verify

Dispatch the same request with `-F publish=true`. This reruns native qualification;
it does not trust an unrelated prior run. The final job compares all three reports
with the checked-out commit and tree before writing anything.

It creates annotated package tags and pushes the batch atomically. GitHub releases
use the package changelog as their notes and are explicitly **not Latest**, leaving
the compiler installer channel unchanged. Publication shares the compiler
release lock. Repository immutable releases must remain enabled; protect
`packages/**` tags against update and deletion as well.

```sh
gh release view packages/json/v0.1.0
git ls-remote origin refs/tags/packages/json/v0.1.0 'refs/tags/packages/json/v0.1.0^{}'
```

The `package-publication` artifact records each package URL and exact source
commit. Check it, then smoke-test a clean external application with the documented
`fai add` command. The normal consumer validation already checks package graphs,
but a real fresh installation remains useful end-user verification.

If publication is interrupted, rerun the **same source, compiler, and package
versions**. Matching annotated tags and immutable releases are retained; missing
release pages are completed. A conflicting tag or release is an error, never an
excuse to rewrite it. Atomic tag publication prevents a partially advertised
dependency batch, although GitHub release pages are necessarily created one at
a time. Correct a published defect with a new version and a new preparation PR.
