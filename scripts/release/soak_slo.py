#!/usr/bin/env python3
"""Latency SLOs for the soak (#1400): what a user feels on a quiet cluster.

The soak's other checks gate internal health (agent-loop turns, leaders,
leaks). These are the numbers a user would notice: how long an API call
takes, how long an app takes to start, how long a rolling update takes to
be ready, and how long a job task waits before it starts. Kubernetes
publishes the same kind of SLO (API call p99 <= 1 s, pod start-up p99 <=
5 s) and measures each release against it with ClusterLoader2; see
docs/plans/2026-10-28-research-kubernetes-release-testing.md.

Every sample is taken outside fault windows. Samples go into a bucketed
histogram in EVIDENCE/slo.json, so a day-long run stays a few kilobytes,
and each p99 is the upper bound of the bucket holding it. A job's start
lag comes from the job campaign's own buckets (job_soak.BUCKETS), summed
over only the runs no fault window overlapped (#1438); the campaign's M8
start lag counts every run.

`measure_api` is the reusable part: give it endpoints, a token and a CA
and it times the API reads `relish status` and `relish nodes` make. The
hollow-node test (#1401) calls it against its own cluster.

Subcommands:
  api-latency SNAPSHOT --api-port P --ca CA --auth-header F --node VM... [--down I]
                         time the API reads on every node that is up
  rolled BEFORE AFTER N  exit 0 (printing each node's start in ms) once N nodes
                         have a new completed deploy entry, 1 while waiting,
                         2 when a new entry did not complete
  rollout EVIDENCE --apply-ms N [--ready-ms N] [--start-ms N...] [--failed REASON]
                         record one quiet rolling update
  trend DIRECTORY        every record's SLO values, oldest first
"""
import argparse
from dataclasses import dataclass
import http.client
import json
import os
from pathlib import Path
import socket
import ssl
import sys
import time

import job_soak

# 1, 2, 3, 5, 7, 10, 15, 20, 30, 50, 70, 100, ... 700,000 ms.
BOUNDS_MS = tuple(sorted({int(mantissa * 10 ** exponent) for exponent in range(6)
                          for mantissa in (1, 1.5, 2, 3, 5, 7)}))
JOB_BOUNDS_MS = tuple(bound for bound in job_soak.BUCKETS if bound is not None)
# The reads `relish status` and `relish nodes` make.
READ_PATHS = ("/v1/status?cluster=true", "/v1/cluster/nodes")
READ_TIMEOUT = 5
# Tiers whose records the gated SLOs can fail. A day tier runs host jobs at
# six times the fast tier's rate, which no record has measured yet; a custom
# run says nothing about any gate.
GATED_TIERS = ("fast", "final")
# Quiet job starts a p99 needs before it can be judged (#1485). A fast tier's
# faults leave only a handful of runs outside every window, and the p99 of 20
# is their largest; a final's quiet tail gives millions.
MIN_JOB_START_SAMPLES = 1000
FAILURES_KEPT = 20
RECORD_ONLY_NOTE = ("Record only from 28 October 2026 (#1400): no soak before 0.2.4 measured it. The threshold is "
                    "provisional; the first 0.2.4 fast and final records set the gate.")


@dataclass(frozen=True)
class Slo:
    """One latency objective: its p99 must stay at or under `threshold_ms`."""
    name: str
    label: str
    noun: str
    threshold_ms: int
    gated: bool
    source: str
    measures: str


SLOS = (
    Slo("api-read", "API read p99", "API read", 1000, False, "probe",
        "`GET /v1/status?cluster=true` and `GET /v1/cluster/nodes` on every node, at every light check"),
    Slo("api-write", "API write p99", "API write", 2000, False, "rollout",
        "`relish apply` of the soak workloads, from the call to the commit"),
    Slo("app-start", "App start p99", "app start", 5000, False, "rollout",
        "each node's deploy of a new `soak-identity` instance, from its start to the instance running"),
    Slo("deploy-ready", "Rolling update p99", "rolling update", 30000, False, "rollout",
        "from `relish apply` returning to every node's `soak-identity` running the new spec"),
    Slo("job-start", "Job task start p99", "job task start", 256, True, "job",
        "a job task's start lag on a pooled executor, from the job campaign's buckets, in runs no fault "
        "window overlapped"),
)


# --- histograms --------------------------------------------------------------

def new_histogram():
    return {"counts": [0] * (len(BOUNDS_MS) + 1), "count": 0, "max": 0, "failed": 0, "failures": []}


def observe(histogram, ms):
    """Add one sample in milliseconds."""
    ms = max(0, int(round(ms)))
    index = next((i for i, bound in enumerate(BOUNDS_MS) if ms <= bound), len(BOUNDS_MS))
    histogram["counts"][index] += 1
    histogram["count"] += 1
    histogram["max"] = max(histogram["max"], ms)


