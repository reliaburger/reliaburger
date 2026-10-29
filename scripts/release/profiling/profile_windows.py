#!/usr/bin/env python3
"""Summarise the release profile's measurement windows.

  profile_windows.py window DIR NAME          one window's summary, as JSON
  profile_windows.py collect ROOT [--out F]   every session under ROOT, as JSON

A window is a set of files named after it in one session directory, written
by profile_session.sh inside the bench VM:

  NAME.json        the `relish bench --report` (job scenarios only)
  NAME.out         bench's progress lines, for the steady rate
  NAME.cpu0/.cpu1  /proc/stat, Bun's CPU, threads, wake-ups and RSS, cgroup CPU,
                   before and after the window
  NAME.vmstat      one line a second
  NAME.status.json a mid-window `relish batch-status` (start lag)
  NAME.bench.json  `relish bench --quick` (the control-plane window)
  NAME.api.txt     one API round trip per line (the control-plane window)

ROOT holds one directory per version label, each with one directory per
round (r1, r2, ...) and a perf directory for the profiling pass:
ROOT/previous/r1/host.json, ROOT/candidate/perf/perfcpu-host.folded, ...

Adapted from the 2026-10-20 host-job profile record's tools/analyse.py.
"""
import argparse
import json
import os
import re
import statistics

HZ = 100  # USER_HZ: /proc/stat and /proc/PID/stat count in these ticks

# Steady rate: accepted successes between these seconds of a window. The
# window's first seconds include submission and cold starts, its last the
# cutoff (the record's "quanta" lesson: medians move in whole grant cycles).
STEADY_FROM, STEADY_TO = 15.0, 58.0


def parse_cpu(text):
    """Parse a profile_session.sh CPU snapshot into numbers."""
    section, out = None, {"cgroups": {}}
    for line in text.splitlines():
        if line.startswith("## "):
            section = line[3:].strip()
            continue
        fields = line.split()
        if not fields:
            continue
        if section == "clock":
            out["clock"] = float(fields[0])
        elif section == "proc_stat" and fields[0] == "cpu":
            out["stat"] = [int(v) for v in fields[1:]]  # user nice system idle iowait irq softirq steal
        elif section == "bun_stat" and len(fields) >= 4:
            values = [int(v) for v in fields[:4]]
            out["bun_ticks"] = values[0] + values[1]  # utime + stime, every thread
        elif section == "bun_status" and len(fields) == 2 and fields[1].isdigit():
            out["bun_" + fields[0]] = int(fields[1])
        elif section == "cgroups" and len(fields) >= 2 and fields[-1].isdigit():
            out["cgroups"][" ".join(fields[:-1])] = int(fields[-1])
        elif section == "ctxt" and len(fields) == 2 and fields[1].isdigit():
            out[fields[0]] = int(fields[1])
    return out


def steady_rate(text, start=STEADY_FROM, end=STEADY_TO):
    """Accepted successes a second between `start` and `end`, from bench's progress lines."""
    points = []
    for line in text.splitlines():
        match = re.match(r"^([\d.]+)s: (\d+) unique accepted successes", line)
        if match:
            points.append((float(match.group(1)), int(match.group(2))))
    after = [p for p in points if p[0] >= start]
    before = [p for p in points if p[0] <= end]
    if not after or not before or before[-1][0] - after[0][0] < (end - start) / 2:
        return None
    return (before[-1][1] - after[0][1]) / (before[-1][0] - after[0][0])


def parse_vmstat(text, skip=15, keep=40):
    """Mean vmstat columns over the steady part of the window."""
    rows = []
    for line in text.splitlines():
        fields = line.split()
        if len(fields) >= 17 and fields[0].isdigit():
            rows.append([int(v) for v in fields])
    rows = rows[skip:skip + keep] or rows
    if not rows:
        return {}
    columns = {"r": 0, "b": 1, "in": 10, "cs": 11, "us": 12, "sy": 13, "id": 14, "wa": 15, "st": 16}
    return {name: statistics.mean(row[i] for row in rows) for name, i in columns.items()}


