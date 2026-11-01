#!/usr/bin/env bash
# shellcheck disable=SC2016,SC2034  # guest commands expand in the guest; LABEL_* are read through ${!name}
# Release profile (#1358): where the time goes in a staged candidate, measured
# beside the previous release in the same fresh VM, and what changed.
#
# It creates a disposable Lima VM with the 0.2.3 host-job profile record's
# shape (VZ, 4 vCPU, 8 GiB, 60 GiB, template:ubuntu-24.04), installs both
# versions' release binaries, checked against their candidate.json, and each
# version's profiling build (frame pointers and line tables, built by
# build.yml beside the candidate and never shipped). Then it runs the fixed
# scenarios in alternating rounds, every session on a fresh boot: previous and
# candidate in round 1, candidate and previous in round 2, and so on, so host
# noise hits both builds alike. One profiling session per version runs `perf`
# on host jobs and on the idle cluster. It writes
# docs/qualification/DATE-vX.Y.Z-profile/ with a generated README, its
# "Compared with vPREV" section and every rule's "would flag" line, and
# deletes the VM.
#
# The rules don't gate the release yet (maintainer decision, 26 October 2026):
# whoever qualifies it writes the README's answer and explains each flagged
# line. It needs a quiet host, so it never overlaps a soak or a build, and it
# refuses to start while `rustc` or `cargo` run.
#
# Usage:
#   scripts/release/qualify-profile.sh --candidate URL --qualified-digest SHA [options]
#
#   --candidate URL         staged candidate (the staging pre-release's download
#                           URL) or a published release's download URL
#   --qualified-digest SHA  require the candidate's candidate.json to have this
#                           SHA-256; without it the record is a diagnostic
#   --candidate-binaries DIR  measure local Linux aarch64 `bun` and `relish`
#                           instead of a candidate URL (a diagnostic record)
#   --previous vX.Y.Z       the release to compare with (default: the candidate's
#                           version with the patch number one lower)
#   --previous-url URL      where its assets are (default: its GitHub release)
#   --candidate-profiling DIR|none, --previous-profiling DIR|none
#                           a directory holding the profiling build's
#                           bun-linux-aarch64 and relish-linux-aarch64; default:
#                           the build run's profiling-build-COMMIT-ATTEMPT
#                           artefact (gh), or none. Without one, perf runs on
#                           the stripped release build and names no Bun frame
#   --scenarios LIST        comma-separated, from raw, host, shared, fresh, idle,
#                           control (default raw,host,shared,idle: fresh
#                           containers belong to the nightly run, the control
#                           plane comes later)
#   --rounds N              A/B rounds (default 3; dropped to fit 2 hours)
#   --window S              seconds per window (default 60)
#   --record DIR            new directory for the record (default
#                           docs/qualification/DATE-vX.Y.Z-profile)
#   --vm NAME               the VM to create (default rb-profile-<random>);
#                           refuses one that exists
#   --keep                  leave the VM afterwards (stopped)
#   --smoke                 one round, 30 s windows, a record under /tmp: shows
#                           the step works end to end, measures nothing; warns
#                           about running compilers instead of refusing
#   --plan, --dry-run       print the plan and the time estimate, touch nothing
set -euo pipefail

