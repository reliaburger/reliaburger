#!/usr/bin/env python3
"""Evaluate V02 sustained-soak evidence and render its record (stdlib only).

qualify-sustained.sh collects raw evidence into snapshot directories; this
turns each snapshot into findings against the invariants in
docs/plans/2026-09-25-v02-sustained.md, keeps the running state those
invariants need (highest acknowledged writes, certificate serials, file
descriptor history, first sightings of `wtf` findings) in `state.json`, and
renders the final Markdown record.

Subcommands:
  event EVIDENCE --phase P [--target T] [--command C] [--exit N] [--duration S] [--verdict V] [--detail D]
  expect EVIDENCE restart NODE          a harnessed bun kill: one systemd restart is explained
  window EVIDENCE open|close [LABEL]    faults in progress; outages are allowed while open
  power-cut EVIDENCE                    a node lost power: a log tail may end below an earlier one once
  baseline EVIDENCE SNAPSHOT            record each node's leak inventory as the baseline
  evaluate EVIDENCE SNAPSHOT            findings; exit 1 on a new failure, 3 when not yet settled
  policies EVIDENCE LISTING             exit 1 unless the soak workload's policies are all in force
  seen EVIDENCE KEY                     exit 0 if a harness failure KEY was bundled in the last half hour
  ingress-expect EVIDENCE NODE SERIAL   the serial a node must serve after a successful reload
  count EVIDENCE NAME [N]               add to a named counter in the record
  toml-set FILE SECTION.KEY=VALUE...    set keys in a TOML file in place (VALUE is a TOML literal)
  nodes-json FILE leader|followers|alive|council
  utc-stamp EPOCH                       YYMMDDHHMMSSZ for `openssl ca -startdate/-enddate`
  registry EVIDENCE push|verify --port P --server-name N --ca CA --token-file F [--tag T]
  forwards SNAPSHOT --limactl L --api-port P --ingress-port P --registry-port P --node VM... [--down I]
                                        probe every forwarded port from the host, and from inside
                                        the guest when the host gets no answer (#987)
  keep-host-logs EVIDENCE --lima-home D --reason R VM...
                                        keep Lima's host-agent logs before a stop truncates them
  curl-reason CODE                      name a curl exit code (refused, timeout, TLS...)
  test-failures REPORT [STDERR]         each failing case's reason from a `relish --output json test` report
  seed EVIDENCE [--seed N]              start (or resume) the run's fault and node choices; prints the seed
  choose EVIDENCE WHAT LOW HIGH         the run's next choice between LOW and HIGH, logged as an event
  render EVIDENCE RECORD                write the Markdown record (refuses to overwrite)
"""
import argparse
import base64
import calendar
import hashlib
import http.client
import json
import os
from pathlib import Path
import random
import re
import secrets
import socket
import ssl
import statistics
import sys
import time
import tomllib
import job_soak
import job_soak_inventory
import soak_charts
import soak_slo

# Thresholds from section 4 of the plan (seconds unless stated).
NODE_CERT_MARGIN = 300
WORKLOAD_CERT_MARGIN = 600
INGRESS_CERT_MARGIN = 300
RETRYING_LIMIT = 600
WTF_CRITICAL_LIMIT = 300
WTF_WARNING_LIMIT = 600
# Mayo alerts the soak raises by design: it drives 2-vCPU guests at the job
# target rate, so guest CPU above 90% is the load, not a fault (#1112). They
# are recorded, not gated. `memory_high` stays gated: it is a leak signal.
UNGATED_ALERTS = {"cpu_throttle"}
NO_LEADER_LIMIT = 30
RSS_WARMUP = 3600
RSS_GROWTH = 1.25
# Recorded job-cost targets for a soak (maintainer decision, 15 October 2026,
# #668). A Bun process's steady-state RSS is the median of the samples taken
# once it has been seen for BUN_STEADY_AFTER seconds.
BUN_STEADY_AFTER = 600
API_BUN_RSS_TARGET_MIB = 512
BUN_RSS_TARGET_MIB = 384
TASK_ARRAY_STORAGE_TARGET_MIB = 64
# A leak-fd window: floors over 10-minute buckets after a 10-minute warm-up,
# each at least FD_STEP above the last, 25% up overall. Bun images rarely
# live more than two hours in the soak, so the window has to be short.
FD_WARMUP = 600
FD_BUCKET = 600
FD_RISES = 3
FD_STEP = 10
FD_GROWTH = 1.25
REPEAT_WINDOW = 1800
# When each leak checker can first judge a process (#1062): leak-rss after
# its warm-up, leak-fd once it has the warm-up and FD_RISES + 1 full windows.
CHECKER_ARMED_AFTER = (("leak-rss", "rss_kb", RSS_WARMUP), ("leak-fd", "fd", FD_WARMUP + (FD_RISES + 1) * FD_BUCKET))
# Snapshot kinds that collect the guest inventories (collect in qualify-sustained.sh).
INVENTORY_KINDS = ("settle", "heavy")
LEAK_KINDS = ("runc", "netns", "lease", "veth", "cgroup", "bpf", "listen")
# The agent loop's turn budget (docs/plans/2026-09-30-agent-loop-review.md,
# decision 1): a tier fails if any node runs one turn longer than this. Bun
# exports its turns as the histogram bun_agent_loop_turn_seconds, and 1 s is
# one of its bucket bounds, so `+Inf` minus `le="1"` counts the turns over.
TURN_BUDGET_LE = "1"
TURN_BUCKET_BOUNDS = ("2.5", "5", "10", "+Inf")
# Pickle resolves every tag through the council's committed catalogue and
# answers 503 while there is no leader. A few tries ride out an election;
# a quorum loss outlasts them and is judged against the fault window.
REGISTRY_UNAVAILABLE_TRIES = 3
REGISTRY_UNAVAILABLE_PAUSE = 5
# The ports the quickstart forwards from the host (src/relish/quickstart/
# runner.rs): every node's API, and node 1's ingress and registry. Each is
# probed with a request the service answers, since Lima's gRPC forwarder
# accepts every connection on the host even when the guest side is gone.
FORWARDS = (("api", 9117, "https", "/v1/health"), ("ingress", 80, "http", "/"), ("registry", 5050, "https", "/v2/"))
# A forward that has just come up after a boot may lag Bun by a few seconds;
# one that stays cut this long is a failure (#987).
HOST_FORWARD_GRACE = 60
CURL_REASONS = {6: "no such host", 7: "refused", 22: "HTTP error", 28: "timeout", 35: "TLS", 52: "empty reply",
                55: "send failed", 56: "reset", 58: "TLS", 60: "TLS", 77: "TLS", 90: "TLS", 91: "TLS"}


# --- state -----------------------------------------------------------------

def load_state(evidence):
    path = Path(evidence) / "state.json"
    if path.exists():
        return json.loads(path.read_text())
    return {}


def save_state(evidence, state):
    path = Path(evidence) / "state.json"
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(state, indent=1, sort_keys=True))
    os.replace(temporary, path)


# --- seeded choices (#1062) -----------------------------------------------------

def start_choices(evidence, seed):
    """Start the run's choices from `seed` (a new random one when None), or
    resume them. A resumed run keeps its seed and its place in the sequence;
    asking it for a different seed is an error. Returns the seed."""
    path = Path(evidence) / "choices.json"
    if path.exists():
        recorded = json.loads(path.read_text())
        if seed is not None and seed != recorded["seed"]:
            raise ValueError(f"this run's choices come from seed {recorded['seed']}, not {seed}")
        return recorded["seed"]
    if seed is None:
        seed = secrets.randbelow(2 ** 32)
    write_choices(path, {"seed": seed, "draws": 0})
    return seed


def write_choices(path, data):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(data, sort_keys=True))
    os.replace(temporary, path)


def choose(evidence, what, low, high):
    """The run's next choice between `low` and `high`, inclusive.

    Draw n of seed s is always the same number, so the same seed makes the
    same choices in the same order. Each choice is logged as a `choice`
    event naming what it chose."""
    path = Path(evidence) / "choices.json"
    data = json.loads(path.read_text())
    draw = data["draws"]
    value = random.Random(f"{data['seed']}:{draw}").randint(low, high)
    write_choices(path, dict(data, draws=draw + 1))
    append_event(evidence, phase="choice", target=what, detail=f"{value} of {low}-{high}, draw {draw}, seed {data['seed']}")
    return value


def append_event(evidence, **fields):
    fields = {key: value for key, value in fields.items() if value is not None}
    fields.setdefault("ts", int(time.time()))
    with open(Path(evidence) / "events.jsonl", "a") as events:
        events.write(json.dumps(fields, sort_keys=True) + "\n")


def read_events(evidence):
    path = Path(evidence) / "events.jsonl"
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def finding(check, severity, detail, target=None):
    return {"check": check, "severity": severity, "target": target, "detail": detail}


# --- parsers -----------------------------------------------------------------

def parse_runs(text, prefix):
    """Numbers from lines like `ACK 12`, in log order, per run.

    `relish logs` labels each line `[instance]`, or `[instance@node]` when one
    instance name ran on two nodes, once the view holds more than one run. A
    run is one process writing in order; the view interleaves runs by their
    nodes' clocks, which isn't an order any of them wrote. Unlabelled lines
    are one run."""
    pattern = re.compile(r"^(?:\[([^\]]+)\] )?.*?\b" + re.escape(prefix) + r" (\d+)\b")
    runs = {}
    for line in text.splitlines():
        match = pattern.match(line)
        if match:
            runs.setdefault(match.group(1), []).append(int(match.group(2)))
    return runs


def parse_inventory(text):
    """`kind value` lines from the guest inventory script into lists."""
    inventory = {}
    for line in text.splitlines():
        kind, _, value = line.strip().partition(" ")
        if kind:
            inventory.setdefault(kind, []).append(value.strip())
    return inventory


def inventory_number(inventory, kind):
    try:
        return int(inventory[kind][0])
    except (KeyError, IndexError, ValueError):
        return None


def parse_openssl(text):
    """`serial=` and `notAfter=` from `openssl x509 -noout -serial -enddate`."""
    result = {}
    for line in text.splitlines():
        key, _, value = line.partition("=")
        if key == "serial":
            result["serial"] = value.strip().upper()
        elif key == "notAfter":
            parsed = time.strptime(re.sub(r"\s+", " ", value.strip()), "%b %d %H:%M:%S %Y %Z")
            result["not_after"] = calendar.timegm(parsed)
    return result


def normalise_serial(serial):
    """Diagnostics print `0a:1b`, openssl prints `0A1B`; compare as integers."""
    digits = serial.replace(":", "").strip()
    return int(digits, 16) if digits else None


def instances_by_node(status):
    nodes = {}
    for instance in status:
        nodes.setdefault(instance["node"], []).append(instance["id"])
    return nodes


# --- invariants (pure) -------------------------------------------------------

def view_end(runs):
    """The newest value in the view: the highest any run ended on."""
    return max((values[-1] for values in runs.values() if values), default=None)


def sequence_findings(check, runs, highest, source, target=None):
    """Log-view ordering: the writer's ACKs and redis INCRs, as `relish logs`
    returns them, must keep rising within each run (see `parse_runs`). Still
    failures, but about the order the log view shows, not what was stored;
    `source` says what decides that."""
    findings = []
    for label, values in runs.items():
        run = f" in run {label}" if label else ""
        for before, after in zip(values, values[1:]):
            if after <= before:
                findings.append(finding(check, "fail", f"log view went backwards from {before} to {after}{run} ({source})", target))
    end = view_end(runs)
    if end is not None and highest is not None and end < highest:
        findings.append(finding(check, "fail", f"log view ends at {end}, below the {highest} an earlier check saw ({source})", target))
    return findings


def writer_file_findings(text, highest_ack):
    """What the writer's file holds, from the harness's one-pass summary.

    `MAX` is the highest value in the file and `LAST` its line count.
    `MISSING` counts values in 1..MAX that aren't there, `DUP` values that
    are there more than once, and `BAD` names the first line that isn't
    its own line number. Only a missing value or a MAX below an ACK is
    lost data. A duplicate means two writers appended at once, which the
    product must never allow on one volume, so it fails too, but it is
    reported as what it is.
    """
    fields = {}
    for line in text.splitlines():
        key, _, rest = line.partition(" ")
        if key in ("BAD", "DUP", "MISSING", "MAX", "LAST"):
            fields.setdefault(key, rest)
    if "MAX" not in fields or "LAST" not in fields:
        return [finding("writer-file", "warn", "no MAX/LAST summary from the writer check")]
    findings = []
    maximum = int(fields["MAX"])
    lines = int(fields["LAST"])
    if "MISSING" in fields:
        findings.append(finding("writer-lost", "fail", f"values absent from 1..{maximum}: {fields['MISSING']}"))
    if highest_ack is not None and maximum < highest_ack:
        findings.append(finding("writer-regression", "fail", f"file holds values up to {maximum} but ACK {highest_ack} was logged"))
    if "DUP" in fields:
        findings.append(finding("writer-duplicate", "fail",
                                f"values written more than once: {fields['DUP']} ({lines} lines, max {maximum}); "
                                "two writers appended to one volume"))
    if "BAD" in fields and "MISSING" not in fields and "DUP" not in fields:
        findings.append(finding("writer-gap", "fail", "sequence file line " + fields["BAD"]))
    return findings


def certificate_findings(check, not_after, now, margin, target):
    if not_after is None:
        return []
    remaining = not_after - now
    if remaining <= margin:
        return [finding(check, "fail", f"not_after is {remaining} s away (limit {margin} s)", target)]
    return []


def rotation_findings(state, node, rotation_state, now):
    """`expired`/`stopped` fail at once; `retrying` fails after ten minutes."""
    retrying = state.setdefault("retrying_since", {})
    if rotation_state in ("expired", "stopped"):
        return [finding("cert-rotation", "fail", f"rotation_state {rotation_state}", node)]
    if rotation_state != "retrying":
        retrying.pop(node, None)
        return []
    since = retrying.setdefault(node, now)
    if now - since > RETRYING_LIMIT:
        return [finding("cert-rotation", "fail", f"retrying for {now - since} s", node)]
    return []


