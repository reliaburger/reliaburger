#!/usr/bin/env bash
#
# Observability demo: starts bun, waits for metrics collection,
# then queries metrics, alerts, and shows the dashboard URL.
#
# Usage:
#   ./scripts/observability-demo.sh

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

echo "=== Reliaburger Observability Demo ==="
echo ""

# Build
echo "--- building bun ---"
cargo build --bin bun --manifest-path "${REPO_DIR}/Cargo.toml" --quiet

# Start
echo "--- starting bun ---"
"${REPO_DIR}/target/debug/bun" &
BUN_PID=$!

# Wait for health
for i in $(seq 1 15); do
    if curl -sf http://localhost:9117/v1/health >/dev/null 2>&1; then
        break
    fi
    if [[ $i -eq 15 ]]; then
        echo "error: bun did not start"
        exit 1
    fi
    sleep 1
done
echo "bun is ready (pid ${BUN_PID})"

# Wait for metrics collection (2 collection cycles = ~20s)
echo ""
echo "--- waiting for metrics collection (20s) ---"
sleep 20

# Query metrics keys
echo ""
echo "--- metric names ---"
curl -sf http://localhost:9117/v1/metrics/keys | python3 -m json.tool 2>/dev/null || \
    curl -sf http://localhost:9117/v1/metrics/keys

# Query metrics summary
echo ""
echo "--- metrics summary ---"
curl -sf http://localhost:9117/v1/metrics/summary | python3 -m json.tool 2>/dev/null || \
    curl -sf http://localhost:9117/v1/metrics/summary

# Query specific metric
echo ""
echo "--- CPU usage data ---"
curl -sf "http://localhost:9117/v1/metrics?name=node_cpu_usage_percent" | python3 -m json.tool 2>/dev/null || \
    curl -sf "http://localhost:9117/v1/metrics?name=node_cpu_usage_percent"

# Query alerts
echo ""
echo "--- alert status ---"
curl -sf http://localhost:9117/v1/alerts | python3 -m json.tool 2>/dev/null || \
    curl -sf http://localhost:9117/v1/alerts

echo ""
echo "=== Dashboard available at: http://localhost:9117/ ==="
echo ""
echo "Press Ctrl+C to stop."
wait "${BUN_PID}"
