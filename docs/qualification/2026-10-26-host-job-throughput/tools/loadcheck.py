#!/usr/bin/env python3
"""a1029: host 1-minute load at the start and end of each window in a progress.log.

  loadcheck.py HOST_LOAD_LOG PROGRESS_LOG [--window 62] [--limit 5]

A window's end is its progress.log timestamp; its start is that minus the
window length plus drain (about 62 s).
"""
import bisect, datetime, re, sys

log, progress = sys.argv[1], sys.argv[2]
window = float(sys.argv[sys.argv.index("--window") + 1]) if "--window" in sys.argv else 62.0
limit = float(sys.argv[sys.argv.index("--limit") + 1]) if "--limit" in sys.argv else 5.0


def ts(s):
    return datetime.datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=datetime.timezone.utc).timestamp()


samples = []
for line in open(log):
    m = re.match(r"(\S+) \{ ([\d.]+) ([\d.]+) ([\d.]+) \}", line)
    if m:
        samples.append((ts(m.group(1)), float(m.group(2))))
times = [s[0] for s in samples]


def load_at(t):
    i = bisect.bisect_left(times, t)
    best = min((j for j in (i - 1, i) if 0 <= j < len(samples)), key=lambda j: abs(times[j] - t))
    return samples[best][1]


bad = 0
seen = []
for line in open(progress):
    m = re.match(r"(\d{4}-\S+Z) (.*)", line)
    if not m or "DONE" in line:
        continue
    end = ts(m.group(1))
    a, b = load_at(end - window), load_at(end)
    seen += [a, b]
    flag = "DISCARD" if max(a, b) > limit else "ok"
    bad += flag == "DISCARD"
    print(f"{m.group(2):60} start={a:5.2f} end={b:5.2f} {flag}")
print(f"windows over {limit}: {bad} of {len(seen) // 2}; 1-minute load at window edges {min(seen):.2f} to {max(seen):.2f}")
