#!/bin/sh
# a1029: named measurement runs in rb-bench-1029 (run as root).
set -e
case $1 in
    prof)
        exec /tmp/session.sh --bun /home/marek.guest/t-prof/release/bun --relish /home/marek.guest/t-prof/release/relish \
            --busybox /opt/bench/busybox --out /root/a1029/prof host:27:2 perfcpu:27 perfoff:27 perfsched:27 perfcpu:32 ;;
    baseline)
        # The unchanged release script, the stripped build.yml-style binaries.
        b=/home/marek.guest/t-rel/release
        [ -x /home/marek.guest/bin-rel/bun ] && b=/home/marek.guest/bin-rel
        exec bash /home/marek.guest/src/scripts/release/job_bench_rounds.sh --bun "$b/bun" \
            --relish "$b/relish" --busybox /opt/bench/busybox --out "/root/a1029/baseline-$2" --rounds "${3:-5}" ;;
    caps)
        # caps VARIANT: VARIANT is rel, e1 or e2, from /home/marek.guest/bin-VARIANT.
        # rel runs the raised- and lowered-cap rounds; the experiments run cap 27 only.
        b=/home/marek.guest/bin-$2
        case $2 in
            rel) steps="probe:27 rates:27 rates:32 rates:16" ;;
            *) steps="probe:27 rates:27" ;;
        esac
        exec /tmp/session.sh --bun "$b/bun" --relish "$b/relish" \
            --busybox /opt/bench/busybox --out "/root/a1029/caps-$2" $steps ;;
    *) echo "unknown run $1" >&2; exit 2 ;;
esac
