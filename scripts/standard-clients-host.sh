#!/usr/bin/env bash
#
# Prepare and check a Linux host for `make test-standard-clients`.
#
# The standard-client tests push to and pull from a Pickle registry with
# real docker, podman, skopeo and crane, verifying the cluster CA the way
# users do. They need three things from the host:
#
#   1. The clients on PATH: docker (with a running daemon this user may
#      use), podman (rootless), skopeo and crane.
#   2. A registry name, pickle-clients.test, that resolves to a non-loopback
#      address of this machine. docker treats every 127.0.0.0/8 registry as
#      insecure: it would skip certificate checks and retry in plaintext,
#      hiding exactly the failure the tests look for.
#   3. /etc/docker/certs.d and /etc/containers/certs.d writable by this user,
#      so each test can install its cluster's CA for its own host:port and
#      remove it afterwards.
#
# Usage:
#   scripts/standard-clients-host.sh               # check, print versions
#   sudo scripts/standard-clients-host.sh --provision
#
# --provision edits /etc/hosts and hands the two certs.d directories to the
# invoking user (SUDO_USER). Do that only on a test machine or CI runner:
# whoever owns those directories decides which registries docker and podman
# trust.

set -euo pipefail

HOST_NAME="pickle-clients.test"
CERTS_DIRS=(/etc/docker/certs.d /etc/containers/certs.d)
CLIENTS=(docker podman skopeo crane)

primary_address() {
    # The source address of the default route: reachable by dockerd and by
    # rootless podman alike, and never loopback.
    ip -4 route get 1.1.1.1 2>/dev/null \
        | awk '{ for (i = 1; i <= NF; i++) if ($i == "src") { print $(i + 1); exit } }'
}

provision() {
    if [[ $EUID -ne 0 ]]; then
        echo "--provision needs root: sudo $0 --provision" >&2
        exit 1
    fi
    local owner="${SUDO_USER:-}"
    if [[ -z "$owner" ]]; then
        echo "--provision must run through sudo, so it knows whose tests will run" >&2
        exit 1
    fi
    local address
    address="$(primary_address)"
    if [[ -z "$address" ]]; then
        echo "no non-loopback IPv4 address with a default route" >&2
        exit 1
    fi
    # Replace any earlier mapping: a runner's address can change.
    sed -i "/[[:space:]]${HOST_NAME//./\\.}\$/d" /etc/hosts
    printf '%s %s\n' "$address" "$HOST_NAME" >> /etc/hosts
    for dir in "${CERTS_DIRS[@]}"; do
        mkdir -p "$dir"
        chown "$owner" "$dir"
    done
    echo "$HOST_NAME -> $address; ${CERTS_DIRS[*]} owned by $owner"
}

check() {
    local failed=0
    for client in "${CLIENTS[@]}"; do
        if ! command -v "$client" >/dev/null 2>&1; then
            echo "missing: $client is not on PATH" >&2
            failed=1
        fi
    done
    local resolved
    resolved="$(getent ahostsv4 "$HOST_NAME" | awk 'NR == 1 { print $1 }')"
    if [[ -z "$resolved" || "$resolved" == 127.* ]]; then
        echo "$HOST_NAME must resolve to a non-loopback address (got '${resolved}')" >&2
        failed=1
    fi
    for dir in "${CERTS_DIRS[@]}"; do
        if [[ ! -w "$dir" ]]; then
            echo "$dir must exist and be writable by $(id -un)" >&2
            failed=1
        fi
    done
    if [[ $failed -ne 0 ]]; then
        echo "prepare this host with: sudo $0 --provision" >&2
        exit 1
    fi
    # Record what the run exercised: these come from the runner image. The
    # docker call also proves this user can reach the daemon.
    local docker_version
    if ! docker_version="$(docker version --format '{{.Client.Version}} (daemon {{.Server.Version}})')"; then
        echo "docker is installed but its daemon isn't reachable by $(id -un)" >&2
        exit 1
    fi
    echo "docker $docker_version"
    podman --version
    skopeo --version
    echo "crane $(crane version)"
}

case "${1:-}" in
    --provision) provision ;;
    "") check ;;
    *)
        echo "usage: $0 [--provision]" >&2
        exit 2
        ;;
esac
