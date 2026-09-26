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
  seen EVIDENCE KEY                     exit 0 if a harness failure KEY was bundled in the last half hour
  ingress-expect EVIDENCE NODE SERIAL   the serial a node must serve after a successful reload
  count EVIDENCE NAME [N]               add to a named counter in the record
  toml-set FILE SECTION.KEY=VALUE...    set keys in a TOML file in place (VALUE is a TOML literal)
  nodes-json FILE leader|followers|alive|council
  utc-stamp EPOCH                       YYMMDDHHMMSSZ for `openssl ca -startdate/-enddate`
  registry EVIDENCE push|verify --port P --server-name N --ca CA --token-file F [--tag T]
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
import re
import socket
import ssl
import sys
import time

# Thresholds from section 4 of the plan (seconds unless stated).
NODE_CERT_MARGIN = 300
WORKLOAD_CERT_MARGIN = 600
INGRESS_CERT_MARGIN = 300
RETRYING_LIMIT = 600
WTF_CRITICAL_LIMIT = 300
WTF_WARNING_LIMIT = 600
NO_LEADER_LIMIT = 30
RSS_WARMUP = 3600
RSS_GROWTH = 1.25
FD_WINDOW = 6 * 3600
REPEAT_WINDOW = 1800
LEAK_KINDS = ("runc", "netns", "lease", "veth", "cgroup", "bpf", "listen")
# Pickle resolves every tag through the council's committed catalogue and
# answers 503 while there is no leader. A few tries ride out an election;
# a quorum loss outlasts them and is judged against the fault window.
REGISTRY_UNAVAILABLE_TRIES = 3
REGISTRY_UNAVAILABLE_PAUSE = 5


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

def parse_sequence(text, prefix):
    """Numbers from lines like `ACK 12`, in log order."""
    pattern = re.compile(r"\b" + re.escape(prefix) + r" (\d+)\b")
    return [int(match.group(1)) for match in pattern.finditer(text)]


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

def sequence_findings(check, values, highest, source, target=None):
    """Log-view ordering: the writer's ACKs and redis INCRs, as `relish logs`
    returns them, must keep rising. Still failures, but about the order the
    log view shows, not what was stored; `source` says what decides that."""
    findings = []
    for before, after in zip(values, values[1:]):
        if after <= before:
            findings.append(finding(check, "fail", f"log view went backwards from {before} to {after} ({source})", target))
    if values and highest is not None and values[-1] < highest:
        findings.append(finding(check, "fail", f"log view ends at {values[-1]}, below the {highest} an earlier check saw ({source})", target))
    return findings


def writer_file_findings(text, highest_ack):
    """The writer's file must hold 1..N with N at least the highest ACK seen."""
    last = None
    findings = []
    for line in text.splitlines():
        if line.startswith("LAST "):
            last = int(line.split()[1])
        elif line.startswith("BAD "):
            findings.append(finding("writer-gap", "fail", "sequence file line " + line[4:]))
    if last is None:
        return findings + [finding("writer-file", "warn", "no LAST line from the writer check")]
    if highest_ack is not None and last < highest_ack:
        findings.append(finding("writer-regression", "fail", f"file ends at {last} but ACK {highest_ack} was logged"))
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


def leak_findings(baseline, current, baseline_instances, current_instances, node):
    """Resources beyond the baseline plus the node's current instances."""
    findings = []
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


