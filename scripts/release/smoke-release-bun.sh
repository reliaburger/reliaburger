#!/usr/bin/env bash
# Start a release `bun` and run one app on it, before the build uploads it.
#
# build.yml's build-binaries job runs this on each Linux target, against the
# `bun` and `relish` it has just built with `--release` and the release
# features (#2486). Until then the only thing that ran the release pair was
# `--version`, and nothing ran the x86-64 one at all: Apple silicon's staged
# installs and soaks run the aarch64 bun in their VMs, and CI tests debug
# builds made on a newer Ubuntu. A release build that can't start a cluster
# (a glibc symbol the runner's Ubuntu lacks, a broken state directory, a
# panic on the start-up path) fails here instead of on a user's server.
#
# It is the feature demos' one-node cluster (scripts/demo/lib.sh): `relish
# init`, then Bun with `--cluster` on the process runtime, mTLS and an admin
# token, in a temporary directory removed on exit. It applies one `exec` app
# that sleeps, waits for its instance to be running, then deletes it. No
# container runtime, root or eBPF: the eBPF object and runc are the
# privileged Linux job's to test.
#
# Usage:
#   scripts/release/smoke-release-bun.sh BUN RELISH
#
# Environment: DEMO_PORT_BASE (default 29200) and DEMO_KEEP=1, as in
# scripts/demo/lib.sh.
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 BUN RELISH" >&2
    exit 2
fi
BUN="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
RELISH="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
for binary in "${BUN}" "${RELISH}"; do
    [[ -x "${binary}" ]] || { echo "smoke-release-bun: ${binary} isn't an executable" >&2; exit 2; }
done
# The app runs `sleep`; lib.sh puts TESTAPP in the node's allowed_binaries.
TESTAPP="$(command -v sleep)"
export BUN RELISH TESTAPP
DEMO_RESOURCES=0

DEMO_DIR="$(cd "$(dirname "$0")/../demo" && pwd)"
. "${DEMO_DIR}/lib.sh"

demo_start

APP="${DEMO_WORK}/smoke.toml"
cat >"${APP}" <<EOF
[app.smoke]
exec = "${TESTAPP}"
command = ["600"]
EOF

smoke_is_running() {
    "${RELISH}" apps smoke | awk '$3 == "smoke" && $5 == "running" { found = 1 } END { exit !found }'
}

demo_step "Apply one app to the release bun"
demo_run "${RELISH}" apply "${APP}"
demo_wait_for "smoke to be running" 60 smoke_is_running
demo_run "${RELISH}" apps smoke

demo_step "Delete it"
demo_run "${RELISH}" delete smoke
demo_wait_for "smoke to be gone" 60 bash -c "! '${RELISH}' apps smoke | grep -q running"
echo "the release bun started a cluster and ran an app"
