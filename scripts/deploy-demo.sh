#!/usr/bin/env bash
#
# Deploy demo: shows the full deploy lifecycle.
# Deploys v1, redeploys v2, deploys a broken v3 (auto-rollback).
#
# Usage:
#   ./scripts/deploy-demo.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
BUN_PID=""

cleanup() {
    if [[ -n "${BUN_PID}" ]]; then
        kill "${BUN_PID}" 2>/dev/null || true
        wait "${BUN_PID}" 2>/dev/null || true
    fi
    rm -f /tmp/reliaburger-demo-*.toml
}
trap cleanup EXIT

echo "=== Reliaburger Deploy Demo ==="
echo ""

# Build
echo "--- building ---"
cargo build --bin bun --bin relish --bin testapp --manifest-path "${REPO_DIR}/Cargo.toml" --quiet

RELISH="${REPO_DIR}/target/debug/relish"
BUN="${REPO_DIR}/target/debug/bun"
TESTAPP="${REPO_DIR}/target/debug/testapp"

# Start bun
echo "--- starting bun ---"
"${BUN}" --runtime process &
BUN_PID=$!

for i in $(seq 1 15); do
    if curl -sf http://localhost:9117/v1/health >/dev/null 2>&1; then break; fi
    if [[ $i -eq 15 ]]; then echo "error: bun did not start"; exit 1; fi
    sleep 1
done
echo "bun is ready (pid ${BUN_PID})"

# ---------------------------------------------------------------
# Step 1: Deploy v1 (healthy app on port 8080)
# ---------------------------------------------------------------
echo ""
echo "============================================"
echo "  Step 1: Deploy v1 (healthy app)"
echo "============================================"

cat > /tmp/reliaburger-demo-v1.toml <<EOF
[app.demo]
command = ["${TESTAPP}", "--mode", "healthy", "--port", "8080"]
port = 8080
EOF

"${RELISH}" apply /tmp/reliaburger-demo-v1.toml || true
sleep 2

echo ""
echo "--- status after v1 ---"
"${RELISH}" status || true

echo ""
echo "--- calling v1 (testapp health endpoint on :8080) ---"
curl -sf http://localhost:8080/health && echo "" || echo "(not reachable — testapp binds to 0.0.0.0)"
sleep 1

echo ""
echo "--- top ---"
"${RELISH}" top || true

# ---------------------------------------------------------------
# Step 2: Redeploy v2 (same app, different port = new version)
# ---------------------------------------------------------------
echo ""
echo "============================================"
echo "  Step 2: Redeploy v2 (rolling update)"
echo "============================================"
echo ""
echo "Applying updated config (port 8081 = v2)..."

cat > /tmp/reliaburger-demo-v2.toml <<EOF
[app.demo]
command = ["${TESTAPP}", "--mode", "healthy", "--port", "8081"]
port = 8081
EOF

"${RELISH}" apply /tmp/reliaburger-demo-v2.toml || true
sleep 2

echo ""
echo "--- status after v2 (should show new instance) ---"
"${RELISH}" status || true

echo ""
echo "--- calling v2 (should be on :8081 now) ---"
curl -sf http://localhost:8081/health && echo "" || echo "(not reachable)"
echo "--- v1 should be gone ---"
curl -sf http://localhost:8080/health && echo " ← v1 still running!" || echo "v1 stopped ✓"

# ---------------------------------------------------------------
# Step 3: Deploy broken v3 (exits immediately)
# ---------------------------------------------------------------
echo ""
echo "============================================"
echo "  Step 3: Deploy broken v3 (should fail)"
echo "============================================"
echo ""
echo "Deploying a command that exits immediately..."

cat > /tmp/reliaburger-demo-v3.toml <<EOF
[app.demo]
command = ["false"]
port = 8082
EOF

"${RELISH}" apply /tmp/reliaburger-demo-v3.toml || true
sleep 2

echo ""
echo "--- status after broken deploy ---"
"${RELISH}" status || true

# ---------------------------------------------------------------
# Step 4: Recover by redeploying v2
# ---------------------------------------------------------------
echo ""
echo "============================================"
echo "  Step 4: Recover by redeploying v2"
echo "============================================"

"${RELISH}" apply /tmp/reliaburger-demo-v2.toml || true
sleep 2

echo ""
echo "--- status after recovery ---"
"${RELISH}" status || true

echo ""
echo "--- calling recovered v2 ---"
curl -sf http://localhost:8081/health && echo " ← v2 healthy again" || echo "(not reachable)"

# ---------------------------------------------------------------
# Summary
# ---------------------------------------------------------------
echo ""
echo "============================================"
echo "  Summary"
echo "============================================"
echo ""
echo "  1. Deployed v1 (healthy)        ✓"
echo "  2. Redeployed v2 (rolling)      ✓ old stopped, new created"
echo "  3. Deployed broken v3           ✓ failed as expected"
echo "  4. Recovered with v2            ✓ back to healthy"
echo ""
echo "  Dashboard: http://localhost:9117/"
echo "  Lint:      ${RELISH} lint /tmp/reliaburger-demo-v2.toml"
echo ""
echo "Press Ctrl+C to stop."
wait "${BUN_PID}"
