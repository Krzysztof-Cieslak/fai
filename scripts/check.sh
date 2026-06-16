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
# CI runs the suite with cargo-nextest (readable summaries) plus doctests
# separately. Mirror that when nextest is installed; otherwise fall back so this
# script works on a stock toolchain.
if command -v cargo-nextest >/dev/null 2>&1; then
  cargo nextest run --workspace --locked
  cargo test --workspace --locked --doc
  echo "== nextest lane partition =="
  # CI runs the unit and property tests in separate parallel lanes; assert they
  # still partition the suite (reuses the build above).
  ./scripts/check-nextest-partition.sh
else
  echo "(cargo-nextest not installed; using 'cargo test'. Install: https://nexte.st)"
  cargo test --workspace --locked
fi

echo "All checks passed."
