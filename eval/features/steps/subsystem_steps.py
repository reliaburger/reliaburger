"""Axis 1, built-in subsystems: each exercised through its documented interface, nothing added."""

from __future__ import annotations

import json
import re
import subprocess
import time

from behave import then, when

from harness import apps
from harness.core import ROOT, Blocked, wait_until
from harness.probes import http_request
from harness.steplib import node_ips

NATIVE = ROOT / "fixtures" / "native"
BUSYBOX = "public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028"


def _secrets(context) -> tuple[bool, str]:
    cl, ev = context.cluster, context.ev
    plain = apps.unique("rbeval-plain")
    pub = cl.relish.run("secret", "pubkey", check=True).stdout.strip().split()[-1]
    enc = cl.relish.run("secret", "encrypt", "--pubkey", pub, plain, check=True).stdout.strip()
    ev.measure("ciphertext_prefix", enc[:12])
    path = apps.render(NATIVE / "secret-probe.toml", ev.dir / "secret-probe.toml", CIPHERTEXT=enc)
    context.cleanups.append(lambda: apps.delete_app(cl, "secretprobe"))
    cl.relish.run("apply", str(path), check=True, timeout=180)
    apps.wait_running(cl, "secretprobe", 1, context.cfg.threshold("bootstrap.app_ready_s"))
    body = {}

    def seen():
        for ip in node_ips(context):
            r = http_request(ip, 80, "/env", "secret.rbeval.test", timeout=3)
            body[ip] = r.body[:2000]
            if f"RBEVAL_SECRET={plain}" in r.body:
                return ip
        return None
    try:
        ip, secs = wait_until(seen, 120, 2, "decrypted secret in the container env")
    except TimeoutError:
        ev.attach("env-responses.json", body)
        return False, "the container never saw the decrypted value"
    ev.measure("decrypted_visible_s", round(secs, 1))
    insp = cl.relish.run("inspect", "secretprobe", check=False)
    ev.attach("inspect.txt", insp.stdout + insp.stderr)
    ev.measure("plaintext_in_inspect", plain in insp.stdout + insp.stderr)
    ev.measure("ciphertext_in_container_env", enc in body.get(ip, ""))
    return plain not in insp.stdout + insp.stderr, f"container env has the plaintext via {ip}; inspect does not print it"


def _registry(context) -> tuple[bool, str]:
    cl, ev = context.cluster, context.ev
    need = context.cfg.threshold("registry.min_copies")
    per = {}
    for n in context.lab.nodes:
        r = cl.relish.run("images", endpoint=f"https://{n.ip}:9117", failover=False, check=False)
        per[n.name] = {"rc": r.rc, "has_podinfo": "podinfo" in r.stdout}
        ev.attach(f"images-{n.name}.txt", r.stdout + r.stderr)
    copies = sum(v["has_podinfo"] for v in per.values())
    ev.expect("podinfo_copies", f">= {need}")
    ev.measure("podinfo_copies", copies)
    ev.measure("images_per_node", per)
    return copies >= need, f"podinfo cached in Pickle on {copies} node(s)"


def _metrics(context) -> tuple[bool, str]:
    cl, ev = context.cluster, context.ev
    limit = context.cfg.threshold("observability.metrics_visible_s")
    last = {}

    def podinfo_metrics():
        r = cl.relish.run("metrics", "frontend", check=False, timeout=60, quiet=True)
        last["out"] = r.stdout + r.stderr
        rows = [line.split() for line in r.stdout.splitlines() if line.startswith("http_request")]
        good = [row for row in rows if len(row) >= 4 and row[3].isdigit() and int(row[3]) >= 3]
        return good or None
    try:
        rows, secs = wait_until(podinfo_metrics, limit, 5, "podinfo http_request metrics from 3 instances")
    except TimeoutError:
        ev.attach("metrics-frontend.txt", last.get("out", ""))
        return False, f"no http_request* metric from 3 frontend instances within {limit} s"
    ev.attach("metrics-frontend.txt", last["out"])
    ev.measure("metrics_visible_s", round(secs, 1))
    ev.measure("http_request_metrics", [r[0] for r in rows])
    return True, f"{len(rows)} podinfo http_request metric(s) from at least 3 instances"


