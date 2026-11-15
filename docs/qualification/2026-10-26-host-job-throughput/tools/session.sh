#!/usr/bin/env bash
# a1029: profile the host-job path on the bench rig (#1029).
#
# Sets up the same single-node cluster as scripts/release/job_bench_rounds.sh
# (fresh /var/lib/reliaburger, eBPF on, allowlisted BusyBox, a 500m BusyBox web
# app beside it), then runs the named steps. Every window is a 60 s
# `relish bench` window; each records vmstat, /proc/stat and cgroup CPU deltas,
# and a mid-window batch-status snapshot (for start_lag_ms_counts).
#
# Usage (as root in the bench VM):
#   a1029-session.sh --bun B --relish R --busybox X --out DIR STEP...
# Steps:
#   rates:C      five rounds of host + raw baseline at --concurrency C, interleaved
#   host:C:N     N host windows at concurrency C (no raw)
#   perfcpu:C    one host window with system-wide perf record -g (20 s), plus one raw window
#   perfoff:C    one host window with perf record -g -e sched:sched_switch (5 s) for off-CPU
#   perfsched:C  one host window with perf sched record (5 s) for timehist
#   compare:C    a1030: five rounds of host and shared-container windows, each run with
#                --compare against the same round's 0.2.0 report in /tmp/v020
set -euo pipefail

fail() { printf 'a1029-session: %s\n' "$*" >&2; exit 1; }
bun='' relish='' busybox='' out=''
while [ "$#" -gt 0 ]; do
    case $1 in
        --bun) bun=$2; shift 2 ;;
        --relish) relish=$2; shift 2 ;;
        --busybox) busybox=$2; shift 2 ;;
        --out) out=$2; shift 2 ;;
        *) break ;;
    esac
done
[ "$(id -u)" -eq 0 ] || fail 'run as root'
[ -x "$bun" ] && [ -x "$relish" ] && [ -x "$busybox" ] || fail 'binaries'
[ -n "$out" ] && [ ! -e "$out" ] || fail '--out must be new'
steps=("$@")

image='public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028'
work=/root/rb-bench-rounds
install -m 0755 "$bun" /usr/local/bin/rb-bun
install -m 0755 "$relish" /usr/local/bin/rb-relish
systemctl stop rb-bun 2>/dev/null || true
rm -rf /var/lib/reliaburger "$work"
mkdir -p "$work" "$out"
sha256sum /usr/local/bin/rb-bun /usr/local/bin/rb-relish "$busybox" > "$out/binaries.txt"
{ uname -r; runc --version | head -n 1; nproc; } >> "$out/binaries.txt"

cd "$work"
rb-relish init cluster --cluster-name bench --node-id node-01 > init.log 2>&1
python3 - "$busybox" <<'EOF'
import sys
path = 'cluster/reliaburger.toml'
text = open(path).read()
text = text.replace('[process_workloads]\nallowed_binaries = []',
                    '[process_workloads]\nallowed_binaries = ["%s"]\nmount_isolation = false' % sys.argv[1])
text = text.replace('[ebpf]\nenabled = false', '[ebpf]\nenabled = true')
open(path, 'w').write(text)
EOF
systemd-run --unit=rb-bun --collect --property=WorkingDirectory="$work" --property=OOMScoreAdjust=-900 \
    --property=StandardOutput=append:"$work/bun.log" --property=StandardError=append:"$work/bun.log" \
    /usr/local/bin/rb-bun --cluster --runtime auto --config cluster/reliaburger.toml
trap 'systemctl stop rb-bun 2>/dev/null || true' EXIT
export RELIABURGER_CA_CERT=$work/cluster/identity/root-ca.crt
for _ in $(seq 1 60); do
    token=$(rb-relish token create --name bench-admin --role admin 2>/dev/null) && break
    sleep 2
done
[ -n "${token:-}" ] || fail 'no admin token; see bun.log'
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
sleep 30
bunpid=$(systemctl show -p MainPID --value rb-bun)
echo "bun pid $bunpid" >> "$out/progress.log"