def leak_findings(baseline, current, baseline_instances, current_instances, node, job_inventory=None,
                  lingering=()):
    """Resources beyond the baseline plus the node's current instances.

    A live job owner's temporary setup peer (`vtc-<hash>`) is excused, unless
    it is in `lingering`: present in the previous inventory too (#944)."""
    findings = []
    if job_inventory is not None:
        owned = job_soak_inventory.exemptions(job_inventory)
        owned["veth"] |= owned["setup_peer"] - set(lingering)
        # Compare the reconciled kernel snapshot with exact live owner proofs.
        current = dict(current, **{kind:[v for v in values if v not in owned[kind]]
                                  for kind,values in job_inventory["resources"].items()})
    instances = set(current_instances)
    for kind, prefix in (("runc", ""), ("netns", "rb-"), ("lease", "")):
        for value in current.get(kind, []):
            if not value.startswith(prefix) or value[len(prefix):] not in instances:
                findings.append(finding("leak-" + kind, "fail", f"{value} has no instance", node))
    extra_instances = len(instances) - len(set(baseline_instances))
    for kind in ("veth", "cgroup"):
        allowed = len(baseline.get(kind, [])) + extra_instances
        if len(current.get(kind, [])) > allowed:
            findings.append(finding("leak-" + kind, "fail", f"{len(current.get(kind, []))} present, at most {allowed} expected", node))
    for kind in ("bpf", "listen"):
        extra = sorted(set(current.get(kind, [])) - set(baseline.get(kind, [])))
        if extra:
            findings.append(finding("leak-" + kind, "fail", "not in the baseline: " + ", ".join(extra[:5]), node))
    return findings


def fd_findings(samples, started, now):
    """Fail when bun's fd floor rose sharply across each of the last four 10-minute windows.

    Samples belong to one bun image (a new pid or exec starts over). The floor
    (each window's minimum) ignores the bursts every settle causes; a leak
    lifts it every time, a peer's restart once.
    """
    floors = {}
    for ts, fd in samples:
        if ts - started >= FD_WARMUP:
            window = (ts - started - FD_WARMUP) // FD_BUCKET
            floors[window] = min(floors.get(window, fd), fd)
    last = (now - started - FD_WARMUP) // FD_BUCKET - 1
    windows = range(last - FD_RISES, last + 1)
    if any(window < 0 or window not in floors for window in windows):
        return []
    values = [floors[window] for window in windows]
    rising = all(after >= before + FD_STEP for before, after in zip(values, values[1:]))
    if rising and values[-1] >= values[0] * FD_GROWTH:
        return [finding("leak-fd", "fail", "bun's lowest fd count rose every 10 minutes for half an hour: "
                        + " ".join(map(str, values)))]
    return []


def rss_findings(node_state, rss, ts, started):
    """After a one-hour warm-up, RSS must stay within 25% of its first warm sample."""
    if rss is None or ts - started < RSS_WARMUP:
        return []
    warm = node_state.setdefault("rss_warm_kb", rss)
    if rss > warm * RSS_GROWTH:
        return [finding("leak-rss", "fail", f"bun RSS {rss} kB is over 125% of the warm {warm} kB")]
    return []


def export_findings(state, node, text):
    """Every Parquet file that leaves the source must be at the destination, byte-identical."""
    findings = []
    source, destination = {}, {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 4 and parts[0] == "src":
            source[(parts[1], parts[2])] = parts[3]
        elif len(parts) == 4 and parts[0] == "dest":
            name = parts[2].rsplit("/", 1)[-1]
            digest, _, original = name.partition("-")
            destination[(parts[1], original, digest)] = parts[3]
            if parts[3] not in ("verified", digest):
                findings.append(finding("export-corrupt", "fail", f"{parts[2]} has SHA-256 {parts[3]}", node))
    seen = state.setdefault("export_seen", {}).setdefault(node, {})
    for (kind, name), digest in source.items():
        seen[f"{kind} {name}"] = digest
    for key, digest in list(seen.items()):
        kind, name = key.split(" ", 1)
        if (kind, name) in source:
            continue
        if (kind, name, digest) not in destination:
            findings.append(finding("export-lost", "fail", f"{kind} {name} ({digest[:12]}) left the source but is not at the destination", node))
        del seen[key]
    state.setdefault("export_counts", {})[node] = {
        "source": len(source), "destination": len(destination)}
    return findings


def snapshot_archive_findings(state, node, text):
    findings = []
    groups = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[0] == "snap":
            group = parts[1].rsplit("/", 1)[0]
            groups[group] = groups.get(group, 0) + 1
            if parts[2] != "ok":
                findings.append(finding("snapshot-unreadable", "fail", parts[1], node))
    highest = state.setdefault("snapshot_highest", {}).setdefault(node, {})
    retain = state.get("snapshot_retain", 4)
    for group, count in groups.items():
        highest[group] = max(highest.get(group, 0), count)
    for group, best in highest.items():
        if groups.get(group, 0) < min(best, retain):
            findings.append(finding("snapshot-missing", "fail", f"{group}: {groups.get(group, 0)} archives after {best}", node))
    return findings


# systemd restarts a killed bun within seconds, so a check this long after a
# harness kill has seen the restart the kill caused, if it caused one. An
# expectation that a check this late didn't use never excuses a later restart.
RESTART_GRACE = 120


def release_version(versions, soak_bun):
    """The version every node reports, without the `v`: the candidate's own
    release, which each upgrade walk rolls back to.

    Nodes that disagree, don't answer ("?") or already run the soak build
    mean the cluster isn't on the candidate, and a walk from it would prove
    nothing."""
    found = {version.removeprefix("v") for version in versions}
    if len(found) != 1 or "?" in found or "" in found:
        raise ValueError(f"nodes don't agree on a release version: {' '.join(versions) or 'none'}")
    version = found.pop()
    if version == Path(soak_bun).name.removeprefix("bun-v"):
        raise ValueError(f"the cluster already runs the soak build {version}")
    return version


def restart_expectations(evidence, node):
    """Times the harness said it would kill `node`'s bun, oldest first.

    They live in their own append-only file: the harness records a kill while
    a background check may be loading and rewriting state.json, and a write
    there could be lost."""
    path = Path(evidence) / "restart-expectations.jsonl"
    if not path.exists():
        return []
    times = []
    for line in path.read_text().splitlines():
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            continue
        if entry.get("node") == node:
            times.append(entry["ts"])
    return times


def restart_findings(state, node, inventory, expectations, now):
    """systemd restarts of bun that no harness kill explains.

    Each expectation explains at most one restart, and stays available until
    a restart uses it or an earlier check ran RESTART_GRACE after it without
    using it. Expiry follows the checks, not the clock. A check that runs
    between the kill being recorded and systemd restarting bun doesn't use it
    up, and a slot that blocks for many minutes after a kill (an upgrade walk)
    can't expire it before any check has looked."""
    boot = (inventory.get("boot") or [None])[0]
    count = inventory_number(inventory, "nrestarts")
    if count is None:
        return []
    restarts = state.setdefault("restarts", {}).setdefault(node, {"boot": boot, "n": count, "used": len(expectations)})
    restarts.pop("expected", None)
    restarts.setdefault("used", 0)
    findings = []
    pending = expectations[restarts["used"]:]
    checked = restarts.get("checked")
    fresh = [ts for ts in pending if checked is None or checked < ts + RESTART_GRACE]
    restarts["used"] += len(pending) - len(fresh)
    if restarts["boot"] == boot:
        observed = max(count - restarts["n"], 0)
        explained = min(observed, len(fresh))
        restarts["used"] += explained
        if observed > explained:
            item = finding("bun-restart", "fail", f"{observed - explained} systemd restart(s) of bun without a harness kill "
                           f"(NRestarts now {count})", node)
            # Each rise is a new restart, not a repeat of the last one (#985).
            item["repeat_key"] = f"{node}|{boot}|{count}"
            findings.append(item)
    else:
        # A new boot starts systemd's counter again; nothing carries over.
        restarts["used"] = len(expectations)
    restarts.update(boot=boot, n=count, checked=max(now, checked or now))
    panics = inventory_number(inventory, "panics") or 0
    if panics:
        findings.append(finding("bun-panic", "fail", f"{panics} panic line(s) in the journal", node))
    return findings


def nodes_findings(nodes, expected, fault_window):
    """One leader, every expected node alive and a voter."""
    severity = "info" if fault_window else "fail"
    findings = []
    leaders = [node["node_id"] for node in nodes if node.get("is_leader")]
    if len(leaders) != 1:
        findings.append(finding("leader", severity, f"{len(leaders)} leaders: {leaders}"))
    by_name = {node["node_id"]: node for node in nodes}
    for name in expected:
        node = by_name.get(name)
        if node is None or node.get("state") != "alive":
            findings.append(finding("node-alive", severity, "not alive: " + (node or {}).get("state", "missing"), name))
        elif not node.get("is_council"):
            findings.append(finding("council", severity, "not a voter", name))
    return findings


def wtf_findings(state, report, now, fault_window):
    """Critical findings may last five minutes and warnings ten, counted from the last settle."""
    findings = []
    seen = state.setdefault("wtf_seen", {})
    reported = state.setdefault("wtf_reported", [])
    settled = state.get("last_settled", 0)
    current = set()
    for severity, entries, limit in (("critical", report.get("critical", []), WTF_CRITICAL_LIMIT),
                                     ("warning", report.get("warnings", []), WTF_WARNING_LIMIT)):
        for entry in entries:
            key = f"{severity}|{entry['id']}|{entry.get('affected_resource', '')}"
            # Every alert of one resource shares an id, so the rule names it.
            rule = entry.get("title", "").split(":")[0] if entry["id"] == "alerts-firing" else None
            if rule:
                key += "|" + rule
            current.add(key)
            first = max(seen.setdefault(key, now), settled)
            if fault_window or now - first <= limit or key in reported:
                continue
            reported.append(key)
            if rule in UNGATED_ALERTS:
                findings.append(finding("wtf-alert", "info", f"{rule} for {now - first} s (recorded, not gated)",
                                        entry.get("affected_resource")))
                continue
            findings.append(finding("wtf-" + severity, "fail",
                                    f"{entry['id']} for {now - first} s: {entry.get('title', '')}",
                                    entry.get("affected_resource")))
    for key in list(seen):
        if key not in current:
            del seen[key]
            if key in reported:
                reported.remove(key)
    return findings


def settle_clean(report, nodes, nodes_problems, leaks, http_ok):
    """What the harness waits for after a fault, twice in a row. Missing evidence is not clean."""
    if report is None or nodes is None or report.get("critical"):
        return False
    if "replicas" not in {entry["id"] for entry in report.get("ok", [])}:
        return False
    return not nodes_problems and not leaks and http_ok is not False


# --- host forwards (#987) --------------------------------------------------------

def test_failure_reasons(report_text, stderr_text=""):
    """Why a `relish --output json test` run failed, in one line (#1068).

    Names each case that did not pass with its outcome's reason. Without a
    readable report it falls back to the tail of stderr, then of stdout."""
    try:
        results = json.loads(report_text)["results"]
    except (ValueError, KeyError, TypeError):
        results = None
    if results is not None:
        reasons = [f"{result.get('name', '?')}: {result.get('outcome', {}).get('status', '?')}: "
                   f"{result.get('outcome', {}).get('reason', 'no reason given')}"
                   for result in results if result.get("outcome", {}).get("status") != "pass"]
        return "; ".join(reasons) if reasons else "no failing case in the report"
    for text in (stderr_text, report_text):
        tail = " ".join(line.strip() for line in text.strip().splitlines()[-3:])
        if tail:
            return tail
    return "no output"


def curl_reason(code):
    """What a curl exit code means for a probe, in a word or two."""
    return CURL_REASONS.get(code, f"curl exit {code}")


def http_probe(port, scheme, path, timeout=5):
    """None when something answers HTTP on 127.0.0.1:`port`, else why not.

    Any status counts: the probe asks whether the forward reaches the
    service, not whether the service is happy."""
    if scheme == "https":
        context = ssl.create_default_context()
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        connection = http.client.HTTPSConnection("127.0.0.1", port, timeout=timeout, context=context)
    else:
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    try:
        connection.request("GET", path, headers={"Host": "podinfo.localhost"} if scheme == "http" else {})
        connection.getresponse().read(64)
        return None
    except ConnectionRefusedError:
        return "refused"
    except (socket.timeout, TimeoutError):
        return "timeout"
    except ssl.SSLError:
        return "TLS"
    except (http.client.RemoteDisconnected, http.client.BadStatusLine):
        return "empty reply"
    except ConnectionResetError:
        return "reset"
    except OSError as error:
        return "error " + (os.strerror(error.errno) if error.errno else type(error).__name__)
    finally:
        connection.close()


def guest_probe(limactl, vm, port, scheme, path):
    """The same probe from inside the guest, over `limactl shell` (SSH, not
    the port forwarder): None when the service answers there."""
    import subprocess
    url = f"{scheme}://127.0.0.1:{port}{path}"
    try:
        result = subprocess.run([str(limactl), "shell", "--workdir", "/", vm, "sudo", "curl", "-sk", "-o", "/dev/null",
                                 "-w", "%{http_code}", "--connect-timeout", "3", "--max-time", "5", url],
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, timeout=30)
    except subprocess.TimeoutExpired:
        return "limactl shell timeout"
    except OSError as error:
        return "limactl " + type(error).__name__
    if result.returncode == 0 and result.stdout.strip() not in ("", "000"):
        return None
    return curl_reason(result.returncode) if result.returncode else "no HTTP answer"


def probe_forwards(nodes, ports, down, limactl, host=http_probe, guest=guest_probe):
    """One row per forwarded port of every node that is up. The guest is
    asked only when the host gets no answer."""
    rows = []
    for index, node in enumerate(nodes, 1):
        if index in down:
            continue
        for name, guest_port, scheme, path in FORWARDS:
            if name != "api" and index != 1:
                continue
            port = ports["api"] + index - 1 if name == "api" else ports[name]
            failed = host(port, scheme, path)
            inside = None
            if failed is not None:
                inside = guest(limactl, node, guest_port, scheme, path) or "ok"
            rows.append(dict(node=node, name=name, port=port, host=failed or "ok", guest=inside))
    return rows


def host_forward_findings(state, rows, now):
    """A port the host can't reach while the guest serves it is cut off.
    Once it has stayed cut for HOST_FORWARD_GRACE it is one failure for the
    whole incident (later checks see it as info), not a cascade. Returns the
    findings and the cut (node, name) pairs, which excuse the checks that
    go through them."""
    seen = state.setdefault("host_forward", {})
    findings, cut, current = [], set(), set()
    for row in rows:
        if row.get("host") == "ok" or row.get("guest") != "ok":
            continue
        key = f"{row['node']} {row['port']}"
        current.add(key)
        cut.add((row["node"], row["name"]))
        entry = seen.setdefault(key, {"since": now, "reported": False})
        detail = (f"{row['name']} port {row['port']} {row['host']} from the host since {entry['since']} "
                  f"while {row['node']} serves it inside the guest; checks through it are excused")
        if entry["reported"] or now - entry["since"] < HOST_FORWARD_GRACE:
            findings.append(finding("host-forward", "info", detail, key))
        else:
            entry["reported"] = True
            findings.append(finding("host-forward", "fail", detail, key))
    for key in list(seen):
        if key not in current:
            del seen[key]
    return findings, cut


def keep_host_logs(evidence, lima_home, reason, vms, now=None):
    """Copy each VM's `ha.stderr.log`, which Lima truncates when the
    instance starts again, into EVIDENCE/lima/VM/, gzipped."""
    import gzip
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(now if now is not None else time.time()))
    for vm in vms:
        source = Path(lima_home) / vm / "ha.stderr.log"
        if not source.is_file():
            continue
        target = Path(evidence) / "lima" / vm
        target.mkdir(parents=True, exist_ok=True)
        name = f"{stamp}-{reason}"
        sequence = 0
        while (target / f"{name}.ha.stderr.log.gz").exists():
            sequence += 1
            name = f"{stamp}-{reason}-{sequence}"
        with source.open("rb") as data, gzip.open(target / f"{name}.ha.stderr.log.gz", "wb") as out:
            out.write(data.read())