def _top(context) -> tuple[bool, str]:
    cl, ev = context.cluster, context.ev
    limit = context.cfg.threshold("observability.metrics_visible_s")
    last = {}

    def sampled():
        r = cl.relish.run("top", check=False, timeout=60, quiet=True)
        last["out"] = r.stdout
        rows = [line.split() for line in r.stdout.splitlines()[1:] if line.strip()]
        mine = [row for row in rows if len(row) >= 8 and row[1] in ("frontend", "backend") and row[3] == "running"]
        missing = [row for row in mine if row[6] == "-" or row[7] == "-"]
        return mine if len(mine) >= 5 and not missing else None
    try:
        rows, secs = wait_until(sampled, limit, 5, "CPU and memory for every running fixture instance")
    except TimeoutError:
        ev.attach("top.txt", last.get("out", ""))
        return False, f"relish top lacked CPU or memory for a running fixture instance after {limit} s"
    ev.attach("top.txt", last["out"])
    ev.measure("top_rows_with_samples", len(rows))
    return True, f"{len(rows)} running fixture instances with CPU and memory samples"


def _logs(context) -> tuple[bool, str]:
    cl, ev = context.cluster, context.ev
    limit = context.cfg.threshold("observability.logs_visible_s")
    token = apps.unique("rbeval-log")
    path = apps.render(NATIVE / "log-probe.toml", ev.dir / "log-probe.toml", TOKEN=token)
    context.cleanups.append(lambda: apps.delete_app(cl, "logprobe"))
    ev.external(f"container registry: {BUSYBOX.split('/')[0]}")
    cl.relish.run("apply", str(path), check=True, timeout=180)
    apps.wait_running(cl, "logprobe", 1, context.cfg.threshold("bootstrap.app_ready_s"))
    t = time.monotonic()
    last = {}

    def visible():
        r = cl.relish.run("logs", "logprobe", "--grep", token, "--since", "10m", "--tail", "20", check=False,
                          timeout=30, quiet=True)
        last["out"] = r.stdout + r.stderr
        return r.stdout.count(token) or None
    try:
        count, _ = wait_until(visible, limit, 2, "log marker visible")
    except TimeoutError:
        ev.attach("logs.txt", last.get("out", ""))
        return False, f"marker not returned by relish logs within {limit} s of the app running"
    ev.attach("logs.txt", last["out"])
    ev.measure("logs_visible_s", round(time.monotonic() - t, 1))
    ev.measure("marker_lines", count)
    return True, f"relish logs returned {count} marker line(s)"