usage() { sed -n '3,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
fail() { printf 'qualify-profile: %s\n' "$*" >&2; exit 1; }
python3 "$(dirname "$0")/require_python.py" || exit 1
say() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*"; }

# A two-hour run must not sleep on idle.
if [ "$(uname -s)" = Darwin ] && [ -z "${RELIABURGER_PROFILE_CAFFEINATED:-}" ] && command -v caffeinate >/dev/null; then
    RELIABURGER_PROFILE_CAFFEINATED=1 exec caffeinate -dimsu "$0" "$@"
fi

repository=$(cd "$(dirname "$0")/../.." && pwd)
tools=$repository/scripts/release/profiling
github=reliaburger/reliaburger
command_line="scripts/release/qualify-profile.sh $*"

candidate_url='' qualified_digest='' candidate_binaries=''
previous_version='' previous_url=''
candidate_profiling='' previous_profiling=''
scenarios=raw,host,shared,idle rounds=3 window=60 settle=30
record='' vm='' keep=false smoke=false plan=false
while [ "$#" -gt 0 ]; do
    case $1 in
        --candidate) candidate_url=${2:-}; shift 2 ;;
        --qualified-digest) qualified_digest=${2:-}; shift 2 ;;
        --candidate-binaries) candidate_binaries=${2:-}; shift 2 ;;
        --previous) previous_version=${2:-}; shift 2 ;;
        --previous-url) previous_url=${2:-}; shift 2 ;;
        --candidate-profiling) candidate_profiling=${2:-}; shift 2 ;;
        --previous-profiling) previous_profiling=${2:-}; shift 2 ;;
        --scenarios) scenarios=${2:-}; shift 2 ;;
        --rounds) rounds=${2:-}; shift 2 ;;
        --window) window=${2:-}; shift 2 ;;
        --record) record=${2:-}; shift 2 ;;
        --vm) vm=${2:-}; shift 2 ;;
        --keep) keep=true; shift ;;
        --smoke) smoke=true; shift ;;
        --plan|--dry-run) plan=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown argument: $1" ;;
    esac
done

if [ "$smoke" = true ]; then
    rounds=1 window=30 settle=10
fi
[[ $rounds =~ ^[1-9][0-9]*$ ]] || fail '--rounds must be a positive number'
[[ $window =~ ^[0-9]+$ ]] && [ "$window" -ge 10 ] || fail '--window must be at least 10 seconds'
[ -n "$candidate_url" ] || [ -n "$candidate_binaries" ] || fail 'name the candidate with --candidate URL (or --candidate-binaries DIR)'
[ -z "$candidate_url" ] || [ -z "$candidate_binaries" ] || fail '--candidate and --candidate-binaries are alternatives'
[ -z "$qualified_digest" ] || [[ $qualified_digest =~ ^[0-9a-f]{64}$ ]] || fail '--qualified-digest must be a SHA-256'
[ -n "$previous_version" ] || [ -n "$candidate_url" ] || fail '--candidate-binaries needs --previous: there is no candidate.json to derive it from'
[ -z "$previous_version" ] || [[ $previous_version =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail '--previous must look like v1.2.3'
case $candidate_url$previous_url in *' '*) fail 'URLs must not contain spaces' ;; esac

plan_args=(--scenarios "$scenarios" --rounds "$rounds" --window "$window" --settle "$settle")
python3 "$tools/profile_plan.py" "${plan_args[@]}" --format shell > /dev/null || fail 'bad --scenarios, --rounds or --window'
if [ "$plan" = true ]; then
    echo "candidate: ${candidate_url:-$candidate_binaries}"
    [ -z "$qualified_digest" ] || echo "qualified digest: $qualified_digest"
    echo "previous: ${previous_version:-derived: the candidate version with the patch number one lower}"
    afterwards='deleted afterwards'
    [ "$keep" = false ] || afterwards='left stopped afterwards (--keep)'
    echo "vm: ${vm:-rb-profile-<random>}, vz, 4 vCPU, 8 GiB, 60 GiB, template:ubuntu-24.04, $afterwards"
    [ "$smoke" = false ] || echo "smoke: one round, ${window} s windows, a record under /tmp"
    python3 "$tools/profile_plan.py" "${plan_args[@]}"
    exit 0
fi

mode=release
[ -n "$qualified_digest" ] && [ -z "$candidate_binaries" ] || mode=diagnostic
[ "$smoke" = false ] || mode=smoke

# --- Preflight: a quiet host, Lima, a VM name nobody uses -------------------
[ "$(uname -s)" = Darwin ] || fail 'the bench VM is Lima with VZ on macOS'
command -v limactl >/dev/null || fail 'limactl is required'
command -v curl >/dev/null || fail 'curl is required'
# pgrep exits 1 when nothing matches, which pipefail would turn into a silent exit (#1488).
compilers() { { pgrep -x rustc || true; pgrep -x cargo || true; } 2>/dev/null | wc -l | tr -d ' '; }
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
running=$(compilers)
if [ "$running" -gt 0 ]; then
    [ "$smoke" = true ] || fail "$running rustc/cargo processes are running; the profile needs a quiet host (wait for builds and soaks to finish)"
    say "warning: $running rustc/cargo processes are running; a smoke run carries on, its numbers mean nothing anyway"
fi
[ -n "$vm" ] || vm=rb-profile-$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')
[[ $vm =~ ^rb-profile-[A-Za-z0-9-]+$ ]] || fail '--vm must start with rb-profile-, so it can never name a VM this script did not make'
if limactl list --format '{{.Name}}' 2>/dev/null | grep -qx -- "$vm"; then
    fail "VM $vm already exists; this script only uses a VM it creates"
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/rb-profile.XXXXXX")
sha256() { shasum -a 256 "$1" | awk '{print $1}'; }

# fetch_release LABEL URL [DIGEST]: download and verify one version's binaries,
# then set LABEL_version, LABEL_commit, LABEL_run_id, LABEL_run_attempt.
fetch_release() {
    local label=$1 url=$2 digest=${3:-} dir=$work/$1 key value
    mkdir -p "$dir/release"
    for asset in candidate.json bun-linux-aarch64 relish-linux-aarch64; do
        curl -fsSL --proto '=https' --retry 3 "$url/$asset" -o "$dir/release/$asset" || fail "could not download $url/$asset"
    done
    python3 "$tools/profile_fetch.py" verify --record "$dir/release/candidate.json" ${digest:+--digest "$digest"} \
        --asset "bun-linux-aarch64=$dir/release/bun-linux-aarch64" \
        --asset "relish-linux-aarch64=$dir/release/relish-linux-aarch64" > "$dir/identity" \
        || fail "$label binaries don't match $url/candidate.json"
    while IFS='=' read -r key value; do
        case $key in
            version|commit|run_id|run_attempt) printf -v "${label}_${key}" '%s' "$value" ;;
        esac
    done < "$dir/identity"
    mv "$dir/release/bun-linux-aarch64" "$dir/release/bun"
    mv "$dir/release/relish-linux-aarch64" "$dir/release/relish"
    chmod 0755 "$dir/release/bun" "$dir/release/relish"
}