# --- evaluation ----------------------------------------------------------------

def read(snapshot, name):
    path = Path(snapshot) / name
    return path.read_text() if path.exists() else None


def read_json(snapshot, name):
    text = read(snapshot, name)
    if not text or not text.strip():
        return None
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return None


def job_findings(evidence, snapshot, now, fault_window, state=None):
    """The job campaign's findings for one snapshot. The campaign keeps each
    error it records for the whole run, so a check fails on an error only
    the first time it sees it; after that it is `info`, and the final
    verdict still fails on it through `job_soak.coverage_failures` (#968)."""
    metadata=read_json(evidence,"metadata.json") or {}
    if metadata.get("job_soak") is not True: return []
    value=read_json(snapshot,"jobs.json")
    problems=job_soak.findings(value,now,fault_window)
    # The harness sets job_stop_at only once the last cycle is over (#1338).
    stop_at=metadata.get("job_stop_at")
    if value and value.get("stopped") and (stop_at is None or now < stop_at):
        problems.append("job controller stopped before the final drain interval")
    errors=set(value.get("errors",[])) if isinstance(value,dict) else set()
    reported=state.setdefault("job_errors_reported",[]) if state is not None else []
    result=[]
    for item in problems:
        if item in errors and item in reported:
            result.append(finding("jobs","info","reported earlier: "+item))
            continue
        if item in errors and state is not None:
            reported.append(item); state["job_errors_reported"]=reported[-64:]
        result.append(finding("jobs","fail",item))
    return result


def collection_end(snapshot, meta, now):
    """When the harness finished collecting `snapshot`: meta.json's
    `collected`, or for a snapshot from before it was recorded, when its last
    collected file was written. The guests are asked after meta.json's `ts`,
    so their evidence is stamped between the two."""
    if type(meta.get("collected")) is int:
        return meta["collected"]
    written = [path.stat().st_mtime for path in Path(snapshot).iterdir()
               if path.name not in ("meta.json", "verdict.json", "findings.txt")]
    return max(now, int(max(written))) if written else now


def evaluate(evidence, snapshot):
    state = load_state(evidence)
    meta = read_json(snapshot, "meta.json") or {}
    now = meta.get("ts", int(time.time()))
    fault_window = bool(state.get("window"))
    expected_nodes = state.get("nodes", [])
    findings = []

    forwards = read_json(snapshot, "forwards.json")
    cut = set()
    if isinstance(forwards, list):
        forward_findings, cut = host_forward_findings(state, forwards, now)
        findings += forward_findings
    ingress_cut = any(name == "ingress" for _, name in cut)
    # The latency SLOs' API reads (#1400) count only outside fault windows.
    soak_slo.fold_snapshot(evidence, snapshot, fault_window, cut)

    http = read(snapshot, "http.txt")
    http_ok = None
    if http is not None:
        http_ok = http.split()[:1] == ["200"]
        if not http_ok and ingress_cut:
            # The probe went through the cut forward: the product isn't judged by it.
            http_ok = None
            findings.append(finding("ingress-http", "info",
                                    "podinfo answered " + (http.strip() or "nothing") + " through a cut host forward"))
        elif not http_ok:
            findings.append(finding("ingress-http", "info" if fault_window else "fail",
                                    "podinfo answered " + (http.strip() or "nothing")))

    # The writer file (writer-lost, writer-regression) is the data-loss check;
    # these only see lines through the log view. State keeps the old keys:
    # state[check] is the highest value ever seen and never goes down (the
    # writer-regression check compares the file against it), while
    # "log-baseline" is what the next tail must not end below.
    baselines = state.setdefault("log-baseline", {})
    power_cut_excused = False
    advanced = False
    for check, order_check, name, prefix, source in (
            ("writer-ack", "writer-log-order", "writer-log.txt", "ACK",
             "line order in the log view; the writer file checks decide data loss"),
            ("redis-counter", "redis-log-order", "redis-log.txt", "INCR",
             "line order in the log view, not a read of the stored counter")):
        text = read(snapshot, name)
        if text is None:
            continue
        runs = parse_runs(text, prefix)
        end = view_end(runs)
        baseline = baselines.get(check, state.get(check))
        order = sequence_findings(order_check, runs, baseline, source)
        if state.get("power_cut") and end is not None and baseline is not None and end < baseline:
            # A powered-off node loses the stdout it hadn't synced yet, as any
            # log does; the tail may end below what an earlier check saw. Only
            # the "ends below" finding is excused, and the baseline restarts
            # from here; lines going backwards within one tail still fail.
            order = [dict(item, severity="info", detail=item["detail"] + "; after a power cut, lines not yet synced are lost from the log view")
                     if "below the" in item["detail"] else item for item in order]
            baselines[check] = end
            power_cut_excused = True
        elif fault_window and end is not None and baseline is not None and end < baseline:
            # A bun that just restarted (killed, upgraded) re-reads its
            # capture files; until it has, the newest lines are missing from
            # the view. That's judged once the window closes, against the
            # unchanged baseline, so a dip that never recovers still fails.
            order = [dict(item, severity="info", detail=item["detail"] + "; inside a fault window, judged again once it settles")
                     if "below the" in item["detail"] else item for item in order]
        elif end is not None:
            advanced = advanced or baseline is None or end > baseline
            baselines[check] = max(end, baseline or 0)
        findings += order
        if end is not None:
            if end == state.get(check) and not fault_window:
                findings.append(finding(check, "warn", f"not advancing at {end}"))
            state[check] = max(end, state.get(check, 0))
            state.setdefault("progress", {})[check] = end
    if state.get("power_cut") and not fault_window and advanced and not power_cut_excused:
        state.pop("power_cut")

    writer = read(snapshot, "writer-file.txt")
    if writer is not None:
        findings += writer_file_findings(writer, state.get("writer-ack"))

    report = read_json(snapshot, "wtf.json")
    if report is not None:
        findings += wtf_findings(state, report, now, fault_window)
        if report.get("critical"):
            state["wtf_critical_samples"] = state.get("wtf_critical_samples", 0) + 1

    nodes = read_json(snapshot, "nodes.json")
    node_problems = []
    if nodes is not None:
        node_problems = nodes_findings(nodes, expected_nodes, fault_window)
        findings += node_problems
        leaders = [node for node in nodes if node.get("is_leader")]
        if leaders:
            state.pop("no_leader_since", None)
        else:
            since = state.setdefault("no_leader_since", now)
            if now - since > NO_LEADER_LIMIT and state.get("nodes_down", 0) <= 1:
                findings.append(finding("no-leader", "fail", f"no leader for {now - since} s with at most one node down"))

    status = read_json(snapshot, "status.json")
    by_node = instances_by_node(status) if isinstance(status, list) else None
    baseline = state.get("baseline", {})
    leaks = []
    jobs_enabled=(read_json(evidence,"metadata.json") or {}).get("job_soak") is True
    job_found=job_findings(evidence,snapshot,now,fault_window,state)
    findings += job_found
    # An error reported by an earlier check doesn't hold back this settle.
    job_problems=[item for item in job_found if item["severity"]!="info"]
    for node in expected_nodes:
        diagnostics = read_json(snapshot, f"diagnostics-{node}.json")
        if diagnostics is not None:
            findings += diagnostics_findings(state, node, diagnostics, now)
        for kind, margin, check in (("ingress", INGRESS_CERT_MARGIN, "ingress-cert"),
                                    ("identity", WORKLOAD_CERT_MARGIN, "workload-cert"),
                                    ("api", NODE_CERT_MARGIN, "api-cert")):
            text = read(snapshot, f"{kind}-{node}.txt")
            if not text:
                continue
            served = parse_openssl(text)
            findings += certificate_findings(check, served.get("not_after"), now, margin, node)
            findings += served_serial_findings(state, kind, node, served.get("serial"), now)
        text = read(snapshot, f"inventory-{node}.txt")
        if text is not None:
            inventory = parse_inventory(text)
            findings += restart_findings(state, node, inventory, restart_expectations(evidence, node), now)
            findings += resource_trend_findings(state, node, inventory, now)
            findings += clock_findings(state, node, inventory, fault_window)
            if jobs_enabled:
                job_metric_samples(state, node, inventory, now)
            owned=None
            if jobs_enabled:
                text=read(snapshot,f"jobs-{node}.txt")
                try:
                    owned=job_soak_inventory.validate(json.loads(text) if text else None,now,(inventory.get("boot") or [None])[0],
                                                      collection_end(snapshot,meta,now))
                    # Refuse a forged proof even when there are no app instances.
                    job_soak_inventory.exemptions(owned)
                    usage=state.setdefault("job_resources",{}).setdefault(node,{})
                    usage.update(ts=owned["ts"],complete=True,available_kb=owned["available_kb"],storage_kb=owned["storage_kb"],live_owners=len(owned["owners"]))
                    usage["peak_owners"]=max(usage.get("peak_owners",0),len(owned["owners"]))
                    peaks=usage.setdefault("peak_storage_kb",{})
                    for scope,size in owned["storage_kb"].items(): peaks[scope]=max(peaks.get(scope,0),size)
                    campaign_snapshot=read_json(snapshot,"jobs.json") or {}
                    if campaign_snapshot.get("stopped") and now-campaign_snapshot.get("heartbeat",0)>5 and owned["owners"]:
                        raise job_soak_inventory.InvalidInventory("job-owned resources remain after positive campaign drain")
                except (job_soak_inventory.InvalidInventory,ValueError,KeyError,TypeError) as error:
                    problem=finding("job-inventory","info" if fault_window else "fail",str(error),node)
                    job_problems.append(problem); findings.append(problem)
            lingering=state.get("setup_peers",{}).get(node,[])
            if owned is not None:
                # A setup peer lasts milliseconds; one seen by two inventories leaked.
                state.setdefault("setup_peers",{})[node]=sorted(v for v in owned["resources"].get("veth",[]) if v.startswith("vtc-"))
            if by_node is not None and node in baseline:
                leaks += leak_findings(baseline[node]["inventory"], inventory,
                                       baseline[node]["instances"], by_node.get(node, []), node,owned,lingering)
        elif jobs_enabled and not fault_window and meta.get("kind") in INVENTORY_KINDS:
            # Light snapshots don't collect guest inventories (#943).
            problem=finding("job-inventory","fail","kernel/storage inventory missing",node)
            job_problems.append(problem); findings.append(problem)
        text = read(snapshot, f"export-{node}.txt")
        if text is not None:
            findings += export_findings(state, node, text)
            findings += snapshot_archive_findings(state, node, text)
        turns = read_json(snapshot, f"turns-{node}.json")
        if turns is not None:
            findings += turn_findings(state, node, turns)
    # Leaks only count once the cluster has settled; before that they are progress.
    findings += [dict(leak, severity="info") if fault_window else leak for leak in leaks]

    expected = expected_policies(evidence)
    listing = read_json(snapshot, "policies.json")
    if expected and listing is not None:
        findings += policy_findings(state, expected, listing)

    registry = read_json(snapshot, "registry.json")
    registry_cut = any(name == "registry" for _, name in cut)
    if registry is not None and registry_cut:
        for problem in registry.get("problems", []) + registry.get("unavailable", []):
            findings.append(finding("registry", "info", problem + " (through a cut host forward)"))
    elif registry is not None:
        for problem in registry.get("problems", []):
            findings.append(finding("registry", "fail", problem))
        # Refusing reads without a leader is Pickle failing closed, not data
        # loss. Outside a fault window nothing excuses it.
        for problem in registry.get("unavailable", []):
            findings.append(finding("registry", "info" if fault_window else "fail", problem))

    clean = None
    if meta.get("kind") in ("settle", "heavy"):
        # Without status the leak check couldn't run, so the node isn't known to be clean.
        clean = settle_clean(report, nodes, node_problems, leaks, http_ok) and by_node is not None and not job_problems
    findings = suppress_repeats(state, findings, now)
    verdict = {"ts": now, "kind": meta.get("kind"), "fault_window": fault_window,
               "settle_clean": clean, "findings": findings}
    (Path(snapshot) / "verdict.json").write_text(json.dumps(verdict, indent=1))
    failures = [item for item in findings if item["severity"] == "fail"]
    state["checks"] = state.get("checks", 0) + 1
    state.setdefault("check_counts", {})[meta.get("kind", "other")] = state.get("check_counts", {}).get(meta.get("kind", "other"), 0) + 1
    save_state(evidence, state)
    for item in findings:
        if item["severity"] in ("fail", "warn"):
            print(f"{item['severity']}: {item['check']}{' ' + item['target'] if item['target'] else ''}: {item['detail']}")
    if failures:
        return 1
    return 0 if clean in (None, True) else 3


def suppress_repeats(state, findings, now):
    """One failure bundle per check and target every half hour; repeats stay in the verdict."""
    reported = state.setdefault("reported", {})
    result = []
    for item in findings:
        key = f"{item['check']}|{item.get('repeat_key') or item['target'] or ''}"
        if item["severity"] == "fail":
            if now - reported.get(key, -REPEAT_WINDOW) < REPEAT_WINDOW:
                item = dict(item, severity="repeat")
            else:
                reported[key] = now
        result.append(item)
    return result


# --- policies (#1311) --------------------------------------------------------

def expected_policies(evidence):
    """The `[policy.*]` tables of the workload file the harness applied
    (EVIDENCE/workloads.toml), or None before it has written one."""
    path = Path(evidence) / "workloads.toml"
    if not path.exists():
        return None
    return tomllib.loads(path.read_text()).get("policy", {})


# What a policy decides. The message is for people, so a reworded one isn't
# a lost policy.
POLICY_KEYS = ("match", "deny", "action", "applies_to", "default")
POLICY_DEFAULTS = {"action": "deny", "applies_to": "apps"}


