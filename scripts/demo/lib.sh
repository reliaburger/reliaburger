# Shared by the feature demos in scripts/demo/ (rollout.sh, gitops.sh).
#
# A demo sources this file, calls demo_start once, and drives the node it
# started with `relish`. demo_start and demo_stop are the one place a demo's
# cluster starts and stops: anything that should happen around every demo
# (showing its resource use, say) goes there.
#
# The node is a real one-node cluster, set up the way the manual says:
# `relish init` writes its identity, CA and keys, and Bun runs it with
# `--cluster` on the process runtime, so no container runtime is needed and
# the demo works on macOS and Linux alike. Everything it writes goes into a
# fresh temporary directory, removed on exit.
#
# Environment:
#   RELISH, BUN, TESTAPP  binaries to use (default: build this checkout with
#                         cargo and use target/debug)
#   DEMO_PORT_BASE        first of seven local ports the demo binds: the
#                         API, gossip, Raft, reporting, and three for the
#                         demo's apps (default 29200)
#   DEMO_KEEP=1           keep the temporary directory, Bun's log included
#   DEMO_RUNTIME          the node's runtime: process (default), or runc as root
#                         on Linux to try the container examples
#
# The scripts need bash 3.2 (macOS's /bin/bash).

REPO_DIR="$(cd "${DEMO_DIR}/../.." && pwd)"
DEMO_PORT_BASE="${DEMO_PORT_BASE:-29200}"
DEMO_RUNTIME="${DEMO_RUNTIME:-process}"
DEMO_API_PORT="${DEMO_PORT_BASE}"
# Ports for the demo's own apps to listen on.
DEMO_APP_PORT=$((DEMO_PORT_BASE + 4))
DEMO_SECOND_APP_PORT=$((DEMO_PORT_BASE + 5))
DEMO_THIRD_APP_PORT=$((DEMO_PORT_BASE + 6))
DEMO_WORK=""
DEMO_BUN_PID=""

# A section heading, so a reader can follow along.
demo_step() {
    echo ""
    echo "==> $*"
}

# Show a command, then run it. A failing command fails the demo. The binary
# shows as `relish` and files in the demo's directory by their own names,
# the way you'd type them.
demo_run() {
    local shown="$*"
    shown="${shown//${RELISH}/relish}"
    shown="${shown//${DEMO_WORK}\//}"
    echo "\$ ${shown}"
    "$@"
}

# Fail the demo with a reason, Bun's last log lines and the cluster status.
demo_fail() {
    echo "demo failed: $*" >&2
    if [[ -n "${DEMO_WORK}" && -f "${DEMO_WORK}/bun.log" ]]; then
        echo "--- last lines of bun's log ---" >&2
        tail -n 40 "${DEMO_WORK}/bun.log" >&2
    fi
    exit 1
}

# demo_wait_for DESCRIPTION SECONDS COMMAND...
# Run COMMAND every half second until it succeeds; fail the demo if it
# hasn't within SECONDS, or if Bun exits first.
demo_wait_for() {
    local description="$1" seconds="$2"
    shift 2
    local deadline=$((SECONDS + seconds))
    until "$@" >/dev/null 2>&1; do
        if [[ -n "${DEMO_BUN_PID}" ]] && ! kill -0 "${DEMO_BUN_PID}" 2>/dev/null; then
            demo_fail "bun exited while waiting for ${description}"
        fi
        if ((SECONDS >= deadline)); then
            demo_fail "timed out after ${seconds}s waiting for ${description}"
        fi
        sleep 0.5
    done
    echo "    (${description}: done)"
}

# What an app listening on PORT returns for its VERSION variable (testapp's
# /env/VERSION route); empty when nothing answers.
demo_serving() {
    curl -fsS --max-time 2 "http://127.0.0.1:$1/env/VERSION" 2>/dev/null
}

# Succeeds when the app on PORT serves VERSION.
demo_serves() {
    [[ "$(demo_serving "$1")" == "$2" ]]
}

demo_binaries() {
    if [[ -n "${RELISH:-}" && -n "${BUN:-}" && -n "${TESTAPP:-}" ]]; then
        return
    fi
    demo_step "Building relish, bun and testapp"
    cargo build --locked --quiet --bin relish --bin bun --bin testapp \
        --manifest-path "${REPO_DIR}/Cargo.toml"
    local target="${CARGO_TARGET_DIR:-${REPO_DIR}/target}"
    RELISH="${RELISH:-${target}/debug/relish}"
    BUN="${BUN:-${target}/debug/bun}"
    TESTAPP="${TESTAPP:-${target}/debug/testapp}"
}

