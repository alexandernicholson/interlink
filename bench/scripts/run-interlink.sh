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

log "generating interlink test certificates (DER format for interlinkd)"
CA_DIR="${RESULTS}/certs"
mkdir -p "${CA_DIR}"
# interlinkd expects DER/PKCS#8 format, not PEM.
# Generate CA key (PKCS#8 DER) and self-signed cert (DER).
openssl genpkey -algorithm ed25519 -outform DER -out "${CA_DIR}/ca.key" 2>/dev/null
openssl req -x509 -key "${CA_DIR}/ca.key" -keyform DER -out "${CA_DIR}/ca.der" -outform DER -days 1 -nodes -subj "/CN=interlink-bench-ca" 2>/dev/null
# Generate server key (PKCS#8 DER), CSR, and signed cert (DER).
openssl genpkey -algorithm ed25519 -outform DER -out "${CA_DIR}/server.key" 2>/dev/null
openssl req -new -key "${CA_DIR}/server.key" -keyform DER -out "${CA_DIR}/server.csr" -subj "/" 2>/dev/null
openssl x509 -req -in "${CA_DIR}/server.csr" -CA "${CA_DIR}/ca.der" -CAform DER -CAkey "${CA_DIR}/ca.key" -CAkeyform DER -CAcreateserial -out "${CA_DIR}/server.der" -outform DER -days 1 2>/dev/null

# Verify DER files.
for f in ca.key ca.der server.key server.der; do
    if [[ ! -s "${CA_DIR}/${f}" ]]; then
        log "ERROR: failed to generate ${CA_DIR}/${f}"
        exit 1
    fi
done

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
# Wait for the deployment to be available; tolerate timeout since metrics-server is non-critical for Fortio.
kubectl wait --for=condition=available deployment/metrics-server -n kube-system --timeout=120s 2>/dev/null || true

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
for QPS in 320 3200 12800; do
    case "${QPS}" in
        320) CONNECTIONS=160 ;;
        3200) CONNECTIONS=1600 ;;
        12800) CONNECTIONS=6400 ;;
    esac
    DURATION=300
    PAYLOAD_SIZE=1024
    FORTIO_TIMEOUT=120
    LABEL="interlink-q${QPS}-c${CONNECTIONS}"
    run_profile "${QPS}" "${CONNECTIONS}"
done

log "interlink benchmark complete. Results in ${RESULTS}"
