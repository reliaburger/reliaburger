#!/bin/bash
# a023: one three-node probe window: batch-status every 0.15 s from node 1
# during a host-job window, to see when each node's chunks are accepted.
set -u
W=$(cd "$(dirname "$0")" && pwd); out=$W/scale
on() { local n=$1; shift; limactl shell "$n" -- sudo bash -c "$*"; }
configure() {
  local n=$1 allowed=/opt/bench/busybox
  [ "$2" = yes ] || allowed=/opt/bench/other
  on "$n" "cp /etc/reliaburger/node.toml.dev /etc/reliaburger/node.toml && printf '\n[process_workloads]\nallowed_binaries = [\"$allowed\"]\nmount_isolation = false\n\n[ebpf]\nenabled = true\n' >> /etc/reliaburger/node.toml
    pkill -x bun; for i in \$(seq 1 30); do pgrep -x bun >/dev/null || break; sleep 1; done
    echo -900 > /proc/self/oom_score_adj; setsid nohup bun --cluster --config /etc/reliaburger/node.toml --listen 0.0.0.0:9117 --runtime auto >> /var/log/bun.log 2>&1 < /dev/null & disown
    sleep 2; pgrep -x bun"
}
for n in reliaburger-scale-1 reliaburger-scale-2 reliaburger-scale-3; do configure $n yes >/dev/null 2>&1; done
sleep 60
limactl shell reliaburger-scale-1 -- bash -c '
  rm -f /tmp/p.json /tmp/probe.jsonl
  relish --output json bench --scenario jobs-host-processes --exec /opt/bench/busybox --concurrency 27 --report /tmp/p.json > /tmp/p.out 2>&1 &
  b=$!; sleep 10
  id=$(for i in $(seq 200 -1 1); do relish --output json batch-status $i >/dev/null 2>&1 && { echo $i; break; }; done)
  for _ in $(seq 1 150); do printf "%s " "$(date +%s.%N)" >> /tmp/probe.jsonl; relish --output json batch-status $id 2>/dev/null | tr -d "\n" >> /tmp/probe.jsonl; echo >> /tmp/probe.jsonl; sleep 0.15; done
  wait $b; echo "batch $id"' > $out/n3-probe-host.log 2>&1
limactl shell reliaburger-scale-1 -- cat /tmp/p.json > $out/n3-probe-host.json
limactl shell reliaburger-scale-1 -- cat /tmp/probe.jsonl > $out/n3-probe-host.probe.jsonl
echo "$(date -u +%FT%TZ) n3-probe-host done" >> $out/progress.log