def policy_differences(declared, listed):
    """The keys whose value differs between a declared policy and the one in
    force, comparing absent keys by their default."""
    return [key for key in POLICY_KEYS
            if declared.get(key, POLICY_DEFAULTS.get(key)) != listed.get(key, POLICY_DEFAULTS.get(key))]


def policy_findings(state, expected, listing):
    """Fail when a policy the soak applied is gone or changed, or the policy
    generation went backwards: Raft keeps both through every fault."""
    policies = listing.get("policies") if isinstance(listing, dict) else None
    if not isinstance(policies, dict):
        return [finding("policies", "warn", "`relish policy list` answered without a policy map")]
    findings = []
    for name, declared in sorted(expected.items()):
        listed = policies.get(name)
        if listed is None:
            findings.append(finding("policies", "fail", f"policy {name} is not in force", name))
            continue
        changed = policy_differences(declared, listed)
        if changed:
            findings.append(finding("policies", "fail",
                                    f"policy {name} differs from the soak workload's in {', '.join(changed)}", name))
    generation = listing.get("generation")
    previous = state.get("policy_generation")
    if isinstance(generation, int):
        if isinstance(previous, int) and generation < previous:
            findings.append(finding("policies", "fail", f"policy generation went from {previous} back to {generation}"))
        state["policy_generation"] = max(generation, previous or 0)
    state["policy_checks"] = state.get("policy_checks", 0) + 1
    return findings


def check_policies(evidence, listing_path):
    """The configure step's check: 0 when every policy in the applied
    workload file is in force, 1 (printing why) when one isn't."""
    expected = expected_policies(evidence) or {}
    try:
        listing = json.loads(Path(listing_path).read_text())
    except (OSError, json.JSONDecodeError) as error:
        print(f"fail: policies: cannot read {listing_path}: {error}")
        return 1
    problems = policy_findings({}, expected, listing)
    for item in problems:
        print(f"{item['severity']}: policies: {item['detail']}")
    return 1 if problems else 0


def turn_scrapes(samples):
    """A node's bun_agent_loop_turn_seconds_bucket samples, as /v1/metrics
    returns them, grouped per branch into scrapes, oldest first:
    {branch: [(timestamp, {le: cumulative count})]}."""
    scrapes = {}
    for sample in samples if isinstance(samples, list) else []:
        labels = sample.get("labels") or {}
        if isinstance(labels, str):
            try:
                labels = json.loads(labels)
            except json.JSONDecodeError:
                continue
        branch, le = labels.get("branch"), labels.get("le")
        if branch is None or le is None:
            continue
        stamp = sample.get("timestamp", 0)
        scrapes.setdefault(branch, {}).setdefault(stamp, {})[le] = sample.get("value", 0)
    return {branch: sorted(by_stamp.items()) for branch, by_stamp in scrapes.items()}


def turn_findings(state, node, samples):
    """Fail on every agent-loop turn over the 1 s budget since the last check.

    The buckets count from the bun process's start, so the check remembers
    the last scrape it counted per branch and adds up only the scrapes after
    it, the way Prometheus's increase() does. A count going down is a new
    process (a kill, an upgrade's exec), whose turns all count. A branch the
    new process hasn't reported yet still shows the old process's last
    scrapes, which add nothing (#1261).

    On the node whose writes the soak throttles (`node-slow-fsync`), a turn
    that journals is slow by construction: those turns are info, and stay out
    of the record's over-budget count, while the window lasts (#972)."""
    throttled = state.get("window") == "node-slow-fsync" and state.get("window_node") == node
    seen = state.setdefault("turns", {}).setdefault(node, {})
    findings = []
    for branch, scrapes in sorted(turn_scrapes(samples).items()):
        last = seen.get(branch, {})
        new, buckets = 0, None
        for stamp, counts in scrapes:
            total, within = counts.get("+Inf"), counts.get(TURN_BUDGET_LE)
            if total is None or within is None or stamp <= last.get("at", float("-inf")):
                continue
            over = int(total - within)
            restarted = total < last.get("total", 0) or over < last.get("over", 0)
            previous = 0 if restarted else last.get("over", 0)
            if over > previous:
                new, buckets = new + over - previous, counts
            last = {"at": stamp, "total": total, "over": over}
        seen[branch] = last
        if new <= 0:
            continue
        total = buckets["+Inf"]
        worst = next((bound for bound in TURN_BUCKET_BOUNDS
                      if buckets.get(bound) == total), "+Inf")
        lower = {"2.5": "1", "5": "2.5", "10": "5", "+Inf": "10"}[worst]
        span = f"over {lower} s" if worst == "+Inf" else f"between {lower} and {worst} s"
        detail = f"{new} {branch} turn(s) over the 1 s budget, the worst {span}"
        if throttled:
            findings.append(finding("agent-loop-turn", "info", detail + ", during the injected slow-fsync fault", node))
            continue
        findings.append(finding("agent-loop-turn", "fail", detail, node))
        state.setdefault("turns_over_budget", {})[node] = state.get("turns_over_budget", {}).get(node, 0) + new
    return findings


def diagnostics_findings(state, node, diagnostics, now):
    findings = []
    certificates = diagnostics.get("certificates", {})
    if certificates.get("state") not in ("available", "degraded"):
        return [finding("diagnostics", "warn", "certificates " + certificates.get("state", "missing"), node)]
    for certificate in certificates.get("value", []):
        kind = certificate.get("certificate_kind")
        identity = certificate.get("identity", "")
        margin = NODE_CERT_MARGIN if kind == "node" else WORKLOAD_CERT_MARGIN
        findings += certificate_findings(f"{kind}-cert", certificate.get("not_after"), now, margin, f"{node} {identity}")
        if kind == "node":
            findings += rotation_findings(state, node, certificate.get("rotation_state"), now)
            record_serial(state, "node-leaf", node, certificate.get("serial", ""))
    return findings


def record_serial(state, kind, node, serial):
    """Count renewals as changes of the observed serial."""
    serials = state.setdefault("serials", {}).setdefault(kind, {})
    value = normalise_serial(serial) if serial else None
    if value is None:
        return
    previous = serials.get(node)
    if previous is not None and previous != value:
        renewals = state.setdefault("renewals", {}).setdefault(kind, {})
        renewals[node] = renewals.get(node, 0) + 1
    serials[node] = value


def served_serial_findings(state, kind, node, serial, now):
    if serial is None:
        return []
    kind_name = {"ingress": "ingress-leaf", "identity": "workload-identity", "api": "api-leaf"}[kind]
    if kind != "ingress":
        record_serial(state, kind_name, node, serial)
        return []
    expected = state.get("ingress_expected", {}).get(node)
    if expected is None or normalise_serial(serial) == expected["serial"]:
        return []
    return [finding("ingress-serial", "fail",
                    f"serves {serial}, not the {expected['serial']:X} reloaded {now - expected['since']} s ago", node)]


def clock_findings(state, node, inventory, fault_window):
    """One clock source per guest (#608).

    Lima's guest agent sets the guest clock to the host's every 10 s once
    they differ by more than 100 ms. systemd-timesyncd running beside it
    fought it: the agent stepped the clock back every 10 s. Ubuntu 26.04's
    chrony would fight it the same way. Each step the agent logged since the
    last check is counted; one after a power-on is expected, a steady run of
    them outside a fault window is not."""
    findings = []
    for kind, daemon in (("timesyncd", "systemd-timesyncd"), ("chrony", "chrony")):
        if (inventory.get(kind) or [None])[0] == "active":
            findings.append(finding("guest-clock", "fail",
                                    f"{daemon} is running beside Lima's guest agent", node))
    steps = inventory_number(inventory, "clock_steps")
    if steps:
        counts = state.setdefault("clock_steps", {})
        counts[node] = counts.get(node, 0) + steps
        findings.append(finding("guest-clock-steps", "info" if fault_window else "warn",
                                f"Lima's guest agent stepped the clock {steps} time(s) since the last check", node))
    return findings


def resource_trend_findings(state, node, inventory, now):
    node_state = state.setdefault("resources", {}).setdefault(node, {})
    pid = inventory_number(inventory, "bun_pid")
    image = (inventory.get("bun_image") or [None])[0]
    fd = inventory_number(inventory, "bun_fd")
    rss = inventory_number(inventory, "bun_rss_kb")
    # An upgrade walk execs bun in place: same pid, a new image whose RSS
    # starts from nothing. Either change is a new process to warm up.
    changed = pid != node_state.get("pid") or image != node_state.get("image")
    if changed:
        node_state.update(pid=pid, image=image, fd_samples=[], started=now)
        node_state.pop("rss_warm_kb", None)
    record_exposure(state, node, changed, dict(rss_kb=rss, fd=fd), now - node_state["started"], now)
    if fd is not None:
        node_state["fd_samples"] = [sample for sample in node_state["fd_samples"] if sample[0] >= now - (FD_RISES + 3) * FD_BUCKET] + [[now, fd]]
    trend = state.setdefault("trends", {}).setdefault(node, {})
    for key, value in (("fd", fd), ("rss_kb", rss)):
        if value is None:
            continue
        entry = trend.setdefault(key, {"first": value, "max": value})
        entry["last"] = value
        entry["max"] = max(entry["max"], value)
    for line in inventory.get("disk", []):
        name, _, size = line.partition(" ")
        if size.isdigit():
            entry = trend.setdefault("disk_kb " + name, {"first": int(size), "max": int(size)})
            entry["last"] = int(size)
            entry["max"] = max(entry["max"], int(size))
    findings = [dict(item, target=node) for item in fd_findings(node_state["fd_samples"], node_state["started"], now)]
    findings += [dict(item, target=node) for item in rss_findings(node_state, rss, now, node_state["started"])]
    return findings


def record_exposure(state, node, changed, values, age, now):
    """How long each leak checker could judge this node (#1062).

    A checker is armed once the Bun process it watches has lived past the
    checker's warm-up; a new pid or image starts the warm-up again. The
    time between two armed samples of one process counts as armed; the
    record reports the total, the longest unbroken window, the samples
    judged and how many processes ever armed it. Reported only."""
    for check, key, armed_after in CHECKER_ARMED_AFTER:
        entry = state.setdefault("exposure", {}).setdefault(node, {}).setdefault(check, dict(
            armed_seconds=0, longest_seconds=0, samples=0, processes_armed=0, window_seconds=0, last=None))
        if changed:
            entry.update(last=None, window_seconds=0)
        if values[key] is None or age < armed_after:
            continue
        if entry["last"] is None:
            entry["processes_armed"] += 1
        else:
            entry["armed_seconds"] += now - entry["last"]
            entry["window_seconds"] += now - entry["last"]
            entry["longest_seconds"] = max(entry["longest_seconds"], entry["window_seconds"])
        entry["samples"] += 1
        entry["last"] = now


def exposure_lines(state, gated):
    """The record's table of armed time per node and leak checker; a final
    tier gates on it, other tiers record it."""
    short = set(exposure_shortfalls(state))
    lines = [f"## Leak-checker exposure ({'gated' if gated else 'recorded, not gated'})", "",
             "How long each leak checker could judge each node's Bun. `leak-rss` arms after "
             f"{RSS_WARMUP // 60} minutes in one process and `leak-fd` after "
             f"{(FD_WARMUP + (FD_RISES + 1) * FD_BUCKET) // 60}; a restart, kill or upgrade exec starts the warm-up "
             f"again. A final needs each armed for {EXPOSURE_ARMED_SECONDS // 60} minutes on every node, with one "
             f"unbroken window of {EXPOSURE_WINDOW_SECONDS // 60} (#1062); its quiet tail gives them that.", "",
             "| Node | Checker | Armed | Longest armed window | Evaluated samples | Processes armed | Verdict |",
             "|---|---|---|---|---|---|---|"]
    for node in state.get("nodes", []):
        for check, _, _ in CHECKER_ARMED_AFTER:
            entry = state.get("exposure", {}).get(node, {}).get(check, {})
            verdict = ("SHORT" if gated else "short") if (node, check) in short else "ok"
            lines.append(f"| {node} | {check} | {entry.get('armed_seconds', 0) // 60} min | "
                         f"{entry.get('longest_seconds', 0) // 60} min | {entry.get('samples', 0)} | "
                         f"{entry.get('processes_armed', 0)} | {verdict} |")
    return lines + [""]


def job_metric_samples(state, node, inventory, now):
    """Accumulate Bun and executor-helper CPU for the M8 measurements.

    Counters only grow within one process or cgroup, so a restart or a
    retired helper starts a new baseline instead of counting backwards."""
    metrics = state.setdefault("job_metrics", {}).setdefault(node, {"bun_cpu_seconds": 0.0, "executor_cpu_seconds": 0.0})
    pid = inventory_number(inventory, "bun_pid")
    ticks = inventory_number(inventory, "bun_cpu_ticks")
    hertz = inventory_number(inventory, "clk_tck")
    mark = metrics.get("bun_mark")
    if ticks is not None and hertz:
        if mark and mark[0] == pid and ticks >= mark[1]:
            metrics["bun_cpu_seconds"] += (ticks - mark[1]) / hertz
            metrics["bun_wall_seconds"] = metrics.get("bun_wall_seconds", 0) + now - mark[2]
        metrics["bun_mark"] = [pid, ticks, now]
    helpers = {}
    for line in inventory.get("executor_cpu", []):
        group, _, usec = line.rpartition(" ")
        if group and usec.isdigit():
            helpers[group] = int(usec)
    if "executor_mark" in metrics:
        previous, since = metrics["executor_mark"]
        for group, usec in helpers.items():
            if group in previous and usec >= previous[group]:
                metrics["executor_cpu_seconds"] += (usec - previous[group]) / 1e6
        metrics["executor_wall_seconds"] = metrics.get("executor_wall_seconds", 0) + now - since
    metrics["executor_mark"] = [helpers, now]
    bun_rss_samples(metrics, inventory, pid, now)


def bun_rss_samples(metrics, inventory, pid, now):
    """Keep each Bun process's RSS samples for its steady state.

    The inventory gives no start time, so a process is its boot, pid and
    image (an upgrade execs in place: same pid, new image), timed from the
    first snapshot that saw it. A sample counts towards the steady state once
    the process has been seen for BUN_STEADY_AFTER seconds."""
    rss = inventory_number(inventory, "bun_rss_kb")
    if rss is None:
        return
    key = dict(boot=(inventory.get("boot") or [None])[0], pid=pid, image=(inventory.get("bun_image") or [None])[0])
    processes = metrics.setdefault("bun_processes", [])
    if not processes or any(processes[-1][name] != value for name, value in key.items()):
        processes.append(dict(key, first_seen=now, peak_kb=rss, steady_kb=[]))
    process = processes[-1]
    process["peak_kb"] = max(process["peak_kb"], rss)
    if now - process["first_seen"] >= BUN_STEADY_AFTER:
        process["steady_kb"].append(rss)


