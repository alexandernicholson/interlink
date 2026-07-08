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
    # kubectl wait errors with "no matching resources" if the controller has
    # not created the pod object yet — poll for existence first.
    local waited=0
    while ! kubectl get pod -n "${namespace}" -l "${selector}" -o name 2>/dev/null | grep -q .; do
        sleep 2
        waited=$((waited + 2))
        if [[ ${waited} -ge ${timeout} ]]; then
            log "ERROR: no pod matching ${selector} appeared within ${timeout}s"
            return 1
        fi
    done
    kubectl wait --for=condition=ready pod -n "${namespace}" -l "${selector}" --timeout="${timeout}s"
}

# Create the shared 3-node benchmark cluster and label the workers so
# workloads can be pinned: fortio -> bench-role=client, echo -> bench-role=server.
create_bench_cluster() {
    local name="$1"
    kind create cluster --name "${name}" --config "${BENCH_DIR}/manifests/kind-3node.yaml"
    kubectl label node "${name}-worker" bench-role=client --overwrite
    kubectl label node "${name}-worker2" bench-role=server --overwrite
}

sample_metrics() {
    local namespace="$1"
    local selector="$2"
    local output="$3"
    local duration="$4"
    log "sampling metrics for -l ${selector} (summed) for ${duration}s -> ${output}"
    echo "timestamp,cpu_millicores,memory_rss_kb" > "${output}"
    local end
    end=$(($(date +%s) + duration))
    while [[ $(date +%s) -lt ${end} ]]; do
        # Sum CPU/mem across every pod matching the selector — the mesh's total
        # proxy footprint (interlink runs one daemon per node; sum both).
        local metrics
        metrics=$(kubectl top pod -n "${namespace}" -l "${selector}" --no-headers 2>/dev/null || true)
        if [[ -n "${metrics}" ]]; then
            local ts
            ts=$(date +%s)
            echo "${metrics}" | awk -v ts="${ts}" '{
                cpu = $2; mem = $3
                gsub(/[^0-9.]/, "", cpu)
                if (mem ~ /Ki$/) { gsub(/Ki$/, "", mem) }
                else if (mem ~ /Mi$/) { gsub(/Mi$/, "", mem); mem = mem * 1024 }
                else if (mem ~ /Gi$/) { gsub(/Gi$/, "", mem); mem = mem * 1048576 }
                sc += cpu; sm += int(mem)
            } END { print ts "," sc "," sm }' >> "${output}"
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
        # Keep only the JSON object: from the first line that is exactly "{" to
        # the closing "}". Fortio interleaves a "Successfully wrote N bytes..."
        # status line into stdout that corrupts a naive /^{/,$p capture.
        kubectl logs -n "${namespace}" "${pod}" --tail=-1 2>/dev/null \
            | sed -n '/^{$/,/^}$/p' > "${output}" || true
    fi
}

aggregate_metrics() {
    local input="$1"
    awk -F, 'NR>1 {sum_cpu+=$2; sum_mem+=$3; if($2>max_cpu) max_cpu=$2; if($3>max_mem) max_mem=$3; n++} END {if(n>0) printf "avg_cpu_millicores=%.1f peak_cpu_millicores=%.1f avg_memory_rss_kb=%.1f peak_memory_rss_kb=%.1f\n", sum_cpu/n, max_cpu, sum_mem/n, max_mem}' "${input}"
}

parse_fortio_json() {
    local file="$1"
    python3 - "$file" <<'PY'
import json, sys
text = open(sys.argv[1]).read()
dec = json.JSONDecoder(); i = 0; d = None
while True:
    i = text.find('{', i)
    if i < 0: break
    try:
        o, e = dec.raw_decode(text, i)
        if isinstance(o, dict) and 'DurationHistogram' in o:
            d = o; break
        i = e
    except json.JSONDecodeError:
        i += 1
if d is None:
    print('parse_error=no Fortio JSON found'); sys.exit(0)
h = d['DurationHistogram']
ps = {p['Percentile']: p['Value']*1000 for p in h.get('Percentiles', [])}
rc = d.get('RetCodes', {})
errs = sum(rc.values()) - rc.get('200', 0) - rc.get(200, 0)
print(f"p50_ms={ps.get(50,0):.2f} p90_ms={ps.get(90,0):.2f} p99_ms={ps.get(99,0):.2f} actual_qps={d.get('ActualQPS',0):.1f} errors={errs}")
PY
}
