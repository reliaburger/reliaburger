#!/usr/bin/env python3
"""a1029: print the probe's timeline: held tasks, accepted successes, attempts started."""
import json, sys

t0 = prev = None
print("   t(s)   held  accepted  started  started/s  active")
for line in open(sys.argv[1]):
    ts, _, body = line.partition(" ")
    try:
        s = json.loads(body)
    except ValueError:
        continue
    t = float(ts)
    t0 = t if t0 is None else t0
    node = (s.get("nodes") or [{}])[0].get("counters", {})
    started = node.get("attempts_started", 0)
    rate = ""
    if prev:
        rate = f"{(started - prev[1]) / (t - prev[0]):9.0f}"
    prev = (t, started)
    print(f"{t - t0:7.2f} {s.get('held', 0):6d} {s.get('succeeded', 0):9d} {started:8d} {rate:>10} {node.get('active_commands', '')!s:>7}")
