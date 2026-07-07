#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${BENCH_DIR}/bin"
RESULTS_DIR="${BENCH_DIR}/results"
mkdir -p "${BIN_DIR}" "${RESULTS_DIR}"

export PATH="${BIN_DIR}:${PATH}"

# Pinned versions.
KIND_VERSION="v0.24.0"
KUBECTL_VERSION="v1.30.0"
LINKERD_VERSION="stable-2.14.10"
LINKERD_CLI_VERSION="stable-2.14.10"
ISTIO_VERSION="1.24.0"
FORTIO_VERSION="1.66.0"

log() {
    echo "[$(date -Iseconds)] $*" >&2
}

require() {
    if ! command -v "$1" >/dev/null 2>&1; then
        log "ERROR: required command '$1' not found in PATH"
        exit 1
    fi
}

wait_for_pod() {
    local namespace="$1"
    local selector="$2"
    local timeout="${3:-300}"
    log "waiting for pod ${selector} in namespace ${namespace}"
    kubectl wait --for=condition=ready pod -n "${namespace}" -l "${selector}" --timeout="${timeout}s"
}

sample_metrics() {
    local namespace="$1"
    local selector="$2"
    local output="$3"
    local duration="$4"
    local pod
    pod=$(kubectl get pod -n "${namespace}" -l "${selector}" -o jsonpath='{.items[0].metadata.name}')
    log "sampling metrics for pod ${pod} for ${duration}s -> ${output}"
    echo "timestamp,cpu_millicores,memory_rss_kb" > "${output}"
    local end
    end=$(($(date +%s) + duration))
    while [[ $(date +%s) -lt ${end} ]]; do
        local metrics
        metrics=$(kubectl top pod -n "${namespace}" "${pod}" --no-headers 2>/dev/null || true)
        if [[ -n "${metrics}" ]]; then
            local ts
            ts=$(date +%s)
            echo "${metrics}" | awk -v ts="${ts}" '{
                cpu = $2
                mem = $3
                gsub(/[^0-9.]/, "", cpu)
                if (mem ~ /Ki$/) { gsub(/Ki$/, "", mem) }
                else if (mem ~ /Mi$/) { gsub(/Mi$/, "", mem); mem = mem * 1024 }
                else if (mem ~ /Gi$/) { gsub(/Gi$/, "", mem); mem = mem * 1048576 }
                print ts "," cpu "," int(mem)
            }' >> "${output}"
        fi
        sleep 2
    done
}

collect_fortio_logs() {
    local namespace="$1"
    local label="$2"
    local output="$3"
    local pod
    pod=$(kubectl get pods -n "${namespace}" -l "${label}" -o jsonpath='{.items[-1].metadata.name}' 2>/dev/null || true)
    if [[ -n "${pod}" ]]; then
        # Strip the human-readable report; Fortio JSON starts with a line containing '{'.
        kubectl logs -n "${namespace}" "${pod}" --tail=-1 2>/dev/null | sed -n '/^{/,$p' > "${output}" || true
    fi
}

aggregate_metrics() {
    local input="$1"
    awk -F, 'NR>1 {sum_cpu+=$2; sum_mem+=$3; if($2>max_cpu) max_cpu=$2; if($3>max_mem) max_mem=$3; n++} END {if(n>0) printf "avg_cpu_millicores=%.1f peak_cpu_millicores=%.1f avg_memory_rss_kb=%.1f peak_memory_rss_kb=%.1f\n", sum_cpu/n, max_cpu, sum_mem/n, max_mem}' "${input}"
}

parse_fortio_json() {
    local file="$1"
    if [[ ! -f "${file}" ]]; then
        echo "N/A"
        return
    fi
    # Fortio JSON: DurationHistogram.Avg, Percentiles[].Percentile
    python3 - <<PY
import json, sys
try:
    with open("${file}") as f:
        d = json.load(f)
    hist = d.get("DurationHistogram", {})
    avg = hist.get("Avg", 0) * 1000  # seconds -> ms
    p50 = p90 = p99 = 0
    for p in hist.get("Percentiles", []):
        v = p.get("Value", 0) * 1000
        if p.get("Percentile") == 50: p50 = v
        elif p.get("Percentile") == 90: p90 = v
        elif p.get("Percentile") == 99: p99 = v
    actual_qps = d.get("ActualQPS", 0)
    errors = sum(d.get("RetCodes", {}).values()) - d.get("RetCodes", {}).get("200", 0)
    print(f"p50_ms={p50:.3f} p90_ms={p90:.3f} p99_ms={p99:.3f} avg_ms={avg:.3f} actual_qps={actual_qps:.1f} errors={errors}")
except Exception as e:
    print(f"parse_error={e}")
PY
}
