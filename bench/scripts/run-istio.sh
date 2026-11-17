#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/common.sh
source "${BENCH_DIR}/scripts/common.sh"

capture_environment
CLUSTER_NAME="${BENCH_CLUSTER_PREFIX}-istio"
create_bench_cluster "${CLUSTER_NAME}"
load_echo_image "${CLUSTER_NAME}"
istioctl install --set profile=ambient --set "tag=${ISTIO_VERSION}" --skip-confirmation --readiness-timeout 300s
kubectl apply -f "${BENCH_DIR}/manifests/istio-ambient/namespace-label.yaml"
apply_workload "${BENCH_DIR}/workloads/echo-server-deployment.yaml"
apply_workload "${BENCH_DIR}/workloads/fortio-client-plain.yaml"
wait_for_pod bench echo-server
wait_for_pod bench fortio-client
kubectl rollout status daemonset/ztunnel -n istio-system --timeout=300s
run_workloads istio-ambient istio-system istio-proxy "http://echo-server:8080/echo"
