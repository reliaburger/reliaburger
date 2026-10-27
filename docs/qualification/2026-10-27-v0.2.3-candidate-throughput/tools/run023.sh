#!/bin/sh
# a023: named measurement runs for the 0.2.3 candidate in rb-bench-1030 (run as
# root, on a fresh boot). Adapted from the 2026-10-26 throughput record's
# tools/run.sh: the binaries are the released Linux aarch64 assets (cand: the
# staged 0.2.3 candidate, 022: v0.2.2), not VM builds, and the output root is
# /root/a023.
set -e
h=/home/miko.guest
case $1 in
    caps)
        b=$h/bin-$2
        exec $h/session.sh --bun "$b/bun" --relish "$b/relish" \
            --busybox /opt/bench/busybox --out "/root/a023/caps-$2" probe:27 rates:27 ;;
    rounds)
        b=$h/bin-$2
        exec bash $h/job_bench_rounds.sh --bun "$b/bun" \
            --relish "$b/relish" --busybox /opt/bench/busybox-image --out "/root/a023/rounds-$2" --rounds 5 ;;
    *) echo "unknown run $1" >&2; exit 2 ;;
esac