def fd_findings(samples, now):
    """Fail when every hourly minimum over the last six hours is higher than the one before."""
    window = [(ts, fd) for ts, fd in samples if ts >= now - FD_WINDOW]
    if not window or window[0][0] > now - FD_WINDOW + 600:
        return []
    minima = {}
    for ts, fd in window:
        bucket = min(5, int((ts - (now - FD_WINDOW)) // 3600))
        minima[bucket] = min(minima.get(bucket, fd), fd)
    if len(minima) < 6:
        return []
    values = [minima[bucket] for bucket in range(6)]
    if all(after > before for before, after in zip(values, values[1:])):
        return [finding("leak-fd", "fail", "bun file descriptors grew for six hours: " + " ".join(map(str, values)))]
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


def restart_findings(state, node, inventory):
    """systemd restarts of bun that no harness kill explains."""
    boot = (inventory.get("boot") or [None])[0]
    count = inventory_number(inventory, "nrestarts")
    if count is None:
        return []
    restarts = state.setdefault("restarts", {}).setdefault(node, {"boot": boot, "n": count, "expected": 0})
    findings = []
    if restarts["boot"] == boot:
        unexplained = count - restarts["n"] - restarts["expected"]
        if unexplained > 0:
            findings.append(finding("bun-restart", "fail", f"{unexplained} systemd restart(s) of bun without a harness kill", node))
    restarts.update(boot=boot, n=count, expected=0)
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
            current.add(key)
            first = max(seen.setdefault(key, now), settled)
            if fault_window or now - first <= limit or key in reported:
                continue
            reported.append(key)
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


def evaluate(evidence, snapshot):
    state = load_state(evidence)
    meta = read_json(snapshot, "meta.json") or {}
    now = meta.get("ts", int(time.time()))
    fault_window = bool(state.get("window"))
    expected_nodes = state.get("nodes", [])
    findings = []

    http = read(snapshot, "http.txt")
    http_ok = None
    if http is not None:
        http_ok = http.split()[:1] == ["200"]
        if not http_ok:
            findings.append(finding("ingress-http", "info" if fault_window else "fail",
                                    "podinfo answered " + (http.strip() or "nothing")))

    # The writer file (writer-gap, writer-regression) is the data-loss check;
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
        values = parse_sequence(text, prefix)
        baseline = baselines.get(check, state.get(check))
        order = sequence_findings(order_check, values, baseline, source)
        if state.get("power_cut") and values and baseline is not None and values[-1] < baseline:
            # A powered-off node loses the stdout it hadn't synced yet, as any
            # log does; the tail may end below what an earlier check saw. Only
            # the "ends below" finding is excused, and the baseline restarts
            # from here; lines going backwards within one tail still fail.
            order = [dict(item, severity="info", detail=item["detail"] + "; after a power cut, lines not yet synced are lost from the log view")
                     if "below the" in item["detail"] else item for item in order]
            baselines[check] = values[-1]
            power_cut_excused = True
        elif values:
            advanced = advanced or baseline is None or values[-1] > baseline
            baselines[check] = max(values[-1], baseline or 0)
        findings += order
        if values:
            if values[-1] == state.get(check) and not fault_window:
                findings.append(finding(check, "warn", f"not advancing at {values[-1]}"))
            state[check] = max(values[-1], state.get(check, 0))
            state.setdefault("progress", {})[check] = values[-1]
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
            findings += restart_findings(state, node, inventory)
            findings += resource_trend_findings(state, node, inventory, now)
            if by_node is not None and node in baseline:
                leaks += leak_findings(baseline[node]["inventory"], inventory,
                                       baseline[node]["instances"], by_node.get(node, []), node)
        text = read(snapshot, f"export-{node}.txt")
        if text is not None:
            findings += export_findings(state, node, text)
            findings += snapshot_archive_findings(state, node, text)
    # Leaks only count once the cluster has settled; before that they are progress.
    findings += [dict(leak, severity="info") if fault_window else leak for leak in leaks]

    registry = read_json(snapshot, "registry.json")
    if registry is not None:
        for problem in registry.get("problems", []):
            findings.append(finding("registry", "fail", problem))
        # Refusing reads without a leader is Pickle failing closed, not data
        # loss. Outside a fault window nothing excuses it.
        for problem in registry.get("unavailable", []):
            findings.append(finding("registry", "info" if fault_window else "fail", problem))

    clean = None
    if meta.get("kind") in ("settle", "heavy"):
        # Without status the leak check couldn't run, so the node isn't known to be clean.
        clean = settle_clean(report, nodes, node_problems, leaks, http_ok) and by_node is not None
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
        key = f"{item['check']}|{item['target'] or ''}"
        if item["severity"] == "fail":
            if now - reported.get(key, -REPEAT_WINDOW) < REPEAT_WINDOW:
                item = dict(item, severity="repeat")
            else:
                reported[key] = now
        result.append(item)
    return result


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


def resource_trend_findings(state, node, inventory, now):
    node_state = state.setdefault("resources", {}).setdefault(node, {})
    pid = inventory_number(inventory, "bun_pid")
    fd = inventory_number(inventory, "bun_fd")
    rss = inventory_number(inventory, "bun_rss_kb")
    if pid != node_state.get("pid"):
        node_state.update(pid=pid, fd_samples=[], started=now)
        node_state.pop("rss_warm_kb", None)
    if fd is not None:
        node_state["fd_samples"] = [sample for sample in node_state["fd_samples"] if sample[0] >= now - FD_WINDOW] + [[now, fd]]
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
    findings = [dict(item, target=node) for item in fd_findings(node_state["fd_samples"], now)]
    findings += [dict(item, target=node) for item in rss_findings(node_state, rss, now, node_state["started"])]
    return findings


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


def acceptance_line(metadata, result, elapsed):
    """What the run says about the V02 gate (plan D3): only a clean final tier passes it."""
    tier = metadata.get("tier") or "custom"
    clean = result == "PASS"
    if tier == "fast":
        if clean:
            return ("**Fast tier: clean.** The iteration loop only, not acceptance: the V02 gate needs a clean "
                    "final-tier run (8 h, full schedule) on the final candidate.")
        return "**Fast tier: not clean.** Fix what failed and run the fast tier again before a final-tier run."
    if tier != "final":
        return "**No tier (a custom run).** Neither tier's evidence, so it says nothing about the V02 gate."
    if not clean:
        return ("**Final tier: not clean. The V02 gate does not pass.** A product fix means a fresh fast-tier run, "
                "then a fresh final-tier run.")
    shortfalls = []
    if metadata.get("schedule", "full") != "full":
        shortfalls.append(f"it ran the {metadata['schedule']} schedule")
    if min(elapsed, metadata.get("duration_target") or 0) < FINAL_TIER_SECONDS:
        shortfalls.append(f"it soaked for {duration_text(elapsed)}")
    digest = metadata.get("candidate_digest") or "`not checked`"
    if "not checked" in digest:
        shortfalls.append("the candidate digest was not checked (--qualified-digest)")
    if shortfalls:
        return (f"**Final tier: clean, but not acceptance:** {'; '.join(shortfalls)}. The V02 gate needs 8 h on "
                "the full schedule against a pinned candidate.")
    return (f"**Final tier: clean. The V02 soak gate passes** for the candidate whose `candidate.json` has "
            f"SHA-256 {digest}, if it is the final candidate.")


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
    gate_failed = any(row[3] == "FAIL" for row in gates)
    open_failures = [row for row in failures if row.get("disposition", "open") == "open"]
    result = metadata.get("result") or ("FAIL" if open_failures or gate_failed else "PASS")
    if (open_failures or gate_failed) and result == "PASS":
        result = "FAIL"

    tier = metadata.get("tier") or "custom"
    lines = [f"# Sustained soak (V02): {result}", ""]
    lines += [f"{time.strftime('%-d %B %Y', time.gmtime(started))}. {tier} tier, {metadata.get('schedule', 'full')} "
              f"schedule, {duration_text(elapsed)} of soak on a three-node quickstart cluster.", ""]
    lines += [acceptance_line(metadata, result, elapsed), ""]
    lines += ["## Candidate and host", "", "| | |", "|---|---|"]
    for key, label in (("base_url", "Staged base URL"), ("candidate_digest", "`candidate.json` SHA-256"),
                       ("versions", "Running versions"), ("soak_bun", "Soak build"), ("host", "Host"),
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
    lines += ["## Renewals and rotations", "", "| Measure | Observed | Required | Verdict |", "|---|---|---|---|"]
    lines += [f"| {label} | {observed} | {need} | {verdict} |" for label, observed, need, verdict in gates]
    lines.append("")
    progress = state.get("progress", {})
    lines += ["## Data", "",
              f"- Volume writer: highest ACK {state.get('writer-ack', 'none')} in the log view; "
              "the writer file checks (writer-gap, writer-regression) decide data loss, "
              "and `*-log-order` failures are about the order the log view returned lines in",
              f"- Redis counter: highest INCR {state.get('redis-counter', 'none')} in the log view"]
    for node, count in sorted(state.get("export_counts", {}).items()):
        lines.append(f"- Export {node}: {count['source']} source files, {count['destination']} at the destination")
    lines.append(f"- Registry images pushed and re-verified: {len(state.get('registry_pushed', []))}")
    lines.append(f"- Checks evaluated: {state.get('checks', 0)} ({', '.join(f'{kind} {n}' for kind, n in sorted(state.get('check_counts', {}).items()))})")
    if progress:
        lines.append(f"- Last observed progress: {', '.join(f'{key} {value}' for key, value in sorted(progress.items()))}")
    lines.append("")
    lines += ["## Resource trends", "", "| Node | Measure | First | Last | Max |", "|---|---|---|---|---|"]
    for node, trend in sorted(state.get("trends", {}).items()):
        for measure, entry in sorted(trend.items()):
            lines.append(f"| {node} | {measure} | {entry['first']} | {entry.get('last', entry['first'])} | {entry['max']} |")
    lines.append("")
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
    power_cut = commands.add_parser("power-cut", help="a node's power was cut: the log view may lose unsynced lines")
    power_cut.add_argument("evidence")
    window = commands.add_parser("window")
    window.add_argument("evidence")
    window.add_argument("action", choices=["open", "close", "settled"])
    window.add_argument("label", nargs="?")
    window.add_argument("--down", type=int, default=0)
    for name in ("baseline", "evaluate"):
        sub = commands.add_parser(name)
        sub.add_argument("evidence")
        sub.add_argument("snapshot")
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
    stamp = commands.add_parser("utc-stamp")
    stamp.add_argument("epoch", type=int)
    registry = commands.add_parser("registry")
    registry.add_argument("evidence")
    registry.add_argument("action", choices=["push", "verify"])
    for name in ("port", "server-name", "ca", "token-file"):
        registry.add_argument("--" + name, required=True, type=int if name == "port" else str)
    registry.add_argument("--tag", default="pulse")
    record = commands.add_parser("render")
    record.add_argument("evidence")
    record.add_argument("record")
    args = parser.parse_args(argv)

    if args.command == "event":
        append_event(args.evidence, phase=args.phase, target=args.target, command=args.command_text,
                     exit=args.exit, duration=args.duration, verdict=args.verdict, detail=args.detail,
                     settle_seconds=args.settle_seconds)
        return 0
    if args.command == "expect":
        state = load_state(args.evidence)
        restarts = state.setdefault("restarts", {}).setdefault(args.node, {"boot": None, "n": 0, "expected": 0})
        restarts["expected"] += 1
        save_state(args.evidence, state)
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
        else:
            state.pop("window", None)
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
    if args.command == "utc-stamp":
        print(time.strftime("%y%m%d%H%M%SZ", time.gmtime(args.epoch)))
        return 0
    if args.command == "registry":
        return registry_command(args)
    return render(args.evidence, args.record)


if __name__ == "__main__":
    sys.exit(main())
