#!/usr/bin/env bash
# One session of the release profile step, inside the bench VM, as root, on a
# fresh boot (a Bun that follows another on the same boot fails on the old
# node's mounts). qualify-profile.sh copies this directory into the VM and
# runs it once per round and version, then once per version with the
# profiling build.
#
# It sets up the cluster scripts/release/job_bench_rounds.sh sets up: a fresh
# /var/lib/reliaburger, eBPF on, the BusyBox allowlisted for host jobs, a 500m
# BusyBox web app beside the work. Then it runs the named windows. Every
# window records CPU snapshots (/proc/stat, Bun's CPU, threads, context
# switches and RSS, cgroup CPU) before and after, and vmstat once a second.
#
# Usage:
#   profile_session.sh --bun B --relish R --busybox X --out DIR [--window S]
#       [--settle S] WINDOW...
#
# Windows:
#   raw, host, shared, fresh  one `relish bench` window of jobs-vm-baseline,
#                             jobs-host-processes, jobs-shared-containers or
#                             jobs-containers at the default cap
#   idle                      the cluster with no work for --window seconds
#   control                   `relish bench --quick` (deploy, scheduling), then
#                             100 API round trips to /v1/status
#   perfcpu-host, perfcpu-idle  perf record -F 499 -a -g for a third of the
#                             window, from a third of the way in
#   perfoff-host              perf record -e sched:sched_switch, 5 s
#   perfsched-host            perf sched record, 5 s
#   probe-host                batch-status every 0.15 s for about 22 s
# After perf windows it folds their stacks (profile_flame.py), sorts them into
# categories (profile_categories.py), and deletes perf.data and the perf script
# text: they're tens of megabytes and only make sense next to these binaries.
#
# Adapted from the 2026-10-20 host-job profile record's tools/session.sh.
set -euo pipefail

fail() { printf 'profile_session: %s\n' "$*" >&2; exit 1; }

tools=$(cd "$(dirname "$0")" && pwd)
bun='' relish='' busybox='' out='' window=60 settle=30
while [ "$#" -gt 0 ]; do
    case $1 in
        --bun) bun=${2:-}; shift 2 ;;
        --relish) relish=${2:-}; shift 2 ;;
        --busybox) busybox=${2:-}; shift 2 ;;
        --out) out=${2:-}; shift 2 ;;
        --window) window=${2:-}; shift 2 ;;
        --settle) settle=${2:-}; shift 2 ;;
        --) shift; break ;;
        -*) fail "unknown argument: $1" ;;
        *) break ;;
    esac
