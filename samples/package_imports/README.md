# Package imports and public facades

Three local packages demonstrate module-only imports and identity-preserving
re-exports. `facade` publicly re-exports `collections.Values`; the application
imports that path and also imports Values directly under another alias. Both
paths refer to the same `Wrapped` type, so a directly constructed value can be
passed to the facade-typed function.

The compiler integration test checks formatting, type identity, and execution:

```sh
cargo test -p fai-tests --test package_imports package_import_sample
```
