#!/usr/bin/env bash
#
# Rolling deploys, rollback and automatic rollback, on a one-node cluster.
#
# Applies examples/phase-1/proc-rollout.toml as v1, rolls it to v2, rolls it
# back with `relish rollback`, then applies a v3 that fails its health check.
# Bun restores the previous version by itself and the cluster marks v3
# RolloutFailed; applying v3 again doesn't retry it, and a changed spec
# clears the mark. Each step checks what the app really serves and fails the
# demo if it's wrong.
#
# Usage: scripts/demo/rollout.sh
# Environment: see scripts/demo/lib.sh (RELISH, BUN, TESTAPP, DEMO_PORT_BASE).
# tests/demos.rs runs it under `make test-cluster`.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "$0")" && pwd)"
. "${DEMO_DIR}/lib.sh"

EXAMPLE="examples/phase-1/proc-rollout.toml"

demo_start

# The shipped example, pointed at this build's testapp and the demo's port,
# with VERSION set to $1. A second argument "broken" makes it fail every
# health check.
version_of_example() {
    local version="$1" mode="${2:-}" file="${DEMO_WORK}/web-$1.toml"
    sed -e "s|\"target/debug/testapp\"|\"${TESTAPP}\"|" \
        -e "s|8080|${DEMO_APP_PORT}|g" \
        -e "s|VERSION = \"v1\"|VERSION = \"${version}\"|" \
        "${REPO_DIR}/${EXAMPLE}" >"${file}"
    if [[ "${mode}" == broken ]]; then
        sed -e 's|"--mode", "healthy"|"--mode", "unhealthy-after", "--count", "0"|' \
            "${file}" >"${file}.broken"
        mv "${file}.broken" "${file}"
    fi
    echo "${file}"
}

status_shows_rollout_failed() {
    "${RELISH}" status | grep -q "RolloutFailed"
}

status_is_clear() {
    ! "${RELISH}" status | grep -q "RolloutFailed"
}

V1="$(version_of_example v1)"
V2="$(version_of_example v2)"
V3="$(version_of_example v3 broken)"

demo_step "Deploy v1 (${EXAMPLE})"
demo_run "${RELISH}" apply "${V1}"
demo_wait_for "web to serve v1" 60 demo_serves "${DEMO_APP_PORT}" v1
echo "serving $(demo_serving "${DEMO_APP_PORT}")"

demo_step "Roll out v2: a new instance has to pass its health check"
demo_run "${RELISH}" apply "${V2}"
demo_wait_for "web to serve v2" 60 demo_serves "${DEMO_APP_PORT}" v2
echo "serving $(demo_serving "${DEMO_APP_PORT}")"
demo_run "${RELISH}" history web

demo_step "Roll back: the council remembers the last version that rolled out healthy"
demo_run "${RELISH}" rollback web
demo_wait_for "web to serve v1 again" 60 demo_serves "${DEMO_APP_PORT}" v1
echo "serving $(demo_serving "${DEMO_APP_PORT}") after the rollback"

demo_step "Deploy a broken v3: it never passes its health check"
demo_run "${RELISH}" apply "${V3}"
demo_wait_for "the cluster to mark v3 RolloutFailed" 90 status_shows_rollout_failed
demo_run "${RELISH}" status
if ! demo_serves "${DEMO_APP_PORT}" v1; then
    demo_fail "expected v1 after the failed v3, got '$(demo_serving "${DEMO_APP_PORT}")'"
fi
echo "serving $(demo_serving "${DEMO_APP_PORT}") after the bad v3: auto_rollback restored it"

demo_step "Apply v3 again: the same spec isn't retried, only reported"
demo_run "${RELISH}" apply "${V3}"
status_shows_rollout_failed || demo_fail "re-applying v3 cleared RolloutFailed"

demo_step "Fix it: any change to the spec clears the mark and rolls again"
demo_run "${RELISH}" apply "${V2}"
demo_wait_for "web to serve v2" 60 demo_serves "${DEMO_APP_PORT}" v2
demo_wait_for "RolloutFailed to clear" 30 status_is_clear
echo "serving $(demo_serving "${DEMO_APP_PORT}")"
demo_run "${RELISH}" history web

echo ""
echo "rollout demo complete"
