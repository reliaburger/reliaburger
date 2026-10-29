#!/bin/bash
# a023 scale-out (#1031): host-job windows on a three-node `relish dev` cluster
# of 4 vCPU / 6 GiB VMs on this Mac, with 3, 2 and 1 nodes eligible. A node is
# made ineligible by an allowlist that doesn't name the BusyBox: it then answers
# every array with zero slots, so the council stays three voters throughout.
#   scale.sh OUTDIR
set -u
W=$(cd "$(dirname "$0")" && pwd)
out=$1; mkdir -p "$out"
relish_mac=$W/cand/relish-macos-aarch64
nodes=(reliaburger-scale-1 reliaburger-scale-2 reliaburger-scale-3)
log() { echo "$(date -u +%FT%TZ) $*" | tee -a "$out/progress.log"; }
on() { local n=$1; shift; limactl shell "$n" -- sudo bash -c "$*"; }

if ! limactl list --format '{{.Name}}' | grep -qx reliaburger-scale-3; then
  log "dev create"
  "$relish_mac" dev create scale --nodes 3 --cpus 4 --memory 6GiB \
      --bun "$W/cand/bun-linux-aarch64" --relish "$W/cand/relish-linux-aarch64" > "$out/dev-create.log" 2>&1 \
      || { log "dev create failed"; exit 1; }
fi
log "dev create done"

# BusyBox: the same Ubuntu copy the single-node cap sessions run (52151e7f...).
for n in "${nodes[@]}"; do
  limactl copy "$W/busybox-ubuntu" "$n:/tmp/busybox" && on "$n" 'install -D -m 0755 /tmp/busybox /opt/bench/busybox; install -m 0755 /tmp/busybox /opt/bench/other'
  on "$n" 'systemctl disable --now unattended-upgrades apt-daily.timer apt-daily-upgrade.timer dpkg-db-backup.timer logrotate.timer sysstat-collect.timer 2>/dev/null; cp /etc/reliaburger/node.toml /etc/reliaburger/node.toml.dev; true'
  on "$n" '{ uname -r; nproc; free -m | sed -n 2p; runc --version | head -1; sha256sum /usr/local/bin/bun /usr/local/bin/relish /opt/bench/busybox; }' > "$out/env-$n.txt" 2>&1
done

# configure NODE ELIGIBLE(yes|no): rewrite the node's config and restart Bun.
configure() {
  local n=$1 allowed=/opt/bench/busybox
  [ "$2" = yes ] || allowed=/opt/bench/other
  on "$n" "cp /etc/reliaburger/node.toml.dev /etc/reliaburger/node.toml && printf '\n[process_workloads]\nallowed_binaries = [\"$allowed\"]\nmount_isolation = false\n\n[ebpf]\nenabled = true\n' >> /etc/reliaburger/node.toml
    pkill -x bun; for i in \$(seq 1 30); do pgrep -x bun >/dev/null || break; sleep 1; done
    echo -900 > /proc/self/oom_score_adj; setsid nohup bun --cluster --config /etc/reliaburger/node.toml --listen 0.0.0.0:9117 --runtime auto >> /var/log/bun.log 2>&1 < /dev/null & disown
    sleep 2; pgrep -x bun"
}
quorum() { # wait until every node answers with one leader and three voters
  for i in $(seq 1 60); do
    limactl shell reliaburger-scale-1 -- relish --output json council status > "$out/council.tmp" 2>/dev/null \
      && python3 - "$out/council.tmp" <<'PY' && return 0
import json, sys
nodes = json.load(open(sys.argv[1]))["nodes"]
ok = len(nodes) == 3 and all(n.get("error") is None for n in nodes) \
    and len({n.get("leader") for n in nodes}) == 1 and None not in {n.get("leader") for n in nodes}
sys.exit(0 if ok else 1)
PY
    sleep 5
  done
  return 1
}
snap() { # per node: bun CPU ticks (utime stime), RSS, and the VM's /proc/stat cpu line
  for n in "${nodes[@]}"; do
    echo "## $n"; on "$n" 'p=$(pgrep -x bun | head -1); echo "bun $(cut -d" " -f14,15 /proc/$p/stat) rss_kb $(awk "/VmRSS/{print \$2}" /proc/$p/status)"; head -1 /proc/stat'
  done
}
window() { # NAME
  local name=$1
  limactl shell reliaburger-scale-1 -- relish --output json council status > "$out/$name.council0.json" 2>&1
  snap > "$out/$name.snap0" 2>&1
  limactl shell reliaburger-scale-1 -- bash -c "rm -f /tmp/w.json; relish --output json bench --scenario jobs-host-processes --exec /opt/bench/busybox --concurrency 27 --report /tmp/w.json" > "$out/$name.out" 2>&1; local rc=$?
  snap > "$out/$name.snap1" 2>&1
  limactl shell reliaburger-scale-1 -- relish --output json council status > "$out/$name.council1.json" 2>&1
  limactl shell reliaburger-scale-1 -- cat /tmp/w.json > "$out/$name.json" 2>/dev/null
  log "$name exit $rc"
  sleep 10
}

# The 500m BusyBox web app beside the jobs, as job_bench_rounds.sh runs it.
cat > "$W/app.toml" <<'TOML'
[app.web]
image = "public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028"
command = ["/bin/busybox", "httpd", "-f", "-p", "8080", "-h", "/etc"]
port = 8080
cpu = "500m"
memory = "64Mi"
TOML

for count in ${COUNTS:-3 2 1}; do
  log "configure: $count eligible"
  for i in 0 1 2; do
    if [ "$i" -lt "$count" ]; then configure "${nodes[i]}" yes; else configure "${nodes[i]}" no; fi > /dev/null 2>&1
  done
  sleep 15
  quorum || log "quorum not confirmed after configure ($count)"
  if [ ! -e "$out/app-applied" ]; then
    limactl copy "$W/app.toml" reliaburger-scale-1:/tmp/app.toml && limactl shell reliaburger-scale-1 -- relish apply /tmp/app.toml > "$out/app-apply.log" 2>&1 && touch "$out/app-applied"
  fi
  sleep 30
  for r in 1 2 3 4 5; do window "n$count-r$r-host"; done
done
for n in "${nodes[@]}"; do on "$n" 'cat /var/log/bun.log' > "$out/bun-$n.log" 2>&1; done
log "DONE"
