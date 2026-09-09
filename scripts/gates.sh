#!/usr/bin/env bash
# Full CI gate for rust-backup. Any failure halts (set -e). Mirrors bore's
# test_gates.sh. Run from the workspace root: `bash scripts/gates.sh`.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt --all --check"
cargo fmt --all --check

echo "==> cargo clippy --all-targets --all-features -- -D warnings"
cargo clippy --all-targets --all-features -- -D warnings

echo "==> cargo build --all-features"
cargo build --all-features

echo "==> cargo build --no-default-features (relay-only, no quinn)"
cargo build --no-default-features

echo "==> cargo test --all-features"
cargo test --all-features

echo "==> crate lint invariants"
bash scripts/crate_invariants.sh

echo "==> help/USAGE parity"
bash scripts/help_parity.sh

echo "==> ALL GATES PASSED"
