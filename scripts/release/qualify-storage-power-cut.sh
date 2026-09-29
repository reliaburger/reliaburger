#!/usr/bin/env bash
# The Markdown record quotes values in backticks, inside single-quoted formats.
# shellcheck disable=SC2016
#
# Cut the power of an explicitly selected disposable Lima VM while storage
# workers acknowledge operations, then prove every acknowledged operation
# survived the next boot. Never run this against a development VM.
set -euo pipefail

usage() {
    echo "usage: $0 --vm DISPOSABLE_LIMA_VM --fixture exporter|leases|backups [--iterations N] [--record FILE.md]" >&2
    exit 2
}

vm=
fixture=
iterations=1
record=
while [[ $# -gt 0 ]]; do
    [[ $# -ge 2 ]] || usage
    case $1 in
        --vm) vm=$2 ;;
        --fixture) fixture=$2 ;;
        --iterations) iterations=$2 ;;
        --record) record=$2 ;;
        *) usage ;;
    esac
    shift 2
done
[[ -n $vm ]] || usage
[[ $iterations =~ ^[1-9][0-9]*$ ]] || usage
case $fixture in
    exporter) test=actual_power_cut_preserves_acknowledged_log_exports ;;
    leases) test=actual_power_cut_preserves_acknowledged_lease_operations ;;
    backups) test=actual_power_cut_preserves_acknowledged_council_backups ;;
    *) usage ;;
esac
# The shared development VM carries other people's builds and clusters.
if [[ $vm == reliaburger-test ]]; then
    echo "refusing to cut the power of the development VM $vm; create a disposable one" >&2
    exit 2
fi

repository=$(cd "$(dirname "$0")/../.." && pwd)
evidence=$(mktemp -d "${TMPDIR:-/tmp}/reliaburger-power-cut.XXXXXX")
record=${record:-$evidence/record.md}
printf 'Power-cut qualification (%s, %s iterations) will force-stop VM %s; evidence: %s\n' \
    "$fixture" "$iterations" "$vm" "$evidence"

# Build once, and keep a private copy of the binary: workers re-execute it,
# and a later build in the shared target directory must not replace it.
build=$(limactl shell "$vm" mktemp -d /var/tmp/reliaburger-power-cut-build.XXXXXX)
[[ $build == /var/tmp/reliaburger-power-cut-build.* ]]
limactl shell "$vm" bash -s -- "$repository" "$build" > "$evidence/build.log" 2>&1 <<'GUEST'
set -euo pipefail
source "$HOME/.cargo/env"
cd "$1"
build=$2
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_TARGET_DIR="${RELIABURGER_QUALIFICATION_TARGET:-$HOME/reliaburger-release-target}"
cargo test --test power_cut --no-run --message-format=json > "$build/artifacts.json"
binary=$(python3 - "$build/artifacts.json" <<'PY'
import json, sys
rows = [json.loads(line) for line in open(sys.argv[1])]
print(next(row['executable'] for row in rows if row.get('reason') == 'compiler-artifact' and row.get('target', {}).get('name') == 'power_cut' and row.get('executable')))
PY
)
install -m 0755 "$binary" "$build/power_cut"
sha256sum "$build/power_cut" > "$build/power_cut.sha256"
uname -r > "$build/kernel"
# The first power cut must not take the binary or its checksum with it. No
# worker is running yet, so a global sync weakens nothing under test.
sync
GUEST
binary_sha=$(limactl shell "$vm" cut -d' ' -f1 "$build/power_cut.sha256")
commit=$(git -C "$repository" rev-parse HEAD)
kernel=$(limactl shell "$vm" cat "$build/kernel")

{
    printf '# Storage power-cut qualification: %s\n\n' "$fixture"
    printf -- '- VM: `%s` (guest kernel %s)\n' "$vm" "$kernel"
    printf -- '- Test: `power_cut::%s`\n' "$test"
    printf -- '- Commit: `%s`; binary SHA-256 `%s`\n' "$commit" "$binary_sha"
    printf -- '- Started: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf -- '- Host evidence: `%s`\n\n' "$evidence"
    printf '| # | Cut after (s) | Guest directory | Result | Verified summary |\n'
    printf '|---|---|---|---|---|\n'
} > "$record"