cpu_snapshot() { # $1 file
    { echo "## proc_stat"; head -n 1 /proc/stat
      echo "## bun_stat"; cut -d' ' -f14-17 "/proc/$bunpid/stat"
      echo "## cgroups"
      find /sys/fs/cgroup -maxdepth 4 -name cpu.stat | while read -r f; do
          printf '%s %s\n' "${f%/cpu.stat}" "$(awk '$1=="usage_usec"{print $2}' "$f")"
      done
      echo "## schedstat"; awk '{print $1, $2}' "/proc/$bunpid/schedstat"
      echo "## ctxt"; grep -E '^(ctxt|processes)' /proc/stat
      echo "## psi_cpu"; cat /proc/pressure/cpu
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

# window NAME SCENARIO CONCURRENCY [PERF-MODE]
window() {
    local name=$1 scenario=$2 conc=$3 perf=${4:-none} extra=() result=0
    case $scenario in jobs-host-processes|jobs-vm-baseline) extra=(--exec "$busybox") ;; esac
    [ -n "${COMPARE:-}" ] && extra+=(--compare "$COMPARE")
    cpu_snapshot "$out/$name.cpu0"
    vmstat -n 1 62 > "$out/$name.vmstat" &
    local vm=$!
    rb-relish --output json bench --scenario "$scenario" --concurrency "$conc" \
        --report "$out/$name.json" "${extra[@]}" > "$out/$name.out" 2>&1 &
    local bench=$!
    case $perf in
        cpu)
            sleep 20
            perf record -F 499 -a -g -o "$out/$name.perf.data" -- sleep 20 > /dev/null 2>&1 ;;
        off)
            sleep 25
            perf record -a -g -e sched:sched_switch -o "$out/$name.perf.data" -- sleep 5 > /dev/null 2>&1 ;;
        sched)
            sleep 25
            perf sched record -a -o "$out/$name.sched.data" -- sleep 5 > /dev/null 2>&1 ;;
        none)
            sleep 45; lag_snapshot "$out/$name.status.json" || true ;;
        probe)
            sleep 8
            lag_snapshot "$out/$name.status.json" || true
            local id
            id=$(cat "$out/$name.status.json.id" 2>/dev/null || echo 1)
            for _ in $(seq 1 150); do
                printf '%s ' "$(date +%s.%N)" >> "$out/$name.probe.jsonl"
                rb-relish --output json batch-status "$id" 2>/dev/null | tr -d '\n' >> "$out/$name.probe.jsonl" || true
                echo >> "$out/$name.probe.jsonl"
                sleep 0.15
            done ;;
    esac
    wait "$bench" || result=$?
    cpu_snapshot "$out/$name.cpu1"
    wait "$vm" || true
    echo "$(date -u +%FT%TZ) $name $scenario c=$conc perf=$perf exit $result" >> "$out/progress.log"
    sleep 10
}

for step in "${steps[@]}"; do
    IFS=: read -r kind conc n <<< "$step"
    case $kind in
        rates)
            for r in 1 2 3 4 5; do
                if [ $((r % 2)) -eq 1 ]; then
                    window "c$conc-r$r-host" jobs-host-processes "$conc"
                    window "c$conc-r$r-raw" jobs-vm-baseline "$conc"
                else
                    window "c$conc-r$r-raw" jobs-vm-baseline "$conc"
                    window "c$conc-r$r-host" jobs-host-processes "$conc"
                fi
            done ;;
        host)
            for r in $(seq 1 "$n"); do window "c$conc-r$r-host" jobs-host-processes "$conc"; done ;;
        perfcpu)
            window "c$conc-perfcpu-host" jobs-host-processes "$conc" cpu
            window "c$conc-perfcpu-raw" jobs-vm-baseline "$conc" cpu ;;
        perfoff)
            window "c$conc-perfoff-host" jobs-host-processes "$conc" off ;;
        perfsched)
            window "c$conc-perfsched-host" jobs-host-processes "$conc" sched ;;
        probe)
            window "c$conc-probe-host" jobs-host-processes "$conc" probe ;;
        compare)
            for r in 1 2 3 4 5; do
                COMPARE=/tmp/v020/r$r-jobs-host-processes.json window "c$conc-r$r-host-vs020" jobs-host-processes "$conc"
                COMPARE=/tmp/v020/r$r-jobs-shared-containers.json window "c$conc-r$r-shared-vs020" jobs-shared-containers "$conc"
            done ;;
        *) fail "unknown step $step" ;;
    esac
done
cp "$work/bun.log" "$out/bun.log" 2>/dev/null || true
echo "$(date -u +%FT%TZ) DONE" >> "$out/progress.log"
