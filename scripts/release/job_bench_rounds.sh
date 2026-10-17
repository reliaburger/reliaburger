#!/usr/bin/env bash
# Five rotating rounds of the four `relish bench` job scenarios on a fresh
# single-node cluster, beside a 500m BusyBox web app: the "independent
# five-round rerun" recorded in
# docs/qualification/2026-10-09-timed-job-scenarios/README.md, whose reports
# are committed under five-round-rerun/. Re-run it on a release candidate to
# re-record the published throughput table, then compare each scenario with
# `relish bench --scenario S --compare OLD.json`.
#
# Run as root inside a disposable Linux VM (4 vCPU and 8 GiB for the recorded
# rig): it replaces /var/lib/reliaburger and runs Bun as the transient unit
# rb-bun. Nothing else may run during the rounds.
#
# Usage:
#   job_bench_rounds.sh --bun PATH --relish PATH --busybox PATH --out DIR [--rounds N]
#
#   --bun, --relish  the candidate's binaries (installed as rb-bun, rb-relish)
#   --busybox        the BusyBox executable host jobs and the raw baseline run;
#                    use the copy from the pinned image (sha256 f1947045...)
#   --out            new directory for the reports and progress.log
#   --rounds         rounds to run (default 5); each runs every scenario once
#                    for 60 s, in an order that rotates by one every round
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
fail() { printf 'job_bench_rounds: %s\n' "$*" >&2; exit 1; }

bun='' relish='' busybox='' out='' rounds=5
while [ "$#" -gt 0 ]; do
    case $1 in
        --bun) bun=${2:-}; shift 2 ;;
        --relish) relish=${2:-}; shift 2 ;;
        --busybox) busybox=${2:-}; shift 2 ;;
        --out) out=${2:-}; shift 2 ;;
        --rounds) rounds=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown argument: $1" ;;
    esac
done
[ -x "$bun" ] && [ -x "$relish" ] && [ -x "$busybox" ] || fail '--bun, --relish and --busybox must be executables'
[ -n "$out" ] && [ ! -e "$out" ] || fail '--out must name a new directory'
[[ $rounds =~ ^[1-9][0-9]*$ ]] || fail '--rounds must be a positive number'
[ "$(id -u)" -eq 0 ] || fail 'run as root inside a disposable VM'
case $busybox in /*) ;; *) fail '--busybox must be an absolute path' ;; esac

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
[ -n "${token:-}" ] || fail 'Bun never issued an admin token; see bun.log'
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

scenarios=(jobs-vm-baseline jobs-containers jobs-shared-containers jobs-host-processes)
for (( round = 1; round <= rounds; round++ )); do
    for i in 0 1 2 3; do
        scenario=${scenarios[$(( (i + round - 1) % 4 ))]}
        extra=()
        case $scenario in jobs-host-processes|jobs-vm-baseline) extra=(--exec "$busybox") ;; esac
        result=0
        rb-relish --output json bench --scenario "$scenario" --report "$out/r$round-$scenario.json" "${extra[@]}" \
            > "$out/r$round-$scenario.out" 2>&1 || result=$?
        echo "$(date -u +%FT%TZ) round $round $scenario exit $result" >> "$out/progress.log"
        sleep 10
    done
done
echo "$(date -u +%FT%TZ) DONE" >> "$out/progress.log"
