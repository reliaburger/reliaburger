#!/usr/bin/env bash
#
# Deploy demo: shows the full deploy lifecycle.
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
}
trap cleanup EXIT

echo "=== Reliaburger Deploy Demo ==="
echo ""

# Build
echo "--- building ---"
cargo build --bin bun --bin relish --manifest-path "${REPO_DIR}/Cargo.toml" --quiet

RELISH="${REPO_DIR}/target/debug/relish"
BUN="${REPO_DIR}/target/debug/bun"

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

# Deploy v1
echo ""
echo "--- deploying v1 ---"
cat > /tmp/reliaburger-demo-app.toml <<EOF
[app.demo]
command = ["${REPO_DIR}/target/debug/testapp", "--mode", "healthy", "--port", "8080"]
port = 8080
EOF

"${RELISH}" apply /tmp/reliaburger-demo-app.toml || true
sleep 2

echo ""
echo "--- status after v1 deploy ---"
"${RELISH}" status || true

# Lint the config
echo ""
echo "--- lint config ---"
"${RELISH}" lint /tmp/reliaburger-demo-app.toml || true

# Show deploy history
echo ""
echo "--- deploy history ---"
curl -sf http://localhost:9117/v1/deploys/history/demo | python3 -m json.tool 2>/dev/null || \
    curl -sf http://localhost:9117/v1/deploys/history/demo

# Show active deploys
echo ""
echo "--- active deploys ---"
curl -sf http://localhost:9117/v1/deploys/active | python3 -m json.tool 2>/dev/null || \
    curl -sf http://localhost:9117/v1/deploys/active

echo ""
echo "=== Dashboard: http://localhost:9117/ ==="
echo ""
echo "Available commands:"
echo "  relish deploy <config.toml>   # trigger rolling deploy"
echo "  relish history <app>          # show deploy history"
echo "  relish rollback <app>         # revert to previous"
echo "  relish lint <config.toml>     # validate config"
echo "  relish top                    # resource usage"
echo ""
echo "Press Ctrl+C to stop."
wait "${BUN_PID}"