# fetch_profiling LABEL CHOICE: set LABEL_profiling to how its perf session runs.
fetch_profiling() {
    local label=$1 choice=$2 dir=$work/$1 run_var=${1}_run_id commit_var=${1}_commit attempt_var=${1}_run_attempt
    local run=${!run_var:-} commit=${!commit_var:-} attempt=${!attempt_var:-}
    case $choice in
        none) printf -v "${label}_profiling" '%s' 'none' ;;
        '')
            if [ -n "$run" ] && command -v gh >/dev/null \
                && gh run download "$run" --repo "$github" --name "profiling-build-$commit-$attempt" \
                    --dir "$dir/profiling" > "$dir/profiling.log" 2>&1; then
                printf -v "${label}_profiling" '%s' "artefact profiling-build-$commit-$attempt of build run $run"
            else
                printf -v "${label}_profiling" '%s' 'none'
            fi ;;
        *)
            [ -f "$choice/bun-linux-aarch64" ] && [ -f "$choice/relish-linux-aarch64" ] \
                || fail "$choice holds no bun-linux-aarch64 and relish-linux-aarch64"
            mkdir -p "$dir/profiling"
            cp "$choice/bun-linux-aarch64" "$choice/relish-linux-aarch64" "$dir/profiling/"
            printf -v "${label}_profiling" '%s' "local directory $choice" ;;
    esac
    if [ -f "$dir/profiling/bun-linux-aarch64" ]; then
        mv "$dir/profiling/bun-linux-aarch64" "$dir/profiling/bun"
        mv "$dir/profiling/relish-linux-aarch64" "$dir/profiling/relish"
        chmod 0755 "$dir/profiling/bun" "$dir/profiling/relish"
    else
        # perf still sees the kernel, the helper and the tasks; Bun's own frames stay unnamed.
        rm -rf "$dir/profiling"
        cp -R "$dir/release" "$dir/profiling"
        printf -v "${label}_profiling" '%s' 'none: perf ran on the stripped release build, so it names no Bun frame'
    fi
}

