#!/usr/bin/env bash
# shellcheck disable=SC2016  # single-quoted lane commands expand when the lane runs
# V02 loop lanes on one Mac (#287): the lanes .github/workflows/v02-loops.yml
# runs on GitHub's hosted runners, run on a Mac Studio instead. macOS lanes run
# natively; arm64 Linux lanes run in Lima. There is no x86-64 host, so those
# lanes are recorded as skipped (v02-loop-lanes-local.json), not missing.
#
# It builds one nextest archive per platform from HEAD (the same --test list as
# the workflow's build job), then runs each lane from a fresh copy of the
# source in its own directory, extracted and remapped there as the workflow
# does in its checkout, so every lane has its own target/nextest/ci/junit.xml.
# Upgrade lanes go through the Makefile's targets; the rest use the workflow's
# filters and options, word for word.
#
# Each host (the Mac, each VM) has three queues that run side by side with
# --parallel: upgrade-heavy lanes, cluster-heavy lanes, and the rest. A queue
# runs its lanes one at a time, counted ones first, so no host ever runs two
# upgrade-heavy or two cluster-heavy lanes at once (the nextest groups only
# serialise inside one nextest process, and tests/self_upgrade.rs binds fixed
# ports). Linux lanes are dealt round-robin within each group across the
# --vm list.
#
# That makes the Mac's upgrade queue two time-boxed lanes back to back, so
# --duration 5h30m (the workflow's) takes at least 11 h of wall time. --window
# fits the run into a wall-clock window instead: each time-boxed lane gets an
# equal share of what its queue has left when it starts. --plan prints the
# queues and the estimate.
#
# It refuses to start when power_cut/quiet.py says a release gate holds the
# host (exit 75), and holds the quiet lock while it runs, so the power-cut
# endurance runs stay paused. Evidence: per-lane loop.log, junit.xml,
# summary.md and lane.sh under DIR/lanes/, the builds' logs, environment.txt,
# schedule.log and v02-loops-summary.md (loop_summary.py --combine).
#
# Usage:
#   scripts/release/qualify-v02-loops-local.sh [options]
#
#   --parallel        run every host's queues side by side (without it, one
#                     lane at a time across every host)
#   --duration D      how long each time-boxed lane loops, like 5h30m, 90m or
#                     600s (default 5h30m)
#   --window W        instead of --duration: finish within W of the start,
#                     builds included
#   --count N         iterations for each counted lane (default 200)
#   --vm NAME         a running Lima VM for the arm64 Linux lanes (default
#                     reliaburger-test); repeat it to spread them over more.
#                     With --parallel it must not forward the test ports to
#                     the Mac: lima_forwards.py prints the rule to add
#   --manifest FILE   the lanes the record must hold (default
#                     scripts/release/v02-loop-lanes-local.json)
#   --evidence DIR    evidence directory (default
#                     /var/tmp/reliaburger-v02-loops-SHORTSHA); its builds are
#                     reused when they are of the same commit
#   --build-only      build the archives and stop
#   --plan            print the queues and the estimate, touching nothing
set -euo pipefail

usage() { sed -n '3,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
fail() { printf 'v02-loops-local: %s\n' "$*" >&2; exit 1; }
say() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*"; }
now() { date +%s; }
q() { printf '%q' "$1"; }

repository=$(cd "$(dirname "$0")/../.." && pwd)
here=$repository/scripts/release

# The workflow's build job archives exactly these test binaries.
archive_tests=(--test self_upgrade --test self_upgrade_cluster --test council_disaster_recovery --test cluster_failover --test registry_recovery --test suite --test test_storage)
# The workflow's lanes, per platform. macOS runs no council lane and no Btrfs
# lane, as on GitHub; the council lanes run on arm64 Linux here, since the
# workflow's x86-64 runner isn't available.
macos_lanes="test-upgrade-cluster test-upgrade-node registry-lease-expiry registry-new-leader export-lock-checkpoint"
linux_lanes="test-upgrade-cluster test-upgrade-node council_disaster_recovery cluster_failover registry-lease-expiry registry-new-leader export-lock-checkpoint test-storage-owner-death"
macos_platform=macos-studio
linux_platform=linux-arm64-lima

lane_group() {
    case $1 in
        test-upgrade-*) echo upgrade ;;
        council_disaster_recovery|cluster_failover|registry-*) echo cluster ;;
        *) echo other ;;
    esac
}

