# Native paths and filesystem I/O

Filesystem I/O is part of the standard library. The `FileSystem` capability is
available as `Runtime.fs` and `stdFs`; pure path manipulation needs no capability.
File, directory and temporary-resource operations return structured errors.

## Paths

`Path` is an opaque immutable value. It stores the exact native spelling: Unix
filename bytes or Windows little-endian UTF-16 code units. A path returned by
directory enumeration can always be passed back to the filesystem, even when it
has no Unicode representation.

```fai
match Path.fromString "settings.json" with
| Err error -> console.writeLine (Path.errorToString error)
| Ok path ->
  match File.readText fs 1048576 path with
  | Ok text -> console.writeLine text
  | Err error -> console.writeLine (FileSystem.errorToString error)
```

`fromString` rejects embedded NULs. `fromNativeBytes` additionally validates native
unit boundaries; `toNativeBytes` is its lossless inverse on the same platform.
`toString` returns `Err InvalidEncoding` for non-Unicode names. `display` returns
quoted, escaped diagnostic text, never a path to pass to an OS operation.

Equality, ordering and hashing compare exact native encoding, without filesystem
case folding or Unicode normalization. Use `fs.sameFile` to compare actual file
identity. Identity is a snapshot, not a permanently unique name after deletion.

`join base child` follows native joining rules: an absolute child replaces the
base. Windows root-relative and drive-prefixed operands follow native rules.
`resolve base child` additionally requires an absolute base, rejects drive-relative
children such as `C:file`, and normalizes the result. It never reads a process or
per-drive working directory.

`normalize` removes lexical dot segments, cancels ordinary components against
`..`, and clamps traversal at roots. Empty stays empty; a nonempty path collapsing
to its current directory becomes `.`. It removes redundant/trailing separators
and writes native separators. Windows verbatim paths retain their exact spelling.
Lexical normalization does not resolve symlinks and need not identify the same
file as the original path. `fs.canonicalize` performs actual filesystem resolution.

`parent`, `fileName`, `stem`, `extension`, `root` and `components` inspect lexical
structure. Roots have no parent or filename; `.gitignore` has no extension.
`relativeTo base target` fails for incompatible roots or an unresolved `..` in
the remaining base. `withFileName name path` and `withExtension suffix path`
require a single native filename component; an empty suffix removes an extension.

## Errors and handles

`FileSystem.Error` contains `kind`, `operation`, `path`, `destination`, `nativeCode`
and `message`. Kinds include absence, permission, type-of-entry, encoding, closed
handle, size, cancellation and general I/O errors. Use the kind for control flow;
native codes and rendered messages provide additional diagnostics.

`FileSystem.exists fs path` follows links; `entryExists` observes the link itself.
Only `NotFound` becomes false. A permission error remains an error.

The capability supports text/binary reads and writes, reader/writer handles,
chunking, absolute seeking, flushing and synchronization. `readBytes path limit`
has an explicit nonnegative limit. `readFile` reads complete UTF-8 contents;
prefer `File.readText fs limit path` when the input size is not otherwise bounded.
Chunk reads require a positive count and return at most 64 KiB per native read,
even for larger requested counts. Only a successful empty read means EOF.

`openWriter path mode` makes creation policy explicit:

| Mode | Behavior |
|---|---|
| `CreateNew` | Exclusive creation; fails if the entry exists |
| `Truncate` | Create or truncate |
| `Append` | Create or append |
| `Existing` | Open existing without truncation; initial position is zero |

`openWrite` and `openAppend` are convenient truncate/append forms. Seeking an
append writer does not change the OS append behavior of subsequent writes.

Explicit close is idempotent and expires **every alias**. A failed writer flush
still closes the handle, and is not silently retried on final drop. The scoped
`File.withReader` and `withWriter` helpers close under cancellation-masked cleanup.
`ScopeError` distinguishes open, callback and close errors, including both a
primary error and failed cleanup. Retaining a handle does not extend its scope.

Native operations run on the blocking pool under the scheduler. Cancellation is
checked before work and between bounded read/write chunks. An OS operation
already blocked in the kernel is not generally interruptible.

## Metadata and mutation