def job_host_metrics(state, accepted):
    """M8 measurements the guests' inventories give, recorded and never gated."""
    trends = state.get("trends", {})
    # Node 1 serves the API the harness and the job controller talk to.
    api_node = (state.get("nodes") or [None])[0]
    per_node = {}
    for node, metrics in sorted(state.get("job_metrics", {}).items()):
        logs = trends.get(node, {}).get("disk_kb logs", {})
        ledger = state.get("job_resources", {}).get(node, {}).get("peak_storage_kb", {}).get("task-arrays")
        processes = [dict(boot=process["boot"], pid=process["pid"], image=process["image"], first_seen=process["first_seen"],
                          steady_samples=len(process["steady_kb"]),
                          steady_rss_mib=statistics.median(process["steady_kb"]) / 1024 if process["steady_kb"] else None,
                          peak_rss_mib=process["peak_kb"] / 1024)
                     for process in metrics.get("bun_processes", [])]
        steady = [process["steady_rss_mib"] for process in processes if process["steady_rss_mib"] is not None]
        per_node[node] = dict(
            bun_processes=processes,
            bun_steady_rss_mib=max(steady) if steady else None,
            bun_peak_rss_mib=max((process["peak_rss_mib"] for process in processes), default=None),
            bun_rss_target_mib=API_BUN_RSS_TARGET_MIB if node == api_node else BUN_RSS_TARGET_MIB,
            bun_cpu_cores=metrics["bun_cpu_seconds"] / metrics["bun_wall_seconds"] if metrics.get("bun_wall_seconds") else None,
            executor_helper_cpu_cores=metrics["executor_cpu_seconds"] / metrics["executor_wall_seconds"] if metrics.get("executor_wall_seconds") else None,
            retained_log_mb=logs.get("max", 0) * 1024 / 1e6 if logs else None,
            peak_task_array_storage_mib=ledger / 1024 if ledger is not None else None)
    def worst(key):
        values = [row[key] for row in per_node.values() if row[key] is not None]
        return max(values) if values else None
    # Targets differ by node, so the worst node is the one closest to its own.
    measured = [(node, row) for node, row in per_node.items() if row["bun_steady_rss_mib"] is not None]
    rss_node, rss_row = max(measured, key=lambda item: item[1]["bun_steady_rss_mib"] / item[1]["bun_rss_target_mib"], default=(None, None))
    logs = [row["retained_log_mb"] for row in per_node.values() if row["retained_log_mb"] is not None]
    ledgers = [row["peak_task_array_storage_mib"] for row in per_node.values() if row["peak_task_array_storage_mib"] is not None]
    return dict(per_node=per_node,
                api_node=api_node,
                bun_rss_worst=dict(node=rss_node, steady_rss_mib=rss_row["bun_steady_rss_mib"], peak_rss_mib=rss_row["bun_peak_rss_mib"],
                                   target_mib=rss_row["bun_rss_target_mib"]) if rss_row else None,
                # The leader moves during the soak: the busiest Bun bounds it.
                leader_cpu_cores=worst("bun_cpu_cores"),
                executor_overhead_cores_per_node=worst("executor_helper_cpu_cores"),
                retained_log_mb=sum(logs) if logs else None,
                node_ledger_mib_per_million_tasks=(sum(ledgers) / accepted * 1e6) if ledgers and accepted else None)


def record_baseline(evidence, snapshot):
    state = load_state(evidence)
    status = read_json(snapshot, "status.json")
    if not isinstance(status, list):
        raise SystemExit("baseline: no status.json in " + str(snapshot))
    by_node = instances_by_node(status)
    baseline = {}
    for node in state.get("nodes", []):
        text = read(snapshot, f"inventory-{node}.txt")
        if text is None:
            raise SystemExit(f"baseline: no inventory for {node}")
        baseline[node] = {"inventory": parse_inventory(text), "instances": by_node.get(node, [])}
    state["baseline"] = baseline
    save_state(evidence, state)


# --- TOML editing ----------------------------------------------------------------

HEADER = re.compile(r"^\[([^\[\]]+)\]\s*(#.*)?$")


def toml_set(text, assignments):
    """Set `section.key = literal` pairs in TOML text, keeping everything else.

    The file is the flat, pretty-printed node.toml that quickstart writes:
    one `[section]` header per table and multi-line arrays closed by `]`.
    """
    lines = text.splitlines()
    for dotted, literal in assignments:
        section, _, key = dotted.rpartition(".")
        if not section or not key:
            raise ValueError(f"expected SECTION.KEY, got {dotted!r}")
        start = next((index for index, line in enumerate(lines)
                      if HEADER.match(line.strip()) and HEADER.match(line.strip()).group(1).strip() == section), None)
        if start is None:
            if lines and lines[-1].strip():
                lines.append("")
            lines += [f"[{section}]", f"{key} = {literal}"]
            continue
        end = next((index for index in range(start + 1, len(lines)) if HEADER.match(lines[index].strip())), len(lines))
        position = None
        for index in range(start + 1, end):
            if re.match(r"^\s*" + re.escape(key) + r"\s*=", lines[index]):
                position = index
                break
        if position is None:
            insert = end
            while insert > start + 1 and not lines[insert - 1].strip():
                insert -= 1
            lines.insert(insert, f"{key} = {literal}")
            continue
        stop = position + 1
        value = lines[position].split("=", 1)[1].strip()
        if value.startswith("[") and value.count("[") > value.count("]"):
            while stop < end and lines[stop - 1].strip() != "]":
                stop += 1
        lines[position:stop] = [f"{key} = {literal}"]
    return "\n".join(lines) + "\n"


# --- registry (Pickle) --------------------------------------------------------------

class PinnedHTTPSConnection(http.client.HTTPSConnection):
    """Connect to a forwarded port but verify the certificate for the node's name."""

    def __init__(self, port, server_name, context):
        super().__init__("127.0.0.1", port, context=context, timeout=20)
        self.server_name = server_name
        self.tls = context

    def connect(self):
        raw = socket.create_connection(("127.0.0.1", self.port), self.timeout)
        self.sock = self.tls.wrap_socket(raw, server_hostname=self.server_name)


def registry_request(args, method, path, body=None, headers=None):
    context = ssl.create_default_context(cafile=args.ca)
    connection = PinnedHTTPSConnection(args.port, args.server_name, context)
    token = Path(args.token_file).read_text().strip()
    headers = dict(headers or {}, Authorization="Bearer " + token)
    connection.request(method, path, body=body, headers=headers)
    response = connection.getresponse()
    data = response.read()
    return response.status, dict(response.getheaders()), data


def registry_push_blob(args, repository, data):
    digest = "sha256:" + hashlib.sha256(data).hexdigest()
    status, headers, _ = registry_request(args, "POST", f"/v2/{repository}/blobs/uploads/")
    if status != 202:
        raise RuntimeError(f"upload start returned {status}")
    location = headers.get("Location") or headers.get("location")
    separator = "&" if "?" in location else "?"
    status, _, body = registry_request(args, "PUT", f"{location}{separator}digest={digest}", data,
                                       {"Content-Type": "application/octet-stream"})
    if status != 201:
        raise RuntimeError(f"blob upload returned {status}: {body[:200]!r}")
    return digest


def registry_get(args, path, headers=None):
    """GET from Pickle, retrying a 503 a bounded number of times."""
    for attempt in range(REGISTRY_UNAVAILABLE_TRIES):
        if attempt:
            time.sleep(REGISTRY_UNAVAILABLE_PAUSE)
        status, _, body = registry_request(args, "GET", path, None, headers)
        if status != 503:
            break
    return status, body


def registry_judge(result, what, status, body, digest):
    """A 503 means the catalogue is unavailable; anything else must be the exact bytes."""
    if status == 503:
        result["unavailable"].append(f"{what} returned 503 after {REGISTRY_UNAVAILABLE_TRIES} tries")
    elif status != 200:
        result["problems"].append(f"{what} returned {status}")
    elif "sha256:" + hashlib.sha256(body).hexdigest() != digest:
        result["problems"].append(f"{what} changed")


def registry_command(args):
    """Push a tiny unique image, or re-fetch every pushed one and check its bytes."""
    state = load_state(args.evidence)
    pushed = state.setdefault("registry_pushed", [])
    result = {"action": args.action, "problems": [], "checked": 0}
    repository = "soak/pulse"
    try:
        if args.action == "push":
            layer = f"reliaburger soak {args.tag} {time.time()}\n".encode()
            config = json.dumps({"architecture": "arm64", "os": "linux", "rootfs": {"type": "layers", "diff_ids": []}}).encode()
            layer_digest = registry_push_blob(args, repository, layer)
            config_digest = registry_push_blob(args, repository, config)
            manifest = json.dumps({
                "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": len(config)},
                "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": layer_digest, "size": len(layer)}],
            }).encode()
            status, _, body = registry_request(args, "PUT", f"/v2/{repository}/manifests/{args.tag}", manifest,
                                               {"Content-Type": "application/vnd.oci.image.manifest.v1+json"})
            if status != 201:
                raise RuntimeError(f"manifest push returned {status}: {body[:200]!r}")
            pushed.append({"tag": args.tag, "manifest": "sha256:" + hashlib.sha256(manifest).hexdigest(),
                           "blobs": [layer_digest, config_digest]})
            result["pushed"] = args.tag
        else:
            result["unavailable"] = []
            for image in pushed:
                status, body = registry_get(args, f"/v2/{repository}/manifests/{image['tag']}",
                                            {"Accept": "application/vnd.oci.image.manifest.v1+json"})
                registry_judge(result, f"{repository}:{image['tag']} manifest", status, body, image["manifest"])
                for digest in image["blobs"]:
                    status, body = registry_get(args, f"/v2/{repository}/blobs/{digest}")
                    registry_judge(result, f"{repository} blob {digest[:19]}", status, body, digest)
                result["checked"] += 1
    except (OSError, RuntimeError, ssl.SSLError, http.client.HTTPException) as error:
        # Unreachable is not data loss; the next check retries.
        result["unreachable"] = str(error)
    save_state(args.evidence, state)
    print(json.dumps(result))
    return 0


# --- record ------------------------------------------------------------------

def duration_text(seconds):
    seconds = int(seconds)
    return f"{seconds // 3600} h {seconds % 3600 // 60:02d} min" if seconds >= 3600 else f"{seconds // 60} min {seconds % 60:02d} s"


def failure_rows(evidence):
    rows = []
    root = Path(evidence) / "failures"
    if not root.is_dir():
        return rows
    for directory in sorted(root.iterdir(), key=lambda path: int(path.name) if path.name.isdigit() else 0):
        summary = directory / "summary.json"
        if summary.exists():
            rows.append(dict(json.loads(summary.read_text()), number=directory.name))
    return rows


