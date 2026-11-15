#!/bin/sh
# a1030: named measurement runs in rb-bench-1030 (run as root, on a fresh boot).
# Adapted from the 2026-10-20 profile record's tools/run.sh: the guest home,
# the output root and the variant names (base, cur) differ; the steps don't.
set -e
h=/home/miko.guest
case $1 in
    caps)
        # caps VARIANT: one probe window, then five rounds of host + raw at cap 27,
        # from $h/bin-VARIANT.
        b=$h/bin-$2
        exec /tmp/session.sh --bun "$b/bun" --relish "$b/relish" \
            --busybox /opt/bench/busybox --out "/root/a1030/caps-$2" probe:27 rates:27 ;;
    rounds)
        # rounds VARIANT: the unchanged release script, five rounds, with the
        # pinned image's BusyBox (f1947045...) that its usage text names and the
        # 0.2.0 throughput record ran.
        b=$h/bin-$2
        exec bash $h/src-cur/scripts/release/job_bench_rounds.sh --bun "$b/bun" \
            --relish "$b/relish" --busybox /opt/bench/busybox-image --out "/root/a1030/rounds-$2" --rounds 5 ;;
    prof)
        # prof VARIANT: profiling build, one host window with perf -g at cap 27
        # (and its raw partner), then one probe window.
        b=$h/bin-$2
        exec /tmp/session.sh --bun "$b/bun" --relish "$b/relish" \
            --busybox /opt/bench/busybox --out "/root/a1030/prof-$2" perfcpu:27 perfoff:27 probe:27 ;;
    compare)
        # compare VARIANT: five rounds of host and shared windows at cap 27, each
        # with --compare against the 0.2.0 throughput record's report of the same
        # round, using that record's BusyBox (the pinned image's, f1947045...) at
        # that record's path, /opt/bench/busybox, so the comparison isn't
        # informational. The Ubuntu copy is kept as busybox-ubuntu and put back
        # by hand afterwards.
        b=$h/bin-$2
        rm -rf /tmp/v020; cp -r $h/v020 /tmp/v020
        [ -e /opt/bench/busybox-ubuntu ] || cp -p /opt/bench/busybox /opt/bench/busybox-ubuntu
        install -m 0755 /opt/bench/busybox-image /opt/bench/busybox
        exec /tmp/session.sh --bun "$b/bun" --relish "$b/relish" \
            --busybox /opt/bench/busybox --out "/root/a1030/compare-$2" compare:27 ;;
    *) echo "unknown run $1" >&2; exit 2 ;;
esac