def p99(histogram):
    """The upper bound of the bucket holding the 99th percentile, or the
    largest sample once that is past the last bucket. None when empty."""
    total = histogram.get("count", 0)
    if not total:
        return None
    seen = 0
    for bound, count in zip(BOUNDS_MS, histogram["counts"]):
        seen += count
        if seen >= 0.99 * total:
            return bound
    return histogram["max"]


def job_bucket_index(bound):
    return JOB_BOUNDS_MS.index(bound)


def job_p99(counts):
    """The job campaign's own p99 of its start-lag buckets."""
    return job_soak.percentile(counts, 0.99)


def record(data, name, ms):
    observe(data.setdefault(name, new_histogram()), ms)


def record_failure(data, name, reason):
    histogram = data.setdefault(name, new_histogram())
    histogram["failed"] += 1
    histogram["failures"] = (histogram["failures"] + [reason])[-FAILURES_KEPT:]


# --- evidence ------------------------------------------------------------------

def load(evidence):
    path = Path(evidence) / "slo.json"
    return json.loads(path.read_text()) if path.exists() else {}


def save(evidence, data):
    path = Path(evidence) / "slo.json"
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(data, indent=1, sort_keys=True))
    os.replace(temporary, path)


def fold_reads(evidence, rows, fault_window, cut):
    """Add one check's API reads. Nothing counts inside a fault window, nor
    through a host forward the forwards check found cut (#987): neither is
    what a user of a healthy cluster sees."""
    if fault_window:
        return
    rows = [row for row in rows if row.get("node") not in cut]
    if not rows:
        return
    data = load(evidence)
    for row in rows:
        record(data, "api-read", row["ms"])
        if not row.get("ok"):
            record_failure(data, "api-read", f"{row.get('node')} {row.get('path')}: {row.get('error', 'failed')}")
    save(evidence, data)


def fold_snapshot(evidence, snapshot, fault_window, cut):
    """The checker's hook: fold SNAPSHOT/api-latency.json, if the harness
    wrote one. `cut` is the forwards check's set of (node, forward) pairs."""
    path = Path(snapshot) / "api-latency.json"
    if not path.exists():
        return
    try:
        rows = json.loads(path.read_text())
    except json.JSONDecodeError:
        return
    if isinstance(rows, list):
        fold_reads(evidence, rows, fault_window, {node for node, name in cut if name == "api"})


def record_rollout(evidence, apply_ms, ready_ms=None, starts=(), failed=None):
    """One quiet rolling update: its apply is an API write; once every node
    runs the new spec it gives one deploy-to-ready and a start per node."""
    data = load(evidence)
    record(data, "api-write", apply_ms)
    if failed:
        record_failure(data, "deploy-ready", failed)
    else:
        if ready_ms is not None:
            record(data, "deploy-ready", ready_ms)
        for ms in starts:
            record(data, "app-start", ms)
    save(evidence, data)


# --- measuring -----------------------------------------------------------------

class _NamedHTTPSConnection(http.client.HTTPSConnection):
    """Connect to a forwarded port, verifying the certificate for the node's name."""

    def __init__(self, host, port, server_name, context, timeout):
        super().__init__(host, port, context=context, timeout=timeout)
        self.server_name = server_name
        self.tls = context

    def connect(self):
        raw = socket.create_connection((self.host, self.port), self.timeout)
        self.sock = self.tls.wrap_socket(raw, server_hostname=self.server_name)


def https_get(endpoint, path, token, ca, timeout):
    """GET `path` from one node's API and read the whole answer; the status."""
    context = ssl.create_default_context(cafile=ca)
    connection = _NamedHTTPSConnection(endpoint.get("host", "127.0.0.1"), endpoint["port"], endpoint["name"],
                                       context, timeout)
    try:
        connection.request("GET", path, headers={"Authorization": "Bearer " + token,
                                                 "Host": f"{endpoint['name']}:9117"})
        response = connection.getresponse()
        response.read()
        return response.status
    finally:
        connection.close()


def measure_api(endpoints, token, ca, paths=READ_PATHS, timeout=READ_TIMEOUT, request=None, clock=time.perf_counter):
    """Time each read in `paths` on each endpoint (a dict with the node's
    `name`, the `port` that reaches its API and optionally a `host`).

    Returns one row per call: node, path, ms, ok, and the error when it
    failed. A failed call counts with the time the caller waited for it."""
    request = request or https_get
    rows = []
    for endpoint in endpoints:
        for path in paths:
            started = clock()
            error = None
            try:
                status = request(endpoint, path, token, ca, timeout)
                if not 200 <= status < 300:
                    error = f"HTTP {status}"
            except (OSError, http.client.HTTPException) as failure:
                error = f"{type(failure).__name__}: {failure}"
            row = dict(node=endpoint["name"], path=path, ms=int(round((clock() - started) * 1000)), ok=error is None)
            if error:
                row["error"] = error
            rows.append(row)
    return rows