lane_kind() {
    case $1 in
        test-upgrade-*|council_disaster_recovery|cluster_failover) echo time ;;
        *) echo count ;;
    esac
}

# The counted lanes' nextest filters, as v02-loops.yml's leases matrix has them.
lane_filter() {
    case $1 in
        registry-lease-expiry) echo 'binary(registry_recovery) & test(=expired_registry_lease_recovers_after_bun_sigkill_and_preserves_shared_content)' ;;
        registry-new-leader) echo 'binary(registry_recovery) & test(=new_leader_retains_registry_cleanup_until_the_killed_writer_returns)' ;;
        export-lock-checkpoint) echo 'binary(suite) & (test(=log_export::active_export_lock_refuses_another_writer_without_uploading) | test(=log_export::export_persists_and_reloads_the_authoritative_checkpoint))' ;;
        test-storage-owner-death) echo 'binary(test_storage) & test(=lease_loop_storage_survives_owner_process_death)' ;;
        *) fail "no filter for lane $1" ;;
    esac
}

# 5h30m, 90m, 600s or 2h → seconds.
to_seconds() {
    local text=$1 hours minutes secs
    [[ -n $text && $text =~ ^([0-9]+h)?([0-9]+m)?([0-9]+s)?$ ]] \
        || fail "a duration looks like 5h30m, 90m or 600s (not $text)"
    hours=${BASH_REMATCH[1]%h}
    minutes=${BASH_REMATCH[2]%m}
    secs=${BASH_REMATCH[3]%s}
    echo $(( 10#${hours:-0} * 3600 + 10#${minutes:-0} * 60 + 10#${secs:-0} ))
}
hm() { printf '%dh%02dm' $(( $1 / 3600 )) $(( $1 % 3600 / 60 )); }

# with_timeout SECONDS COMMAND...: as in qualify-power-cut-endurance.sh. limactl
# is a Go program that ignores SIGALRM, so a `limactl shell` to a guest that
# went away would hang; the command gets its own process group and dies whole.
with_timeout() {
    perl -e '
        my $seconds = shift;
        my $pid = fork() // die "fork: $!\n";
        if ($pid == 0) { setpgrp(0, 0); exec(@ARGV) or die "exec: $!\n"; }
        $SIG{ALRM} = sub { kill("KILL", -$pid); waitpid($pid, 0); exit(124); };
        alarm($seconds);
        waitpid($pid, 0);
        exit($? & 127 ? 128 + ($? & 127) : $? >> 8);' "$@" < /dev/null
}
gsh() { local vm=$1; shift; with_timeout "${GSH_TIMEOUT:-120}" limactl shell --workdir / "$vm" "$@"; }

parallel=false
duration=5h30m
window=
count=200
vms=()
manifest=$here/v02-loop-lanes-local.json
evidence=
build_only=false
plan=false
while [ "$#" -gt 0 ]; do
    case $1 in
        --parallel) parallel=true; shift ;;
        --duration) duration=${2:-}; shift 2 ;;
        --window) window=${2:-}; shift 2 ;;
        --count) count=${2:-}; shift 2 ;;
        --vm) vms+=("${2:-}"); shift 2 ;;
        --manifest) manifest=${2:-}; shift 2 ;;
        --evidence) evidence=${2:-}; shift 2 ;;
        --build-only) build_only=true; shift ;;
        --plan|--dry-run) plan=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown option $1" ;;
    esac
