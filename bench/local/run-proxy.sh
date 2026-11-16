#!/usr/bin/env bash
set -euo pipefail

# interlink proxy benchmark — measures the full proxy path:
#   Fortio → HTTPS/mTLS → interlinkd → HTTP → echo-server (PROFILE_DELAY delay)
# All three components run natively on the host: the 2026-11-16 Docker
# Desktop update broke VM→host reachability and made the host→VM loopback
# tunnel cost ~3 ms per request (measured), so the July-era containerized
# legs are unreproducible. Deltas vs July sections include this path change.
# Set QUICK=1 for short profiles (60 s each, default 30 s).

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${LOCAL_DIR}/../.." && pwd)"
RESULTS="${LOCAL_DIR}/results"
mkdir -p "${RESULTS}"

DEMO_SRC="/tmp/interlink-demo"
# ca_bootstrap refuses to touch an existing directory (fresh-material
# contract, symlink safety) and writes to std::env::temp_dir()/interlink-demo
# — pin TMPDIR so that resolves to DEMO_SRC, and clear stale material
# instead of pre-creating the dir the bootstrap must create itself.
export TMPDIR="/tmp"
# Docker Desktop's file sharing does not cover the RAM-backed /tmp —
# bind mounts from there surface empty inside containers (broke the
# fortio cert load on 2026-11-16). Stage the material into the repo's
# build tree, which lives on a shared path.
CERT_DIR="${REPO_ROOT}/target/bench/demo-certs"
rm -rf -- "${DEMO_SRC}" "${CERT_DIR}"

FORTIO_BIN="${REPO_ROOT}/target/bench/bin/fortio" # pinned 1.66.0, native (see run_profile)
ECHO_BIN="${REPO_ROOT}/target/bench/bin/echo-server" # native (see below)
INBOUND_PORT="${INBOUND_PORT:-14443}"
ECHO_PORT="${ECHO_PORT:-18080}"
# Build on disk, never tmpfs: the LTO release build needs several GB of
# intermediate artifacts and /tmp is a RAM-backed tmpfs (caused ENOSPC).
CARGO_TARGET="${CARGO_TARGET:-${REPO_ROOT}/target/bench}"
INTERLINK_BIN="${INTERLINK_BIN:-${CARGO_TARGET}/release/interlinkd}" # override to A/B a different build

