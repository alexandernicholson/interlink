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

log "deploying fortio client"
kubectl apply -n bench -f "${BENCH_DIR}/workloads/fortio-client-plain.yaml"
wait_for_pod bench "app=fortio-client"

log "installing metrics-server"
kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
kubectl patch deployment metrics-server -n kube-system --type='json' -p='[{"op": "add", "path": "/spec/template/spec/containers/0/args/-", "value": "--kubelet-insecure-tls"}]'
# Wait for the deployment to be available; tolerate timeout since metrics-server is non-critical for Fortio.
kubectl wait --for=condition=available deployment/metrics-server -n kube-system --timeout=120s 2>/dev/null || true

run_profile() {
    local qps="$1" conns="$2"
    local label="linkerd-q${qps}-c${conns}"
    log "running profile ${label}"
    local metrics_out="${RESULTS}/metrics-${label}.csv"
    sample_container_metrics bench linkerd-proxy "${metrics_out}" "${BENCH_DURATION:-60}" &
    local sampler_pid=$!
    exec_fortio_profile "http://echo-server:8080/echo" "${qps}" "${conns}" \
        "${BENCH_DURATION:-60}" "${RESULTS}/fortio-${label}.json"
    kill "${sampler_pid}" 2>/dev/null || true
    {
        echo "profile=${label}"
        parse_fortio_json "${RESULTS}/fortio-${label}.json"
        aggregate_metrics "${metrics_out}"
    } >> "${RESULTS}/summary.txt"
}

PROFILES_SPEC="${BENCH_PROFILES:-320:80,800:200,1600:400}"
IFS=',' read -ra _PROFILES <<< "${PROFILES_SPEC}"
for _p in "${_PROFILES[@]}"; do
    run_profile "${_p%%:*}" "${_p##*:}"
done

log "linkerd benchmark complete. Results in ${RESULTS}"
