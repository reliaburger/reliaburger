#!/usr/bin/env python3
"""Turn a probe window's `relish batch-status` samples into a timeline.

  profile_probe.py PROBE_JSONL > timeline.txt

Each input line is "UNIX_SECONDS JSON", one `relish --output json batch-status`
every 0.15 s. Each output row shows the tasks the node held, the successes the
leader accepted, attempts started (and their rate since the last row) and the
node's active commands: the grant cycle as the node sees it. From the
2026-10-20 host-job profile record's tools/probe.py.
"""
import json
import sys

HEADER = "   t(s)   held  accepted  started  started/s  active"


def timeline(lines):
    """Return the timeline's rows, header first; lines that aren't samples are skipped."""
    rows = [HEADER]
    t0 = prev = None
    for line in lines:
        ts, _, body = line.partition(" ")
        try:
            t = float(ts)
            status = json.loads(body)
        except ValueError:
            continue
        if not isinstance(status, dict):
            continue
        t0 = t if t0 is None else t0
        node = (status.get("nodes") or [{}])[0].get("counters", {})
        started = node.get("attempts_started", 0)
        rate = ""
        if prev and t > prev[0]:
            rate = f"{(started - prev[1]) / (t - prev[0]):9.0f}"
        prev = (t, started)
        rows.append(f"{t - t0:7.2f} {status.get('held', 0):6d} {status.get('succeeded', 0):9d} "
                    f"{started:8d} {rate:>10} {node.get('active_commands', '')!s:>7}")
    return rows


def main():
    with open(sys.argv[1]) as source:
        print("\n".join(timeline(source)))


if __name__ == "__main__":
    main()
