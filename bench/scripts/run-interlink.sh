#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/common.sh
source "${BENCH_DIR}/scripts/common.sh"

CLUSTER_NAME="interlink-bench-interlink"
RESULTS="${RESULTS_DIR}/interlink"
mkdir -p "${RESULTS}"

log "creating kind cluster ${CLUSTER_NAME}"
cat > "${RESULTS_DIR}/kind-config.yaml" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    extraPortMappings:
      - containerPort: 8080
        hostPort: 18080
EOF

kind create cluster --name "${CLUSTER_NAME}" --config "${RESULTS_DIR}/kind-config.yaml"

log "loading images into kind"
kind load docker-image --name "${CLUSTER_NAME}" interlink-bench/echo-server:latest
kind load docker-image --name "${CLUSTER_NAME}" interlink-bench/interlinkd:latest

log "generating interlink test certificates"
CA_DIR="${RESULTS}/certs"
mkdir -p "${CA_DIR}"
# Use rcgen or openssl to generate a small CA + leaf cert.
# For reproducibility we generate with fixed filenames expected by interlinkd.
openssl req -x509 -newkey ed25519 -keyout "${CA_DIR}/ca.key" -out "${CA_DIR}/ca.der" -days 1 -nodes -subj "/CN=interlink-bench-ca" 2>/dev/null || {
    log "openssl not available; please provide certs at ${CA_DIR}"
    exit 1
}
openssl req -newkey ed25519 -keyout "${CA_DIR}/server.key" -out "${CA_DIR}/server.csr" -nodes -subj "/" 2>/dev/null
openssl x509 -req -in "${CA_DIR}/server.csr" -CA "${CA_DIR}/ca.der" -CAkey "${CA_DIR}/ca.key" -CAcreateserial -out "${CA_DIR}/server.der" -days 1 2>/dev/null

kubectl create namespace bench || true
kubectl create secret generic interlink-certs -n bench \
    --from-file="${CA_DIR}/ca.der" \
    --from-file="${CA_DIR}/server.der" \
    --from-file="${CA_DIR}/server.key"

log "deploying interlink"
kubectl apply -n bench -f "${BENCH_DIR}/manifests/interlink/configmap.yaml"
kubectl apply -n bench -f "${BENCH_DIR}/manifests/interlink/rbac.yaml"
kubectl apply -n bench -f "${BENCH_DIR}/manifests/interlink/daemonset.yaml"

log "deploying echo server"
kubectl apply -n bench -f "${BENCH_DIR}/workloads/echo-server-deployment.yaml"
wait_for_pod bench "app=echo-server"
wait_for_pod bench "app=interlink"

log "installing metrics-server"
kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
kubectl patch deployment metrics-server -n kube-system --type='json' -p='[{"op": "add", "path": "/spec/template/spec/containers/0/args/-", "value": "--kubelet-insecure-tls"}]'

# Wait for metrics-server to be ready.
sleep 30
kubectl wait --for=condition=ready pod -n kube-system -l k8s-app=metrics-server --timeout=120s

run_profile() {
    local qps="$1"
    local conns="$2"
    local label="interlink-q${qps}-c${conns}"
    log "running profile ${label}"

    local metrics_out="${RESULTS}/metrics-${label}.csv"
    sample_metrics bench "app=interlink" "${metrics_out}" 300 &
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
    LABEL="interlink-q${QPS}-c${CONNECTIONS}"
    run_profile "${QPS}" "${CONNECTIONS}"
done

log "interlink benchmark complete. Results in ${RESULTS}"
