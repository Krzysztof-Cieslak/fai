# fai-package-store

Filesystem and Git operations for package preparation, kept outside compiler
queries and the pure `fai-package` models.

- `GitStore` uses installed Git, captures operation-local refs, reads raw commit
  blobs, and atomically publishes checksum-verified package subdirectories.
- Full SHA-1/SHA-256 commits, peeled release tags, and cached offline sources
  retain their original identity. Observed releases cannot silently move.
- Tree checksums include canonical paths, bytes, symlink destinations, and Git
  executable modes. External-content layouts such as submodules and LFS pointers
  require explicit support rather than becoming incomplete source trees.
- `MetadataTransaction` journals exact manifest/lock replacements and checks
  preconditions before writing. Recovery preserves independently edited files.
- `VendorIndex` maps full locked descriptors to verified content-addressed trees.
  Configured entries are strict; callers explicitly select cache/Git bypass or
  regenerate source trees before journaling the index and root configuration.

Callers own root selection, source eligibility, resolution policy, and lock
validity. Retrieval errors are distinct from version incompatibilities.
