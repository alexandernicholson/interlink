#!/usr/bin/env bash
set -euo pipefail

# interlink proxy benchmark — measures the full proxy path:
#   Fortio → HTTPS/mTLS → interlinkd → HTTP → echo-server (200 ms delay)
# Set QUICK=1 for short profiles (60 s each, default 30 s).

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${LOCAL_DIR}/../.." && pwd)"
RESULTS="${LOCAL_DIR}/results"
mkdir -p "${RESULTS}"

CERT_DIR="/tmp/interlink-demo"
mkdir -p "${CERT_DIR}"

INBOUND_PORT="${INBOUND_PORT:-14443}"
ECHO_PORT="${ECHO_PORT:-18080}"
FORTIO_IMAGE="fortio/fortio:1.66.0"
CARGO_TARGET="/tmp/opencode/interlink-target"
INTERLINK_BIN="${CARGO_TARGET}/release/interlinkd"

log() { echo "[$(date -Iseconds)] $*" >&2; }
cleanup() {
    log "cleaning up"
    docker kill echo-server 2>/dev/null || true
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

# Build interlinkd if missing.
if [[ ! -x "${INTERLINK_BIN}" ]]; then
    log "building interlinkd (release)"
    CARGO_TARGET_DIR="${CARGO_TARGET}" cargo build --release --bin interlinkd 2>&1
fi

PROFILE_DELAY="${PROFILE_DELAY:-200ms}"
log "starting echo-server (plain HTTP, port ${ECHO_PORT}, delay ${PROFILE_DELAY})"
docker rm -f echo-server 2>/dev/null || true
docker run -d --rm --network host --name echo-server \
    interlink-bench/echo-server:latest \
    -addr ":${ECHO_PORT}" -delay "${PROFILE_DELAY}" >/dev/null
sleep 1

log "starting interlinkd proxy on port ${INBOUND_PORT}"
INTERLINK_ALLOW_ALL=true \
INTERLINK_DEFAULT_UPSTREAM="localhost:${ECHO_PORT}" \
INTERLINK_PROXY_INBOUND_PORT="${INBOUND_PORT}" \
INTERLINK_PROXY_OUTBOUND_PORT=0 \
INTERLINK_TRUST_DOMAIN="example.local" \
INTERLINK_IDENTITY="spiffe://example.local/ns/default/sa/proxy" \
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

    # Warm-up (discarded).
    docker run --rm --network host \
        -v "${CERT_DIR}:/certs:ro" \
        "${FORTIO_IMAGE}" load \
            -qps "${qps}" -c "${conns}" -t 30s -payload-size 1024 \
            ${extra_flags} \
            -cacert /certs/ca.pem -cert /certs/client.pem -key /certs/client-key.pem \
            -json /tmp/fortio-warmup.json \
            "https://localhost:${INBOUND_PORT}/echo" >/dev/null 2>&1 || true

    # Measured run (Fortio defaults to HTTP/1.1 — proxy doesn't do h2→h1 conversion).
    docker run --rm --network host \
        -v "${CERT_DIR}:/certs:ro" \
        "${FORTIO_IMAGE}" load \
            -timeout 120s \
            -qps "${qps}" -c "${conns}" -t "${duration}s" -payload-size 1024 \
            ${extra_flags} \
            -cacert /certs/ca.pem -cert /certs/client.pem -key /certs/client-key.pem \
            -json /dev/stdout \
            -labels "${label}" \
            "https://localhost:${INBOUND_PORT}/echo" > "${RESULTS}/fortio-${label}.json" 2>&1

    kill "${sampler_pid}" 2>/dev/null || true

    {
        echo "## ${label}"
        echo ""
        parse_fortio_json "${RESULTS}/fortio-${label}.json"
        aggregate_metrics "${metrics_out}"
        echo ""
    } >> "${RESULTS}/proxy-summary.md"
}

# --------------- quick / full ---------------
if [[ "${QUICK:-0}" == "1" ]]; then
    QUICK_DURATION=60
    log "QUICK mode — ${QUICK_DURATION}s profiles"
else
    QUICK_DURATION=300
    log "FULL mode — ${QUICK_DURATION}s profiles"
fi

echo "# interlink proxy benchmark summary" > "${RESULTS}/proxy-summary.md"
echo "" >> "${RESULTS}/proxy-summary.md"
echo "Server: Go echo-server (plain HTTP, ${PROFILE_DELAY:-200ms} delay)" >> "${RESULTS}/proxy-summary.md"
echo "Proxy: interlinkd (inbound mTLS → plain TCP)" >> "${RESULTS}/proxy-summary.md"
echo "Load: Fortio HTTPS + mTLS" >> "${RESULTS}/proxy-summary.md"
echo "Profiles: ${QUICK_DURATION}s each" >> "${RESULTS}/proxy-summary.md"
echo "" >> "${RESULTS}/proxy-summary.md"

for qps in 320 3200 12800; do
    case "${qps}" in
        320) conns=160 ;;
        3200) conns=1600 ;;
        12800) conns=6400 ;;
    esac
    run_profile "${qps}" "${conns}"
done

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
    run_profile 100 2 "-payload-size 65536" "proxy-interlink-bulk-64kb"
    run_profile 50 2 "-payload-size 262144" "proxy-interlink-bulk-256kb"
fi

log "proxy benchmark complete. Results in ${RESULTS}/proxy-summary.md"
