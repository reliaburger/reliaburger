#!/usr/bin/env python3
"""a023: per scale-out window, from the snapshots either side of it: the
leader's Raft write rate (last_log_index delta over the window), each node's
Bun CPU (cores) and RSS, and each VM's CPU busy share.

  scale_analyse.py DIR
"""
import json, re, statistics, sys

d = sys.argv[1]


def snaps(path):
    out, node = {}, None
    for line in open(path):
        line = line.strip()
        if line.startswith("## "):
            node = line[3:]
            continue
        m = re.match(r"bun (\d+) (\d+) rss_kb (\d+)", line)
        if m:
            out.setdefault(node, {})["bun"] = int(m[1]) + int(m[2])
            out[node]["rss_mb"] = int(m[3]) / 1024
        elif line.startswith("cpu "):
            f = [int(x) for x in line.split()[1:]]
            out.setdefault(node, {})["total"] = sum(f[:8])
            out[node]["idle"] = f[3] + f[4]
    return out


def leader_index(path):
    nodes = json.load(open(path))["nodes"]
    leader = nodes[0]["leader"]
    for n in nodes:
        if n["node_id"] == leader:
            return leader, n["last_log_index"]
    return leader, None


print(f"{'window':14} {'rate':>9} {'raft/s':>7} {'leader':>20} {'lead bun cores':>14} {'lead rss MiB':>12}   per node: bun cores / VM busy")
summary = {}
for n in (3, 2, 1):
    for r in range(1, 6):
        w = f"n{n}-r{r}-host"
        rep = json.load(open(f"{d}/{w}.json"))
        a, b = snaps(f"{d}/{w}.snap0"), snaps(f"{d}/{w}.snap1")
        leader, i0 = leader_index(f"{d}/{w}.council0.json")
        _, i1 = leader_index(f"{d}/{w}.council1.json")
        node1 = sorted(a)[0]
        cpus = 4
        elapsed = (b[node1]["total"] - a[node1]["total"]) / cpus / 100
        raft = (i1 - i0) / elapsed
        cols = []
        for node in sorted(a):
            cores = (b[node]["bun"] - a[node]["bun"]) / 100 / elapsed
            tot = b[node]["total"] - a[node]["total"]
            busy = 1 - (b[node]["idle"] - a[node]["idle"]) / tot
            cols.append(f"{node[-1]}: {cores:4.2f} / {busy:4.0%}")
            if node == leader:
                lead_cores, lead_rss = cores, b[node]["rss_mb"]
        summary.setdefault(n, []).append((rep["successes_per_second"], raft, lead_cores, lead_rss))
        print(f"{w:14} {rep['successes_per_second']:9.1f} {raft:7.1f} {leader:>20} {lead_cores:14.2f} {lead_rss:12.0f}   " + "  ".join(cols))
print()
for n, rows in summary.items():
    med = lambda i: statistics.median(x[i] for x in rows)
    print(f"{n} node(s): rate median {med(0):.1f}, Raft writes/s median {med(1):.1f}, leader Bun cores median {med(2):.2f}, leader RSS median {med(3):.0f} MiB")