done
[ "$(id -u)" -eq 0 ] || fail 'run as root inside the bench VM'
[ -x "$bun" ] && [ -x "$relish" ] && [ -x "$busybox" ] || fail '--bun, --relish and --busybox must be executables'
[ -n "$out" ] && [ ! -e "$out" ] || fail '--out must name a new directory'
[[ $window =~ ^[0-9]+$ ]] && [ "$window" -ge 10 ] || fail '--window must be at least 10 seconds'
[[ $settle =~ ^[0-9]+$ ]] || fail '--settle must be a whole number of seconds'
[ "$#" -gt 0 ] || fail 'name at least one window'
case $busybox in /*) ;; *) fail '--busybox must be an absolute path' ;; esac

# A disposable VM is still a whole operating system with its own plans: the
# #1030 record lost a run to unattended-upgrades restarting rb-bun mid-window.
# qualify-profile.sh turns these off when it provisions; every session checks.
systemctl disable --now apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service \
    fwupd-refresh.timer motd-news.timer update-notifier-download.timer man-db.timer > /dev/null 2>&1 || true
systemctl mask apt-daily.service apt-daily-upgrade.service > /dev/null 2>&1 || true

image='public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028'
work=/root/rb-profile
install -m 0755 "$bun" /usr/local/bin/rb-bun
install -m 0755 "$relish" /usr/local/bin/rb-relish
systemctl stop rb-bun 2>/dev/null || true
rm -rf /var/lib/reliaburger "$work"
mkdir -p "$work" "$out"
sha256sum /usr/local/bin/rb-bun /usr/local/bin/rb-relish "$busybox" > "$out/binaries.txt"
{ uname -r; runc --version | head -n 1; nproc; } >> "$out/binaries.txt"
progress() { echo "$(date -u +%FT%TZ) $*" >> "$out/progress.log"; }

cd "$work"
rb-relish init cluster --cluster-name bench --node-id node-01 > init.log 2>&1
cp cluster/reliaburger.toml "$out/reliaburger.generated.toml"
python3 "$tools/profile_config.py" cluster/reliaburger.toml "$busybox"
cp cluster/reliaburger.toml "$out/reliaburger.toml"
systemd-run --unit=rb-bun --collect --property=WorkingDirectory="$work" --property=OOMScoreAdjust=-900 \
    --property=StandardOutput=append:"$work/bun.log" --property=StandardError=append:"$work/bun.log" \
    /usr/local/bin/rb-bun --cluster --runtime auto --config cluster/reliaburger.toml
cleanup() {
    systemctl stop rb-bun 2>/dev/null || true
    tail -n 2000 "$work/bun.log" > "$out/bun.log" 2>/dev/null || true
}
trap cleanup EXIT
export RELIABURGER_CA_CERT=$work/cluster/identity/root-ca.crt
token=
for _ in $(seq 1 60); do
    token=$(rb-relish token create --name bench-admin --role admin 2>/dev/null) && break
    sleep 2
done
[ -n "$token" ] || fail 'Bun never issued an admin token; see bun.log'
export RELIABURGER_TOKEN=$token
cat > app.toml <<EOF
[app.web]
image = "$image"
command = ["/bin/busybox", "httpd", "-f", "-p", "8080", "-h", "/etc"]
port = 8080
cpu = "500m"
memory = "64Mi"
EOF
rb-relish apply app.toml > /dev/null
sleep "$settle"
bunpid=$(systemctl show -p MainPID --value rb-bun)
[ -n "$bunpid" ] && [ "$bunpid" != 0 ] || fail 'Bun is not running; see bun.log'
progress "bun pid $bunpid"

cpu_snapshot() { # $1 file
    {
        echo "## clock"; date +%s.%N
        echo "## proc_stat"; head -n 1 /proc/stat
        echo "## bun_stat"; cut -d' ' -f14-17 "/proc/$bunpid/stat"
        echo "## bun_status"
        awk '/^VmRSS:/{print "rss_kb", $2} /^VmHWM:/{print "hwm_kb", $2} /^Threads:/{print "threads", $2}' \
            "/proc/$bunpid/status"
        # Every thread's switches: Bun's wake-ups, the idle cluster's measure.
        cat /proc/"$bunpid"/task/*/status 2>/dev/null \
            | awk '/^(voluntary|nonvoluntary)_ctxt_switches:/{n += $2} END {print "ctxt", n + 0}'
        echo "## cgroups"
        find /sys/fs/cgroup -maxdepth 4 -name cpu.stat | while read -r f; do
            printf '%s %s\n' "${f%/cpu.stat}" "$(awk '$1=="usage_usec"{print $2}' "$f")"
        done
        echo "## ctxt"; grep -E '^(ctxt|processes)' /proc/stat
        echo "## psi_cpu"; cat /proc/pressure/cpu 2>/dev/null || true
    } > "$1"
}

lag_snapshot() { # $1 file: status of the newest batch
    local id
    for id in $(seq 80 -1 1); do
        if rb-relish --output json batch-status "$id" > "$1.tmp" 2>/dev/null; then
            mv "$1.tmp" "$1"; echo "$id" > "$1.id"; return 0
        fi
    done
    rm -f "$1.tmp"
}

third=$(( window / 3 ))
late=$(( window * 25 / 60 ))

# bench_window NAME SCENARIO [PERF-MODE]
bench_window() {
    local name=$1 scenario=$2 mode=${3:-none} extra=() result=0
    case $scenario in jobs-host-processes|jobs-vm-baseline) extra=(--exec "$busybox") ;; esac
    cpu_snapshot "$out/$name.cpu0"
    vmstat -n 1 $(( window + 2 )) > "$out/$name.vmstat" &
    local vm=$!
    rb-relish --output json bench --scenario "$scenario" --seconds "$window" \
        --report "$out/$name.json" "${extra[@]}" > "$out/$name.out" 2>&1 &
    local bench=$!
    case $mode in
        cpu)
            sleep "$third"
            perf record -F 499 -a -g -o "$out/$name.perf.data" -- sleep "$third" > /dev/null 2>&1 || true ;;
        off)
            sleep "$late"
            perf record -a -g -e sched:sched_switch -o "$out/$name.perf.data" -- sleep 5 > /dev/null 2>&1 || true ;;
        sched)
            sleep "$late"
            perf sched record -a -o "$out/$name.sched.data" -- sleep 5 > /dev/null 2>&1 || true ;;
        probe)
            sleep $(( third < 8 ? third : 8 ))
            lag_snapshot "$out/$name.status.json" || true
            local id
            id=$(cat "$out/$name.status.json.id" 2>/dev/null || echo 1)
            local polls=$(( window * 150 / 60 ))
            for _ in $(seq 1 "$polls"); do
                printf '%s ' "$(date +%s.%N)" >> "$out/$name.probe.jsonl"
                rb-relish --output json batch-status "$id" 2>/dev/null | tr -d '\n' >> "$out/$name.probe.jsonl" || true
                echo >> "$out/$name.probe.jsonl"
                sleep 0.15
            done ;;
        none)
            sleep $(( window * 3 / 4 )); lag_snapshot "$out/$name.status.json" || true ;;
    esac
    wait "$bench" || result=$?
    cpu_snapshot "$out/$name.cpu1"
    wait "$vm" || true
    progress "$name $scenario perf=$mode exit $result"
    sleep 10
}

