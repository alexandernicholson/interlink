#!/usr/bin/env bash
# BOLT post-link layout optimization for interlinkd on Linux x86_64.
#
# EXPERIMENTAL — this repository's CI and dev machines are macOS/arm64 and
# cannot run BOLT; no measured numbers exist for it yet (D10: treat as
# unverified until someone runs this on Linux x86_64 and records the A/B).
#
# Requirements: Linux x86_64, llvm-bolt in PATH (LLVM 17+ packages), perf
# with kernel cycle events enabled. Runs the same QUICK churn+bulk bench
# workload as scripts/pgo.sh as the profile source.
#
# Usage:
#   scripts/bolt-optimize.sh          # profile with the default bench workload
#   scripts/bolt-optimize.sh -- <cmd> # profile with a custom command instead
#
# Output: target/bolt-out/interlinkd (the rewritten binary).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

if [[ "$(uname -s)-$(uname -m)" != "Linux-x86_64" ]]; then
    echo "bolt: Linux x86_64 only (this host is $(uname -s)/$(uname -m))" >&2
    exit 1
fi
command -v llvm-bolt >/dev/null 2>&1 || {
    echo "bolt: llvm-bolt not found in PATH (LLVM 17+ distribution packages)" >&2
    exit 1
}
command -v perf >/dev/null 2>&1 || {
    echo "bolt: perf not found in PATH" >&2
    exit 1
}

BOLT_GEN_DIR="${ROOT}/target/bolt-gen"
BOLT_OUT_DIR="${ROOT}/target/bolt-out"
PERF_DATA="${ROOT}/target/bolt.perf"

log() { echo "[bolt $(date -Iseconds)] $*" >&2; }

log "building relocatable release interlinkd (BOLT needs --emit-relocs)"
RUSTFLAGS="-C link-arg=-Wl,--emit-relocs" \
CARGO_TARGET_DIR="${BOLT_GEN_DIR}" \
    cargo build --release --bin interlinkd

if [[ "${1:-}" == "--" ]]; then
    shift
    log "profiling custom workload: $*"
    perf record -e cycles:u -o "${PERF_DATA}" -- "$@"
else
    log "profiling bench workload (QUICK churn + bulk)"
    CARGO_TARGET="${BOLT_GEN_DIR}" \
    QUICK=1 CHURN=1 BULK=1 SKIP_STANDARD=1 \
        perf record -e cycles:u -o "${PERF_DATA}" -- bench/local/run-proxy.sh
fi

mkdir -p "${BOLT_OUT_DIR}"
log "rewriting binary layout"
llvm-bolt "${BOLT_GEN_DIR}/release/interlinkd" \
    -o "${BOLT_OUT_DIR}/interlinkd" \
    -fdo-data="${PERF_DATA}" \
    -reorder-blocks=ext-tsp \
    -reorder-functions=hfsort+ \
    -split-functions \
    -split-all-cold \
    -split-eh \
    -dyno-stats

log "BOLT binary: ${BOLT_OUT_DIR}/interlinkd"
log "Record the interleaved A/B against the stock binary in docs/performance-plan.md before shipping."
