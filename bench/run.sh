#!/usr/bin/env bash
set -euo pipefail
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${BENCH_DIR}/scripts/common.sh"
export PYTHONDONTWRITEBYTECODE=1
exec python3 "${BENCH_DIR}/scripts/run_capture.py"
