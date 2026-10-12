# fai-package

`resolve_graph` selects one source per logical package for a root/compiler/target
request. Source providers supply immutable candidate metadata and branch snapshots.
The resolver combines the root's normal/development constraints, follows only
normal edges in consumed packages, and supports conservative, targeted-update,
and full-update preferences. Retrieval failures stay distinct from unsatisfiable
constraints. Replacements preserve the original logical key and version ranges.

Pure project metadata for Fai's package system. The crate validates manifests,
package source identities, version requirements, platform declarations, and source
ownership without filesystem access or compiler database queries.

`ManifestDocument::parse` accepts a versioned package/workspace manifest or a
legacy native-only document. `parse_local` restricts personal configuration to
root overrides. Errors are ordinary located Fai diagnostics, including secondary
locations for conflicts. Semantic `Manifest` values are independent of text
offsets; `ManifestDocument::location` retains the source side table.

`SourcePlan::build` takes the parsed document, a target, and an explicit inventory
of package-relative files. It selects shared library modules, isolated executable
entries, and test modules while excluding inactive platform subtrees and nested
packages. The caller owns file enumeration and physical symlink-containment
checks.

`with_package_version` prepares a new validated document while retaining comments.
It does not write files. Git retrieval, graph resolution, lockfile transactions,
and integration with compiler source contexts build on these data models.

The complete design and current implementation status are in
[`docs/PACKAGES.md`](../../docs/PACKAGES.md).

```sh
cargo test -p fai-package
cargo clippy -p fai-package --all-targets -- -D warnings
```
