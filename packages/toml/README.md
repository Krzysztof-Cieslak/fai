# TOML

A pure Fai TOML 1.1 library. The scalar foundation preserves temporal spellings
and precision, checks signed 64-bit integers, and rounds decimal floats using
the JSON package's checked binary64 converter. Explicit `inf` and `nan` tokens
remain distinct from finite overflow. String scanning supports TOML 1.1 escapes
and normalizes multiline line endings to LF while retaining original byte spans.

Run native contracts with `fai test -C packages toml`.
