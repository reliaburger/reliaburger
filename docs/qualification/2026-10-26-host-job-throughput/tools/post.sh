#!/bin/sh
# a1030: turn the profiling session's perf data into folded stacks, tops, the
# category tables, a 0.1 s timeline and SVGs. Adapted from the 2026-10-20 profile
# record's tools/post.sh: the directory, the window names and the task rates
# (read from each window's report) differ; the steps don't.
set -e
d=/root/a1030/prof-curprof
cd "$d"
bunpid=$(awk '/bun pid/{print $3}' progress.log)
rate() { python3 -c "import json,sys; r=json.load(open(sys.argv[1])); print(r.get('successes_per_second') or 0)" "$1"; }
for w in c27-perfcpu-host c27-perfcpu-raw; do
    [ -s "$w.script" ] || perf script -i "$w.perf.data" --no-inline -F comm,pid,tid,time,event,ip,sym,dso 2>/dev/null > "$w.script"
    python3 /tmp/flame.py fold-cpu "$w.script" > "$w.folded"
    python3 /tmp/flame.py fold-cpu "$w.script" --tgid "$bunpid" > "$w.bun.folded"
    python3 /tmp/flame.py top "$w.folded" --n 60 > "$w.top.txt"
    python3 /tmp/flame.py top "$w.bun.folded" --n 80 > "$w.bun.top.txt"
    python3 /tmp/flame.py svg "$w.folded" --title "$w, whole VM" > "$w.svg"
    python3 /tmp/flame.py svg "$w.bun.folded" --title "$w, Bun only" > "$w.bun.svg"
done
# The task rate for the per-task column is the forks per second the window's
# CPU accounting saw, as in the profile record (analyse.py's forks/s).
python3 /tmp/categorise.py c27-perfcpu-host.folded c27-perfcpu-host.bun.folded "$(python3 /tmp/analyse.py . | awk '/^c27-perfcpu-host /{for(i=1;i<=NF;i++) if($i ~ /^forks\/s=/){sub("forks/s=","",$i); print $i}}')" > c27-host-categories.txt
python3 /tmp/categorise.py c27-perfcpu-raw.folded c27-perfcpu-raw.bun.folded > c27-raw-categories.txt
python3 /tmp/flame.py timeline c27-perfcpu-host.script --bucket 0.1 --pattern task_array_node --pattern ReusablePool > c27-perfcpu-host.timeline.txt
w=c27-perfoff-host
[ -s "$w.script" ] || perf script -i "$w.perf.data" --no-inline -F comm,pid,tid,time,event,trace,ip,sym,dso 2>/dev/null > "$w.script"
python3 /tmp/flame.py fold-off "$w.script" --tgid "$bunpid" > "$w.bun.folded"
python3 /tmp/flame.py fold-off "$w.script" --comm helper > "$w.helper.folded"
python3 /tmp/flame.py top "$w.bun.folded" --n 80 > "$w.bun.top.txt"
python3 /tmp/flame.py top "$w.helper.folded" --n 60 > "$w.helper.top.txt"
python3 /tmp/flame.py svg "$w.helper.folded" --title "helpers off-CPU" --unit ns > "$w.helper.svg"
python3 /tmp/analyse.py . > summary.txt
ls -la "$d"
