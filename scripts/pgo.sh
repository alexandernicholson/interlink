#!/usr/bin/env bash
# Profile-guided optimization for interlinkd.
#
# Usage:
#   scripts/pgo.sh gen   # instrumented build + bench workload + profdata merge
#   scripts/pgo.sh use   # release build trained on the merged profile
#   scripts/pgo.sh all   # both, in order
#
# Layout (under target/, never /tmp — a RAM-backed tmpfs; see CLAUDE.md):
#   target/pgo-gen/      instrumented build + .profraw files
#   target/pgo.profdata  merged profile
#   target/pgo-use/      PGO-trained release build (interlinkd binary)
#
# The profile workload is the two-proxy bench harness (churn + bulk, QUICK
# mode): the accept, relay, handshake, and identity-cache loops the profile
# should train. The daemon shuts down gracefully, so its counters are flushed
# to .profraw on exit.
#
# Verify the win before shipping it (D10): interleave arms — one run against
# the stock release binary, one against the PGO binary, at least twice each —
# because single-run p99 on this harness varies 44–84 ms on identical configs
# (R29). Numbers and their deltas belong in docs/performance-plan.md.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT}"

PGO_GEN_DIR="${ROOT}/target/pgo-gen"
PGO_PROFDATA="${ROOT}/target/pgo.profdata"
PGO_USE_DIR="${ROOT}/target/pgo-use"
PROFRAW_DIR="${PGO_GEN_DIR}/profraw"

log() { echo "[pgo $(date -Iseconds)] $*" >&2; }

profdata_tool() {
    local host tool
    host="$(rustc -vV | sed -n 's/^host: //p')"
    tool="$(rustc --print sysroot)/lib/rustlib/${host}/bin/llvm-profdata"
    if [[ ! -x "${tool}" && -n "$(command -v rustup)" ]]; then
        # Homebrew rustc ships no llvm-tools; fall back to the rustup-managed
        # toolchain's copy (profile formats are compatible across these
        # LLVM generations).
        local rustc_bin
        rustc_bin="$(rustup which rustc 2>/dev/null || true)"
        if [[ -n "${rustc_bin}" ]]; then
            tool="$(dirname "$(dirname "${rustc_bin}")")/lib/rustlib/${host}/bin/llvm-profdata"
        fi
    fi
    if [[ ! -x "${tool}" ]]; then
        echo "pgo: llvm-profdata not found. Install it with: rustup component add llvm-tools-preview" >&2
        exit 1
    fi
    printf '%s' "${tool}"
}

pgo_gen() {
    mkdir -p "${PROFRAW_DIR}"
    rm -f "${PROFRAW_DIR}"/*.profraw "${PGO_PROFDATA}"

    log "building instrumented interlinkd (fat LTO + profile-generate)"
    RUSTFLAGS="-Cprofile-generate=${PGO_GEN_DIR}" \
    LLVM_PROFILE_FILE="${PROFRAW_DIR}/default_%p_%m.profraw" \
    CARGO_TARGET_DIR="${PGO_GEN_DIR}" \
        cargo build --release --bin interlinkd

    log "running bench workload (QUICK churn + bulk) against the instrumented daemon"
    # PROFILE_DELAY=0: the default 200 ms echo delay is delay-bound (measured
    # 4.8-24 rps vs 100-500 requested on 2026-11-16) and would train the
    # profile on idle sleeping, not on the accept/relay/handshake loops.
    # The July baselines used the same zero-delay condition.
    PROFILE_DELAY=0 \
    LLVM_PROFILE_FILE="${PROFRAW_DIR}/default_%p_%m.profraw" \
    CARGO_TARGET="${PGO_GEN_DIR}" \
        QUICK=1 CHURN=1 BULK=1 SKIP_STANDARD=1 \
        bench/local/run-proxy.sh

    log "merging profiles"
    "$(profdata_tool)" merge -sparse -o "${PGO_PROFDATA}" "${PROFRAW_DIR}"/*.profraw
    log "profile data ready: ${PGO_PROFDATA}"
}

pgo_use() {
    if [[ ! -f "${PGO_PROFDATA}" ]]; then
        echo "pgo: ${PGO_PROFDATA} missing — run 'scripts/pgo.sh gen' first" >&2
        exit 1
    fi
    log "building PGO-trained interlinkd"
    RUSTFLAGS="-Cprofile-use=${PGO_PROFDATA}" \
    CARGO_TARGET_DIR="${PGO_USE_DIR}" \
        cargo build --release --bin interlinkd
    log "PGO binary: ${PGO_USE_DIR}/release/interlinkd"
    log "A/B it (interleaved arms, R29):"
    log "  CARGO_TARGET=${PGO_USE_DIR} QUICK=1 CHURN=1 BULK=1 SKIP_STANDARD=1 bench/local/run-proxy.sh"
}

case "${1:-}" in
    gen) pgo_gen ;;
    use) pgo_use ;;
    all) pgo_gen; pgo_use ;;
    *) sed -n '2,10p' "${BASH_SOURCE[0]}"; exit 1 ;;
esac