def start_lag_counts(status):
    """Sum every node's start_lag_ms_counts histogram in a batch-status document."""
    counts = None

    def walk(value):
        nonlocal counts
        if isinstance(value, dict):
            for key, inner in value.items():
                if key == "start_lag_ms_counts" and isinstance(inner, list):
                    counts = [a + b for a, b in zip(counts or [0] * len(inner), inner)]
                else:
                    walk(inner)
        elif isinstance(value, list):
            for inner in value:
                walk(inner)

    walk(status)
    return counts


def histogram_percentile(counts, q):
    """Upper bound in ms of the bucket holding quantile q (bucket b is (2^(b-1), 2^b] ms)."""
    total = sum(counts or [])
    if not total:
        return None
    seen = 0
    for bucket, count in enumerate(counts):
        seen += count
        if seen >= q * total:
            return 2 ** bucket
    return None


def percentile(values, q):
    """Nearest-rank percentile of a non-empty list."""
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, int(round(q * len(ordered) + 0.5)) - 1))
    return ordered[index]


def cpu_delta(before, after):
    """CPU accounting between two snapshots, or {} when either lacks /proc/stat."""
    if "stat" not in before or "stat" not in after:
        return {}
    ticks = [b - a for a, b in zip(before["stat"], after["stat"])]
    total = sum(ticks[:8]) or 1
    busy = total - ticks[3] - ticks[4]
    elapsed = after.get("clock", 0.0) - before.get("clock", 0.0)
    out = {
        "cpu_util": busy / total,
        "cpu_busy_seconds": busy / HZ,
        "iowait": ticks[4] / total,
    }
    if elapsed > 0:
        out["elapsed_seconds"] = elapsed
    if "bun_ticks" in before and "bun_ticks" in after:
        bun_seconds = (after["bun_ticks"] - before["bun_ticks"]) / HZ
        out["bun_cpu_seconds"] = bun_seconds
        if elapsed > 0:
            out["bun_cpu_cores"] = bun_seconds / elapsed
    if "bun_ctxt" in before and "bun_ctxt" in after and elapsed > 0:
        out["bun_wakeups_per_second"] = (after["bun_ctxt"] - before["bun_ctxt"]) / elapsed
    for key in ("bun_rss_kb", "bun_hwm_kb", "bun_threads"):
        if key in after:
            out[key] = after[key]
    if "processes" in before and "processes" in after:
        forks = after["processes"] - before["processes"]
        out["forks"] = forks
        if elapsed > 0:
            out["forks_per_second"] = forks / elapsed
    if "ctxt" in before and "ctxt" in after and elapsed > 0:
        out["context_switches_per_second"] = (after["ctxt"] - before["ctxt"]) / elapsed
    return out


def read(path):
    with open(path, errors="replace") as source:
        return source.read()


