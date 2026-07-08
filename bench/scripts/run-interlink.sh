#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/common.sh
source "${BENCH_DIR}/scripts/common.sh"

CLUSTER_NAME="interlink-bench-interlink"
# INTERLINK_MUX=false runs the no-tunnel control variant.
export INTERLINK_MUX="${INTERLINK_MUX:-true}"
VARIANT="interlink"
if [[ "${INTERLINK_MUX}" == "false" ]]; then
    VARIANT="interlink-nomux"
fi
RESULTS="${RESULTS_DIR}/${VARIANT}"
mkdir -p "${RESULTS}"

log "creating kind cluster ${CLUSTER_NAME} (3-node shared topology)"
create_bench_cluster "${CLUSTER_NAME}"

log "loading images into kind"
kind load docker-image --name "${CLUSTER_NAME}" interlink-bench/echo-server:latest
kind load docker-image --name "${CLUSTER_NAME}" interlink-bench/interlinkd:latest

kubectl create namespace bench || true

# Echo server first: its pod IP goes into the proxy certificate's IP SANs
# (mesh peers dial pods by ip:port; an IP ServerName never matches a DNS SAN).
log "deploying echo server"
kubectl apply -n bench -f "${BENCH_DIR}/workloads/echo-server-deployment.yaml"
wait_for_pod bench "app=echo-server"
ECHO_IP=$(kubectl get pod -n bench -l app=echo-server -o jsonpath='{.items[0].status.podIP}')
log "echo pod IP: ${ECHO_IP}"

log "generating interlink certificates (SPIFFE URI + IP SANs, DER)"
CA_DIR="${RESULTS}/certs"
mkdir -p "${CA_DIR}"
SPIFFE_ID="spiffe://bench.local/ns/bench/sa/proxy"
openssl genpkey -algorithm ed25519 -outform DER -out "${CA_DIR}/ca.key" 2>/dev/null
openssl req -x509 -key "${CA_DIR}/ca.key" -keyform DER -out "${CA_DIR}/ca.der" -outform DER \
    -days 1 -subj "/CN=interlink-bench-ca" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign,digitalSignature" 2>/dev/null
openssl genpkey -algorithm ed25519 -outform DER -out "${CA_DIR}/server.key" 2>/dev/null
openssl req -new -key "${CA_DIR}/server.key" -keyform DER -out "${CA_DIR}/server.csr" -subj "/" 2>/dev/null
openssl x509 -req -in "${CA_DIR}/server.csr" \
    -CA "${CA_DIR}/ca.der" -CAform DER -CAkey "${CA_DIR}/ca.key" -CAkeyform DER \
    -CAcreateserial -out "${CA_DIR}/server.der" -outform DER -days 1 \
    -extfile <(printf "subjectAltName=URI:%s,IP:%s,DNS:localhost\nextendedKeyUsage=serverAuth,clientAuth\nkeyUsage=digitalSignature\n" "${SPIFFE_ID}" "${ECHO_IP}") 2>/dev/null

for f in ca.der server.key server.der; do
    [[ -s "${CA_DIR}/${f}" ]] || { log "ERROR: failed to generate ${f}"; exit 1; }
done

kubectl create secret generic interlink-certs -n bench \
    --from-file="${CA_DIR}/ca.der" \
    --from-file="${CA_DIR}/server.der" \
    --from-file="${CA_DIR}/server.key"

log "deploying interlink (mux=${INTERLINK_MUX})"
kubectl apply -n bench -f "${BENCH_DIR}/manifests/interlink/rbac.yaml"
envsubst '${INTERLINK_MUX}' < "${BENCH_DIR}/manifests/interlink/daemonset.yaml" | kubectl apply -n bench -f -
wait_for_pod bench "app=interlink"

log "installing metrics-server"
kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
kubectl patch deployment metrics-server -n kube-system --type='json' -p='[{"op": "add", "path": "/spec/template/spec/containers/0/args/-", "value": "--kubelet-insecure-tls"}]'
kubectl wait --for=condition=available deployment/metrics-server -n kube-system --timeout=120s 2>/dev/null || true

# Smoke check: one meshed request must succeed before burning profile time.
log "smoke check through the mesh"
kubectl delete pod smoke -n bench --ignore-not-found=true >/dev/null 2>&1
kubectl run smoke -n bench --restart=Never --image=fortio/fortio:${FORTIO_VERSION} \
    --overrides='{"spec":{"nodeSelector":{"bench-role":"client"}}}' \
    -- load -qps 2 -t 2s -c 1 http://echo-server:8080/echo
kubectl wait --for=jsonpath='{.status.phase}'=Succeeded pod/smoke -n bench --timeout=60s || {
    log "ERROR: smoke request failed; interlink data path is broken"
    kubectl logs -n bench -l app=interlink --tail=20 || true
    exit 1
}
kubectl delete pod smoke -n bench --ignore-not-found=true >/dev/null 2>&1

run_profile() {
    local qps="$1"
    local conns="$2"
    local label="${VARIANT}-q${qps}-c${conns}"
    log "running profile ${label}"

    local metrics_out="${RESULTS}/metrics-${label}.csv"
    sample_metrics bench "app=interlink" "${metrics_out}" "${BENCH_DURATION:-300}" &
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
    DURATION="${BENCH_DURATION:-300}"
    PAYLOAD_SIZE=1024
    FORTIO_TIMEOUT=120
    LABEL="${VARIANT}-q${QPS}-c${CONNECTIONS}"
    run_profile "${QPS}" "${CONNECTIONS}"
done

log "${VARIANT} benchmark complete. Results in ${RESULTS}"
