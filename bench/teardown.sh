#!/usr/bin/env bash
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/common.sh
source "${BENCH_DIR}/scripts/common.sh"

for cluster in interlink-bench-interlink interlink-bench-linkerd interlink-bench-istio; do
    if kind get clusters 2>/dev/null | grep -q "^${cluster}$"; then
        log "deleting kind cluster ${cluster}"
        kind delete cluster --name "${cluster}"
    fi
done

log "teardown complete"
