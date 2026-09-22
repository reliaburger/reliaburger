#!/usr/bin/env bash
# Power-cycle only an explicitly selected disposable Lima VM.
set -euo pipefail
if [[ $# != 2 || $1 != --vm ]]; then
    echo "usage: $0 --vm DISPOSABLE_LIMA_VM" >&2
    exit 2
fi
vm=$2
repository=$(cd "$(dirname "$0")/../.." && pwd)
evidence=$(mktemp -d "${TMPDIR:-/tmp}/reliaburger-discovery-reboot.XXXXXX")
directory=$(limactl shell "$vm" mktemp -d /var/tmp/reliaburger-discovery-reboot.XXXXXX)
[[ $directory == /var/tmp/reliaburger-discovery-reboot.* ]]
printf 'Power-cycling %s; evidence: %s (guest %s)\n' "$vm" "$evidence" "$directory"
printf '%s\n' "$directory" > "$evidence/guest-directory"
limactl shell "$vm" bash -s -- "$repository" "$directory" > "$evidence/prepare.log" 2>&1 <<'GUEST'
set -euo pipefail
source "$HOME/.cargo/env"
cd "$1"
directory=$2
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR="${RELIABURGER_QUALIFICATION_TARGET:-$HOME/reliaburger-release-target}"
cargo test --features ebpf --test oci_crash --no-run --message-format=json > "$directory/artifacts.json"
python3 - "$directory" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
rows = [json.loads(line) for line in (root / 'artifacts.json').open()]
for name in ['oci_crash', 'bun']:
    path = next(row['executable'] for row in rows if row.get('reason') == 'compiler-artifact' and row.get('target', {}).get('name') == name and row.get('executable'))
    (root / name).write_text(path + '\n')
PY
sha256sum "$(cat "$directory/oci_crash")" "$(cat "$directory/bun")" > "$directory/binaries.sha256"
mountpoint -q /sys/fs/bpf || sudo mount -t bpf bpf /sys/fs/bpf
timeout 90s sudo unshare --mount --net --propagation private bash -c '
    set -eu
    mkdir -p /run/netns
    mount -t tmpfs tmpfs /run/netns
    ip link set lo up
    exec env RELIABURGER_DISCOVERY_REBOOT_DIRECTORY="$1" RELIABURGER_REBOOT_PHASE=prepare "$2" --ignored --exact actual_bun_kernel_discovery_host_reboot --nocapture
' qualification "$directory" "$(cat "$directory/oci_crash")"
sync
if pgrep -x cargo || pgrep -x rustc || pgrep -x cargo-nextest; then
    echo 'another build/test driver is active; refusing VM stop' >&2
    exit 1
fi
GUEST
limactl stop --force "$vm" > "$evidence/stop.log" 2>&1
limactl start "$vm" > "$evidence/start.log" 2>&1
limactl shell "$vm" bash -s -- "$directory" > "$evidence/verify.log" 2>&1 <<'GUEST'
set -euo pipefail
directory=$1
sha256sum --check "$directory/binaries.sha256"
mountpoint -q /sys/fs/bpf || sudo mount -t bpf bpf /sys/fs/bpf
timeout 90s sudo unshare --mount --net --propagation private bash -c '
    set -eu
    mkdir -p /run/netns
    mount -t tmpfs tmpfs /run/netns
    ip link set lo up
    exec env RELIABURGER_DISCOVERY_REBOOT_DIRECTORY="$1" RELIABURGER_REBOOT_PHASE=verify "$2" --ignored --exact actual_bun_kernel_discovery_host_reboot --nocapture
' qualification "$directory" "$(cat "$directory/oci_crash")"
sudo cat "$directory/verified-boot"
GUEST
cat "$evidence/verify.log"
printf '\nPASS: actual Bun/kernel/discovery reboot; evidence: %s (guest %s)\n' "$evidence" "$directory"
