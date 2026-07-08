#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/common.sh
source "${BENCH_DIR}/scripts/common.sh"

CLUSTER_NAME="interlink-bench-linkerd"
RESULTS="${RESULTS_DIR}/linkerd"
mkdir -p "${RESULTS}"

log "creating kind cluster ${CLUSTER_NAME}"
create_bench_cluster "${CLUSTER_NAME}"

log "loading echo-server image"
kind load docker-image --name "${CLUSTER_NAME}" interlink-bench/echo-server:latest

log "installing linkerd ${LINKERD_VERSION}"
linkerd install --crds | kubectl apply -f -
linkerd install | kubectl apply -f -
linkerd check

log "deploying namespace and echo server"
kubectl apply -f "${BENCH_DIR}/manifests/linkerd/namespace-annotation.yaml"
kubectl apply -n bench -f "${BENCH_DIR}/workloads/echo-server-deployment.yaml"
wait_for_pod bench "app=echo-server"

log "installing metrics-server"
kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
kubectl patch deployment metrics-server -n kube-system --type='json' -p='[{"op": "add", "path": "/spec/template/spec/containers/0/args/-", "value": "--kubelet-insecure-tls"}]'
# Wait for the deployment to be available; tolerate timeout since metrics-server is non-critical for Fortio.
kubectl wait --for=condition=available deployment/metrics-server -n kube-system --timeout=120s 2>/dev/null || true

run_profile() {
    local qps="$1"
    local conns="$2"
    local label="linkerd-q${qps}-c${conns}"
    log "running profile ${label}"

    local metrics_out="${RESULTS}/metrics-${label}.csv"
    sample_metrics bench "app=echo-server" "${metrics_out}" "${BENCH_DURATION:-300}" &
    local sampler_pid=$!

    kubectl delete job fortio-load -n bench --ignore-not-found=true
    envsubst < "${BENCH_DIR}/load/fortio-job.yaml" | kubectl apply -f -
    kubectl wait --for=condition=complete job/fortio-load -n bench --timeout=400s 2>/dev/null || true

    kill "${sampler_pid}" 2>/dev/null || true

    collect_fortio_logs bench "job-name=fortio-load" "${RESULTS}/fortio-${label}.json"
    kubectl delete job fortio-load -n bench --ignore-not-found=true

    {
        echo "profile=${label}"
        parse_fortio_json "${RESULTS}/fortio-${label}.json"
        aggregate_metrics "${metrics_out}"
    } >> "${RESULTS}/summary.txt"
}

export QPS CONNECTIONS DURATION PAYLOAD_SIZE LABEL FORTIO_TIMEOUT
# BENCH_PROFILES="qps:conns,..." overrides the default set. Default targets a
# dedicated host; connections are Little's-law sized (rps x 0.2s delay x1.25).
PROFILES_SPEC="${BENCH_PROFILES:-320:80,3200:800,12800:3200}"
IFS=',' read -ra _PROFILES <<< "${PROFILES_SPEC}"
for _p in "${_PROFILES[@]}"; do
    QPS="${_p%%:*}"
    CONNECTIONS="${_p##*:}"
    DURATION="${BENCH_DURATION:-300}"
    PAYLOAD_SIZE=1024
    FORTIO_TIMEOUT=120
    LABEL="linkerd-q${QPS}-c${CONNECTIONS}"
    run_profile "${QPS}" "${CONNECTIONS}"
done

log "linkerd benchmark complete. Results in ${RESULTS}"