# One prepare → power cut → verify cycle. Returns non-zero on any failure;
# errexit does not apply inside a function called as a condition, so every
# step checks its own status.
iteration() {
    local number=$1 log=$evidence/$1 directory delay
    mkdir -p "$log" || return 1
    directory=$(limactl shell "$vm" mktemp -d /var/tmp/reliaburger-power-cut.XXXXXX) || return 1
    [[ $directory == /var/tmp/reliaburger-power-cut.* ]] || return 1
    printf '%s\n' "$directory" > "$log/guest-directory"
    limactl shell "$vm" bash -s -- "$build" "$directory" "$test" > "$log/prepare.log" 2>&1 <<'GUEST' || return 1
set -euo pipefail
build=$1
directory=$2
sha256sum --quiet --check "$build/power_cut.sha256"
timeout 120s env RELIABURGER_POWER_CUT_DIRECTORY="$directory" RELIABURGER_REBOOT_PHASE=prepare \
    "$build/power_cut" --ignored --exact "$3" --nocapture --test-threads=1
cd "$directory"
for pid in *.pid; do
    if ! kill -0 "$(cat "$pid")"; then
        echo "worker ${pid%.pid} died during prepare" >&2
        tail -n 20 "${pid%.pid}.log" >&2
        exit 1
    fi
done
# Never interrupt somebody else's build to obtain power-cut evidence.
if pgrep -x cargo || pgrep -x rustc || pgrep -x cargo-nextest; then
    echo 'another build/test driver is active; refusing VM stop' >&2
    exit 1
fi
GUEST
    delay=$((RANDOM % 21))
    printf '%s\n' "$delay" > "$log/cut-after-seconds"
    sleep "$delay"
    # An abrupt whole-kernel loss while the workers are mid-write, not a
    # graceful shutdown.
    limactl stop --force "$vm" > "$log/stop.log" 2>&1 || return 1
    if ! limactl start "$vm" > "$log/start.log" 2>&1; then
        # Lima's VZ driver sometimes refuses the disk attachment straight after
        # a forced stop ("storage device attachment is invalid"). The guest's
        # state is untouched, so one delayed retry keeps the iteration.
        sleep 10
        limactl start "$vm" >> "$log/start.log" 2>&1 || return 1
        echo retried > "$log/start-retried"
    fi
    limactl shell "$vm" bash -s -- "$build" "$directory" "$test" > "$log/verify.log" 2>&1 <<'GUEST' || return 1
set -euo pipefail
build=$1
directory=$2
sha256sum --quiet --check "$build/power_cut.sha256"
timeout 300s env RELIABURGER_POWER_CUT_DIRECTORY="$directory" RELIABURGER_REBOOT_PHASE=verify \
    "$build/power_cut" --ignored --exact "$3" --nocapture --test-threads=1
printf 'verified boot: %s\n' "$(cat "$directory/verified-boot")"
GUEST
    limactl shell "$vm" cat "$directory/summary.json" > "$log/summary.json" || return 1
    local result=PASS
    [[ ! -f $log/start-retried ]] || result='PASS (VM start retried)'
    printf '| %s | %s | `%s` | %s | `%s` |\n' "$number" "$delay" "$directory" "$result" \
        "$(tr -d ' \n' < "$log/summary.json")" >> "$record"
}

passed=0
for ((number = 1; number <= iterations; number++)); do
    if iteration "$number"; then
        passed=$((passed + 1))
        printf 'iteration %s/%s: PASS\n' "$number" "$iterations"
        continue
    fi
    printf '| %s | %s | `%s` | FAIL | see `%s/%s` |\n' "$number" \
        "$(cat "$evidence/$number/cut-after-seconds" 2>/dev/null || echo -)" \
        "$(cat "$evidence/$number/guest-directory" 2>/dev/null || echo -)" \
        "$evidence" "$number" >> "$record"
    printf '\n%s of %s iterations passed before the first failure.\n' "$passed" "$iterations" >> "$record"
    printf 'FAIL: iteration %s; evidence retained at %s\n' "$number" "$evidence" >&2
    exit 1
done
{
    printf '\n%s of %s iterations passed. ' "$passed" "$iterations"
    # Rule of three: no failure in n trials bounds the true rate below 3/n at 95 %.
    printf 'Rule-of-three bound: failure rate < %s%% at 95%% confidence.\n' \
        "$(awk -v n="$iterations" 'BEGIN { b = 300 / n; if (b > 100) b = 100; printf "%.1f", b }')"
} >> "$record"
printf 'PASS: %s power cuts; record %s\n' "$iterations" "$record"
