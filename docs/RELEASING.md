# Compiler release runbook

Compiler releases are explicit, manually dispatched operations. Source packages
under `packages/` retain their own [publishing workflow and tags](RELEASING_PACKAGES.md).

## Development and preparation

Every compiler, runtime, tooling, or embedded-standard-library change updates
`changelog.md` under `[Unreleased]`, including internal improvements. Package-only
changes are excluded. CI checks the actual PR/base diff, including deletions and
renames. Use Added, Changed, Fixed, or Removed headings as appropriate, and include
migration instructions for breaking changes.

Prepare on a feature branch:

```sh
git fetch origin
git switch -c prepare-v0.2.0 origin/main
python3 scripts/release.py prepare 0.2.0
```

`prepare` also accepts `major`, `minor`, or `patch` from a stable workspace version.
Prerelease transitions require an exact version. It updates the workspace version
and internal Cargo.lock entries, moves Unreleased notes into a dated version
section, and leaves a fresh Unreleased section. Changes are journaled under the
Git directory; after interruption rerun the same exact version. Conflicting user
edits are retained and reported.

Review and commit the edits, then validate:

```sh
python3 scripts/release.py validate --version 0.2.0
```

Validation requires a clean checkout, matching Cargo versions and lock records,
an exact nonempty dated changelog section, and an empty Unreleased section.
Publish the preparation PR and require all protected checks before merging.
Prerelease compiler compatibility follows the package requirement rules: source
packages exercised by full CI must explicitly accept the candidate prerelease.

## Dispatch and monitor

Use a clean checkout of the exact merged release commit:

```sh
python3 scripts/release.py dispatch --version 0.2.0 --source-ref COMMIT
gh run watch RUN_ID --exit-status
```

The helper returns JSON containing the source commit, run ID, and run URL. A
unique request ID correlates the dispatch with its actual workflow, avoiding races
with other releases. If dispatch succeeds but run discovery times out, search for
that request ID before dispatching again.

The equivalent direct operation is:

```sh
gh workflow run release.yml --ref main -f version=0.2.0 -f source_ref=COMMIT
```

The Actions UI accepts the same `version` and `source_ref` inputs. The latter
defaults to main but is resolved once to a commit. Allowed source branches are
configured in `scripts/release-targets.json`; add protected maintenance branches
there deliberately. The workflow has no tag-push or release-event trigger.
The `publish` helper is workflow-only; local maintainers use `dispatch` so the
complete qualification dependency graph runs before any public release is made.

If publication fails after creating the annotated tag, keep the tag and tested
artifacts unchanged. After correcting the publication helper, use the manual
recovery workflow from `main`:

```sh
gh workflow run release-resume.yml --ref main -f version=0.0.1 -f run_id=SOURCE_RUN_ID
```

Recovery accepts only a completed manual Release run from a protected release
branch, with every native qualification and full-source check successful in that
same run. The existing tag must identify its commit. The corrected helper checks
out that original source separately, downloads its original artifacts, rechecks
the complete matrix and checksums, and publishes the draft. It never moves the
tag or substitutes a new compiler build. The original build attestations remain
attached to the exact artifact bytes. Runs dispatched against source different
from their workflow commit require a fresh complete qualification rather than
this recovery path.

To diagnose a candidate on one native host before merging, dispatch the workflow
from that branch with `build_only=true` and an optional `qualification_target`:

```sh
gh workflow run release.yml --ref BRANCH -f version=0.0.1 -f source_ref=COMMIT -F build_only=true -f qualification_target=aarch64-pc-windows-msvc
```

Build-only runs retain artifacts and execute native qualification; tag creation,
publication, and the full-source verification job are disabled. Actual publication
still requires a prepared, merged source commit and the complete six-host matrix.

## Qualification and artifacts

`scripts/release-targets.json` defines six required native targets: Linux/glibc,
macOS, and Windows/MSVC, each on x86-64 and ARM64. All must pass. Build baselines
are pinned runner families: Ubuntu 22.04/glibc 2.35, macOS 15, Windows Server 2022
for x64, and Windows 11 for ARM64. Changing a baseline is a documented support
policy change.

The reusable CI workflow forces full verification on the selected commit. Native
jobs also test runtime and installer code, build optimized compiler/launcher
executables with the pinned Rust toolchain, and qualify the archives through a
local GitHub-shaped download server. Qualification covers checksums, installation,
selection, missing pins without downloads, contracts, JIT, AOT, daemon, LSP,
upgrade behavior, and corrupt-transfer recovery. Published previous stable
compilers are exercised alongside the candidate when available; first releases
also have native multi-version launcher fixtures.
Native build archives are retained before smoke tests to support diagnosis of a
failed qualification. Publishing still requires every qualification job to pass.

For local native packaging:

```sh
python3 scripts/release.py pack --target x86_64-unknown-linux-gnu --output dist
python3 scripts/tests/release_smoke.py --directory dist --target x86_64-unknown-linux-gnu
```

Public archives differ from the assertion-enabled `scripts/compiler.py`
development bundles. They contain exactly `bin/fai` (or `bin/fai.exe`),
`metadata.json`, and `LICENSE`, with normalized archive metadata. The runtime
archive is built with the same target and locked dependencies as the compiler.
Windows release binaries and their embedded runtime use the static MSVC CRT.
Any signing must precede hashing and qualification of the final binaries.

The publishing job assembles all target reports and checks that they identify
the same source commit and source digest. It publishes compiler and installer
archives plus `release.json`, `SHA256SUMS`, `install.sh`, and `install.ps1`.
`release.json` is the versioned machine-readable contract: canonical version/tag,
commit, installer compatibility, target requirements, archive sizes/hashes,
executable hashes, and compiler build information. Build provenance is attested.

## Publication, retry, and verification

Before first publication, enable GitHub **immutable releases** in repository
settings and protect `v*` tags against updates/deletion. The release job needs
contents write permission; build/test jobs have read-only repository permissions.

Repository administrators can enable and inspect the immutable-release setting:

```sh
gh api --method PUT repos/Krzysztof-Cieslak/fai/immutable-releases
gh api repos/Krzysztof-Cieslak/fai/immutable-releases
```

Publication verification requires GitHub to report the release as immutable.

After all verification succeeds, the workflow creates annotated `vVERSION` on
the tested commit, uploads a complete draft release, verifies uploaded digests,
and publishes. Notes are exactly the committed changelog version section.
Prereleases are opt-in. Stable Latest promotion is serialized and compares SemVer,
so publishing an older maintenance line does not replace a newer stable release.

```sh
python3 scripts/release.py verify --version 0.2.0
```

Verification checks source identity, asset completeness and digests, release
notes, and prerelease status. `--directory PATH` can use already-downloaded release
files. Report its release URL and commit with the workflow outcome.

A failed run leaves the existing stable release usable. Inspect logs and rerun
that workflow with the same commit/version. Draft assets can be replaced by a
new fully verified attempt; published assets cannot. An already-published matching
release is verified on retry. A conflicting tag is an error, never force-moved.

Correct a published defect with a new version. If needed, restore the stable
pointer explicitly:

```sh
gh release edit vKNOWN_GOOD --latest
```

Keep the original exact-version artifacts available and explain the correction
in the new release's changelog. Installed project pins remain exact.
