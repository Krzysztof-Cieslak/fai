#!/usr/bin/env sh
# Run the same gates CI enforces, in the same order. Run before pushing.
#
# The pinned toolchain (rust-toolchain.toml) is selected automatically by rustup.
set -eu

echo "== rustfmt =="
cargo fmt --all -- --check

echo "== clippy =="
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

echo "== build =="
cargo build --workspace --all-targets --locked

echo "== test =="
cargo test --workspace --locked

echo "All checks passed."