# idle_window NAME [cpu]: the cluster with nothing to do.
idle_window() {
    local name=$1 mode=${2:-none}
    cpu_snapshot "$out/$name.cpu0"
    vmstat -n 1 "$window" > "$out/$name.vmstat" &
    local vm=$!
    if [ "$mode" = cpu ]; then
        sleep "$third"
        perf record -F 499 -a -g -o "$out/$name.perf.data" -- sleep "$third" > /dev/null 2>&1 || true
        sleep $(( window - 2 * third ))
    else
        sleep "$window"
    fi
    cpu_snapshot "$out/$name.cpu1"
    wait "$vm" || true
    progress "$name idle perf=$mode"
}

control_window() {
    local name=$1 result=0 endpoint=https://127.0.0.1:9117
    cpu_snapshot "$out/$name.cpu0"
    timeout 900 rb-relish --output json bench --quick > "$out/$name.bench.json" 2> "$out/$name.out" || result=$?
    for _ in $(seq 1 100); do
        curl -s -o /dev/null -w '%{http_code} %{time_total}\n' --cacert "$RELIABURGER_CA_CERT" \
            -H "Authorization: Bearer $RELIABURGER_TOKEN" "$endpoint/v1/status" >> "$out/$name.api.txt" || true
    done
    cpu_snapshot "$out/$name.cpu1"
    progress "$name control exit $result"
}

for step in "$@"; do
    case $step in
        raw) bench_window raw jobs-vm-baseline ;;
        host) bench_window host jobs-host-processes ;;
        shared) bench_window shared jobs-shared-containers ;;
        fresh) bench_window fresh jobs-containers ;;
        idle) idle_window idle ;;
        control) control_window control ;;
        perfcpu-host) bench_window perfcpu-host jobs-host-processes cpu ;;
        perfoff-host) bench_window perfoff-host jobs-host-processes off ;;
        perfsched-host) bench_window perfsched-host jobs-host-processes sched ;;
        probe-host) bench_window probe-host jobs-host-processes probe ;;
        perfcpu-idle) idle_window perfcpu-idle cpu ;;
        *) fail "unknown window $step" ;;
    esac
done

# Fold, categorise and drop the bulky perf files while these binaries are installed.
cd "$out"
for data in *.perf.data; do
    [ -e "$data" ] || continue
    w=${data%.perf.data}
    case $w in
        perfoff-*)
            perf script -i "$data" --no-inline -F comm,pid,tid,time,event,trace,ip,sym,dso 2>/dev/null > "$w.script" || true
            python3 "$tools/profile_flame.py" fold-off "$w.script" --tgid "$bunpid" > "$w.bun.folded"
            python3 "$tools/profile_flame.py" fold-off "$w.script" --comm helper > "$w.helper.folded"
            python3 "$tools/profile_flame.py" top "$w.bun.folded" --n 60 > "$w.bun.top.txt" ;;
        *)
            perf script -i "$data" --no-inline -F comm,pid,tid,time,event,ip,sym,dso 2>/dev/null > "$w.script" || true
            python3 "$tools/profile_flame.py" fold-cpu "$w.script" > "$w.folded"
            python3 "$tools/profile_flame.py" fold-cpu "$w.script" --tgid "$bunpid" > "$w.bun.folded"
            python3 "$tools/profile_flame.py" top "$w.folded" --n 60 > "$w.top.txt"
            python3 "$tools/profile_flame.py" top "$w.bun.folded" --n 80 > "$w.bun.top.txt"
            python3 "$tools/profile_categories.py" "$w.folded" "$w.bun.folded" \
                --json "$w.categories.json" > "$w.categories.txt" ;;
    esac
    rm -f "$data" "$w.script"
done
for data in *.sched.data; do
    [ -e "$data" ] || continue
    w=${data%.sched.data}
    perf sched latency -i "$data" --sort max 2>/dev/null | head -n 60 > "$w.latency.txt" || true
    rm -f "$data"
done
for probe in *.probe.jsonl; do
    [ -e "$probe" ] || continue
    python3 "$tools/profile_probe.py" "$probe" > "${probe%.probe.jsonl}.timeline.txt"
done
progress DONE
