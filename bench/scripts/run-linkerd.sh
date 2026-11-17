#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/common.sh
source "${BENCH_DIR}/scripts/common.sh"

capture_environment
CLUSTER_NAME="${BENCH_CLUSTER_PREFIX}-linkerd"
create_bench_cluster "${CLUSTER_NAME}"
load_echo_image "${CLUSTER_NAME}"
linkerd install --crds | kubectl apply -f -
linkerd install --set "proxy.image.version=${LINKERD_VERSION}" | kubectl apply -f -
linkerd check --wait 300s
kubectl apply -f "${BENCH_DIR}/manifests/linkerd/namespace-annotation.yaml"
apply_workload "${BENCH_DIR}/workloads/echo-server-deployment.yaml"
apply_workload "${BENCH_DIR}/workloads/fortio-client-plain.yaml"
wait_for_pod bench echo-server
wait_for_pod bench fortio-client
run_profiles linkerd bench linkerd-proxy "http://echo-server:8080/echo"
