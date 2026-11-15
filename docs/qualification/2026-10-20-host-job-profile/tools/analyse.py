#!/usr/bin/env python3
"""a1029: summarise a session directory: rate, steady-state rate, CPU per task, vmstat, start lag."""
import json, os, re, statistics, sys
from collections import defaultdict

d = sys.argv[1]
HZ = 100


def parse_cpu(path):
    sec, out = None, {"cg": {}}
    for line in open(path):
        line = line.rstrip("\n")
        if line.startswith("## "):
            sec = line[3:]
            continue
        if sec == "proc_stat":
            v = list(map(int, line.split()[1:]))
            out["stat"] = v  # user nice system idle iowait irq softirq steal
        elif sec == "bun_stat":
            v = list(map(int, line.split()))
            out["bun"] = v[0] + v[1]  # utime + stime, ticks
            out["bun_children"] = v[2] + v[3]
        elif sec == "cgroups" and line:
            p, _, u = line.rpartition(" ")
            if u.isdigit():
                out["cg"][p] = int(u)
        elif sec == "schedstat" and line:
            a, b = map(int, line.split())
            out["sched_run"], out["sched_wait"] = a, b
        elif sec == "ctxt":
            k, v = line.split()
            out[k] = int(v)
    return out


def steady(path):
    pts = []
    for line in open(path):
        m = re.match(r"^([\d.]+)s: (\d+) unique accepted successes", line)
        if m:
            pts.append((float(m.group(1)), int(m.group(2))))
    a = [p for p in pts if p[0] >= 15]
    b = [p for p in pts if p[0] <= 58]
    if not a or not b or b[-1][0] - a[0][0] < 20:
        return None
    return (b[-1][1] - a[0][1]) / (b[-1][0] - a[0][0])


def vmstat(path):
    rows = []
    for line in open(path):
        f = line.split()
        if len(f) >= 17 and f[0].isdigit():
            rows.append(list(map(int, f)))
    rows = rows[15:55]  # steady part of the window
    if not rows:
        return {}
    cols = {"r": 0, "b": 1, "in": 10, "cs": 11, "us": 12, "sy": 13, "id": 14, "wa": 15, "st": 16}
    return {k: statistics.mean(r[i] for r in rows) for k, i in cols.items()}


def lag(path):
    try:
        s = json.load(open(path))
    except Exception:
        return None
    counts = None

    def find(o):
        nonlocal counts
        if isinstance(o, dict):
            for k, v in o.items():
                if k == "start_lag_ms_counts" and isinstance(v, list):
                    counts = [a + b for a, b in zip(counts or [0] * len(v), v)]
                else:
                    find(v)
        elif isinstance(o, list):
            for v in o:
                find(v)
    find(s)
    return counts


def pct(counts, q):
    # bucket b holds lag in (2^(b-1), 2^b] ms, bucket 0 is <= 1 ms
    total = sum(counts)
    if not total:
        return None
    acc = 0
    for b, c in enumerate(counts):
        acc += c
        if acc >= q * total:
            return 2 ** b
    return None


rows = []
for name in sorted(os.listdir(d)):
    if not name.endswith(".json") or name.endswith(".status.json"):
        continue
    base = name[:-5]
    rep = json.load(open(os.path.join(d, name)))
    r = {"name": base, "rate": rep.get("successes_per_second") or 0.0,
         "done": rep.get("unique_accepted_successes") or 0, "complete": rep.get("measurement_complete"),
         "fail": rep.get("terminal_failures"), "retry": rep.get("accepted_retries"), "conc": rep.get("concurrency")}
    out = os.path.join(d, base + ".out")
    if os.path.exists(out):
        r["steady"] = steady(out)
    c0, c1 = os.path.join(d, base + ".cpu0"), os.path.join(d, base + ".cpu1")
    if os.path.exists(c0) and os.path.exists(c1):
        a, b = parse_cpu(c0), parse_cpu(c1)
        ds = [y - x for x, y in zip(a["stat"], b["stat"])]
        total = sum(ds[:8])
        busy = total - ds[3] - ds[4]
        r["cpu_busy_s"] = busy / HZ
        r["cpu_util"] = busy / total
        r["iowait"] = ds[4] / total
        r["sys_share"] = (ds[2] + ds[5] + ds[6]) / max(busy, 1)
        r["bun_cpu_s"] = (b["bun"] - a["bun"]) / HZ
        r["ctxt"] = b.get("ctxt", 0) - a.get("ctxt", 0)
        r["forks"] = b.get("processes", 0) - a.get("processes", 0)
        if "sched_wait" in a:
            r["bun_runq_wait_s"] = (b["sched_wait"] - a["sched_wait"]) / 1e9
        cg = {k: b["cg"][k] - a["cg"].get(k, 0) for k in b["cg"]}
        r["cgroups"] = sorted(((v / 1e6, k) for k, v in cg.items() if v > 2e6), reverse=True)[:12]
    vm = os.path.join(d, base + ".vmstat")
    if os.path.exists(vm):
        r["vm"] = vmstat(vm)
    st = os.path.join(d, base + ".status.json")
    if os.path.exists(st):
        counts = lag(st)
        if counts:
            r["lag"] = counts
            r["lag_p50"], r["lag_p99"] = pct(counts, 0.5), pct(counts, 0.99)
    rows.append(r)

groups = defaultdict(list)
for r in rows:
    key = re.sub(r"-r\d+-", "-", r["name"])
    groups[key].append(r)
    extra = ""
    if "cpu_busy_s" in r:
        forks = r["forks"]
        extra = (f" busy={r['cpu_busy_s']:.1f}cpu-s util={100 * r['cpu_util']:.0f}% iow={100 * r['iowait']:.1f}%"
                 f" sys/busy={100 * r['sys_share']:.0f}% bun={r['bun_cpu_s']:.1f}s"
                 f" forks/s={forks / 62:.0f} ctxt/s={r['ctxt'] / 62:.0f}")
        if r["forks"]:
            extra += f" cpu-ms/fork={1000 * r['cpu_busy_s'] / forks:.3f} bun-ms/fork={1000 * r['bun_cpu_s'] / forks:.3f}"
    vm = r.get("vm") or {}
    if vm:
        extra += f" vm[r={vm['r']:.1f} us={vm['us']:.0f} sy={vm['sy']:.0f} id={vm['id']:.0f} wa={vm['wa']:.0f} cs={vm['cs']:.0f}]"
    if r.get("lag_p50"):
        extra += f" lag p50<={r['lag_p50']}ms p99<={r['lag_p99']}ms"
    st = r.get("steady")
    st_s = f"{st:.0f}" if st else "-"
    print(f"{r['name']:28} rate={r['rate']:8.1f} steady={st_s:>6} done={r['done']} complete={r['complete']} fail={r['fail']} retry={r['retry']}{extra}")

print()
for key, rs in sorted(groups.items()):
    rates = [r["rate"] for r in rs]
    st = [r["steady"] for r in rs if r.get("steady")]
    line = f"{key:24} n={len(rs)} median={statistics.median(rates):8.1f} range={min(rates):.1f}-{max(rates):.1f}"
    if st:
        line += f" steady-median={statistics.median(st):.0f}"
    busy = [1000 * r["cpu_busy_s"] / r["forks"] for r in rs if r.get("forks")]
    if busy:
        line += f" cpu-ms/fork median={statistics.median(busy):.3f}"
    util = [r["cpu_util"] for r in rs if "cpu_util" in r]
    if util:
        line += f" util median={100 * statistics.median(util):.0f}%"
    print(line)
