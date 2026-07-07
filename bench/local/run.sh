#!/usr/bin/env bash
set -euo pipefail

LOCAL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${LOCAL_DIR}/../.." && pwd)"
RESULTS="${LOCAL_DIR}/results"
mkdir -p "${RESULTS}"

CERT_DIR="/tmp/interlink-demo"
mkdir -p "${CERT_DIR}"

# Port must match a SAN in the test certificates (localhost).
SERVER_PORT=18443

log() {
    echo "[$(date -Iseconds)] $*" >&2
}

# Build the benchmark server and generate certs.
log "building interlink bench server"
cd "${REPO_ROOT}"
cargo build --release --example bench_server

log "generating certificates"
cargo run --quiet --example ca_bootstrap

SERVER_BIN="${REPO_ROOT}/target/release/examples/bench_server"
SERVER_PID=""
FORTIO_IMAGE="fortio/fortio:1.66.0"

# Cleanup on exit.
cleanup() {
    log "cleaning up"
    if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
        kill "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# Start the mTLS echo server.
log "starting bench server on port ${SERVER_PORT}"
"${SERVER_BIN}" "${CERT_DIR}" "${SERVER_PORT}" 200ms &
SERVER_PID=$!
sleep 2

# Verify server is up.
if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
    log "ERROR: bench server failed to start"
    exit 1
fi

sample_metrics() {
    local output="$1"
    local duration="$2"
    log "sampling server CPU/memory for ${duration}s -> ${output}"
    echo "timestamp,cpu_percent,memory_rss_kb" > "${output}"
    local end
    end=$(($(date +%s) + duration))
    while [[ $(date +%s) -lt ${end} ]]; do
        local stats
        stats=$(ps -p "${SERVER_PID}" -o %cpu=,rss= 2>/dev/null || true)
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
        d = json.load(f)
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
    local label="interlink-q${qps}-c${conns}"
    log "running profile ${label}"

    local duration="${BENCH_DURATION:-300}"
    local metrics_out="${RESULTS}/metrics-${label}.csv"
    sample_metrics "${metrics_out}" "${duration}" &
    local sampler_pid=$!

    # Warm-up run (discarded).
    docker run --rm --network host \
        -v "${CERT_DIR}:/certs:ro" \
        "${FORTIO_IMAGE}" load \
            -qps "${qps}" -c "${conns}" -t 30s -payload-size 1024 \
            -cacert /certs/ca.pem -cert /certs/client.pem -key /certs/client-key.pem \
            -json /tmp/fortio-warmup.json \
            "https://localhost:${SERVER_PORT}/echo" >/dev/null 2>&1 || true

    # Measured run.
    docker run --rm --network host \
        -v "${CERT_DIR}:/certs:ro" \
        -v "${RESULTS}:/tmp/results" \
        "${FORTIO_IMAGE}" load \
            -qps "${qps}" -c "${conns}" -t "${duration}s" -payload-size 1024 \
            -cacert /certs/ca.pem -cert /certs/client.pem -key /certs/client-key.pem \
            -json "/tmp/results/fortio-${label}.json" \
            -labels "${label}" \
            "https://localhost:${SERVER_PORT}/echo"

    kill "${sampler_pid}" 2>/dev/null || true

    {
        echo "## ${label}"
        echo ""
        parse_fortio_json "${RESULTS}/fortio-${label}.json"
        aggregate_metrics "${metrics_out}"
        echo ""
    } >> "${RESULTS}/summary.md"
}

# Main benchmark loop.
echo "# interlink local benchmark summary" > "${RESULTS}/summary.md"
echo "" >> "${RESULTS}/summary.md"
echo "Server: interlink TlsServer (examples/bench_server)" >> "${RESULTS}/summary.md"
echo "Load: Fortio HTTPS + mTLS" >> "${RESULTS}/summary.md"
echo "" >> "${RESULTS}/summary.md"

for qps in 320 3200 12800; do
    case "${qps}" in
        320) conns=160 ;;
        3200) conns=1600 ;;
        12800) conns=6400 ;;
    esac
    run_profile "${qps}" "${conns}"
done

log "benchmark complete. Results in ${RESULTS}"
cat "${RESULTS}/summary.md"
