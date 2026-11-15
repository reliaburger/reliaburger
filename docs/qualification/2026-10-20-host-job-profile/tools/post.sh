#!/bin/sh
# a1029: turn the profiling session's perf data into folded stacks, tops and SVGs.
set -e
d=/root/a1029/prof
cd "$d"
bunpid=$(awk '/bun pid/{print $3}' progress.log)
for w in c27-perfcpu-host c27-perfcpu-raw c32-perfcpu-host; do
    [ -s "$w.script" ] || perf script -i "$w.perf.data" --no-inline -F comm,pid,tid,time,event,ip,sym,dso 2>/dev/null > "$w.script"
    python3 /tmp/flame.py fold-cpu "$w.script" > "$w.folded"
    python3 /tmp/flame.py fold-cpu "$w.script" --tgid "$bunpid" > "$w.bun.folded"
    python3 /tmp/flame.py top "$w.folded" --n 60 > "$w.top.txt"
    python3 /tmp/flame.py top "$w.bun.folded" --n 80 > "$w.bun.top.txt"
done
w=c27-perfoff-host
[ -s "$w.script" ] || perf script -i "$w.perf.data" --no-inline -F comm,pid,tid,time,event,trace,ip,sym,dso 2>/dev/null > "$w.script"
python3 /tmp/flame.py fold-off "$w.script" --tgid "$bunpid" > "$w.bun.folded"
python3 /tmp/flame.py top "$w.bun.folded" --n 80 > "$w.bun.top.txt"
w=c27-perfsched-host
perf sched timehist -i "$w.sched.data" -s 2>/dev/null | tail -n 120 > "$w.timehist-summary.txt" || true
perf sched latency -i "$w.sched.data" --sort max 2>/dev/null | head -n 60 > "$w.latency.txt" || true
ls -la "$d"
