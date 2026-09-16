#!/usr/bin/env bash
# Full CI gate for rust-backup. Any failure halts (set -e). Mirrors bore's
# test_gates.sh. Run from the workspace root: `bash scripts/gates.sh`.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt --all --check"
cargo fmt --all --check

echo "==> cargo clippy --locked --all-targets --all-features -- -D warnings"
cargo clippy --locked --all-targets --all-features -- -D warnings

echo "==> cargo build --locked --all-features"
cargo build --locked --all-features

echo "==> cargo build --locked --no-default-features (relay-only, no quinn)"
cargo build --locked --no-default-features

echo "==> cargo test --locked --all-features"
cargo test --locked --all-features

echo "==> cargo test --workspace --locked --no-default-features (relay-only)"
cargo test --workspace --locked --no-default-features

# A public item may only link to another public item: `-D warnings` turns a
# link to a `pub(crate)` type into an error, and CI ran this step where the
# gate did not.
echo "==> cargo doc --workspace --locked --all-features --no-deps (warnings denied)"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --locked --all-features --no-deps

echo "==> crate lint invariants"
bash scripts/crate_invariants.sh

echo "==> source read-only lint (I-IMMUT)"
bash scripts/source_readonly_lint.sh
bash scripts/source_readonly_lint.sh --selftest

echo "==> help/USAGE parity"
bash scripts/help_parity.sh

# Reuses the binary built above (env_inventory.sh honours RUST_BACKUP_BIN and
# defaults to the same path), so the parity step costs no second build.
echo "==> documentation parity (flags, variables, defaults)"
RUST_BACKUP_BIN=${RUST_BACKUP_BIN:-target/debug/rust-backup} bash scripts/docs_parity.sh
RUST_BACKUP_BIN=${RUST_BACKUP_BIN:-target/debug/rust-backup} bash scripts/docs_parity.sh --selftest

echo "==> ALL GATES PASSED"
