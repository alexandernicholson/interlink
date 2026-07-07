#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="${BENCH_DIR}/bin"
mkdir -p "${BIN_DIR}"

# shellcheck source=scripts/common.sh
source "${BENCH_DIR}/scripts/common.sh"

log "installing pinned tools into ${BIN_DIR}"

OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)
case "${ARCH}" in
    x86_64) ARCH="amd64" ;;
    aarch64) ARCH="arm64" ;;
esac

# kind
if [[ ! -x "${BIN_DIR}/kind" ]]; then
    log "downloading kind ${KIND_VERSION}"
    curl -sL "https://kind.sigs.k8s.io/dl/${KIND_VERSION}/kind-${OS}-${ARCH}" -o "${BIN_DIR}/kind"
    chmod +x "${BIN_DIR}/kind"
fi

# kubectl
if [[ ! -x "${BIN_DIR}/kubectl" ]]; then
    log "downloading kubectl ${KUBECTL_VERSION}"
    curl -sL "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/${OS}/${ARCH}/kubectl" -o "${BIN_DIR}/kubectl"
    chmod +x "${BIN_DIR}/kubectl"
fi

# linkerd
if [[ ! -x "${BIN_DIR}/linkerd" ]]; then
    log "downloading linkerd ${LINKERD_VERSION}"
    curl -sL "https://github.com/linkerd/linkerd2/releases/download/${LINKERD_VERSION}/linkerd2-cli-${LINKERD_VERSION}-${OS}-${ARCH}" -o "${BIN_DIR}/linkerd"
    chmod +x "${BIN_DIR}/linkerd"
fi

# istioctl
if [[ ! -x "${BIN_DIR}/istioctl" ]]; then
    log "downloading istioctl ${ISTIO_VERSION}"
    curl -sL "https://github.com/istio/istio/releases/download/${ISTIO_VERSION}/istioctl-${ISTIO_VERSION}-${OS}-${ARCH}.tar.gz" | tar -xz -C "${BIN_DIR}"
    chmod +x "${BIN_DIR}/istioctl"
fi

log "tool versions:"
"${BIN_DIR}/kind" version
"${BIN_DIR}/kubectl" version --client
"${BIN_DIR}/linkerd" version --client
"${BIN_DIR}/istioctl" version --remote=false

log "building local container images"
docker build -t interlink-bench/echo-server:latest -f "${BENCH_DIR}/workloads/echo-server.Dockerfile" "${BENCH_DIR}/workloads"
docker build -t interlink-bench/interlinkd:latest -f "${BENCH_DIR}/../Dockerfile" "${BENCH_DIR}/.." || {
    log "no Dockerfile in repo root; build interlinkd binary manually and copy into image"
}

log "setup complete"