`metadata` follows links; `linkMetadata` observes links. Both return entry kind,
byte length, portable read-only state, binary native identity and optional
access/creation/modification `Instant`s with nanosecond precision. These are
snapshots; later operations still handle concurrent changes and errors.

Directory creation, empty-directory removal, file removal, symlink/hard-link
creation and link-target reading are explicit capability methods. On Windows,
symlink creation may require developer mode or privileges; the error is returned.

`rename` uses atomic no-clobber semantics. `replace` permits native replacement.
Neither falls back to copy-and-delete across filesystems. `copy source destination
overwrite` copies bytes, rejects a destination symlink, and checks the opened
source/destination identities before truncation. It does not copy ownership or
ACLs. `copyPermissions` copies portable permissions only.

`removeTree` uses native recursive deletion without following symlink targets.
Failure can leave a partially removed tree. Its native traversal is not
interruptible; cancellation is checked before starting. It does not promise
transactional rollback or a snapshot of a concurrently modified tree.

## Listing and walking

`openDirectory`, `nextDirectory` and `closeDirectory` operate an opaque cursor in
native enumeration order. Each entry has a lossless name/path and an optional
cheap kind hint. Cursor aliases follow the same close rules as file handles.

`Directory.list env limit path` collects and sorts a bounded directory by native
path encoding. `Directory.walk env options root initial step` is a depth-first
fold over descendants; immediate children have depth one. The callback returns a
new state and `Continue`, `SkipChildren` or `Stop`, or its own typed error.

Defaults: no link following, native order, stop on error, one million entries,
depth 256, at most 64 simultaneous directory cursors. Depth intentionally prunes;
entry and cursor limits fail explicitly. Sorted order buffers each bounded
directory, so memory also depends on pending entries in ancestor frames.

Link following detects cycles against active ancestor identities. Native
enumeration is not a snapshot; external replacement of entries can affect later
operations. `WalkResult` retains committed state, counts, termination/failure and
a separate cleanup error. All active cursors close on stop, failure or cancellation.

## Temporary resources and atomic writes

`Temp.withFile` and `withDirectory` atomically create randomly named resources in
an explicit parent (`fs.temporaryDirectory ()` supplies the OS default). Unix
creation modes are private; Windows inherits the parent's ACL. Callbacks receive
the native path, opaque resource and, for files, a writer. Scope exit closes writer
aliases and deletes the entry. Final drop is a fallback without an error channel.

`Temp.persist fs resource destination overwrite` transfers ownership on successful
rename. A failed transfer remains owned for cleanup. A retained path value does
not extend the temporary resource's lifetime.

`File.withAtomicWriter`, `writeBytesAtomic` and `writeTextAtomic` stage a temporary
sibling and then rename it. Options explicitly select overwrite, destination
symlink replacement, portable permission preservation, and durability:

- `VisibilityOnly`: flush and close before rename.
- `SyncFile`: additionally synchronize the staged file before rename.
- `SyncFileAndDirectory`: additionally synchronize the parent after rename.

Directory synchronization currently returns `Unsupported` on Windows. Permission
preservation copies Unix modes / Windows read-only state, not ownership or ACLs.
The default creates a new private file and rejects an observed symlink destination.
Rename replaces a link entry, never its target, when replacement is enabled.

`AtomicResult` keeps outcome, cleanup failure and commit phase separately:

- `NotCommitted`: failed before attempting rename.
- `CommitUnknown`: rename returned an error; a remote filesystem's outcome cannot
  be inferred reliably from that error alone.
- `Committed`: rename succeeded, including a later synchronization failure.

## Migration and verification

Existing filesystem calls now take `Path` instead of `String`, and return
`FileSystem.Error` instead of text errors. Convert at textual input boundaries.
Stream file adapters also take Path; they explicitly render errors into Stream's
String error channel. File stream sinks require `Concurrency` for scoped cleanup.

See `samples/Paths.fai`, `samples/Files.fai`, and `samples/Capabilities.fai`.
`python scripts/tests/filesystem_toolchain.py --fai /path/to/fai` checks native
names, aliases, links, cancellation, temporary cleanup and atomic writes through
a relocated compiler's JIT and AOT paths. Rust tests additionally cover native
error paths, spelling properties, traversal and ownership/space guards.