def _batch(context) -> tuple[bool, str]:
    cl, ev = context.cluster, context.ev
    tasks = context.cfg.threshold("batch.tasks")
    limit = context.cfg.threshold("batch.completion_s")
    name = apps.unique("rbeval-batch")
    ev.external(f"container registry: {BUSYBOX.split('/')[0]}")
    t = time.monotonic()
    r = cl.relish.run("run", "--batch", name, "--count", str(tasks), "--image", BUSYBOX, "--cpu", "50m-200m",
                      "--memory", "16Mi-32Mi", "--", "echo", "task", "{index}", check=False, timeout=120)
    ev.attach("run.txt", r.stdout + "\n--- stderr ---\n" + r.stderr)
    if not r.ok:
        return False, f"relish run refused: {r.stderr.strip()[:300]}"
    m = re.search(r"(?:batch|array|id)\D{0,12}?(\d+)", r.stdout + r.stderr, re.I)
    if not m:
        raise Blocked(f"could not find a batch id in relish run output: {r.stdout[:300]!r}")
    bid = m.group(1)
    st = cl.relish.run("batch-status", bid, "--wait", "--timeout", str(limit), output="json", check=False,
                       timeout=limit + 60)
    secs = round(time.monotonic() - t, 1)
    ev.attach("batch-status.json", st.stdout + st.stderr)
    try:
        summary = json.loads(st.stdout)
    except json.JSONDecodeError:
        return False, f"batch-status --output json did not print JSON: {st.stdout[:200]!r}"
    flat = json.dumps(summary).lower()
    ev.measure("batch_id", bid)
    ev.measure("batch_wall_s", secs)
    ev.measure("batch_summary_keys", sorted(summary)[:30] if isinstance(summary, dict) else type(summary).__name__)
    succeeded = summary.get("succeeded") if isinstance(summary, dict) else None
    failed = summary.get("failed") if isinstance(summary, dict) else None
    ev.measure("succeeded", succeeded)
    ev.measure("failed", failed)
    ok = st.rc == 0 and (succeeded == tasks or ('"succeeded"' in flat and failed in (0, None)))
    return ok and secs <= limit, f"{tasks} tasks, batch-status rc {st.rc}, succeeded {succeeded}, failed {failed}, {secs} s"


def _dashboard(context) -> tuple[bool, str]:
    cl, ev = context.cluster, context.ev
    limit = context.cfg.threshold("dashboard.ready_s")
    ep = cl.relish.endpoints()[0]
    proc = subprocess.Popen([cl.relish.binary, "dashboard", "--no-open", "--port", "0"], env=cl.relish.env(ep),
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    context.cleanups.append(proc.terminate)
    t = time.monotonic()
    url = None
    lines = []
    while time.monotonic() - t < limit and proc.poll() is None:
        line = proc.stdout.readline()
        lines.append(context.log.redact(line))
        m = re.search(r"https?://(127\.0\.0\.1|localhost):(\d+)(\S*)", line)
        if m:
            url = m
            break
    ev.attach("dashboard-stdout.txt", "".join(lines))
    if not url:
        return False, f"no loopback link printed within {limit} s (exit {proc.poll()})"
    port, path = int(url.group(2)), url.group(3) or "/"
    r = http_request("127.0.0.1", port, path, timeout=5)
    ev.measure("dashboard_status", r.status)
    ev.measure("dashboard_ready_s", round(time.monotonic() - t, 1))
    ev.measure("dashboard_is_html", "<html" in r.body.lower())
    return r.status == 200 and "<html" in r.body.lower(), f"GET loopback dashboard: {r.status}"


def _audit(context) -> tuple[bool, str]:
    cl, ev = context.cluster, context.ev
    r = cl.relish.run("audit", "--since", "6h", check=False, timeout=60)
    ev.attach("audit.txt", r.stdout + r.stderr)
    lines = [line for line in r.stdout.splitlines() if "frontend" in line]
    ev.measure("audit_lines_naming_frontend", len(lines))
    return r.ok and bool(lines), f"{len(lines)} audit line(s) name the fixture's frontend"


SUBSYSTEMS = {"secrets": _secrets, "registry": _registry, "metrics": _metrics, "resource top": _top,
              "logs": _logs, "task array": _batch, "dashboard": _dashboard, "audit log": _audit}


@when("I exercise the built-in {subsystem} through its documented interface")
def step_exercise(context, subsystem):
    fn = SUBSYSTEMS.get(subsystem)
    if fn is None:
        raise Blocked(f"no check for subsystem {subsystem!r}")
    ok, detail = fn(context)
    context.s["sub"] = (ok, detail)
    context.ev.measure("result", detail)
    context.ev.classify("native")


@then("it should give the documented result within the configured time")
def step_subsystem_result(context):
    ok, detail = context.s["sub"]
    context.ev.expect("documented_result", True)
    context.ev.measure("documented_result", ok)
    assert ok, detail