def expected_renewals(elapsed, period):
    """Renewals a node must show: one per half-lifetime, less one for phase."""
    return max(0, int(elapsed // period) - 1)


FINAL_TIER_SECONDS = 8 * 3600
# A final ends with a quiet tail after its 8 hours of faults: no faults, walks
# or specials, so every node's Bun lives long enough to arm the leak checkers
# and give them comparison samples (#1062, maintainer decision 20 October
# 2026). Each checker must be armed this long on every node, with one
# unbroken window at least EXPOSURE_WINDOW_SECONDS long; a fast or custom run
# records its exposure without gating on it. qualify-sustained.sh has the
# same tail length. Two hours would be the warm-up plus the armed hour, but
# the last cycle's graceful stop comes after its heavy sample, so each new Bun
# is first seen up to one heavy interval (300 s) into the tail, and every
# heavy observe pushes the next one back by its own run time. The extra
# 15 minutes cover that sampling.
QUIET_TAIL_SECONDS = 2 * 3600 + 15 * 60
# The day tier (#1032) is one real day.
DAY_TIER_SECONDS = 24 * 3600
EXPOSURE_ARMED_SECONDS = 3600
EXPOSURE_WINDOW_SECONDS = 1800

# The execution inventory (#1061). The schedule below mirrors run_cycle and
# special_for_cycle in qualify-sustained.sh; test_sustained_check compares
# it with the harness's --plan, so the two can't drift apart unnoticed.
CHAOS_KINDS = ("kill", "delay", "partition", "scenario", "pause", "drop", "dns", "cpu", "memory")
SCHEDULES = {"full": dict(cycle=3600, chaos_per_slot=3, pulse_every=1, export_every=6),
             "compressed": dict(cycle=600, chaos_per_slot=2, pulse_every=0, export_every=2)}
CANDIDATE_ITEM = "candidate.json bytes match the qualified digest"
BASELINE_ITEM = "app workloads settled at the baseline"
WALK_ITEM = "upgrade walk (to the soak build and back)"
PULSE_ITEM = "catalogue pulse"
EXPORT_ITEM = "offline export"
DRAIN_ITEM = "job campaign drained in its final two minutes"
QUIET_ITEM = "quiet tail with no faults, walks or specials"
SLOT_FAULTS = ("fault:bun-kill-follower", "fault:bun-kill-leader", "fault:deploy-kill", "fault:power-off")
SPECIALS = ("special:graceful-stop", "special:quorum-loss", "special:all-off")


def required_execution(metadata):
    """What a run must have completed, as {item: count}.

    A final tier is held to the 8-hour full schedule with upgrade walks
    whatever its metadata says about schedule, length or soak build, so
    relabelling a shorter run can't lower the bar. Node faults are required
    when the run says it injected them. Other tiers get the schedule they
    say they ran; their inventory is recorded, not gated. The counts are the
    cycles every run completes (min_cycles in the harness), so a run whose
    slots overran still owes them. Each fault kind is owed once; walks,
    specials, pulses and exports are owed as often as the schedule runs them."""
    final = metadata.get("tier") == "final"
    if metadata.get("tier") == "day":
        # The day run (#1032) owes no faults, walks or specials: only the
        # candidate it verified, a settled baseline and the final drain.
        return {CANDIDATE_ITEM: 1, BASELINE_ITEM: 1, DRAIN_ITEM: 1}
    schedule = "full" if final else metadata.get("schedule", "full")
    if schedule not in SCHEDULES:
        schedule = "full"
    plan = SCHEDULES[schedule]
    duration = FINAL_TIER_SECONDS if final else int(metadata.get("duration_target") or 0)
    walks = final or not str(metadata.get("soak_bun", "")).startswith("not supplied")
    cycles = duration // plan["cycle"] if schedule == "full" else 4
    hours = max(duration // 3600, 1)
    half = max(hours // 2, 1)
    required = {CANDIDATE_ITEM: 1, BASELINE_ITEM: 1, WALK_ITEM: 0}
    slots = {phase: 0 for phase in SLOT_FAULTS}
    specials = {phase: 0 for phase in SPECIALS}
    node_kinds = {}
    pulses = exports = chaos_rounds = 0
    for index in range(cycles):
        slots["fault:bun-kill-follower"] += 1
        slots["fault:bun-kill-leader"] += 1
        if index % 2 == 1 and walks:
            required[WALK_ITEM] += 1
            if schedule == "compressed":
                chaos_rounds += plan["chaos_per_slot"]
        else:
            slots["fault:deploy-kill"] += 1
            slots["fault:power-off"] += 1
            chaos_rounds += plan["chaos_per_slot"]
        if metadata.get("node_faults") is True:
            kind = "fault:node-" + job_soak.NODE_FAULT_KINDS[index % 3]
            node_kinds[kind] = 1
        if (plan["pulse_every"] and index % plan["pulse_every"] == 0) or (schedule == "compressed" and index == 0):
            pulses += 1
        if index % plan["export_every"] == plan["export_every"] - 1:
            exports += 1
        if schedule == "compressed":
            if 1 <= index <= 3:
                specials[SPECIALS[index - 1]] += 1
            continue
        hour = index + 1
        if hour % half == 0:
            specials["special:graceful-stop"] += 1
        if hour in ((hours * 40 + 50) // 100, (hours * 85 + 50) // 100):
            specials["special:quorum-loss"] += 1
        if hour == (hours * 70 + 50) // 100:
            specials["special:all-off"] += 1
    for phase, count in slots.items():
        if count:
            required[phase] = 1
    for round_index in range(min(chaos_rounds, len(CHAOS_KINDS))):
        required["fault:chaos-" + CHAOS_KINDS[round_index]] = 1
    required.update(node_kinds)
    required.update({phase: count for phase, count in specials.items() if count})
    if pulses:
        required[PULSE_ITEM] = pulses
    if exports:
        required[EXPORT_ITEM] = exports
    if final:
        required[QUIET_ITEM] = 1
    required[DRAIN_ITEM] = 1
    if not required[WALK_ITEM]:
        del required[WALK_ITEM]
    return required


def candidate_verified(evidence, metadata):
    """The candidate is known only when this run bootstrapped from it and the
    bytes it kept hash to the digest it was asked to qualify. An adopted
    --home installed whatever it installed before this run looked."""
    digest = str(metadata.get("qualified_digest") or "")
    path = Path(evidence) / "candidate.json"
    if metadata.get("bootstrapped") not in (True, "true") or not re.fullmatch(r"[0-9a-f]{64}", digest) or not path.is_file():
        return False
    return hashlib.sha256(path.read_bytes()).hexdigest() == digest


def completed_execution(evidence, metadata, events, job_report):
    """What the run actually completed, as {item: count}, from its events,
    the candidate bytes it kept and the job controller's final snapshot.

    A fault counts once it was injected (no failed or refused injection) and
    the cluster settled after it; a walk once it reached the soak build,
    rolled back and settled; quorum loss once quorum came back too."""
    def count(phase, **match):
        return sum(1 for event in events if event.get("phase") == phase
                   and all(event.get(key) == value for key, value in match.items()))

    def injected(phase):
        return sum(1 for event in events if event.get("phase") == phase and event.get("settle_seconds") is None
                   and event.get("verdict") in (None, "ok") and event.get("exit") in (None, 0))

    completed = {CANDIDATE_ITEM: 1 if candidate_verified(evidence, metadata) else 0,
                 BASELINE_ITEM: min(count("baseline", verdict="ok"), 1),
                 WALK_ITEM: min(count("upgrade:upgrade", verdict="ok"), count("upgrade:rollback", verdict="ok"),
                                count("upgrade:settle", verdict="settled")),
                 PULSE_ITEM: count("pulse", verdict="ok"),
                 EXPORT_ITEM: count("storage:offline-export", verdict="ok")}
    phases = {event.get("phase", "") for event in events}
    for phase in phases | set(SLOT_FAULTS) | set(SPECIALS):
        if phase.startswith(("fault:", "special:")) and phase != "special:quorum-back":
            completed[phase] = min(injected(phase), count(phase, verdict="settled"))
    completed["special:quorum-loss"] = min(completed["special:quorum-loss"], count("special:quorum-back", verdict="ok"))
    report = job_report if isinstance(job_report, dict) else {}
    stop = report.get("stop_requested_at")
    drained = (metadata.get("job_soak") is True and report.get("stopped") is True and not report.get("active")
               and type(stop) is int and stop >= metadata.get("job_stop_at", 0))
    completed[DRAIN_ITEM] = 1 if drained else 0
    completed[QUIET_ITEM] = quiet_tails(events)
    return completed


def quiet_tails(events):
    """Quiet tails that lasted QUIET_TAIL_SECONDS (a minute's slack for the
    clock) with no fault, walk or special inside them."""
    complete, started, disturbed = 0, None, False
    for event in events:
        phase = event.get("phase", "")
        if phase == "quiet" and event.get("verdict") is None:
            started, disturbed = event.get("ts"), False
        elif phase == "quiet" and started is not None:
            if event.get("verdict") == "ok" and (event.get("duration") or 0) >= QUIET_TAIL_SECONDS - 60 and not disturbed:
                complete += 1
            started = None
        elif started is not None and phase.startswith(("fault:", "upgrade", "special:")):
            disturbed = True
    return complete


def exposure_shortfalls(state):
    """(node, checker) pairs armed too briefly, or never for one unbroken window."""
    short = []
    for node in state.get("nodes", []):
        for check, _, _ in CHECKER_ARMED_AFTER:
            entry = state.get("exposure", {}).get(node, {}).get(check, {})
            if (entry.get("armed_seconds", 0) < EXPOSURE_ARMED_SECONDS
                    or entry.get("longest_seconds", 0) < EXPOSURE_WINDOW_SECONDS):
                short.append((node, check))
    return short


def execution_inventory(evidence, metadata, events, job_report):
    """Rows of (item, required, completed) in the order the record lists them."""
    required = required_execution(metadata)
    completed = completed_execution(evidence, metadata, events, job_report)
    return [(item, need, completed.get(item, 0)) for item, need in required.items()]


def job_final_ran(metadata, job_report, missing):
    """A job final is the job controller at the target load with every node
    fault completed; the labels alone don't make one."""
    return (metadata.get("node_faults") is True and isinstance(job_report, dict) and job_report.get("load") == "target"
            and not [item for item in missing if item.startswith("fault:node-")])


def acceptance_line(metadata, result, elapsed, missing=(), job_final=False, exposure=(), day=None):
    """What the run says about the V02 gate (plan D3) and the release gate's
    final soak (#991): only a clean, complete final tier passes either. A day
    tier (#1032) says whether the day held."""
    tier = metadata.get("tier") or "custom"
    clean = result == "PASS"
    if tier == "day":
        return day_acceptance_line(metadata, clean, elapsed, missing, exposure, day)
    if tier == "fast":
        if clean:
            return ("**Fast tier: clean.** The iteration loop only, not acceptance: the V02 gate needs a clean "
                    "final-tier run (8 h, full schedule) on the final candidate.")
        return "**Fast tier: not clean.** Fix what failed and run the fast tier again before a final-tier run."
    if tier != "final":
        return ("**Custom run: diagnostic only.** It ran no tier, so it says nothing about the V02 gate or the "
                "release gate's final soak.")
    shortfalls = []
    if metadata.get("schedule", "full") != "full":
        shortfalls.append(f"it ran the {metadata['schedule']} schedule")
    if min(elapsed, metadata.get("duration_target") or 0) < FINAL_TIER_SECONDS:
        shortfalls.append(f"it soaked for {duration_text(elapsed)}")
    gaps = (["missing from the execution inventory: " + ", ".join(missing)] if missing else []) + (
        ["leak-checker exposure too short: " + ", ".join(f"{node} {check}" for node, check in exposure)]
        if exposure else [])
    if not clean:
        return ("**Final tier: not clean. The V02 gate does not pass.** A product fix means a fresh fast-tier run, "
                "then a fresh final-tier run." + (" It is incomplete too: " + "; ".join(gaps) + "." if gaps else ""))
    if gaps:
        return ("**Final tier: incomplete. It passes neither the V02 gate nor the release gate's final soak:** "
                + "; ".join(shortfalls + gaps)
                + ". A final tier owes every walk, fault kind and special of its 8-hour schedule, then a quiet tail "
                "long enough to arm every leak checker on every node.")
    if shortfalls:
        return (f"**Final tier: clean, but not acceptance:** {'; '.join(shortfalls)}. The V02 gate needs 8 h on "
                "the full schedule against a pinned candidate.")
    digest = f"`{metadata.get('qualified_digest')}`"
    if job_final:
        return (f"**Final job tier: clean. The release gate's final soak passes** for the candidate whose "
                f"`candidate.json` has SHA-256 {digest}, if it is the final candidate.")
    return (f"**Final tier (V02 alone): clean. The V02 soak gate passes** for the candidate whose `candidate.json` "
            f"has SHA-256 {digest}, if it is the final candidate. It is not the release gate's final soak, which is "
            "the job final (`qualify-jobs-sustained.sh --tier final`, #991).")


def day_acceptance_line(metadata, clean, elapsed, missing, exposure, day):
    """What a day tier (#1032) says: the day's accepted host-job tasks, and
    whether every check held for all of it."""
    gaps = (["missing from the execution inventory: " + ", ".join(missing)] if missing else []) + (
        ["leak-checker exposure too short: " + ", ".join(f"{node} {check}" for node, check in exposure)]
        if exposure else [])
    if elapsed < DAY_TIER_SECONDS:
        gaps.append(f"it ran for {duration_text(elapsed)}, not a whole day")
    if not clean or not day:
        return ("**Day tier: not clean.** The day run doesn't hold; the record says what failed."
                + (" It is incomplete too: " + "; ".join(gaps) + "." if gaps else ""))
    if gaps:
        return "**Day tier: incomplete.** " + "; ".join(gaps) + "."
    return (f"**Day tier: clean.** {day['host_successes']:,} host-job tasks accepted in {duration_text(elapsed)}, "
            f"{day['host_successes_per_second']:,.1f}/s against the {day['target_rate']}/s target, for the candidate "
            f"whose `candidate.json` has SHA-256 `{metadata.get('qualified_digest')}`.")


def day_lines(day):
    """The record's section for the day run (#1032): the total against the
    target and the floor, each node's share and every full hour's rate."""
    elapsed = max(1, day.get("elapsed_seconds") or 1)
    lines = ["## Day run (#1032)", "",
             f"{day['host_successes']:,} accepted host-job tasks in {duration_text(elapsed)}: "
             f"{day['host_successes_per_second']:,.1f}/s against the {day['target_rate']}/s target "
             f"({'PASS' if day['target_met'] else 'FAIL'}). The billion-a-day floor is {day['floor_rate']:,}/s "
             f"({'met' if day['floor_met'] else 'not met'}). At this rate a day is {day['day_equivalent']:,} tasks.", "",
             "| Node | Slots | Host-job successes | Rate | Share | Fair share | Verdict |", "|---|---|---|---|---|---|---|"]
    for row in day["per_node"]["nodes"]:
        verdict = "OVER" if row["over_fair_share"] else "ok"
        if row["restarted"]:
            verdict += " (restarted)"
        lines.append(f"| {row['node']} | {row['slots']} | {row['successes']:,} | {row['successes'] / elapsed:,.1f}/s | "
                     f"{row['share_percent']:.1f}% | {row['fair_share_percent']:.1f}% | {verdict} |")
    lines += ["", f"A node may take up to {day['per_node']['tolerance_percent']}% more than its fair share by slots, "
              "as `relish bench` allows (#1362).", "",
              "| Hour | Host jobs a second | |", "|---|---|---|"]
    peak = max(day["hourly"], default=0) or 1
    for hour, rate in enumerate(day["hourly"], 1):
        lines.append(f"| Hour {hour} | {rate:,.1f} | {'█' * round(20 * rate / peak)} |")
    if day["first_hour"] is not None:
        lines += ["", f"Last hour against the first: {day['last_hour']:,.1f}/s against {day['first_hour']:,.1f}/s, "
                  f"{'ok' if day['slowdown_ok'] else 'FAIL'} (at most {day['slowdown_percent']}% slower; the first "
                  "hour includes the campaign's start and its host-job probes)."]
    return lines + [""]


def council_history_section(evidence, metadata):
    """The council history check (#1399): whether it failed the run, and the record's lines.

    `linearizability.py check` writes council-history-check.json once the
    soak is over. A run that started the history client (metadata
    `council_history`) owes that check: without it, or with a violation, the
    run fails. An inconclusive search is reported but fails nothing.
    """
    path = Path(evidence) / "council-history-check.json"
    if not path.exists():
        if metadata.get("council_history") is not True:
            return False, []
        return True, ["## Council history (linearizability)", "",
                      "- Not checked: the soak recorded a council history but never checked it", ""]
    result = json.loads(path.read_text())
    verdict = result.get("verdict")
    lines = ["## Council history (linearizability)", "",
             "Every test-lease grant, renew, read and release the history client made, checked against a "
             "model of the council's leases. A write with no answer may or may not have happened; the check "
             "allows both.", "",
             f"- Operations checked: {result.get('checked', 0)} of {result.get('operations', 0)}, "
             f"in {result.get('partitions', 0)} namespaces; {result.get('unanswered_writes', 0)} writes had no answer"]
    if verdict == "linearizable":
        lines.append("- Verdict: linearizable")
    elif verdict == "inconclusive":
        lines.append("- Verdict: inconclusive: the search ran out of budget in "
                     + ", ".join(result.get("inconclusive", [])))
    else:
        violation = result.get("violation", {})
        lines += [f"- Verdict: **not linearizable** in {violation.get('namespace', 'unknown')}; "
                  "the smallest failing slice of its history:", "",
                  "| Id | Client | Operation | Lease | Call | Return | Outcome | Expiry | Cleaning |",
                  "|---|---|---|---|---|---|---|---|---|"]
        for op in violation.get("slice", []):
            returned = "no answer" if op.get("return") is None else f"{op['return']:.3f}"
            lines.append(f"| {op.get('id')} | {op.get('client')} | {op.get('op')} | {op.get('lease') or ''} | "
                         f"{op.get('call', 0):.3f} | {returned} | {op.get('outcome')} | "
                         f"{op.get('expires_at_unix_ms') or ''} | {op.get('cleaning') if op.get('cleaning') is not None else ''} |")
    lines.append("")
    return verdict not in ("linearizable", "inconclusive"), lines


def render(evidence, record):
    evidence = Path(evidence)
    record = Path(record)
    if record.exists():
        raise SystemExit(f"{record} already exists")
    metadata = json.loads((evidence / "metadata.json").read_text()) if (evidence / "metadata.json").exists() else {}
    state = load_state(evidence)
    events = read_events(evidence)
    failures = failure_rows(evidence)
    started = metadata.get("started_at") or (events[0]["ts"] if events else int(time.time()))
    finished = metadata.get("finished_at") or (events[-1]["ts"] if events else started)
    paused = sum(event.get("duration", 0) for event in events if event.get("phase") == "pause")
    elapsed = max(0, finished - started - paused)

    gates = []
    identity_period = 1800
    for kind, period, label in (("workload-identity", identity_period, "workload identity rotations (soak-identity, per node)"),
                                ("node-leaf", (metadata.get("leaf_lifetime_secs") or 0) / 2, "node leaf renewals (per node)")):
        if not period:
            gates.append((label, "not measured", "n/a", "short leaf lifetimes unavailable on this build"))
            continue
        observed = state.get("renewals", {}).get(kind, {})
        need = expected_renewals(elapsed, period)
        worst = min((observed.get(node, 0) for node in state.get("nodes", [])), default=0)
        gates.append((label, str(worst), f"≥ {need}", "PASS" if worst >= need else "FAIL"))
    reloads = state.get("counters", {}).get("ingress-reload", 0)
    reload_period = metadata.get("ingress_rotation_secs") or 0
    if reload_period:
        need = max(0, int(elapsed // reload_period) - 1)
        gates.append(("operator ingress reloads (all nodes)", str(reloads), f"≥ {need}", "PASS" if reloads >= need else "FAIL"))
    job_report=read_json(evidence,"jobs/snapshot.json")
    fault_kinds=job_soak.FAULT_KINDS+(job_soak.NODE_FAULT_KINDS if metadata.get("node_faults") is True else ())
    job_missing=job_soak.coverage_failures(job_report,state.get("nodes",[]),fault_kinds) if metadata.get("job_soak") is True else []
    if metadata.get("job_soak") is True:
        if type(metadata.get("job_stop_at")) is not int:
            job_missing.append("the harness never asked the job campaign to drain")
        elif job_report and (type(job_report.get("stop_requested_at")) is not int or job_report["stop_requested_at"] < metadata["job_stop_at"]):
            job_missing.append("job campaign requested drain before the harness asked for it")
        for node in state.get("nodes",[]):
            observed=state.get("job_resources",{}).get(node,{})
            if observed.get("complete") is not True or observed.get("ts",0)<(job_report or {}).get("heartbeat",0) or observed.get("live_owners")!=0:
                job_missing.append(node+" lacks a complete empty owner inventory after campaign drain")
        gates.append(("mixed job campaign and positive drain",str(len(job_missing))+" missing/failing checks","0","FAIL" if job_missing else "PASS"))
    inventory = execution_inventory(evidence, metadata, events, job_report)
    missing = [item for item, need, done in inventory if done < need]
    # The final and day tiers gate on their inventory and leak-checker exposure.
    final_tier = metadata.get("tier") in ("final", "day")
    day = job_soak.report(job_report).get("day") if metadata.get("tier") == "day" and job_report else None
    exposure = exposure_shortfalls(state)
    # The latency SLOs (#1400): a breached gated one fails the tier.
    slo_data = soak_slo.load(evidence)
    slo_rows = soak_slo.rows(slo_data, job_soak.report(job_report) if job_report else None,
                             metadata.get("tier") or "custom", metadata.get("job_soak") is True)
    slo_failed = bool(soak_slo.failures(slo_rows))
    # The council history (#1399): a violation, or a check owed and missing, fails the run.
    history_failed, history_lines = council_history_section(evidence, metadata)
    gate_failed = (any(row[3] == "FAIL" for row in gates) or (final_tier and bool(missing or exposure))
                   or slo_failed or history_failed)
    open_failures = [row for row in failures if row.get("disposition", "open") == "open"]
    result = metadata.get("result") or ("FAIL" if open_failures or gate_failed else "PASS")
    if (open_failures or gate_failed) and result == "PASS":
        result = "FAIL"

    tier = metadata.get("tier") or "custom"
    lines = [f"# Sustained soak (V02): {result}", ""]
    schedule_text = "no fault" if metadata.get("schedule") == "none" else metadata.get("schedule", "full")
    lines += [f"{time.strftime('%-d %B %Y', time.gmtime(started))}. {tier} tier, {schedule_text} "
              f"schedule, {duration_text(elapsed)} of soak on a three-node quickstart cluster.", ""]
    # The acceptance line tells a failed check apart from a run that was clean but incomplete.
    clean_apart_from_inventory = "FAIL" if open_failures or metadata.get("result") == "FAIL" or slo_failed or history_failed or any(
        row[3] == "FAIL" for row in gates) else "PASS"
    lines += [acceptance_line(metadata, clean_apart_from_inventory, elapsed, missing if final_tier else (),
                              job_final_ran(metadata, job_report, missing), exposure if final_tier else (), day), ""]
    lines += ["## Candidate and host", "", "| | |", "|---|---|"]
    for key, label in (("base_url", "Staged base URL"), ("candidate_digest", "`candidate.json` SHA-256"),
                       ("versions", "Running versions"), ("soak_bun", "Soak build"),
                       ("release_version", "Walks roll back to"), ("host", "Host"),
                       ("lima", "Lima"), ("guest", "Guest"), ("home", "RELIABURGER_HOME"), ("evidence", "Evidence")):
        if metadata.get(key):
            lines.append(f"| {label} | {metadata[key]} |")
    lines.append("")
    lines += ["## Configuration deviations", ""]
    lines += [f"- {item}" for item in metadata.get("deviations", [])] or ["- none"]
    lines.append("")
    lines += ["## Timeline", "", f"- Started {time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime(started))} UTC, "
              f"finished {time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime(finished))} UTC"]
    if paused:
        lines.append(f"- Soak clock paused for {duration_text(paused)} (environment)")
    lines.append(f"- Cycles completed: {metadata.get('cycles', 0)}")
    choices_path = evidence / "choices.json"
    if choices_path.exists():
        choices = json.loads(choices_path.read_text())
        lines.append(f"- Fault and node choices: {choices['draws']}, from seed {choices['seed']} "
                     f"(`--seed {choices['seed']}` repeats them)")
    if metadata.get("teardown"):
        lines.append(f"- Teardown: {metadata['teardown']}")
    lines.append("")

    counts = {}
    for event in events:
        phase = event.get("phase", "")
        if phase.startswith(("fault:", "upgrade", "special:", "pulse", "tls:", "storage:")):
            entry = counts.setdefault(phase, {"runs": 0, "settled": 0, "failed": 0, "skipped": 0, "seconds": []})
            verdict = event.get("verdict", "")
            # A fault logs its injection, then its settle outcome (with settle_seconds).
            if event.get("settle_seconds") is None:
                entry["runs"] += 1
            else:
                entry["seconds"].append(event["settle_seconds"])
            key = {"settled": "settled", "ok": "settled", "fail": "failed", "skipped": "skipped"}.get(verdict)
            if key:
                entry[key] += 1
    lines += ["## Faults and slots", "", "| Class | Runs | Settled/ok | Failed | Skipped | Settle (median / max) |", "|---|---|---|---|---|---|"]
    for phase in sorted(counts):
        entry = counts[phase]
        seconds = sorted(entry["seconds"])
        timing = f"{seconds[len(seconds) // 2]} s / {seconds[-1]} s" if seconds else "n/a"
        lines.append(f"| {phase} | {entry['runs']} | {entry['settled']} | {entry['failed']} | {entry['skipped']} | {timing} |")
    lines.append("")
    lines += [f"## Execution inventory ({'gated' if final_tier else 'recorded, not gated'})", "",
              "What the run completed, from its events and kept bytes, against what its schedule owes. "
              "A fault counts once it was injected and the cluster settled after it.", "",
              "| Item | Required | Completed | Verdict |", "|---|---|---|---|"]
    lines += [f"| {item} | {need} | {done} | {'ok' if done >= need else 'MISSING'} |" for item, need, done in inventory]
    lines.append("")
    effects = [event for event in events if event.get("phase", "").startswith("effect:")]
    if effects:
        lines += ["## Observed fault effects", "", "What each injected fault was seen to do, read back from the node.", "",
                  "| Fault | Target | Observed |", "|---|---|---|"]
        lines += [f"| {event['phase']} | {event.get('target', '')} | "
                  f"{str(event.get('detail', '')).replace('|', '/').replace(chr(10), ' ')} |" for event in effects]
        lines.append("")
    lines += ["## Renewals and rotations", "", "| Measure | Observed | Required | Verdict |", "|---|---|---|---|"]
    lines += [f"| {label} | {observed} | {need} | {verdict} |" for label, observed, need, verdict in gates]
    lines.append("")
    lines += soak_slo.table_lines(slo_rows, slo_data)
    progress = state.get("progress", {})
    if metadata.get("job_soak") is True:
        lines += ["## Jobs", "", "| Runtime | Accepted successes | Retries | Last accepted progress |", "|---|---|---|---|"]
        for mode in job_soak.MODES:
            sample=(job_report or {}).get("progress",{}).get(mode,{})
            lines.append(f"| {mode} | {sample.get('succeeded',0)} | {sample.get('retried',0)} | {sample.get('latest_success','missing')} |")
        lines += ["", "The bounded campaign shares the app soak's fault schedule. These counts are reliability evidence, not a saturation rate or a 100-million-per-day qualification.", ""]
        lines += ["- "+problem for problem in job_missing]
        lines += ["- Independent verifier execution attempts: "+str((job_report or {}).get("totals",{}).get("audit_attempts",0)),
                  "- Explicit test-operator acknowledgements for known replay-safe fenced fixtures: "+str((job_report or {}).get("totals",{}).get("operator_replay_decisions",0)), ""]
        for node, sample in state.get("job_resources",{}).items():
            lines.append(f"- {node}: latest complete job storage (KiB) {sample.get('storage_kb')}; free {sample.get('available_kb')} KiB; live/peak owners {sample.get('live_owners')}/{sample.get('peak_owners')}; peak storage {sample.get('peak_storage_kb')}")
        lines += ["", "Complete disk inventories are recorded. Global retention bounds and the 12-hour job qualification remain #668; this gate cannot certify an unimplemented retention policy.", ""]
        qualification = job_qualification(job_report, state, metadata, elapsed)
        lines += job_qualification_lines(qualification)
        if day:
            lines += day_lines(day)
        evidence_json = record.with_name(record.stem + ".jobs.json")
        if evidence_json.exists():
            raise SystemExit(f"{evidence_json} already exists")
        record.parent.mkdir(parents=True, exist_ok=True)
        evidence_json.write_text(json.dumps(qualification, indent=1, sort_keys=True) + "\n")
    lines += ["## Data", "",
              f"- Volume writer: highest ACK {state.get('writer-ack', 'none')} in the log view; "
              "the writer file checks (writer-lost, writer-regression) decide data loss, "
              "and `*-log-order` failures are about the order the log view returned lines in",
              f"- Redis counter: highest INCR {state.get('redis-counter', 'none')} in the log view"]
    for node, count in sorted(state.get("export_counts", {}).items()):
        lines.append(f"- Export {node}: {count['source']} source files, {count['destination']} at the destination")
    lines.append(f"- Registry images pushed and re-verified: {len(state.get('registry_pushed', []))}")
    over_budget = state.get("turns_over_budget", {})
    checked = sorted(state.get("turns", {}))
    if checked:
        lines.append("- Agent-loop turns over the 1 s budget: " + ", ".join(
            f"{node} {over_budget.get(node, 0)}" for node in checked))
    else:
        lines.append("- Agent-loop turns over the 1 s budget: not measured")
    if state.get("policy_checks"):
        lines.append(f"- Soak policies checked in force: {state['policy_checks']} times, "
                     f"policy generation {state.get('policy_generation', 'unknown')} at the last check")
    else:
        lines.append("- Soak policies checked in force: not measured")
    clock_steps = state.get("clock_steps", {})
    lines.append("- Guest clock steps by Lima's guest agent: " + (", ".join(
        f"{node} {count}" for node, count in sorted(clock_steps.items())) or "none"))
    lines.append(f"- Checks evaluated: {state.get('checks', 0)} ({', '.join(f'{kind} {n}' for kind, n in sorted(state.get('check_counts', {}).items()))})")
    if progress:
        lines.append(f"- Last observed progress: {', '.join(f'{key} {value}' for key, value in sorted(progress.items()))}")
    lines.append("")
    lines += history_lines
    lines += exposure_lines(state, final_tier)
    lines += ["## Resource trends", "", "| Node | Measure | First | Last | Max |", "|---|---|---|---|---|"]
    for node, trend in sorted(state.get("trends", {}).items()):
        for measure, entry in sorted(trend.items()):
            lines.append(f"| {node} | {measure} | {entry['first']} | {entry.get('last', entry['first'])} | {entry['max']} |")
    lines.append("")
    charts_directory = record.parent / f"{record.stem}-charts"
    lines += soak_charts.section(soak_charts.write_charts(evidence, charts_directory), charts_directory.name)
    lines += ["## Failures", ""]
    if failures:
        lines += ["| # | First seen | Check | Symptom | Class | Cause | Action |", "|---|---|---|---|---|---|---|"]
        for row in failures:
            seen = time.strftime("%H:%M:%S", time.gmtime(row.get("ts", started)))
            symptom = str(row.get("detail", "")).replace("|", "\\|").replace("\n", " ")[:300]
            lines.append(f"| {row['number']} | {seen} | {row.get('check', '')} | {symptom} | {row.get('class', 'unclassified')} | "
                         f"{row.get('cause', 'open')} | {row.get('action', 'open')} |")
    else:
        lines.append("None.")
    lines.append("")
    if metadata.get("notes"):
        lines += ["## Notes", ""] + [f"- {note}" for note in metadata["notes"]] + [""]
    lines.append(f"Evidence: `{evidence}` (events.jsonl, snapshots/, failures/<n>/).")
    record.parent.mkdir(parents=True, exist_ok=True)
    record.write_text("\n".join(lines) + "\n")
    return 0 if result == "PASS" else 1


def job_qualification(job_report, state, metadata, elapsed):
    """The job qualification report written beside the record (D18-C)."""
    report = job_soak.report(job_report)
    accepted = sum((report.get("accepted_successes") or {}).values())
    report["m8"] = dict(report.get("m8") or {}, **job_host_metrics(state, accepted))
    report["gate"] = dict(
        decision="D18-C",
        nodes=len(state.get("nodes", [])),
        soak_seconds=elapsed,
        tier=metadata.get("tier"),
        schedule=metadata.get("schedule"),
        node_faults=metadata.get("node_faults") is True,
        fault_kinds=[] if metadata.get("tier") == "day" else
        list(job_soak.FAULT_KINDS + (job_soak.NODE_FAULT_KINDS if metadata.get("node_faults") is True else ())))
    return report


def job_qualification_lines(report):
    """The record's table of the job gate and the recorded M8 measurements."""
    m8 = report.get("m8", {})
    def show(value, unit=""):
        if value is None:
            return "not measured"
        return (f"{value:,.2f}" if isinstance(value, float) else f"{value:,}") + unit
    wall = m8.get("million_task_array_wall_seconds") or {}
    rate = report.get("accepted_successes_per_second") or 0
    target = report.get("target_rate")
    rss = m8.get("bun_rss_worst")
    rss_observed = (f"{rss['node']}: {show(rss['steady_rss_mib'], ' MiB')} steady, {show(rss['peak_rss_mib'], ' MiB')} peak"
                    if rss else "not measured")
    rss_target = (f"≤ {rss['target_mib']} MiB" if rss else f"≤ {BUN_RSS_TARGET_MIB} MiB") + \
        f" (≤ {API_BUN_RSS_TARGET_MIB} MiB on node 1, which serves the API)"
    lines = ["## Job qualification (D18-C)", "",
             f"Load: {report.get('load')}; {report.get('gate', {}).get('nodes', 0)} nodes; "
             f"accepted {rate:,.1f}/s" + (f" against a {target}/s target: {'PASS' if report.get('target_met') else 'FAIL'}" if target else " (calibration load: not gated)") + ".", "",
             "The September plan set its targets for one 1M-task batch on a quiet cluster. A soak runs for hours with "
             "restarts, upgrades and apps, so its storage, Bun memory and Raft targets are restated (maintainer decision, "
             "15 October 2026, [#668](https://github.com/reliaburger/reliaburger/issues/668)). Bun's steady state is the "
             f"median RSS of each process once it has been seen for {BUN_STEADY_AFTER // 60} minutes; "
             "the gated memory checks are unchanged (`leak-rss`, and Mayo's 70% `memory_high` alert through `wtf`).", "",
             "| Measure (recorded, not gated) | Observed | Target |", "|---|---|---|",
             f"| Wall time of a 1,048,576-task array (median / max of {wall.get('runs', 0)}) | {show(wall.get('median'), ' s')} / {show(wall.get('max'), ' s')} | ≤ 600 s |",
             f"| p99 start lag, pooled executors | {show(m8.get('p99_start_lag_ms'), ' ms')} (bucket upper bound) | < 1,000 ms |",
             f"| Raft entries per second, all cluster writes | {show(m8.get('raft_entries_per_second'))} | recorded only; the batch target counts only task-array writes |",
             f"| Raft state of one run, largest seen | {show(m8.get('raft_state_bytes_max'), ' B')} | ≤ 256 KiB per 1M tasks |",
             f"| Bun steady-state RSS per process, worst node against its target | {rss_observed} | {rss_target} |",
             f"| Bun CPU, busiest node | {show(m8.get('leader_cpu_cores'), ' cores')} | ≤ 1 core (leader batch control) |",
             f"| Executor helper CPU, busiest node | {show(m8.get('executor_overhead_cores_per_node'), ' cores')} | ≤ 0.5 core |",
             f"| Node task-array storage per 1M accepted tasks (peaks summed) | {show(m8.get('node_ledger_mib_per_million_tasks'), ' MiB')} | ≤ {TASK_ARRAY_STORAGE_TARGET_MIB} MiB, deleted after retirement |",
             f"| Retained log volume, all nodes | {show(m8.get('retained_log_mb'), ' MB')} | ≤ 50 MB |", ""]
    for node, row in (m8.get("per_node") or {}).items():
        if not row.get("bun_processes"):
            continue
        role = "serves the API" if node == m8.get("api_node") else "node"
        lines.append(f"- Bun RSS on {node} ({role}, target ≤ {row['bun_rss_target_mib']} MiB): " + "; ".join(
            (f"pid {process['pid']}: {show(process['steady_rss_mib'], ' MiB')} steady, {show(process['peak_rss_mib'], ' MiB')} peak"
             if process["steady_rss_mib"] is not None else
             f"pid {process['pid']}: {show(process['peak_rss_mib'], ' MiB')} peak (seen for under {BUN_STEADY_AFTER // 60} minutes)")
            for process in row["bun_processes"]))
    if any((row.get("bun_processes") for row in (m8.get("per_node") or {}).values())):
        lines.append("")
    causes = report.get("retry_causes") or {}
    for mode in job_soak.MODES:
        if causes.get(mode):
            lines.append(f"- {mode} retry causes: " + ", ".join(f"{key} {value}" for key, value in causes[mode].items()))
    fallbacks = report.get("api_fallbacks") or {}
    missed = ", ".join(f"{node} missed {count}" for node, count in sorted((fallbacks.get("missed") or {}).items()))
    answered = ", ".join(f"{node} answered {count}" for node, count in sorted((fallbacks.get("answered") or {}).items()))
    lines += [f"- Audited executions: {report.get('audited_attempts', 0)}; operator replay decisions: {report.get('operator_replay_decisions', 0)}",
              "- Job controller API fallbacks: " + (f"{missed}; {answered}" if missed else "none") +
              " (requests a node didn't answer, and the node that took them instead)", ""]
    return lines


# --- command line ------------------------------------------------------------

def nodes_query(path, what):
    nodes = json.loads(Path(path).read_text())
    if what == "leader":
        names = [node["node_id"] for node in nodes if node.get("is_leader")]
    elif what == "followers":
        names = [node["node_id"] for node in nodes if not node.get("is_leader") and node.get("state") == "alive"]
    elif what == "alive":
        names = [node["node_id"] for node in nodes if node.get("state") == "alive"]
    else:
        names = [node["node_id"] for node in nodes if node.get("is_council")]
    print("\n".join(names))
    return 0 if names else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    event = commands.add_parser("event")
    event.add_argument("evidence")
    event.add_argument("--phase", required=True)
    for name in ("target", "verdict", "detail"):
        event.add_argument("--" + name)
    event.add_argument("--command", dest="command_text")
    for name in ("exit", "duration", "settle-seconds"):
        event.add_argument("--" + name, type=int)
    expect = commands.add_parser("expect")
    expect.add_argument("evidence")
    expect.add_argument("what", choices=["restart"])
    expect.add_argument("node")
    expect.add_argument("--at", type=int, help="when the kill happens (default: now)")
    power_cut = commands.add_parser("power-cut", help="a node's power was cut: the log view may lose unsynced lines")
    power_cut.add_argument("evidence")
    window = commands.add_parser("window")
    window.add_argument("evidence")
    window.add_argument("action", choices=["open", "close", "settled"])
    window.add_argument("label", nargs="?")
    window.add_argument("--down", type=int, default=0)
    window.add_argument("--node", help="the node a node fault targets")
    for name in ("baseline", "evaluate"):
        sub = commands.add_parser(name)
        sub.add_argument("evidence")
        sub.add_argument("snapshot")
    policies = commands.add_parser("policies", help="exit 1 unless the soak workload's policies are all in force (#1311)")
    policies.add_argument("evidence")
    policies.add_argument("listing")
    seen = commands.add_parser("seen")
    seen.add_argument("evidence")
    seen.add_argument("key")
    ingress = commands.add_parser("ingress-expect")
    ingress.add_argument("evidence")
    ingress.add_argument("node")
    ingress.add_argument("serial")
    count = commands.add_parser("count")
    count.add_argument("evidence")
    count.add_argument("name")
    count.add_argument("n", nargs="?", type=int, default=1)
    toml = commands.add_parser("toml-set")
    toml.add_argument("file")
    toml.add_argument("assignments", nargs="+")
    nodes = commands.add_parser("nodes-json")
    nodes.add_argument("file")
    nodes.add_argument("what", choices=["leader", "followers", "alive", "council"])
    release = commands.add_parser("release-version", help="the version every node reports, which upgrade walks roll back to")
    release.add_argument("--soak-bun", required=True, help="the soak build's file name (bun-vVERSION)")
    release.add_argument("versions", nargs="*")
    stamp = commands.add_parser("utc-stamp")
    stamp.add_argument("epoch", type=int)
    registry = commands.add_parser("registry")
    registry.add_argument("evidence")
    registry.add_argument("action", choices=["push", "verify"])
    for name in ("port", "server-name", "ca", "token-file"):
        registry.add_argument("--" + name, required=True, type=int if name == "port" else str)
    registry.add_argument("--tag", default="pulse")
    forwards = commands.add_parser("forwards", help="probe the forwarded ports from the host and the guest (#987)")
    forwards.add_argument("snapshot")
    forwards.add_argument("--limactl", required=True)
    for name in ("api-port", "ingress-port", "registry-port"):
        forwards.add_argument("--" + name, required=True, type=int)
    forwards.add_argument("--node", action="append", required=True, help="the VMs, node 1 first")
    forwards.add_argument("--down", action="append", type=int, default=[], help="a node (1-3) that is powered off")
    keep = commands.add_parser("keep-host-logs", help="keep Lima's host-agent logs before a stop truncates them")
    keep.add_argument("evidence")
    keep.add_argument("--lima-home", required=True)
    keep.add_argument("--reason", required=True)
    keep.add_argument("vms", nargs="+")
    reason = commands.add_parser("curl-reason")
    reason.add_argument("code", type=int)
    failures = commands.add_parser("test-failures", help="why a relish test JSON run failed (#1068)")
    failures.add_argument("report")
    failures.add_argument("stderr", nargs="?")
    seed = commands.add_parser("seed", help="start or resume the run's seeded choices (#1062)")
    seed.add_argument("evidence")
    seed.add_argument("--seed", type=int)
    choice = commands.add_parser("choose", help="the run's next seeded choice, logged")
    choice.add_argument("evidence")
    choice.add_argument("what")
    choice.add_argument("low", type=int)
    choice.add_argument("high", type=int)
    record = commands.add_parser("render")
    record.add_argument("evidence")
    record.add_argument("record")
    charts = commands.add_parser("render-charts", help="rebuild a record's charts from an existing evidence directory")
    charts.add_argument("evidence")
    charts.add_argument("directory")
    args = parser.parse_args(argv)

    if args.command == "event":
        append_event(args.evidence, phase=args.phase, target=args.target, command=args.command_text,
                     exit=args.exit, duration=args.duration, verdict=args.verdict, detail=args.detail,
                     settle_seconds=args.settle_seconds)
        return 0
    if args.command == "expect":
        with open(Path(args.evidence) / "restart-expectations.jsonl", "a") as out:
            out.write(json.dumps({"node": args.node, "ts": args.at if args.at is not None else int(time.time())}) + "\n")
        return 0
    if args.command == "power-cut":
        state = load_state(args.evidence)
        state["power_cut"] = True
        save_state(args.evidence, state)
        return 0
    if args.command == "window":
        state = load_state(args.evidence)
        if args.action == "open":
            state["window"] = args.label or "fault"
            state["nodes_down"] = args.down
            if args.node:
                state["window_node"] = args.node
            else:
                state.pop("window_node", None)
        else:
            state.pop("window", None)
            state.pop("window_node", None)
            state["nodes_down"] = 0
            if args.action == "settled":
                state["last_settled"] = int(time.time())
        save_state(args.evidence, state)
        return 0
    if args.command == "baseline":
        record_baseline(args.evidence, args.snapshot)
        return 0
    if args.command == "evaluate":
        return evaluate(args.evidence, args.snapshot)
    if args.command == "policies":
        return check_policies(args.evidence, args.listing)
    if args.command == "seen":
        # A harness-detected failure already bundled in the last half hour is a repeat.
        state = load_state(args.evidence)
        reported = state.setdefault("harness_reported", {})
        now = int(time.time())
        if now - reported.get(args.key, -REPEAT_WINDOW) < REPEAT_WINDOW:
            return 0
        reported[args.key] = now
        save_state(args.evidence, state)
        return 1
    if args.command == "ingress-expect":
        state = load_state(args.evidence)
        state.setdefault("ingress_expected", {})[args.node] = {"serial": normalise_serial(args.serial), "since": int(time.time())}
        save_state(args.evidence, state)
        return 0
    if args.command == "count":
        state = load_state(args.evidence)
        counters = state.setdefault("counters", {})
        counters[args.name] = counters.get(args.name, 0) + args.n
        save_state(args.evidence, state)
        return 0
    if args.command == "toml-set":
        path = Path(args.file)
        pairs = [assignment.split("=", 1) for assignment in args.assignments]
        if any(len(pair) != 2 for pair in pairs):
            raise SystemExit("expected SECTION.KEY=VALUE")
        path.write_text(toml_set(path.read_text(), [(key.strip(), value.strip()) for key, value in pairs]))
        return 0
    if args.command == "nodes-json":
        return nodes_query(args.file, args.what)
    if args.command == "release-version":
        try:
            print(release_version(args.versions, args.soak_bun))
        except ValueError as e:
            print(f"sustained_check: {e}", file=sys.stderr)
            return 1
        return 0
    if args.command == "utc-stamp":
        print(time.strftime("%y%m%d%H%M%SZ", time.gmtime(args.epoch)))
        return 0
    if args.command == "registry":
        return registry_command(args)
    if args.command == "forwards":
        rows = probe_forwards(args.node, {"api": args.api_port, "ingress": args.ingress_port, "registry": args.registry_port},
                              set(args.down), args.limactl)
        (Path(args.snapshot) / "forwards.json").write_text(json.dumps(rows, indent=1))
        return 0
    if args.command == "keep-host-logs":
        keep_host_logs(args.evidence, args.lima_home, args.reason, args.vms)
        return 0
    if args.command == "curl-reason":
        print(curl_reason(args.code))
        return 0
    if args.command == "test-failures":
        stderr_text = Path(args.stderr).read_text(errors="replace") if args.stderr and Path(args.stderr).exists() else ""
        print(test_failure_reasons(Path(args.report).read_text(errors="replace"), stderr_text))
        return 0
    if args.command == "seed":
        try:
            print(start_choices(args.evidence, args.seed))
        except ValueError as error:
            print(f"sustained_check: {error}", file=sys.stderr)
            return 1
        return 0
    if args.command == "choose":
        print(choose(args.evidence, args.what, args.low, args.high))
        return 0
    if args.command == "render-charts":
        directory = Path(args.directory)
        written = soak_charts.write_charts(args.evidence, directory)
        print("\n".join(soak_charts.section(written, directory.name)))
        return 0
    return render(args.evidence, args.record)


if __name__ == "__main__":
    sys.exit(main())