def _millis(stamp):
    return stamp["secs_since_epoch"] * 1000 + stamp.get("nanos_since_epoch", 0) // 1_000_000


def _entries(history):
    return history.get("history", []) if isinstance(history, dict) else []


def rollout_progress(before, after, expected):
    """Compare two `GET /v1/deploys/history/APP` answers. A deploy entry
    that wasn't there before belongs to this rollout. Returns ("done", each
    new entry's start in ms) once `expected` nodes completed one,
    ("failed", why) when one ended any other way, else ("waiting", [])."""
    known = {json.dumps(entry.get("id"), sort_keys=True) for entry in _entries(before)}
    new = [entry for entry in _entries(after) if json.dumps(entry.get("id"), sort_keys=True) not in known]
    failed = [entry for entry in new if entry.get("result") != "Completed"]
    if failed:
        return "failed", ", ".join(f"{entry.get('node')} {entry.get('result')}" for entry in failed)
    if len({entry.get("node") for entry in new}) < expected:
        return "waiting", []
    return "done", [_millis(entry["completed_at"]) - _millis(entry["created_at"]) for entry in new]


# --- verdicts and the record ---------------------------------------------------

def rows(data, job_report, tier, job_campaign=True):
    """Each SLO's p99, sample count and verdict for one run. A gated SLO in
    a gated tier fails when it is over its threshold or has no samples. A
    run without a job campaign (`job_campaign` false) has no job starts to
    judge, so that row is recorded as not measured. Job starts need
    MIN_JOB_START_SAMPLES: a final short of them fails, and any other tier
    records them as too few to judge."""
    gated_tier = tier in GATED_TIERS
    result = []
    for slo in SLOS:
        over_every_bucket = False
        if slo.source == "job":
            m8 = (job_report or {}).get("m8") or {}
            samples = m8.get("quiet_start_lag_samples") or 0
            value = m8.get("quiet_p99_start_lag_ms") if samples else None
            over_every_bucket = bool(samples) and value is None
            failed = 0
        else:
            histogram = data.get(slo.name) or {}
            samples = histogram.get("count", 0)
            value = p99(histogram)
            failed = histogram.get("failed", 0)
        too_few = slo.source == "job" and 0 < samples < MIN_JOB_START_SAMPLES
        gated = (slo.gated and gated_tier and (job_campaign or slo.source != "job")
                 and not (too_few and tier != "final"))
        reason = None
        if not samples:
            verdict = "FAIL" if gated else "not measured"
            reason = f"no {slo.noun} samples"
        elif too_few:
            verdict = "FAIL" if gated else "too few samples"
            reason = f"only {samples:,} {slo.noun} samples, fewer than the {MIN_JOB_START_SAMPLES:,} a p99 needs"
        elif over_every_bucket or value > slo.threshold_ms:
            verdict = "FAIL" if gated else "over (record only)"
            reason = (f"{slo.noun} p99 is over every start-lag bucket (> {JOB_BOUNDS_MS[-1]:,} ms)"
                      if over_every_bucket else f"{slo.noun} p99 {value:,} ms is over the {slo.threshold_ms:,} ms SLO")
        else:
            verdict = "PASS"
        result.append(dict(name=slo.name, label=slo.label, p99=value, over_every_bucket=over_every_bucket,
                           threshold_ms=slo.threshold_ms, samples=samples, failed=failed,
                           mode="gated" if gated else "record only", verdict=verdict, reason=reason))
    return result


def failures(slo_rows):
    """Each breached gated SLO, as `name: reason`."""
    return [f"{row['name']}: {row['reason']}" for row in slo_rows if row["verdict"] == "FAIL"]


def _shown(row):
    if row["over_every_bucket"]:
        return f"> {JOB_BOUNDS_MS[-1]:,} ms"
    return "not measured" if row["p99"] is None else f"{row['p99']:,} ms"