# Rewrite the node config `relish init` wrote for this laptop: local storage,
# the demo's ports, loopback addresses, and the process runtime's allowlist.
demo_node_config() {
    local generated="$1" config="$2"
    sed -e "s|^data = .*|data = \"${DEMO_WORK}/data\"|" \
        -e "s|^images = .*|images = \"${DEMO_WORK}/images\"|" \
        -e "s|^logs = .*|logs = \"${DEMO_WORK}/logs\"|" \
        -e "s|^metrics = .*|metrics = \"${DEMO_WORK}/metrics\"|" \
        -e "s|^volumes = .*|volumes = \"${DEMO_WORK}/volumes\"|" \
        -e "s|^gossip_port = .*|gossip_port = $((DEMO_PORT_BASE + 1))|" \
        -e "s|^raft_port = .*|raft_port = $((DEMO_PORT_BASE + 2))|" \
        -e "s|^reporting_port = .*|reporting_port = $((DEMO_PORT_BASE + 3))|" \
        -e "s|^registry_port = .*|registry_port = 0|" \
        -e "s|^\[network.port_range\]|[network]\\
advertise_address = \"127.0.0.1\"\\
\\
[network.port_range]|" \
        -e "s|^allowed_binaries = \[\]|allowed_binaries = [\"${TESTAPP}\"]\\
mount_isolation = false|" \
        "${generated}" >"${config}"
    grep -q "^advertise_address = \"127.0.0.1\"" "${config}" \
        || demo_fail "couldn't rewrite the node config from relish init"
}

# Start the demo's one-node cluster and point relish at it.
demo_start() {
    demo_binaries
    trap demo_stop EXIT
    DEMO_WORK="$(mktemp -d "${TMPDIR:-/tmp}/reliaburger-demo.XXXXXX")"

    demo_step "Starting a one-node cluster (${DEMO_RUNTIME} runtime) in ${DEMO_WORK}"
    "${RELISH}" init "${DEMO_WORK}/cluster" --cluster-name demo --node-id demo-node \
        >"${DEMO_WORK}/init.log" 2>&1 \
        || demo_fail "relish init failed: $(cat "${DEMO_WORK}/init.log")"
    demo_node_config "${DEMO_WORK}/cluster/reliaburger.toml" "${DEMO_WORK}/node.toml"
    "${BUN}" --config "${DEMO_WORK}/node.toml" --listen "127.0.0.1:${DEMO_API_PORT}" \
        --runtime "${DEMO_RUNTIME}" --cluster >"${DEMO_WORK}/bun.log" 2>&1 &
    DEMO_BUN_PID=$!

    export RELIABURGER_ENDPOINT="https://127.0.0.1:${DEMO_API_PORT}"
    export RELIABURGER_CA_CERT="${DEMO_WORK}/cluster/identity/root-ca.crt"
    unset RELIABURGER_TOKEN
    demo_wait_for "the council to elect its leader" 60 "${RELISH}" council status
    # The first admin token needs no token: a fresh cluster has none yet.
    RELIABURGER_TOKEN="$("${RELISH}" token create --name demo-admin --role admin \
        2>"${DEMO_WORK}/token.log")" \
        || demo_fail "couldn't create the admin token: $(cat "${DEMO_WORK}/token.log")"
    export RELIABURGER_TOKEN
    echo "    node demo-node is up; relish talks to ${RELIABURGER_ENDPOINT}"
}

# Stop the node and its workloads, and remove what the demo wrote. Runs on
# every exit, a failed step's included.
demo_stop() {
    local status=$?
    # Clean up as far as possible whatever fails here; the demo's own exit
    # status is what's reported.
    set +e
    if [[ -n "${DEMO_BUN_PID}" ]] && kill -0 "${DEMO_BUN_PID}" 2>/dev/null; then
        kill -TERM "${DEMO_BUN_PID}"
        local waited=0
        while kill -0 "${DEMO_BUN_PID}" 2>/dev/null && ((waited < 30)); do
            sleep 0.5
            waited=$((waited + 1))
        done
        if kill -0 "${DEMO_BUN_PID}" 2>/dev/null; then
            kill -KILL "${DEMO_BUN_PID}"
        fi
        wait "${DEMO_BUN_PID}" 2>/dev/null
    fi
    if [[ -n "${DEMO_WORK}" ]]; then
        if [[ "${DEMO_KEEP:-}" == 1 ]]; then
            echo "kept ${DEMO_WORK}"
        else
            rm -rf "${DEMO_WORK}"
        fi
    fi
    exit "${status}"
}
