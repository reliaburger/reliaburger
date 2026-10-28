#!/usr/bin/env python3
"""a023: a multi-node probe's timeline: each node's accepted successes
(the leader's per-node counters) at every sample, and the steps between them.

  probe_nodes.py PROBE_JSONL
"""
import json, sys

t0 = None
last = {}
print("   t(s)  " + "  ".join(f"node {i} accepted (step)" for i in (1, 2, 3)))
for line in open(sys.argv[1]):
    ts, _, body = line.partition(" ")
    try:
        status = json.loads(body)
    except ValueError:
        continue
    t = float(ts)
    t0 = t if t0 is None else t0
    cols = []
    for node in status["nodes"]:
        name = node["node"]
        done = node.get("counters", {}).get("succeeded", 0)
        step = done - last.get(name, done)
        last[name] = done
        cols.append(f"{done:10d} ({step:+6d})")
    print(f"{t - t0:7.2f}  " + "  ".join(cols))