def table_lines(slo_rows, data=None):
    """The record's SLO section."""
    lines = ["## Latency SLOs", "",
             "What a user should feel on a quiet three-node cluster (#1400). Every sample is taken outside fault "
             "windows and through a host forward that works; each p99 is the upper bound of its histogram bucket.",
             "", "| SLO | p99 | Threshold | Samples | Mode | Verdict |", "|---|---|---|---|---|---|"]
    for row in slo_rows:
        samples = f"{row['samples']:,}" + (f" ({row['failed']:,} failed)" if row["failed"] else "")
        lines.append(f"| {row['label']} | {_shown(row)} | ≤ {row['threshold_ms']:,} ms | {samples} | {row['mode']} | "
                     f"{row['verdict']} |")
    lines.append("")
    lines += [f"- {slo.label}: {slo.measures}" for slo in SLOS]
    if any(row["mode"] == "record only" and not slo.gated for row, slo in zip(slo_rows, SLOS)):
        lines.append(f"- {RECORD_ONLY_NOTE}")
    for row in slo_rows:
        if row["verdict"] == "FAIL":
            lines.append(f"- **SLO breached: {row['name']}: {row['reason']}**")
        elif row["verdict"] == "too few samples":
            lines.append(f"- {row['label']} not judged: {row['reason']}")
    for name, histogram in sorted((data or {}).items()):
        for reason in histogram.get("failures", [])[-3:]:
            lines.append(f"- {name} failed call: {reason}")
    return lines + [""]


# --- trend -----------------------------------------------------------------------

def _record_values(path):
    """One record's p99s by label: its SLO table, or for a record from
    before #1400, the job start lag its .jobs.json kept."""
    text = path.read_text(errors="replace")
    if "\n## Latency SLOs" in text:
        section = text.split("\n## Latency SLOs", 1)[1].split("\n## ", 1)[0]
        values = {}
        for line in section.splitlines():
            cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
            if line.startswith("| ") and len(cells) >= 2 and cells[0] not in ("SLO",) and not cells[0].startswith("---"):
                values[cells[0]] = cells[1]
        return values
    jobs = path.with_name(path.stem + ".jobs.json")
    if jobs.exists():
        m8 = (json.loads(jobs.read_text()) or {}).get("m8") or {}
        if m8.get("start_lag_samples"):
            value = m8.get("p99_start_lag_ms")
            return {"Job task start p99": f"{value:,} ms" if value is not None else f"> {JOB_BOUNDS_MS[-1]:,} ms"}
    return None


def trend(directory):
    """Lines listing every soak record's SLO values, oldest first."""
    labels = [slo.label for slo in SLOS]
    found = []
    for path in sorted(Path(directory).glob("*.md")):
        values = _record_values(path)
        if values is not None:
            found.append((path.stem, values))
    width = max([len("Record")] + [len(stem) for stem, _ in found])
    columns = [max(len(label), 12) for label in labels]
    def line(first, cells):
        return "  ".join([first.ljust(width)] + [cell.rjust(size) for cell, size in zip(cells, columns)]).rstrip()
    lines = [line("Record", labels)]
    lines += [line(stem, [values.get(label, "-") for label in labels]) for stem, values in found]
    return lines


# --- command line ----------------------------------------------------------------

def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    latency = sub.add_parser("api-latency")
    latency.add_argument("snapshot")
    latency.add_argument("--api-port", required=True, type=int, help="node 1's forwarded API port; node N is +N-1")
    latency.add_argument("--ca", required=True)
    latency.add_argument("--auth-header", required=True, help="a file holding `Authorization: Bearer TOKEN`")
    latency.add_argument("--node", action="append", required=True, help="the VMs, node 1 first")
    latency.add_argument("--down", action="append", type=int, default=[], help="a node (1-3) that is powered off")
    rolled = sub.add_parser("rolled")
    rolled.add_argument("before")
    rolled.add_argument("after")
    rolled.add_argument("expected", type=int)
    rollout = sub.add_parser("rollout")
    rollout.add_argument("evidence")
    rollout.add_argument("--apply-ms", required=True, type=int)
    rollout.add_argument("--ready-ms", type=int)
    rollout.add_argument("--start-ms", type=int, action="append", default=[])
    rollout.add_argument("--failed")
    listing = sub.add_parser("trend")
    listing.add_argument("directory")
    args = parser.parse_args(argv)

    if args.command == "api-latency":
        header = Path(args.auth_header).read_text().strip()
        token = header.split("Bearer", 1)[1].strip() if "Bearer" in header else header
        endpoints = [dict(name=node, port=args.api_port + index - 1)
                     for index, node in enumerate(args.node, 1) if index not in args.down]
        rows_out = measure_api(endpoints, token, args.ca)
        (Path(args.snapshot) / "api-latency.json").write_text(json.dumps(rows_out, indent=1))
        return 0
    if args.command == "rolled":
        try:
            before = json.loads(Path(args.before).read_text())
            after = json.loads(Path(args.after).read_text())
        except (OSError, json.JSONDecodeError):
            return 1
        state, detail = rollout_progress(before, after, args.expected)
        if state == "done":
            print(" ".join(str(ms) for ms in detail))
            return 0
        if state == "failed":
            print(detail)
            return 2
        return 1
    if args.command == "rollout":
        record_rollout(args.evidence, args.apply_ms, args.ready_ms, args.start_ms, args.failed)
        return 0
    print("\n".join(trend(args.directory)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
