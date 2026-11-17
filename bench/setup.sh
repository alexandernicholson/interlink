#!/usr/bin/env bash
set -euo pipefail
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${BENCH_DIR}/scripts/common.sh"
capture_environment
for cmd in docker curl tar python3 openssl git; do require "${cmd}"; done
[[ "$(uname -s)" == Linux ]] || { log "capture requires Linux /proc/stat"; exit 1; }
mkdir -p "${BIN_DIR}"
ARCH="$(uname -m)"
case "${ARCH}" in
    x86_64) ARCH=amd64 ;;
    aarch64|arm64) ARCH=arm64 ;;
    *) log "unsupported architecture: ${ARCH}"; exit 1 ;;
esac

# Download afresh into private staging. A failed download never falls back to an old CLI.
download() { curl --fail --location --retry 3 --connect-timeout 15 --max-time 300 "$1" -o "$2"; }
TOOLS="${BENCH_PRIVATE_DIR}/tools"
mkdir -p "${TOOLS}"
download "https://kind.sigs.k8s.io/dl/${KIND_VERSION}/kind-linux-${ARCH}" "${TOOLS}/kind"
download "https://dl.k8s.io/release/${KUBECTL_VERSION}/bin/linux/${ARCH}/kubectl" "${TOOLS}/kubectl"
download "https://github.com/linkerd/linkerd2/releases/download/${LINKERD_VERSION}/linkerd2-cli-${LINKERD_VERSION}-linux-${ARCH}" "${TOOLS}/linkerd"
download "https://github.com/istio/istio/releases/download/${ISTIO_VERSION}/istioctl-${ISTIO_VERSION}-linux-${ARCH}.tar.gz" "${TOOLS}/istioctl.tar.gz"
tar -xzf "${TOOLS}/istioctl.tar.gz" -C "${TOOLS}" istioctl
for tool in kind kubectl linkerd istioctl; do
    chmod +x "${TOOLS}/${tool}"
    mv "${TOOLS}/${tool}" "${BIN_DIR}/${tool}"
done
kind version | grep -F "${KIND_VERSION}"
kubectl version --client -o json | python3 -c 'import json,os,sys; assert json.load(sys.stdin)["clientVersion"]["gitVersion"] == os.environ["KUBECTL_VERSION"]'
linkerd version --client | grep -F "${LINKERD_VERSION}"
istioctl version --remote=false | grep -F "${ISTIO_VERSION}"
docker info > "${BENCH_PRIVATE_DIR}/docker-info.txt"
docker build --pull -t "interlink-bench/echo-server:${BENCH_IMAGE_TAG}" \
    -f "${BENCH_DIR}/workloads/echo-server.Dockerfile" "${BENCH_DIR}/workloads"
docker build --pull -t "interlink-bench/interlinkd:${BENCH_IMAGE_TAG}" \
    -f "${BENCH_DIR}/../Dockerfile" "${BENCH_DIR}/.."