say "fetching the candidate"
candidate_version='' candidate_commit='' candidate_run_id='' candidate_run_attempt=''
if [ -n "$candidate_url" ]; then
    fetch_release candidate "$candidate_url" "$qualified_digest"
    candidate_source=$candidate_url
else
    [ -x "$candidate_binaries/bun" ] && [ -x "$candidate_binaries/relish" ] \
        || fail "$candidate_binaries holds no executable bun and relish"
    mkdir -p "$work/candidate/release"
    cp "$candidate_binaries/bun" "$candidate_binaries/relish" "$work/candidate/release/"
    candidate_version=local candidate_commit=unknown candidate_source=$candidate_binaries
fi
if [ -z "$previous_version" ]; then
    [[ $candidate_version =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+) ]] || fail "can't derive --previous from $candidate_version"
    [ "${BASH_REMATCH[3]}" -gt 0 ] || fail "$candidate_version is a .0 release: name --previous"
    previous_version=v${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.$(( BASH_REMATCH[3] - 1 ))
fi
[ -n "$previous_url" ] || previous_url=https://github.com/$github/releases/download/$previous_version
say "fetching $previous_version"
previous_commit='' previous_run_id='' previous_run_attempt=''
saved_previous=$previous_version
fetch_release previous "$previous_url"
[ "$previous_version" = "$saved_previous" ] || fail "$previous_url/candidate.json is $previous_version, not $saved_previous"
fetch_profiling candidate "$candidate_profiling"
fetch_profiling previous "$previous_profiling"

plan_rounds='' plan_dropped='' plan_estimate=''
while IFS='=' read -r key value; do
    case $key in plan_rounds|plan_dropped|plan_estimate) printf -v "$key" '%s' "$value" ;; esac
done < <(python3 "$tools/profile_plan.py" "${plan_args[@]}" --format shell)
date_stamp=$(date +%F)
if [ -z "$record" ]; then
    if [ "$smoke" = true ]; then
        record=$work/record
    else
        record=$repository/docs/qualification/$date_stamp-$candidate_version-profile
    fi
fi
[ ! -e "$record" ] || fail "$record exists; the record never overwrites one"
mkdir -p "$record/raw" "$record/flamegraphs"
meta() { printf '%s\t%s\n' "$1" "$2" >> "$record/metadata.tsv"; }
started=$(date +%s)
meta mode "$mode"; meta date "$date_stamp"; meta command "$command_line"
meta started_at "$(date -u +%FT%TZ)"
for label in candidate previous; do
    for field in version commit profiling; do
        name=${label}_$field
        meta "$name" "${!name}"
    done
    meta "${label}_bun_sha256" "$(sha256 "$work/$label/release/bun")"
    meta "${label}_relish_sha256" "$(sha256 "$work/$label/release/relish")"
done
meta candidate_source "$candidate_source"; meta previous_source "$previous_url"
[ -z "$qualified_digest" ] || meta qualified_digest "$qualified_digest"
meta scenarios "$scenarios"; meta window "$window"
meta rounds_planned "$rounds"; meta rounds_run "$plan_rounds"; meta rounds_dropped "$plan_dropped"
meta estimate_seconds "$plan_estimate"
meta host_model "$(sysctl -n hw.model 2>/dev/null || echo unknown)"
meta host_cpu "$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown), $(sysctl -n hw.ncpu) cores"
meta host_memory "$(( $(sysctl -n hw.memsize) / 1073741824 )) GiB"
meta host_os "macOS $(sw_vers -productVersion 2>/dev/null) (Darwin $(uname -r))"
meta lima_version "$(limactl --version | awk '{print $NF}')"
meta vm_name "$vm"; meta vm_shape 'VZ, 4 vCPU, 8 GiB, 60 GiB disk, template:ubuntu-24.04, no mounts'
python3 "$tools/profile_plan.py" "${plan_args[@]}"
[ "$plan_dropped" = 0 ] || say "dropping $plan_dropped rounds to fit the 2-hour budget"

