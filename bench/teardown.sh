#!/usr/bin/env bash
set -euo pipefail
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${BENCH_DIR}/scripts/common.sh"
: "${BENCH_CLUSTER_PREFIX:?Cleanup requires a unique capture cluster prefix}"
status=0
for mesh in interlink linkerd istio; do
    kind delete cluster --name "${BENCH_CLUSTER_PREFIX}-${mesh}" || status=1
done
exit "${status}"
