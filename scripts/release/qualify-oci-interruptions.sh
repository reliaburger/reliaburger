#!/usr/bin/env bash
# Run on a disposable Linux host with sudo, runc, ip, nft, cc and static BusyBox.
set -euo pipefail
cd "$(dirname "$0")/../.."
export CARGO_INCREMENTAL=0
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1}
evidence=$(mktemp -d /var/tmp/reliaburger-oci-interruptions.XXXXXX)
printf 'OCI interruption evidence: %s\n' "$evidence"
cargo test --features ebpf --test owned_runc --test owned_network --test oci_crash \
    --no-run --message-format=json > "$evidence/artifacts.json"
python3 - "$evidence/artifacts.json" > "$evidence/binaries" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    row = json.loads(line)
    if row.get('reason') == 'compiler-artifact' and row.get('target', {}).get('name') in {'owned_runc', 'owned_network', 'oci_crash'} and row.get('executable'):
        print(row['executable'])
PY
while IFS= read -r binary; do
    name=$(basename "$binary")
    sha256sum "$binary" >> "$evidence/binaries.sha256"
    # Scope links, routing/firewall changes and namespace mount points to the
    # fixture. A failed test must not pollute the host's /run/netns directory.
    # The reboot fixtures panic without their power-cycling drivers
    # (qualify-oci-reboot.sh, qualify-discovery-reboot.sh), so skip them here.
    timeout 420s sudo unshare --mount --net --propagation private bash -c '
        set -eu
        mkdir -p /run/netns
        mount -t tmpfs tmpfs /run/netns
        ip link set lo up
        exec "$1" --ignored --nocapture --test-threads=1 --skip normal_rootless_bun \
            --skip actual_host_reboot --skip actual_bun_kernel_discovery_host_reboot
    ' qualification "$binary" 2>&1 | tee "$evidence/$name.log"
done < "$evidence/binaries"
printf 'PASS: OCI interruptions; evidence retained at %s\n' "$evidence"
