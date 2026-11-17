#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${BENCH_DIR}/bin"
export PATH="${BIN_DIR}:${PATH}"

# Keep these pins in capture.json through run_capture.py; never reuse an unchecked CLI.
export KIND_VERSION="v0.24.0"
export KUBECTL_VERSION="v1.30.0"
export LINKERD_VERSION="stable-2.14.10"
export ISTIO_VERSION="1.24.0"
export FORTIO_VERSION="1.66.0"

log() { printf '[bench] %s\n' "$*" >&2; }

require() {
    command -v "$1" >/dev/null 2>&1 || {
        log "required command not found: $1"
        return 1
    }
}

capture_environment() {
    : "${BENCH_CAPTURE_DIR:?Run ./bench/run.sh, not individual mesh scripts}"
    : "${BENCH_PRIVATE_DIR:?Missing private capture directory}"
    : "${BENCH_IMAGE_TAG:?Missing unique image tag}"
    : "${BENCH_CLUSTER_PREFIX:?Missing unique cluster prefix}"
    : "${KUBECONFIG:?Missing isolated kubeconfig}"
}

wait_for_pod() {
    kubectl rollout status -n "$1" "deployment/$2" --timeout=300s
}

create_bench_cluster() {
    kind create cluster --name "$1" --image "kindest/node:${KUBECTL_VERSION}" \
        --config "${BENCH_DIR}/manifests/kind-3node.yaml" --wait 180s
    kubectl label node "$1-worker" bench-role=client
    kubectl label node "$1-worker2" bench-role=server
}

load_echo_image() {
    kind load docker-image --name "$1" "interlink-bench/echo-server:${BENCH_IMAGE_TAG}"
}

apply_workload() {
    # Only substitute the capture-specific images; no environment-dependent mesh variants.
    python3 - "$1" "${BENCH_IMAGE_TAG}" <<'PY' | kubectl apply -f -
import pathlib, sys
print(pathlib.Path(sys.argv[1]).read_text().replace("${BENCH_IMAGE_TAG}", sys.argv[2]))
PY
}

run_workloads() {
    kubectl version -o json | python3 -c 'import json,os,sys; assert json.load(sys.stdin)["serverVersion"]["gitVersion"] == os.environ["KUBECTL_VERSION"]'
    kubectl exec -n bench deploy/fortio-client -c fortio -- fortio version | grep -F "${FORTIO_VERSION}"
    local shared_args=(--shared-rps "${BENCH_SHARED_RPS:-10000}"
                       --shared-concurrency "${BENCH_SHARED_CONCURRENCY:-64}")
    if [[ "${BENCH_SHARED_ONLY:-0}" == 1 ]]; then
        shared_args+=(--shared-only)
    fi
    python3 "${BENCH_DIR}/scripts/capture_profile.py" \
        --mesh "$1" --namespace "$2" --container "$3" --target "$4" \
        --client-node "${CLUSTER_NAME}-worker" --server-node "${CLUSTER_NAME}-worker2" \
        --results "${BENCH_CAPTURE_DIR}/$1" --duration "${BENCH_DURATION:-20}" \
        --rps-concurrency "${BENCH_RPS_CONCURRENCY:-1,4,16,64,256,1024,4096}" \
        --bulk-concurrency "${BENCH_BULK_CONCURRENCY:-1,4,16,64,256,1024}" \
        "${shared_args[@]}"
}
