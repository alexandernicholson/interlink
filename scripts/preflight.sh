#!/usr/bin/env bash
# Pre-commit gate (engineering-rules.md D8). Run before EVERY commit:
#
#   ./scripts/preflight.sh
#
# Green preflight is necessary, not sufficient — it covers the mechanical
# checks (B6, B8, A1's "tests pass" half). The judgment checks in D3
# (falsifiable tests, per-item finding closure, honest commit message)
# are still on you.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> clippy (all targets, warnings are errors)"
cargo clippy --all-targets -- -D warnings

echo "==> tests"
cargo test --quiet

echo "==> microbenchmark semantic paths and coverage floors"
./scripts/verify-microbench-paths.py

echo "==> preflight OK"
