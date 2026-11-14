#!/usr/bin/env bash
# One malloc point for the day tier (#1417), run as root inside a node over
# `bash -s`, after a line that sets `bun_memory_awk` to bun-memory.awk's
# program. It prints Bun's memory by kind, attaches gdb to Bun to call
# glibc's malloc_stats() (to Bun's stderr, so the journal) and
# malloc_trim(0), waits about 5 s, prints Bun's memory again, then the
# malloc_stats lines from Bun's journal:
#
#   @@ before   bun_pid and bun_mem_* lines
#   @@ attach   start EPOCH, gdb's own output, gdb_exit N, end EPOCH
#   @@ after    bun_pid and bun_mem_* lines
#   @@ stats    Arena N: / system bytes / in use bytes / Total ... lines
#
# sustained_check.py malloc-point turns it into a malloc.jsonl row. gdb
# stops every Bun thread while it's attached; start and end bound it.
set -u
: "${bun_memory_awk:?bun_memory_awk must hold bun-memory.awk}"
pid=$(systemctl show -p MainPID --value reliaburger.service)
memory() {
    echo "bun_pid $pid"
    awk "$bun_memory_awk" /proc/"$pid"/status /proc/"$pid"/smaps_rollup /proc/"$pid"/smaps 2>/dev/null
}
echo '@@ before'
memory
# journalctl --since reads local time to the second; start a second early.
since=$(date -d '-1 second' '+%Y-%m-%d %H:%M:%S')
echo '@@ attach'
echo "start $(date +%s.%N)"
timeout 120 gdb -p "$pid" -batch -nx -iex 'set debuginfod enabled off' \
    -ex 'call (void)malloc_stats()' -ex 'call (int)malloc_trim(0)' -ex detach 2>&1 | tail -n 40
echo "gdb_exit ${PIPESTATUS[0]}"
echo "end $(date +%s.%N)"
sleep 5
echo '@@ after'
memory
echo '@@ stats'
journalctl -u reliaburger.service --no-pager -q -o cat --since "$since" 2>/dev/null \
    | grep -E '(Arena [0-9]+:|system bytes|in use bytes|Total \(incl\. mmap\):|max mmap (regions|bytes))' \
    | tail -n 200
exit 0