host_load() { printf '%s\t%s\t%s\t%s\n' "$(date -u +%FT%TZ)" "$1" "$(load1)" "$(compilers)" >> "$record/host-load.tsv"; }
( while :; do host_load sample; sleep 10; done ) &
sampler=$!

made_vm=no
finish() {
    local status=$?
    kill "$sampler" 2>/dev/null || true
    if [ "$made_vm" = yes ]; then
        if [ "$keep" = true ]; then
            limactl stop "$vm" > /dev/null 2>&1 || true
            say "kept VM $vm (stopped); delete it with: limactl delete --force $vm"
        else
            limactl stop "$vm" > /dev/null 2>&1 || limactl stop --force "$vm" > /dev/null 2>&1 || true
            limactl delete --force "$vm" > /dev/null 2>&1 || echo "could not delete VM $vm" >&2
        fi
    fi
    rm -rf "$work/candidate" "$work/previous"
    [ "$status" -eq 0 ] || echo "qualify-profile: failed; evidence so far is in $record" >&2
}
trap finish EXIT

say "creating VM $vm"
limactl create --name "$vm" --cpus 4 --memory 8 --disk 60 --vm-type vz --mount-none --tty=false \
    template:ubuntu-24.04 > "$work/create.log" 2>&1 || { tail -n 20 "$work/create.log" >&2; fail 'limactl create failed'; }
made_vm=yes
# A busy host can take longer than Lima's 10-minute default over the first boot.
limactl start --tty=false --timeout 30m "$vm" > "$work/start.log" 2>&1 || { tail -n 20 "$work/start.log" >&2; fail 'limactl start failed'; }
# Never let limactl read the caller's stdin.
guest() { limactl shell --workdir / "$vm" "$@" < /dev/null; }
say "provisioning: runc, perf, BusyBox"
# Ubuntu's own upgrades go first: the #1030 record lost a run to
# unattended-upgrades restarting rb-bun mid-window.
guest sudo bash -c 'set -e
    systemctl disable --now apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service \
        fwupd-refresh.timer motd-news.timer update-notifier-download.timer man-db.timer > /dev/null 2>&1 || true
    systemctl mask apt-daily.service apt-daily-upgrade.service > /dev/null 2>&1 || true
    snap refresh --hold > /dev/null 2>&1 || true
    export DEBIAN_FRONTEND=noninteractive
    apt-get -o DPkg::Lock::Timeout=600 update -qq
    apt-get -o DPkg::Lock::Timeout=600 install -y -qq runc python3 busybox-static curl linux-tools-common "linux-tools-$(uname -r)" > /dev/null
    install -D -m 0755 /bin/busybox /opt/bench/busybox' > "$work/provision.log" 2>&1 \
    || { tail -n 20 "$work/provision.log" >&2; fail 'provisioning the VM failed'; }
guest mkdir -p /tmp/rb-upload
limactl copy -r "$tools" "$vm:/tmp/rb-upload/tools"
for label in candidate previous; do
    limactl copy -r "$work/$label/release" "$vm:/tmp/rb-upload/$label-release"
    limactl copy -r "$work/$label/profiling" "$vm:/tmp/rb-upload/$label-profiling"