done
[ "${#vms[@]}" -gt 0 ] || vms=(reliaburger-test)
[[ $count =~ ^[1-9][0-9]*$ ]] || fail "--count is a positive number (not $count)"
duration_seconds=$(to_seconds "$duration")
window_seconds=
[ -z "$window" ] || window_seconds=$(to_seconds "$window")
[ -f "$manifest" ] || fail "no lane manifest at $manifest"

sha=$(git -C "$repository" rev-parse HEAD)
short=${sha:0:12}
evidence=${evidence:-/var/tmp/reliaburger-v02-loops-$short}
case $evidence in *[[:space:]]*) fail "the evidence path can't hold spaces (make splits NEXTEST on them)" ;; esac

# A time-boxed loop stops starting iterations when its time is up and finishes
# the one it is in: a cluster upgrade iteration took about 9.5 minutes on
# GitHub. The window keeps this much back for that and the summaries, and
# never gives a lane less than the minimum.
window_margin=900
minimum_share=900
# A guess at the two builds, side by side from a cold target directory, only
# for --plan's estimate; --window measures the real time.
build_guess=1800

# One queue per line: "LABEL ITEM...", ITEM being LANE@macos or LANE@vm:NAME.
plan_queues() {
    local group kind lane position index vm queue
    local groups="upgrade cluster other"
    if [ "$parallel" = false ]; then
        queue=serial
        for kind in count time; do
            for lane in $macos_lanes; do
                [ "$(lane_kind "$lane")" = "$kind" ] && queue="$queue $lane@macos"
            done
            for group in $groups; do
                position=0
                for lane in $linux_lanes; do
                    [ "$(lane_group "$lane")" = "$group" ] || continue
                    [ "$(lane_kind "$lane")" = "$kind" ] \
                        && queue="$queue $lane@vm:${vms[$(( position % ${#vms[@]} ))]}"
                    position=$(( position + 1 ))
                done
            done
        done
        echo "$queue"
        return
    fi
    for group in $groups; do
        queue=
        for kind in count time; do
            for lane in $macos_lanes; do
                [ "$(lane_group "$lane")" = "$group" ] && [ "$(lane_kind "$lane")" = "$kind" ] \
                    && queue="$queue $lane@macos"
            done
        done
        [ -z "$queue" ] || echo "macos/$group$queue"
        index=0
        for vm in "${vms[@]}"; do
            queue=
            for kind in count time; do
                position=0
                for lane in $linux_lanes; do
                    [ "$(lane_group "$lane")" = "$group" ] || continue
                    if [ "$(lane_kind "$lane")" = "$kind" ] && [ $(( position % ${#vms[@]} )) -eq "$index" ]; then
                        queue="$queue $lane@vm:$vm"
                    fi
                    position=$(( position + 1 ))
                done
            done
            [ -z "$queue" ] || echo "$vm/$group$queue"
            index=$(( index + 1 ))
        done
    done
}

queues=()
while IFS= read -r line; do queues+=("$line"); done < <(plan_queues)

# Each queue's lanes, and how long its time-boxed lanes loop: with --window,
# an equal share of the window less the builds (a guess here) and the margin,
# before its counted lanes take their part.
print_plan() {
    local line label item lane timed longest=0 share
    if [ "$parallel" = true ]; then
        printf 'Candidate %s: %s queues side by side, each one lane at a time\n' "$sha" "${#queues[@]}"
    else
        printf 'Candidate %s: one lane at a time\n' "$sha"
    fi
    for line in "${queues[@]}"; do
        label=${line%% *}
        timed=0
        for item in ${line#* }; do
            [ "$(lane_kind "${item%%@*}")" = time ] && timed=$(( timed + 1 ))
        done
        [ "$timed" -le "$longest" ] || longest=$timed
        if [ -n "$window_seconds" ]; then
            share=$(( (window_seconds - build_guess - window_margin) / (timed > 0 ? timed : 1) ))
            [ "$share" -ge "$minimum_share" ] || share=$minimum_share
        else
            share=$duration_seconds
        fi
        printf '  %s' "$label"
        if [ "$timed" -gt 0 ] && [ -n "$window_seconds" ]; then
            printf ' (%s x about %s, less what its counted lanes take)' "$timed" "$(hm "$share")"
        elif [ "$timed" -gt 0 ]; then
            printf ' (%s x %s, plus its counted lanes)' "$timed" "$(hm "$share")"
        fi
        printf ':\n'
        for item in ${line#* }; do
            lane=${item%%@*}
            [ "$label" = serial ] && lane="$lane on ${item#*@}"
            if [ "$(lane_kind "${item%%@*}")" = time ]; then
                printf '    %s\n' "$lane"
            else
                printf '    %s x%s\n' "$lane" "$count"
            fi
        done
    done
    if [ -n "$window_seconds" ]; then
        printf 'Every lane should end within %s of the start, builds included; how long the counted\n' "$(hm "$window_seconds")"
        printf 'lanes take is unmeasured here, and the time-boxed lanes after them get what is left.\n'
    else
        printf 'The longest queue loops %s lanes of %s back to back: %s of wall time before its\n' \
            "$longest" "$(hm "$duration_seconds")" "$(hm $(( longest * duration_seconds )))"
        printf 'counted lanes and the builds. Pass --window to fit a wall-clock window instead.\n'
    fi
}

if [ "$plan" = true ]; then
    print_plan
    exit 0
fi

# --- the quiet host ---

quiet_lock=${RELIABURGER_QUIET_LOCK:-$HOME/.reliaburger/quiet.lock}
if ! reason=$(python3 -I "$here/power_cut/quiet.py" check); then
    printf 'v02-loops-local: the host is not quiet: %s\n' "$reason" >&2
    exit 75
fi
[ -n "$(git -C "$repository" status --porcelain --untracked-files=no)" ] \
    && fail "the checkout has uncommitted changes; the lanes run HEAD ($short), so commit or stash them"
if [ -d "$evidence/lanes" ] && [ -n "$(ls -A "$evidence/lanes")" ]; then
    fail "$evidence already holds lane results; move $evidence/lanes aside to run again with the same builds, or pass another --evidence"
fi
mkdir -p "$evidence/lanes" "$evidence/work" "$evidence/build"
schedule=$evidence/schedule.log
note() { say "$*" | tee -a "$schedule"; }

# Hold the quiet lock from a helper that lets go when this script exits: macOS
# has no flock(1). power_cut/quiet.py and nightly.py read the same lock.
mkdir -p "$(dirname "$quiet_lock")"
python3 -I -c '
import fcntl, os, sys, time
descriptor = os.open(sys.argv[1], os.O_RDWR | os.O_CREAT, 0o644)
try:
    fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    print("busy", flush=True)
    sys.exit(1)
print("held", flush=True)
parent = int(sys.argv[2])
while True:
    try:
        os.kill(parent, 0)
    except ProcessLookupError:
        break
    time.sleep(5)
' "$quiet_lock" "$$" > "$evidence/quiet-lock.state" 2>&1 &
lock_holder=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ -s "$evidence/quiet-lock.state" ] && break
    sleep 1
done
[ "$(cat "$evidence/quiet-lock.state")" = held ] || fail "could not take the quiet lock $quiet_lock"

queue_pids=()
stop_everything() {
    local pid vm base
    for pid in ${queue_pids[@]+"${queue_pids[@]}"}; do kill "$pid" 2> /dev/null || true; done
    pkill -TERM -f "[n]extest run --archive-file $evidence" 2> /dev/null || true
    for vm in "${vms[@]}"; do
        base=$(guest_base "$vm")
        # Before preflight there is no base, and an empty pattern would stop
        # every archive run in a shared VM.
        [ -n "$base" ] || continue
        GSH_TIMEOUT=30 gsh "$vm" sudo pkill -TERM -f "[n]extest run --archive-file $base" \
            > /dev/null 2>&1 || true
    done
}
on_exit() { kill "$lock_holder" 2> /dev/null || true; }
on_interrupt() {
    note "interrupted; stopping every lane (evidence so far in $evidence)"
    stop_everything
    exit 130
}
trap on_exit EXIT
trap on_interrupt INT TERM

started=$(now)
deadline=
[ -z "$window_seconds" ] || deadline=$(( started + window_seconds ))

# --- preflight ---

free_gib() { df -Pk "$1" | awk 'NR == 2 { print int($4 / 1048576) }'; }
{
    printf 'candidate=%s\nstarted=%s\noptions=parallel:%s duration:%s window:%s count:%s vms:%s\n' \
        "$sha" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$parallel" "$duration" "${window:-none}" "$count" "${vms[*]}"
    printf 'host=%s %s, %s cores, %s GiB\n' "$(sw_vers -productName 2> /dev/null || uname -s)" \
        "$(sw_vers -productVersion 2> /dev/null || uname -r)" "$(sysctl -n hw.ncpu 2> /dev/null || nproc)" \
        "$(( $(sysctl -n hw.memsize 2> /dev/null || echo 0) / 1073741824 ))"
    printf 'host_rustc=%s\nhost_nextest=%s\nlima=%s\n' "$(rustc --version 2>&1)" \
        "$(cargo nextest --version 2>&1 | head -n 1)" "$(limactl --version 2>&1)"
} > "$evidence/environment.txt"
cargo nextest --version > /dev/null 2>&1 || fail "cargo-nextest isn't installed on this host"
command -v limactl > /dev/null || fail "Lima isn't installed; the Linux lanes need it"
[ "$(free_gib "$evidence")" -ge 40 ] || fail "less than 40 GiB free under $evidence"

guest_bases=()
guest_base() {
    local index=0 vm
    for vm in "${vms[@]}"; do
        [ "$vm" = "$1" ] && { echo "${guest_bases[$index]:-}"; return; }
        index=$(( index + 1 ))
    done
}
for vm in "${vms[@]}"; do
    status=$(limactl list --format '{{.Status}}' "$vm" 2> /dev/null || true)
    [ "$status" = Running ] || fail "Lima VM $vm isn't running (status: ${status:-missing}); start it with limactl start $vm"
    # Side by side, the VM's lanes and the Mac's lease ports from the same
    # ranges, and Lima forwards every guest loopback port onto the Mac's
    # unless told not to: a guest test's port then holds a Mac node's.
    if [ "$parallel" = true ]; then
        limactl list --json "$vm" | python3 "$here/lima_forwards.py" check \
            || fail "$vm would collide with the macOS lanes on the test ports; add the rule above, or run without --parallel"
    fi
    facts=$(gsh "$vm" bash -c '
        [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
        printf "home=%s\n" "$HOME"
        printf "arch=%s\n" "$(uname -m)"
        printf "kernel=%s\n" "$(uname -r)"
        printf "cpus=%s\n" "$(nproc)"
        printf "memory=%s\n" "$(awk "/MemTotal/ { print int(\$2 / 1048576) \" GiB\" }" /proc/meminfo)"
        printf "rustc=%s\n" "$(rustc --version 2>&1)"
        printf "nextest=%s\n" "$(cargo nextest --version 2>&1 | head -n 1)"
        printf "make=%s\n" "$(command -v make)"
        printf "sudo=%s\n" "$(sudo -n true 2> /dev/null && echo yes || echo no)"
        printf "btrfs=%s\n" "$(command -v mkfs.btrfs)"
        printf "free_gib=%s\n" "$(df -Pk "$HOME" | awk "NR == 2 { print int(\$4 / 1048576) }")"
    ' 2>&1) || fail "could not ask $vm about itself: $facts"
    printf '%s\n' "$facts" | sed "s/^/$vm: /" >> "$evidence/environment.txt"
    fact() { printf '%s\n' "$facts" | sed -n "s/^$1=//p"; }
    [ "$(fact arch)" = aarch64 ] || fail "$vm is $(fact arch), not arm64"
    [[ $(fact nextest) == cargo-nextest* ]] || fail "$vm has no cargo-nextest: $(fact nextest)"
    [ -n "$(fact make)" ] || fail "$vm has no make"
    [ "$(fact sudo)" = yes ] || fail "$vm has no passwordless sudo, which the Btrfs lane needs"
    [ -n "$(fact btrfs)" ] || fail "$vm has no mkfs.btrfs; install btrfs-progs"
    [ "$(fact free_gib)" -ge 40 ] || fail "$vm has less than 40 GiB free in $(fact home)"
    guest_bases+=("$(fact home)/reliaburger-v02-$short")
done

# --- builds ---

source_tar=$evidence/build/src.tar
git -C "$repository" archive --format=tar -o "$source_tar" "$sha"

build_macos() {
    local directory=$evidence/build/macos
    if [ "$(cat "$directory/candidate" 2> /dev/null)" = "$sha" ] && [ -f "$directory/tests.tar.zst" ]; then
        note "macOS archive of $short already built"
        return 0
    fi
    rm -rf "$directory/src"
    mkdir -p "$directory/src"
    tar -xf "$source_tar" -C "$directory/src"
    # Chained with &&: set -e doesn't apply inside a subshell tested with ||.
    # The lanes' tests start bun, relish and testapp from this target
    # directory, so macOS assesses each before a test times it (#722).
    (
        cd "$directory/src" \
            && export RELIABURGER_GIT_SHA=$sha CARGO_TARGET_DIR=$directory/target CARGO_PROFILE_DEV_DEBUG=line-tables-only \
            && cargo nextest archive --locked --archive-file "$directory/tests.tar.zst" "${archive_tests[@]}" \
            && python3 scripts/ci/warm_executables.py --manifest Cargo.toml --target-dir "$directory/target"
    ) > "$evidence/build-macos.log" 2>&1 || return 1
    echo "$sha" > "$directory/candidate"
}

build_linux() {
    local vm=$1 base script
    base=$(guest_base "$vm")
    gsh "$vm" mkdir -p "$base/lanes" "$base/work" || return 1
    with_timeout 600 limactl copy "$source_tar" "$vm:$base/src.tar" || return 1
    script="set -euo pipefail
[ -f \"\$HOME/.cargo/env\" ] && . \"\$HOME/.cargo/env\"
base=$(q "$base")
if [ \"\$(cat \"\$base/candidate\" 2> /dev/null)\" = $(q "$sha") ] && [ -f \"\$base/tests.tar.zst\" ]; then
    echo 'archive already built'
    exit 0
fi
rm -rf \"\$base/src\"
mkdir -p \"\$base/src\"
tar -xf \"\$base/src.tar\" -C \"\$base/src\"
cd \"\$base/src\"
export RELIABURGER_GIT_SHA=$(q "$sha") CARGO_TARGET_DIR=\"\$base/target\" CARGO_PROFILE_DEV_DEBUG=line-tables-only
cargo nextest archive --locked --archive-file \"\$base/tests.tar.zst\" ${archive_tests[*]}
echo $(q "$sha") > \"\$base/candidate\""
    GSH_TIMEOUT=7200 gsh "$vm" bash -c "$script" > "$evidence/build-$vm.log" 2>&1
}

note "building the test archives of $short (logs: $evidence/build-*.log)"
build_pids=()
build_macos & build_pids+=($!)
for vm in "${vms[@]}"; do build_linux "$vm" & build_pids+=($!); done
built=true
for pid in "${build_pids[@]}"; do wait "$pid" || built=false; done
[ "$built" = true ] || fail "a build failed; see $evidence/build-*.log"
note "archives built in $(hm $(( $(now) - started )))"
if [ "$build_only" = true ]; then
    exit 0
fi

# --- lanes ---

# The bash script that runs one lane: a fresh copy of the source in WORKSPACE,
# the archive extracted into it and remapped to it, as the workflow does in its
# checkout. It copies the JUnit report out, removes the workspace and leaves
# the exit status in STATUS.
lane_script() {
    local lane=$1 platform=$2 workspace=$3 tarball=$4 archive=$5 junit=$6 status=$7 stress=$8
    # $archive and $filter expand when the lane runs; paths hold no spaces.
    local run='cargo nextest run --archive-file $archive --extract-to . --extract-overwrite --workspace-remap . --profile ci --no-tests=fail'
    local command
    case $lane in
        test-upgrade-*)
            command="make $lane NEXTEST=\"$run $stress\"" ;;
        council_disaster_recovery|cluster_failover)
            command="RELIABURGER_CLUSTER_TESTS=1 $run --run-ignored=only -E 'binary($lane)' $stress" ;;
        test-storage-owner-death)
            command="RELIABURGER_BTRFS_TESTS=1 scripts/ci/as-root $run --run-ignored=only -E \"\$filter\" $stress" ;;
        *)
            command="$run -E \"\$filter\" $stress" ;;
    esac
    cat <<EOF
#!/usr/bin/env bash
# $lane ($platform) for $sha, written by qualify-v02-loops-local.sh.
set -u
[ -f "\$HOME/.cargo/env" ] && . "\$HOME/.cargo/env"
export NEXTEST_PROFILE=ci CARGO_TERM_COLOR=always CARGO_PROFILE_DEV_DEBUG=line-tables-only
# In a VM the lane runs under setsid, so this is its process group too.
printf '%s\n' "\$\$" > $(q "$status.pid")
workspace=$(q "$workspace")
archive=$(q "$archive")
filter=$(q "$( [ "$(lane_kind "$lane")" = count ] && lane_filter "$lane")")
code=0
rm -rf "\$workspace" 2> /dev/null || sudo -n rm -rf "\$workspace"
mkdir -p "\$workspace" && tar -xf $(q "$tarball") -C "\$workspace" && cd "\$workspace" || code=\$?
if [ "\$code" -eq 0 ]; then
    printf '+ %s\\n' $(q "$command")
    $command || code=\$?
fi
report=\$workspace/target/nextest/ci/junit.xml
[ -f "\$report" ] || report=\$(find "\$workspace" -path '*/nextest/ci/junit.xml' 2> /dev/null | head -n 1)
[ -z "\$report" ] || cp "\$report" $(q "$junit") || true
cd /
rm -rf "\$workspace" 2> /dev/null || sudo -n rm -rf "\$workspace"
printf '%s\n' "\$code" > $(q "$status.tmp") && mv $(q "$status.tmp") $(q "$status")
exit "\$code"
EOF
}

run_lane() {
    local lane=$1 target=$2 stress=$3 platform slug out vm base gdir code began limit
    if [ "$target" = macos ]; then platform=$macos_platform; else platform=$linux_platform; fi
    slug=$platform-$lane
    out=$evidence/lanes/$slug
    mkdir -p "$out"
    began=$(now)
    note "start $lane ($platform) on ${target#vm:}: $stress"
    if [ "$target" = macos ]; then
        lane_script "$lane" "$platform" "$evidence/work/$slug" "$source_tar" \
            "$evidence/build/macos/tests.tar.zst" "$out/junit.xml" "$out/status" "$stress" > "$out/lane.sh"
        code=0
        bash "$out/lane.sh" > "$out/loop.log" 2>&1 < /dev/null || code=$?
    else
        vm=${target#vm:}
        base=$(guest_base "$vm")
        gdir=$base/lanes/$slug
        lane_script "$lane" "$platform" "$base/work/$slug" "$base/src.tar" \
            "$base/tests.tar.zst" "$gdir/junit.xml" "$gdir/status" "$stress" > "$out/lane.sh"
        code=0
        gsh "$vm" bash -c "rm -rf $(q "$gdir") && mkdir -p $(q "$gdir")" \
            && with_timeout 120 limactl copy "$out/lane.sh" "$vm:$gdir/lane.sh" \
            && gsh "$vm" bash -c "setsid nohup bash $(q "$gdir/lane.sh") > $(q "$gdir/loop.log") 2>&1 < /dev/null &" \
            || code=$?
        # The lane runs detached in the guest, so a dropped SSH session can't
        # kill it; poll for its status file. A counted lane has no time box,
        # so it gets a generous ceiling.
        if [ "$stress" = "${stress#--stress-duration}" ]; then limit=57600; else limit=$(( $(to_seconds "${stress#--stress-duration }") + 10800 )); fi
        while [ "$code" -eq 0 ] && ! gsh "$vm" test -f "$gdir/status" > /dev/null 2>&1; do
            if [ $(( $(now) - began )) -gt "$limit" ]; then
                note "$lane ($platform) on $vm passed its ceiling of $(hm "$limit"); stopping it"
                gsh "$vm" bash -c "sudo kill -TERM -- -\$(cat $(q "$gdir/status.pid"))" > /dev/null 2>&1 || true
                code=124
                break
            fi
            sleep 60
        done
        with_timeout 300 limactl copy "$vm:$gdir/loop.log" "$out/loop.log" > /dev/null 2>&1 || true
        with_timeout 300 limactl copy "$vm:$gdir/junit.xml" "$out/junit.xml" > /dev/null 2>&1 || true
        if [ "$code" -eq 0 ]; then
            code=$(gsh "$vm" cat "$gdir/status" 2> /dev/null || echo 1)
        fi
    fi
    note "end $lane ($platform): exit $code after $(hm $(( $(now) - began )))"
    local expected=()
    [ "$(lane_kind "$lane")" = time ] || expected=(--expected-iterations "$count")
    python3 -I "$here/loop_summary.py" --lane "$lane ($platform)" --junit "$out/junit.xml" \
        --log "$out/loop.log" --candidate "$sha" ${expected[@]+"${expected[@]}"} \
        --output "$out/summary.md" > /dev/null 2>&1 || true
}

run_queue() {
    set +e
    local label=$1 item lane timed left share stress index=0
    shift
    local items=("$@")
    for item in "${items[@]}"; do
        lane=${item%%@*}
        if [ "$(lane_kind "$lane")" = count ]; then
            stress="--stress-count $count"
        else
            share=$duration_seconds
            if [ -n "$deadline" ]; then
                timed=0
                for left in "${items[@]:$index}"; do
                    [ "$(lane_kind "${left%%@*}")" = time ] && timed=$(( timed + 1 ))
                done
                share=$(( (deadline - $(now) - window_margin) / timed ))
                [ "$share" -ge "$minimum_share" ] || share=$minimum_share
            fi
            stress="--stress-duration ${share}s"
        fi
        run_lane "$lane" "${item#*@}" "$stress"
        index=$(( index + 1 ))
    done
    note "queue $label done"
}

print_plan | tee -a "$schedule"
for line in "${queues[@]}"; do
    # shellcheck disable=SC2086  # the items are words without spaces
    run_queue ${line%% *} ${line#* } &
    queue_pids+=($!)
done
for pid in "${queue_pids[@]}"; do wait "$pid" || true; done
queue_pids=()

# --- the record ---

summary=$evidence/v02-loops-summary.md
verdict=0
python3 -I "$here/loop_summary.py" --combine "$evidence/lanes" --manifest "$manifest" \
    --candidate "$sha" --output "$summary" > /dev/null || verdict=$?
rmdir "$evidence/work" 2> /dev/null || true
note "all lanes done in $(hm $(( $(now) - started ))); verdict in $summary"
grep '^Verdict' "$summary" || true
printf 'The builds stay for a rerun of %s: %s, and %s in each VM. Delete them when done.\n' \
    "$short" "$evidence/build" "$(guest_base "${vms[0]}")"
exit "$verdict"
