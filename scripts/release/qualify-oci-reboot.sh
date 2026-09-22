#!/usr/bin/env bash
# Explicitly power-cycle a disposable Lima VM; never run against a development VM.
set -euo pipefail
if [[ $# != 2 || $1 != --vm ]]; then
    echo "usage: $0 --vm DISPOSABLE_LIMA_VM" >&2
    exit 2
fi
vm=$2
repository=$(cd "$(dirname "$0")/../.." && pwd)
evidence=$(mktemp -d "${TMPDIR:-/tmp}/reliaburger-oci-reboot.XXXXXX")
printf 'Reboot qualification will force-stop VM %s; evidence: %s\n' "$vm" "$evidence"
# Keep durable fixtures off /tmp: some guests clear that directory on boot.
directory=$(limactl shell "$vm" mktemp -d /var/tmp/reliaburger-reboot.XXXXXX)
[[ $directory == /var/tmp/reliaburger-reboot.* ]]
printf '%s\n' "$directory" > "$evidence/guest-directory"
limactl shell "$vm" bash -s -- "$repository" "$directory" > "$evidence/prepare.log" 2>&1 <<'GUEST'
set -euo pipefail
source "$HOME/.cargo/env"
cd "$1"
directory=$2
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR="${RELIABURGER_QUALIFICATION_TARGET:-$HOME/reliaburger-release-target}"
cargo test --features ebpf --test owned_runc --no-run --message-format=json > "$directory/artifacts.json"
binary=$(python3 - "$directory/artifacts.json" <<'PY'
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1])]
print(next(row['executable'] for row in rows if row.get('reason') == 'compiler-artifact' and row.get('target', {}).get('name') == 'owned_runc' and row.get('executable')))
PY
)
printf '%s\n' "$binary" > "$directory/binary"
sha256sum "$binary" > "$directory/binary.sha256"
timeout 90s sudo env RELIABURGER_REBOOT_DIRECTORY="$directory" RELIABURGER_REBOOT_PHASE=prepare "$binary" --ignored --exact actual_host_reboot_preserves_holds_and_retires_original_execution --nocapture
sync
# Never interrupt somebody else's build to obtain reboot evidence.
if pgrep -x cargo || pgrep -x rustc || pgrep -x cargo-nextest; then
    echo 'another build/test driver is active; refusing VM stop' >&2
    exit 1
fi
GUEST
# This is an abrupt whole-kernel loss, not graceful application shutdown.
limactl stop --force "$vm" > "$evidence/stop.log" 2>&1
limactl start "$vm" > "$evidence/start.log" 2>&1
limactl shell "$vm" bash -s -- "$directory" > "$evidence/verify.log" 2>&1 <<'GUEST'
set -euo pipefail
directory=$1
sha256sum --check "$directory/binary.sha256"
binary=$(cat "$directory/binary")
timeout 90s sudo env RELIABURGER_REBOOT_DIRECTORY="$directory" RELIABURGER_REBOOT_PHASE=verify "$binary" --ignored --exact actual_host_reboot_preserves_holds_and_retires_original_execution --nocapture
sudo cat "$directory/proof.json"
printf '\nverified boot: '
sudo cat "$directory/verified-boot"
GUEST
cat "$evidence/verify.log"
printf '\nPASS: actual VM power-cycle; evidence retained at %s (guest %s)\n' "$evidence" "$directory"