log() { echo "[$(date -Iseconds)] $*" >&2; }
cleanup() {
    log "cleaning up"
    if [[ -n "${ECHO_PID:-}" ]] && kill -0 "${ECHO_PID}" 2>/dev/null; then
        kill "${ECHO_PID}" 2>/dev/null || true
        wait "${ECHO_PID}" 2>/dev/null || true
    fi
    if [[ -n "${INTERLINK_PID:-}" ]] && kill -0 "${INTERLINK_PID}" 2>/dev/null; then
        kill "${INTERLINK_PID}" 2>/dev/null || true
        wait "${INTERLINK_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# --------------- setup ---------------
log "generating certificates"
cargo build --quiet --example ca_bootstrap --release 2>&1
cargo run --quiet --example ca_bootstrap 2>&1
mkdir -p "${CERT_DIR}"
cp -p "${DEMO_SRC}"/* "${CERT_DIR}"/

# Build interlinkd if missing.
if [[ ! -x "${INTERLINK_BIN}" ]]; then
    log "building interlinkd (release)"
    CARGO_TARGET_DIR="${CARGO_TARGET}" cargo build --release --bin interlinkd 2>&1
fi

PROFILE_DELAY="${PROFILE_DELAY:-200ms}"
ECHO_SRC="${REPO_ROOT}/bench/workloads"
if [[ ! -x "${ECHO_BIN}" ]]; then
    log "building native echo-server"
    (cd "${ECHO_SRC}" && go build -o "${ECHO_BIN}" .)
fi
log "starting echo-server (plain HTTP, port ${ECHO_PORT}, delay ${PROFILE_DELAY})"
"${ECHO_BIN}" -addr ":${ECHO_PORT}" -delay "${PROFILE_DELAY}" &
ECHO_PID=$!
# Wait for the listener, bounded (the proxy dials it lazily per connection).
for _ in $(seq 1 50); do
    nc -z 127.0.0.1 "${ECHO_PORT}" 2>/dev/null && break
    sleep 0.1
done

log "starting interlinkd proxy on port ${INBOUND_PORT}"
INTERLINK_ALLOW_ALL=true \
INTERLINK_DEFAULT_UPSTREAM="127.0.0.1:${ECHO_PORT}" \
INTERLINK_PROXY_INBOUND_PORT="${INBOUND_PORT}" \
INTERLINK_PROXY_OUTBOUND_PORT=0 \
INTERLINK_TRUST_DOMAIN="example.local" \
INTERLINK_IDENTITY="spiffe://example.local/ns/default/sa/backend" \
INTERLINK_CA_BUNDLE_PATH="${CERT_DIR}/ca.der" \
INTERLINK_CERT_PATH="${CERT_DIR}/server.der" \
INTERLINK_KEY_PATH="${CERT_DIR}/server.key" \
INTERLINK_LOG_LEVEL="warn" \
INTERLINK_MAX_CONNECTIONS=10000 \
    "${INTERLINK_BIN}" &
INTERLINK_PID=$!
sleep 2

if ! kill -0 "${INTERLINK_PID}" 2>/dev/null; then
    log "ERROR: interlinkd failed to start"
    exit 1
fi

log "proxy ready on port ${INBOUND_PORT}"

# --------------- measurement ---------------
sample_interlink_metrics() {
    local output="$1"
    local duration="$2"
    log "sampling interlinkd CPU/memory for ${duration}s -> ${output}"
    echo "timestamp,cpu_percent,memory_rss_kb" > "${output}"
    local end
    end=$(($(date +%s) + duration))
    while [[ $(date +%s) -lt ${end} ]]; do
        local stats
        stats=$(ps -p "${INTERLINK_PID}" -o %cpu=,rss= 2>/dev/null || true)
        if [[ -n "${stats}" ]]; then
            local cpu_percent rss_kb
            cpu_percent=$(echo "${stats}" | awk '{print $1}')
            rss_kb=$(echo "${stats}" | awk '{print $2}')
            echo "$(date +%s),${cpu_percent},${rss_kb}" >> "${output}"
        fi
        sleep 2
    done
}

parse_fortio_json() {
    local file="$1"
    python3 - <<PY
import json, sys
try:
    with open("${file}") as f:
        text = f.read()
    # Find the Fortio results JSON using raw_decode (handles text before/after JSON).
    decoder = json.JSONDecoder()
    idx = 0
    d = None
    while True:
        idx = text.find("{", idx)
        if idx < 0:
            break
        try:
            obj, end = decoder.raw_decode(text, idx)
            if isinstance(obj, dict) and "DurationHistogram" in obj:
                d = obj
                break
            idx = end
        except json.JSONDecodeError:
            idx += 1
    if d is None:
        print("parse_error=no Fortio JSON found")
        sys.exit(0)
    hist = d.get("DurationHistogram", {})
    avg = hist.get("Avg", 0) * 1000
    p50 = p90 = p99 = 0
    for p in hist.get("Percentiles", []):
        v = p.get("Value", 0) * 1000
        if p.get("Percentile") == 50: p50 = v
        elif p.get("Percentile") == 90: p90 = v
        elif p.get("Percentile") == 99: p99 = v
    actual_qps = d.get("ActualQPS", 0)
    ret_codes = d.get("RetCodes", {})
    errors = sum(ret_codes.values()) - ret_codes.get("200", 0)
    print(f"p50_ms={p50:.3f} p90_ms={p90:.3f} p99_ms={p99:.3f} avg_ms={avg:.3f} actual_qps={actual_qps:.1f} errors={errors}")
except Exception as e:
    print(f"parse_error={e}")
PY
}

aggregate_metrics() {
    local input="$1"
    awk -F, 'NR>1 {sum_cpu+=$2; sum_mem+=$3; if($2>max_cpu) max_cpu=$2; if($3>max_mem) max_mem=$3; n++} END {if(n>0) printf "avg_cpu_percent=%.2f peak_cpu_percent=%.2f avg_memory_rss_kb=%.1f peak_memory_rss_kb=%.1f\n", sum_cpu/n, max_cpu, sum_mem/n, max_mem}' "${input}"
}

# Prometheus endpoint for the interlinkd metrics server.
METRICS_URL="${METRICS_URL:-http://127.0.0.1:4190}"

scrape_interlink_metrics() {
    local label="$1"
    local output="$2"
    log "scraping handshake metrics for ${label} from ${METRICS_URL}"
    local data
    data=$(curl -s --max-time 2 "${METRICS_URL}/metrics" 2>/dev/null || true)
    if [[ -z "${data}" ]]; then
        log "WARNING: could not scrape metrics from ${METRICS_URL} (proxy may not be running)"
        return
    fi
    local handshake_data
    handshake_data=$(echo "${data}" | grep -E "interlink_handshake_full_total|interlink_handshake_resumed_total|interlink_handshakes_total" || true)
    if [[ -z "${handshake_data}" ]]; then
        log "WARNING: no handshake metrics found in Prometheus output"
        return
    fi
    log "handshake metrics captured"
    {
        echo ""
        echo "## Handshake metrics (${label})"
        echo '```'
        echo "${handshake_data}"
        echo '```'
    } >> "${output}"
}

run_profile() {
    local qps="$1"
    local conns="$2"
    local extra_flags="${3:-}"
    local label="${4:-proxy-interlink-q${qps}-c${conns}}"
    local duration="${BENCH_DURATION:-${QUICK_DURATION:-300}}"
    log "running profile ${label} (${duration}s) flags=${extra_flags}"

    local metrics_out="${RESULTS}/metrics-${label}.csv"
    sample_interlink_metrics "${metrics_out}" "${duration}" &
    local sampler_pid=$!

    # Fortio runs natively (pinned 1.66.0, same as the retired container
    # image): since the 2026-11-16 Docker daemon update, containers can no
    # longer reach host listeners (host networking, host-gateway and the
    # LAN IP are all refused), so the load generator must live on the host.
    # The proxy->echo leg is unchanged: Mac -> VM still works and echo
    # stays in a --network host container, matching the July baselines.
    if [[ ! -x "${FORTIO_BIN}" ]]; then
        log "building native fortio into ${FORTIO_BIN}"
        GOBIN="${REPO_ROOT}/target/bench/bin" go install fortio.org/fortio@1.66.0
    fi

    # Warm-up (discarded).
    "${FORTIO_BIN}" load \
        -qps "${qps}" -c "${conns}" -t 30s -payload-size 1024 \
        ${extra_flags} \
        -cacert "${CERT_DIR}/ca.pem" -cert "${CERT_DIR}/client.pem" \
        -key "${CERT_DIR}/client-key.pem" \
        -json /tmp/fortio-warmup.json \
        "https://localhost:${INBOUND_PORT}/echo" >/dev/null 2>&1 || true

    # Measured run (Fortio defaults to HTTP/1.1 — proxy doesn't do h2→h1 conversion).
    "${FORTIO_BIN}" load \
        -timeout 120s \
        -qps "${qps}" -c "${conns}" -t "${duration}s" -payload-size 1024 \
        ${extra_flags} \
        -cacert "${CERT_DIR}/ca.pem" -cert "${CERT_DIR}/client.pem" \
        -key "${CERT_DIR}/client-key.pem" \
        -json "${RESULTS}/fortio-${label}.json" \
        -labels "${label}" \
        "https://localhost:${INBOUND_PORT}/echo" > "${RESULTS}/fortio-${label}.log" 2>&1

    kill "${sampler_pid}" 2>/dev/null || true

    # Each profile writes its own section file; the summary is assembled from
    # all section files at the end, so a targeted rerun updates only its own
    # sections instead of clobbering the rest.
    {
        echo "## ${label}"
        echo ""
        echo "Captured: $(date -Iseconds) @ $(git -C "${REPO_ROOT}" rev-parse --short HEAD 2>/dev/null || echo unknown)"
        parse_fortio_json "${RESULTS}/fortio-${label}.json"
        aggregate_metrics "${metrics_out}"
        echo ""
    } > "${RESULTS}/section-${label}.md"
}

# --------------- quick / full ---------------
if [[ "${QUICK:-0}" == "1" ]]; then
    QUICK_DURATION=60
    log "QUICK mode — ${QUICK_DURATION}s profiles"
else
    QUICK_DURATION=300
    log "FULL mode — ${QUICK_DURATION}s profiles"
fi

HEADER="${RESULTS}/section-00-header.md"
{
    echo "# interlink proxy benchmark summary"
    echo ""
    echo "Last run: $(date -Iseconds) @ $(git -C "${REPO_ROOT}" rev-parse --short HEAD 2>/dev/null || echo 'unknown')"
    echo "Server: Go echo-server (native, plain HTTP, ${PROFILE_DELAY:-200ms} delay)"
    echo "Load: Fortio HTTPS + mTLS (native, pinned 1.66.0)"
    echo "Proxy: interlinkd (inbound mTLS → plain TCP)"
    echo ""
} > "${HEADER}"

if [[ "${SKIP_STANDARD:-0}" != "1" ]]; then
for qps in 320 3200 12800; do
    case "${qps}" in
        320) conns=160 ;;
        3200) conns=1600 ;;
        12800) conns=6400 ;;
    esac
    run_profile "${qps}" "${conns}"
done
fi

# Connection-churn profile: no keepalive → new TCP connection per request.
# This stresses handshake throughput (TLS 1.3 resumption, identity extraction).
if [[ "${CHURN:-0}" == "1" ]]; then
    log "connection-churn profile"
    # Light churn: moderate QPS with per-request connections.
    run_profile 100 1 "-keepalive=false" "proxy-interlink-churn-q100-c1"
    # Heavy churn: high QPS with fresh connections.
    run_profile 500 5 "-keepalive=false" "proxy-interlink-churn-q500-c5"
fi

# Bulk-throughput profile: large payloads to expose copy-buffer costs.
if [[ "${BULK:-0}" == "1" ]]; then
    log "bulk-throughput profile"
    run_profile 100 2 "-payload-size 65536 -httpbufferkb 512" "proxy-interlink-bulk-64kb"
    # -httpbufferkb: fortio default 128KB buffer cannot hold 256KB responses
    run_profile 50 2 "-payload-size 262144 -httpbufferkb 512" "proxy-interlink-bulk-256kb"
fi

# Scrape handshake metrics from the Prometheus endpoint into its own section.
rm -f "${RESULTS}/section-zz-handshake.md"
scrape_interlink_metrics "final" "${RESULTS}/section-zz-handshake.md"

# Assemble the summary from all section files (persisted across runs).
cat "${RESULTS}"/section-*.md > "${RESULTS}/proxy-summary.md"

log "proxy benchmark complete. Results in ${RESULTS}/proxy-summary.md"
