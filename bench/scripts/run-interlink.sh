#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/common.sh
source "${BENCH_DIR}/scripts/common.sh"

capture_environment
CLUSTER_NAME="${BENCH_CLUSTER_PREFIX}-interlink"
create_bench_cluster "${CLUSTER_NAME}"
load_echo_image "${CLUSTER_NAME}"
kind load docker-image --name "${CLUSTER_NAME}" "interlink-bench/interlinkd:${BENCH_IMAGE_TAG}"
kubectl apply -f "${BENCH_DIR}/manifests/namespace.yaml"

log "generating interlink certificates (SPIFFE URI SAN, DER)"
CA_DIR="${BENCH_PRIVATE_DIR}/certs"
mkdir -m 700 "${CA_DIR}"
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

log "deploying interlink sidecars"
apply_workload "${BENCH_DIR}/manifests/interlink/echo-with-sidecar.yaml"
apply_workload "${BENCH_DIR}/manifests/interlink/fortio-client.yaml"
wait_for_pod bench echo-server
wait_for_pod bench fortio-client
run_workloads interlink bench interlinkd "http://127.0.0.1:4140/echo"
