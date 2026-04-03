#!/usr/bin/env bash
#
# Live push/pull test for the Pickle OCI registry.
#
# Builds a tiny Docker image, pushes it to a local Pickle instance,
# pulls it back, and verifies it runs. Requires Docker daemon.
#
# Usage:
#   ./scripts/pickle-push-test.sh          # starts bun automatically
#   ./scripts/pickle-push-test.sh --no-bun # assumes bun is already running

set -euo pipefail

REGISTRY="localhost:5050"
IMAGE_NAME="pickle-test"
IMAGE_TAG="v1"
FULL_REF="${REGISTRY}/${IMAGE_NAME}:${IMAGE_TAG}"
BUN_PID=""
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
START_BUN=true

if [[ "${1:-}" == "--no-bun" ]]; then
    START_BUN=false
fi

cleanup() {
    echo ""
    echo "--- cleanup ---"
    # Remove test images
    docker rmi "${FULL_REF}" 2>/dev/null || true
    docker rmi "${IMAGE_NAME}:${IMAGE_TAG}" 2>/dev/null || true
    # Kill bun if we started it
    if [[ -n "${BUN_PID}" ]]; then
        kill "${BUN_PID}" 2>/dev/null || true
        wait "${BUN_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# Check Docker is available
if ! command -v docker &>/dev/null; then
    echo "error: docker not found in PATH"
    exit 1
fi
if ! docker info &>/dev/null; then
    echo "error: Docker daemon not running"
    exit 1
fi

# Check insecure registry is configured (Docker defaults to HTTPS)
if ! docker info 2>/dev/null | grep -q "${REGISTRY}"; then
    echo ""
    echo "WARNING: ${REGISTRY} may not be configured as an insecure registry."
    echo ""
    echo "Docker defaults to HTTPS for all registries. To use Pickle over HTTP,"
    echo "add ${REGISTRY} to Docker's insecure registries:"
    echo ""
    echo "  Docker Desktop: Settings → Docker Engine → add to insecure-registries"
    echo "  Linux daemon:   /etc/docker/daemon.json"
    echo ""
    echo '  { "insecure-registries": ["localhost:5050"] }'
    echo ""
    echo "Then restart Docker and re-run this script."
    echo ""
    echo "Attempting push anyway (will fail if not configured)..."
    echo ""
fi

# Start bun if needed
if ${START_BUN}; then
    echo "--- building bun ---"
    cargo build --bin bun --manifest-path "${REPO_DIR}/Cargo.toml" --quiet

    echo "--- starting bun ---"
    "${REPO_DIR}/target/debug/bun" &
    BUN_PID=$!

    # Wait for bun API to be ready
    for i in $(seq 1 30); do
        if curl -sf http://localhost:9117/v1/health >/dev/null 2>&1; then
            break
        fi
        if [[ $i -eq 30 ]]; then
            echo "error: bun did not start within 30 seconds"
            exit 1
        fi
        sleep 1
    done
    echo "bun is ready (pid ${BUN_PID})"
fi

# Verify Pickle registry is up
echo ""
echo "--- checking Pickle registry ---"
curl -sf "http://${REGISTRY}/v2/" || {
    echo "error: Pickle registry not responding at ${REGISTRY}"
    exit 1
}
echo "Pickle registry is up"

# Build the test image
echo ""
echo "--- building test image ---"
docker build -t "${IMAGE_NAME}:${IMAGE_TAG}" -f "${SCRIPT_DIR}/Dockerfile.pickle-test" "${SCRIPT_DIR}"

# Tag for the local registry
docker tag "${IMAGE_NAME}:${IMAGE_TAG}" "${FULL_REF}"

# Push to Pickle
echo ""
echo "--- pushing to Pickle ---"
docker push "${FULL_REF}"

# Check the tag list
echo ""
echo "--- checking tag list ---"
curl -sf "http://${REGISTRY}/v2/${IMAGE_NAME}/tags/list" | python3 -m json.tool 2>/dev/null || \
    curl -sf "http://${REGISTRY}/v2/${IMAGE_NAME}/tags/list"

# Remove local copies
echo ""
echo "--- removing local copies ---"
docker rmi "${FULL_REF}"
docker rmi "${IMAGE_NAME}:${IMAGE_TAG}"

# Pull back from Pickle
echo ""
echo "--- pulling from Pickle ---"
docker pull "${FULL_REF}"

# Run the pulled image
echo ""
echo "--- running pulled image ---"
OUTPUT=$(docker run --rm "${FULL_REF}")
if [[ "${OUTPUT}" == "hello from pickle" ]]; then
    echo "SUCCESS: image round-tripped through Pickle correctly"
    echo "  output: ${OUTPUT}"
else
    echo "FAILURE: unexpected output: ${OUTPUT}"
    exit 1
fi

echo ""
echo "=== All Pickle push/pull tests passed ==="
