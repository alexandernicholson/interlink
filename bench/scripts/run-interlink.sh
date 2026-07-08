#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/common.sh
source "${BENCH_DIR}/scripts/common.sh"

CLUSTER_NAME="interlink-bench-interlink"
export INTERLINK_MUX="${INTERLINK_MUX:-true}"
VARIANT="interlink"
[[ "${INTERLINK_MUX}" == "false" ]] && VARIANT="interlink-nomux"
RESULTS="${RESULTS_DIR}/${VARIANT}"
mkdir -p "${RESULTS}"

log "creating kind cluster ${CLUSTER_NAME} (3-node shared topology)"
create_bench_cluster "${CLUSTER_NAME}"

log "loading images into kind"
kind load docker-image --name "${CLUSTER_NAME}" interlink-bench/echo-server:latest
kind load docker-image --name "${CLUSTER_NAME}" interlink-bench/interlinkd:latest

kubectl create namespace bench || true

log "generating interlink certificates (SPIFFE URI SAN, DER)"
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
    -extfile <(printf "subjectAltName=URI:%s,DNS:localhost\nextendedKeyUsage=serverAuth,clientAuth\nkeyUsage=digitalSignature\n" "${SPIFFE_ID}") 2>/dev/null

for f in ca.der server.key server.der; do
    [[ -s "${CA_DIR}/${f}" ]] || { log "ERROR: failed to generate ${f}"; exit 1; }
done

kubectl create secret generic interlink-certs -n bench \
    --from-file="${CA_DIR}/ca.der" \
    --from-file="${CA_DIR}/server.der" \
    --from-file="${CA_DIR}/server.key"

log "deploying echo server + interlink sidecar (mux=${INTERLINK_MUX})"
envsubst '${INTERLINK_MUX}' < "${BENCH_DIR}/manifests/interlink/echo-with-sidecar.yaml" | kubectl apply -f -
log "deploying fortio client + interlink sidecar"
envsubst '${INTERLINK_MUX}' < "${BENCH_DIR}/manifests/interlink/fortio-client.yaml" | kubectl apply -f -
wait_for_pod bench "app=echo-server"
wait_for_pod bench "app=fortio-client"

log "installing metrics-server"
kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml
kubectl patch deployment metrics-server -n kube-system --type='json' -p='[{"op": "add", "path": "/spec/template/spec/containers/0/args/-", "value": "--kubelet-insecure-tls"}]'
kubectl wait --for=condition=available deployment/metrics-server -n kube-system --timeout=120s 2>/dev/null || true

TARGET="http://127.0.0.1:4140/echo"   # the local outbound sidecar

log "smoke check through the mesh"
kubectl exec -n bench deploy/fortio-client -c fortio -- \
    fortio load -quiet -qps 5 -c 1 -n 10 "${TARGET}" >/dev/null 2>&1 || {
    log "ERROR: smoke request failed; sidecar data path is broken"
    kubectl logs -n bench deploy/fortio-client -c interlinkd --tail=20 || true
    kubectl logs -n bench deploy/echo-server -c interlinkd --tail=20 || true
    exit 1
}

run_profile() {
    local qps="$1" conns="$2"
    local label="${VARIANT}-q${qps}-c${conns}"
    log "running profile ${label}"
    local metrics_out="${RESULTS}/metrics-${label}.csv"
    sample_container_metrics bench interlinkd "${metrics_out}" "${BENCH_DURATION:-60}" &
    local sampler_pid=$!
    exec_fortio_profile "${TARGET}" "${qps}" "${conns}" "${BENCH_DURATION:-60}" \
        "${RESULTS}/fortio-${label}.json"
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

log "${VARIANT} benchmark complete. Results in ${RESULTS}"
