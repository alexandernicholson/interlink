#!/usr/bin/env bash
set -u
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

echo "========================================="
echo " interlink — end-to-end mTLS demo"
echo "========================================="
echo ""

# Find a free port
BACKEND_PORT=$(python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1]); s.close()" 2>/dev/null || echo 9443)
echo "Using port: $BACKEND_PORT"

# Step 1: Generate CA + certs
echo "[1/4] Generating CA and certificates..."
CA_OUTPUT=$(cargo run --example ca_bootstrap 2>&1 | tail -1)
CERT_DIR="/tmp/interlink-demo"
echo "  Cert dir: $CERT_DIR"

# Step 2: Start backend service
echo "[2/4] Starting backend service (port $BACKEND_PORT)..."
cargo run --example backend -- "$CERT_DIR" "$BACKEND_PORT" > /tmp/interlink-backend.log 2>&1 &
BACKEND_PID=$!
sleep 2

# Step 3: Run frontend client
echo "[3/4] Running frontend client..."
FRONTEND_OUTPUT=$(cargo run --example frontend -- "$CERT_DIR" "127.0.0.1:$BACKEND_PORT" 2>&1)
echo "$FRONTEND_OUTPUT"

# Step 4: Cleanup
echo ""
echo "[4/4] Cleaning up..."
kill "$BACKEND_PID" 2>/dev/null || true
wait "$BACKEND_PID" 2>/dev/null || true

# Check result
if echo "$FRONTEND_OUTPUT" | grep -q "PASSED"; then
    echo "========================================="
    echo " ✓ DEMO PASSED — mTLS end-to-end working"
    echo "========================================="
    exit 0
else
    echo "========================================="
    echo " ✗ DEMO FAILED"
    echo "========================================="
    echo ""
    echo "Backend logs:"
    cat /tmp/interlink-backend.log 2>/dev/null || true
    exit 1
fi
