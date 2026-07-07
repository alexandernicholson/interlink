#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/common.sh
source "${BENCH_DIR}/scripts/common.sh"

CLUSTER_NAME="interlink-bench-istio"
RESULTS="${RESULTS_DIR}/istio-ambient"
mkdir -p "${RESULTS}"

log "creating kind cluster ${CLUSTER_NAME}"
kind create cluster --name "${CLUSTER_NAME}"

log "loading echo-server image"
kind load docker-image --name "${CLUSTER_NAME}" interlink-bench/echo-server:latest

log "installing Istio ${ISTIO_VERSION} in ambient mode"
istioctl install --set profile=ambient --skip-confirmation
kubectl get namespace bench >/dev/null 2>&1 || kubectl create namespace bench
kubectl apply -f "${BENCH_DIR}/manifests/istio-ambient/namespace-label.yaml"

log "deploying echo server"
kubectl apply -n bench -f "${BENCH_DIR}/workloads/echo-server-deployment.yaml"
wait_for_pod bench "app=echo-server"

log "installing metrics-server"
kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
kubectl patch deployment metrics-server -n kube-system --type='json' -p='[{"op": "add", "path": "/spec/template/spec/containers/0/args/-", "value": "--kubelet-insecure-tls"}]'
sleep 30
kubectl wait --for=condition=ready pod -n kube-system -l k8s-app=metrics-server --timeout=120s

run_profile() {
    local qps="$1"
    local conns="$2"
    local label="istio-ambient-q${qps}-c${conns}"
    log "running profile ${label}"

    local metrics_out="${RESULTS}/metrics-${label}.csv"
    # ztunnel runs in istio-system as a daemonset.
    sample_metrics istio-system "app=ztunnel" "${metrics_out}" 300 &
    local sampler_pid=$!

    kubectl delete job fortio-load -n bench --ignore-not-found=true
    envsubst < "${BENCH_DIR}/load/fortio-job.yaml" | kubectl apply -f -
    kubectl wait --for=condition=complete job/fortio-load -n bench --timeout=400s

    kill "${sampler_pid}" 2>/dev/null || true

    local pod
    pod=$(kubectl get pod -n bench -l job-name=fortio-load -o jsonpath='{.items[0].metadata.name}')
    kubectl cp "${pod}:/tmp/results/fortio.json" "${RESULTS}/fortio-${label}.json" -n bench
    kubectl delete job fortio-load -n bench --ignore-not-found=true

    {
        echo "profile=${label}"
        parse_fortio_json "${RESULTS}/fortio-${label}.json"
        aggregate_metrics "${metrics_out}"
    } >> "${RESULTS}/summary.txt"
}

export QPS CONNECTIONS DURATION PAYLOAD_SIZE LABEL
for QPS in 320 3200 12800; do
    case "${QPS}" in
        320) CONNECTIONS=160 ;;
        3200) CONNECTIONS=1600 ;;
        12800) CONNECTIONS=6400 ;;
    esac
    DURATION=300
    PAYLOAD_SIZE=1024
    LABEL="istio-ambient-q${QPS}-c${CONNECTIONS}"
    run_profile "${QPS}" "${CONNECTIONS}"
done

log "istio ambient benchmark complete. Results in ${RESULTS}"