def summarise_window(directory, name):
    """Everything one window's files say, as a flat dict."""
    def path(suffix):
        return os.path.join(directory, name + suffix)

    out = {"name": name}
    if os.path.exists(path(".json")):
        report = json.loads(read(path(".json")))
        out.update(
            scenario=report.get("scenario"),
            rate=report.get("successes_per_second") or 0.0,
            accepted=report.get("unique_accepted_successes") or 0,
            complete=report.get("measurement_complete"),
            failures=report.get("terminal_failures"),
            retries=report.get("accepted_retries"),
            concurrency=report.get("concurrency"),
            cleanup_verified=report.get("cleanup_verified"),
            error=report.get("error"),
            seconds=report.get("requested_seconds"),
        )
    if os.path.exists(path(".out")):
        steady = steady_rate(read(path(".out")))
        if steady is not None:
            out["steady_rate"] = steady
    if os.path.exists(path(".cpu0")) and os.path.exists(path(".cpu1")):
        out.update(cpu_delta(parse_cpu(read(path(".cpu0"))), parse_cpu(read(path(".cpu1")))))
    if os.path.exists(path(".vmstat")):
        out["vmstat"] = parse_vmstat(read(path(".vmstat")))
    if os.path.exists(path(".status.json")):
        try:
            counts = start_lag_counts(json.loads(read(path(".status.json"))))
        except ValueError:
            counts = None
        if counts:
            out["start_lag_p50_ms"] = histogram_percentile(counts, 0.5)
            out["start_lag_p99_ms"] = histogram_percentile(counts, 0.99)
    if os.path.exists(path(".bench.json")):
        try:
            bench = json.loads(read(path(".bench.json")))
        except ValueError:
            bench = {}
        out["control_metrics"] = {m["name"]: {"value": m["value"], "unit": m["unit"],
                                              "higher_is_better": m.get("higher_is_better", False)}
                                  for m in bench.get("metrics", [])}
        out["control_failed"] = [f.get("name") for f in bench.get("failed", [])]
    if os.path.exists(path(".api.txt")):
        latencies = []
        for line in read(path(".api.txt")).splitlines():
            fields = line.split()
            if len(fields) == 2 and fields[0] == "200":
                latencies.append(float(fields[1]) * 1000)
        if latencies:
            out["api_p50_ms"] = percentile(latencies, 0.5)
            out["api_p99_ms"] = percentile(latencies, 0.99)
            out["api_requests"] = len(latencies)
    return out


def window_names(directory):
    names = set()
    for entry in os.listdir(directory):
        match = re.match(r"^(.+?)\.(json|cpu0|bench\.json|api\.txt)$", entry)
        if match and not entry.endswith(".status.json"):
            names.add(match.group(1))
    return sorted(names)


def collect(root):
    """{label: {"rounds": {round: {window: summary}}}} for every session under ROOT."""
    versions = {}
    for label in sorted(os.listdir(root)):
        version_dir = os.path.join(root, label)
        if not os.path.isdir(version_dir):
            continue
        rounds = {}
        for entry in sorted(os.listdir(version_dir)):
            match = re.fullmatch(r"r(\d+)", entry)
            session = os.path.join(version_dir, entry)
            if not match or not os.path.isdir(session):
                continue
            rounds[int(match.group(1))] = {name: summarise_window(session, name) for name in window_names(session)}
        perf = os.path.join(version_dir, "perf")
        perf_windows = {}
        if os.path.isdir(perf):
            perf_windows = {name: summarise_window(perf, name) for name in window_names(perf)}
        versions[label] = {"rounds": rounds, "perf": perf_windows}
    return versions


def usable(summary):
    """A window whose bench report finished without an error and completed its measurement."""
    return not summary.get("error") and summary.get("complete") is not False


def values(collected, label, window, field, usable_only=False):
    """One field of one window across a version's rounds, in round order; missing rounds skipped.

    With usable_only, a window whose bench failed (an error, or an incomplete
    measurement) is skipped too: its rate of 0 would read as a regression.
    """
    rounds = collected.get(label, {}).get("rounds", {})
    out = []
    for number in sorted(rounds):
        summary = rounds[number].get(window, {})
        if usable_only and not usable(summary):
            continue
        value = summary.get(field)
        if isinstance(value, (int, float)):
            out.append(value)
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    one = sub.add_parser("window")
    one.add_argument("directory")
    one.add_argument("name")
    every = sub.add_parser("collect")
    every.add_argument("root")
    every.add_argument("--out")
    args = parser.parse_args()
    if args.command == "window":
        print(json.dumps(summarise_window(args.directory, args.name), indent=2, sort_keys=True))
        return
    result = json.dumps(collect(args.root), indent=2, sort_keys=True)
    if args.out:
        with open(args.out, "w") as out:
            out.write(result + "\n")
    else:
        print(result)


if __name__ == "__main__":
    main()