done
guest sudo bash -c 'set -e; rm -rf /opt/rb; mkdir -p /opt/rb; cp -R /tmp/rb-upload/. /opt/rb/
    chmod 0755 /opt/rb/tools/*.sh /opt/rb/*/bun /opt/rb/*/relish; rm -rf /tmp/rb-upload'
meta guest_os "$(guest sh -c '. /etc/os-release; echo "$PRETTY_NAME $(uname -m)"')"
meta guest_kernel "$(guest uname -r)"
meta runc_version "$(guest runc --version | head -n 1)"
meta perf_version "$(guest sudo perf --version 2>/dev/null || echo 'perf missing')"
meta busybox_sha256 "$(guest sha256sum /opt/bench/busybox | awk '{print $1}')"

fresh_boot() {
    limactl stop "$vm" > /dev/null 2>&1 < /dev/null || limactl stop --force "$vm" > /dev/null 2>&1 < /dev/null
    limactl start --tty=false --timeout 30m "$vm" > /dev/null 2>&1 < /dev/null || fail "VM $vm didn't start again"
}

run_session() { # KIND LABEL ROUND
    local kind=$1 label=$2 number=$3 build=release out steps name
    if [ "$kind" = perf ]; then
        build=profiling out=/root/profile/$label/perf name="$label perf"
        steps="perfcpu-host perfoff-host perfsched-host probe-host perfcpu-idle"
    else
        out=/root/profile/$label/r$number name="$label r$number"
        steps=${scenarios//,/ }
    fi
    say "session: $name"
    fresh_boot
    host_load "start $name"
    # shellcheck disable=SC2086  # steps is a list of window names
    guest sudo /opt/rb/tools/profile_session.sh --bun "/opt/rb/$label-$build/bun" \
        --relish "/opt/rb/$label-$build/relish" --busybox /opt/bench/busybox \
        --out "$out" --window "$window" --settle "$settle" $steps \
        > "$work/session.log" 2>&1 || { tail -n 20 "$work/session.log" >&2; collect; fail "session $name failed"; }
    host_load "end $name"
}

collect() {
    guest sudo tar -C /root/profile -cf - . 2>/dev/null | tar -C "$record/raw" -xf - || true
}

plan_sessions=()
while read -r line; do plan_sessions+=("$line"); done < <(python3 "$tools/profile_plan.py" "${plan_args[@]}" --format sessions)
for line in "${plan_sessions[@]}"; do
    # shellcheck disable=SC2086  # "KIND LABEL ROUND"
    run_session $line
done
say "collecting the evidence"
collect

say "drawing flamegraphs"
flame() { python3 "$tools/profile_flame.py" "$@"; }
for label in previous candidate; do
    name=${label}_version
    version=${!name}
    p=$record/raw/$label/perf
    [ ! -s "$p/perfcpu-host.folded" ] || flame svg "$p/perfcpu-host.folded" --title "$version: host jobs, whole VM, on-CPU" > "$record/flamegraphs/$label-host-oncpu-vm.svg"
    [ ! -s "$p/perfcpu-host.bun.folded" ] || flame svg "$p/perfcpu-host.bun.folded" --title "$version: host jobs, Bun only, on-CPU" > "$record/flamegraphs/$label-host-oncpu-bun.svg"
    [ ! -s "$p/perfcpu-idle.bun.folded" ] || flame svg "$p/perfcpu-idle.bun.folded" --title "$version: idle cluster, Bun only, on-CPU" > "$record/flamegraphs/$label-idle-oncpu-bun.svg"
    [ ! -s "$p/perfoff-host.bun.folded" ] || flame svg "$p/perfoff-host.bun.folded" --unit us --title "$version: host jobs, Bun off-CPU" > "$record/flamegraphs/$label-host-offcpu-bun.svg"
    [ ! -s "$p/perfoff-host.helper.folded" ] || flame svg "$p/perfoff-host.helper.folded" --unit us --title "$version: host jobs, helpers off-CPU" > "$record/flamegraphs/$label-host-offcpu-helpers.svg"
done
before=$record/raw/previous/perf after=$record/raw/candidate/perf
if [ -s "$before/perfcpu-host.folded" ] && [ -s "$after/perfcpu-host.folded" ]; then
    flame diff "$before/perfcpu-host.folded" "$after/perfcpu-host.folded" \
        --title "$candidate_version against $previous_version: host jobs, whole VM" > "$record/flamegraphs/diff-host-oncpu-vm.svg"
fi
if [ -s "$before/perfcpu-idle.bun.folded" ] && [ -s "$after/perfcpu-idle.bun.folded" ]; then
    flame diff "$before/perfcpu-idle.bun.folded" "$after/perfcpu-idle.bun.folded" \
        --title "$candidate_version against $previous_version: idle cluster, Bun only" > "$record/flamegraphs/diff-idle-oncpu-bun.svg"
fi

meta finished_at "$(date -u +%FT%TZ)"
meta wall_seconds "$(( $(date +%s) - started ))"
say "writing the README"
python3 "$tools/profile_readme.py" "$record"
say "record: $record"
[ "$mode" != release ] || say "next: write its Answer and explain every 'would flag' line, then commit it"
