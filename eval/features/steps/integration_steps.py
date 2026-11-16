"""Axis 2, integrations: alert webhooks, metrics surfaces, extension points, log export, GitOps."""

from __future__ import annotations

import hashlib
import hmac
import http.server
import json
import os
import re
import secrets
import stat
import subprocess
import threading
import time
from functools import partial
from pathlib import Path

from behave import given, then, when

from harness import apps, nodeconfig
from harness.core import ROOT, Blocked, Unsupported, wait_until
from harness.probes import http_request
from harness.services import Receiver
from harness.steplib import FIXTURE_HOST

NATIVE = ROOT / "fixtures" / "native"
BUSYBOX = "public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028"
PODINFO = "ghcr.io/stefanprodan/podinfo@sha256:ec73780a8425f59ea49f5bc8cdff0d598805a224fbaa1f86c67a244f250fa9da"
EXPOSITION = re.compile(r"^# (TYPE|HELP) \w+", re.M)


def _gateway(context) -> str:
    return context.cfg.lab["network"]["gateway"]


def _probe_app_toml(name: str, replicas: int) -> str:
    return (f'[app.{name}]\nimage = "{PODINFO}"\ncommand = ["./podinfo", "--port=9898", "--level=info"]\n'
            f'replicas = {replicas}\nport = 9898\ncpu = "20m-200m"\nmemory = "16Mi-64Mi"\n')


# --------------------------------------------------------------------------
# EXT-003 alert webhooks
# --------------------------------------------------------------------------


def _alert_secret(context) -> str:
    path = context.cfg.state_dir / "secrets" / "alert-hmac"
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(secrets.token_hex(24))
        path.chmod(0o600)
    value = path.read_text().strip()
    context.log.redact.add(value)
    return value


@given("an alert receiver on the lab gateway")
def step_alert_receiver(context):
    port = context.cfg.lab["host_services"]["alert_receiver_port"]
    recv = Receiver(_gateway(context), port, context.ev.dir / "receiver.jsonl").start()
    context.cleanups.append(recv.stop)
    context.s["receiver"] = recv
    context.ev.attach_path(context.ev.dir / "receiver.jsonl")
    context.ev.external(f"HTTP receiver on {_gateway(context)}:{port} (harness, stands in for a paging tool)")


@given("every node sends alerts to that receiver with a signing secret")
def step_alert_destination(context):
    port = context.cfg.lab["host_services"]["alert_receiver_port"]
    block = (f'[[alerts.destinations]]\ntype = "webhook"\nurl = "http://{_gateway(context)}:{port}/rbeval"\n'
             f'severity = []\nsecret = "{_alert_secret(context)}"\n')
    changed = nodeconfig.set_block(context, "alerts", block)
    context.cleanups.append(lambda: nodeconfig.clear_block(context, "alerts"))
    context.ev.measure("node_toml_changed", changed)
    context.ev.classify("configured")


@when("I run a task array whose only task fails")
def step_failing_job(context):
    name = apps.unique("rbeval-fail")
    context.s.update(job=name, t_job=time.monotonic())
    r = context.cluster.relish.run("run", "--batch", name, "--count", "1", "--max-attempts", "1", "--image", BUSYBOX,
                                   "--cpu", "50m-200m", "--memory", "16Mi-32Mi", "--", "sh", "-c", "exit 3",
                                   check=True, timeout=120)
    context.ev.attach("run.txt", r.stdout + r.stderr)
    context.ev.external(f"container registry: {BUSYBOX.split('/')[0]}")


def _job_alerts(context) -> list[dict]:
    out = []
    for req in context.s["receiver"].requests:
        try:
            body = json.loads(req["body"])
        except json.JSONDecodeError:
            continue
        if (body.get("alert") or {}).get("name") == "job_run_failed":
            out.append({**req, "json": body})
    return out


@then("the receiver should get a job_run_failed alert within the configured delivery time")
def step_alert_arrives(context):
    limit = context.cfg.threshold("observability.webhook_delivery_s")
    try:
        got, _ = wait_until(lambda: _job_alerts(context) or None, limit, 2, "job_run_failed at the receiver")
        secs = round(got[0]["mono"] - context.s["t_job"], 1)
    except TimeoutError:
        got, secs = [], None
    context.ev.expect("delivery_s", f"<= {limit}")
    context.ev.measure("delivery_s", secs)
    context.ev.measure("requests_received", len(context.s["receiver"].requests))
    context.s["alerts"] = got
    assert got, f"no job_run_failed alert within {limit} s ({len(context.s['receiver'].requests)} other requests)"


@then("the alert body should carry the documented fields")
def step_alert_fields(context):
    body = context.s["alerts"][0]["json"]
    alert = body.get("alert") or {}
    want = {"version": body.get("version"), "alert.name": alert.get("name"), "alert.severity": alert.get("severity"),
            "alert.status": alert.get("status"), "alert.fired_at": alert.get("fired_at"),
            "cluster": body.get("cluster"), "timestamp": body.get("timestamp")}
    context.ev.attach("alert-body.json", body)
    context.ev.measure("alert_fields", want)
    context.ev.expect("alert_fields", "version 1, name job_run_failed, severity critical, status firing, timestamps set")
    missing = [k for k, v in want.items() if v in (None, "")]
    assert not missing, f"documented fields missing: {missing}"
    assert str(alert.get("severity")).lower() == "critical", f"severity {alert.get('severity')}"


@then("the X-Mayo-Signature-256 header should be the HMAC-SHA256 of the body")
def step_alert_signature(context):
    req = context.s["alerts"][0]
    headers = {k.lower(): v for k, v in req["headers"].items()}
    got = headers.get("x-mayo-signature-256")
    want = "sha256=" + hmac.new(_alert_secret(context).encode(), req["body"].encode(), hashlib.sha256).hexdigest()
    context.ev.expect("signature_valid", True)
    context.ev.measure("signature_header_present", got is not None)
    context.ev.measure("signature_valid", got is not None and hmac.compare_digest(got, want))
    assert got is not None, f"no X-Mayo-Signature-256 header: {sorted(headers)}"
    assert hmac.compare_digest(got, want), "signature does not match HMAC-SHA256 of the body"


@then("only one job_run_failed alert should arrive for the run")
def step_alert_once(context):
    time.sleep(30)
    n = len(_job_alerts(context))
    context.ev.expect("job_run_failed_alerts", 1)
    context.ev.measure("job_run_failed_alerts", n)
    assert n == 1, f"{n} job_run_failed alerts for one failed run"


# --------------------------------------------------------------------------
# EXT-004 metrics surfaces
# --------------------------------------------------------------------------


@when("I request metrics through {surface} from the workstation")
def step_metrics_surface(context, surface):
    node = context.lab.nodes[0]
    if "/v1/read" in surface:
        status, headers, body = nodeconfig.api_request(context, node, "POST", "/v1/read", body=b"",
                                                       headers={"Content-Type": "application/x-protobuf"})
    elif "/metrics" in surface and "node API" in surface:
        status, headers, body = nodeconfig.api_request(context, node, "GET", "/metrics")
    elif "ingress" in surface:
        r = http_request(node.ip, 80, "/metrics", FIXTURE_HOST, timeout=5)
        status, headers, body = r.status, {}, r.body
        context.s["via_ingress"] = True
    else:
        raise Blocked(f"no mapping for surface {surface!r}")
    context.s["metrics_response"] = (status, body)
    context.ev.attach("response.txt", f"status {status}\n\n{body[:20000]}")
    context.ev.measure("http_status", status)
    context.ev.measure("content_type", headers.get("Content-Type") or headers.get("content-type"))


@then("the response should be in the Prometheus text exposition format")
def step_exposition(context):
    status, body = context.s["metrics_response"]
    ok = status == 200 and bool(EXPOSITION.search(body or ""))
    context.ev.expect("prometheus_exposition", True)
    context.ev.measure("prometheus_exposition", ok)
    if ok:
        if context.s.get("via_ingress"):
            context.ev.classify("workaround")
            context.ev.limitation("this is the application's own /metrics exposed through its public ingress; "
                                  "it carries no cluster or node metrics and is reachable by anyone who can reach the ingress")
        else:
            context.ev.classify("native")
        return
    context.ev.classify("unsupported")
    if status in (404, 405, None):
        raise Unsupported(f"no Prometheus surface: HTTP {status}; body starts {str(body)[:160]!r}")
    raise AssertionError(f"HTTP {status} without Prometheus exposition: {str(body)[:200]!r}")


# --------------------------------------------------------------------------
# EXT-005 extension points
# --------------------------------------------------------------------------


def _custom_kind(context):
    path = context.ev.dir / "custom-kind.toml"
    path.write_text('[widget.rbeval]\nsize = 3\ncolour = "orange"\n')
    lint = context.cluster.relish.run("lint", str(path), check=False)
    r = context.cluster.relish.run("apply", str(path), check=False)
    context.ev.attach("lint.txt", lint.stdout + lint.stderr)
    context.ev.attach("apply.txt", r.stdout + r.stderr)
    context.ev.measure("lint_rc", lint.rc)
    context.ev.measure("apply_rc", r.rc)
    if r.rc != 0:
        raise Unsupported(f"relish apply refuses a table it does not know: {r.stderr.strip()[:300]}")
    raise AssertionError(f"relish apply accepted [widget.rbeval] with exit 0 and no way to read it back: {r.stdout[:200]}")


def _cli_plugin(context):
    d = context.ev.dir / "plugin-bin"
    d.mkdir(parents=True, exist_ok=True)
    plugin = d / "relish-rbevalhello"
    plugin.write_text("#!/bin/sh\necho rbeval-plugin-ran \"$@\"\n")
    plugin.chmod(plugin.stat().st_mode | stat.S_IXUSR)
    env = {**context.cluster.relish.env(context.cluster.relish.endpoints()[0]),
           "PATH": f"{d}:{os.environ.get('PATH', '')}"}
    r = context.log.run([context.cluster.relish.binary, "rbevalhello", "x"], env=env, check=False)
    context.ev.measure("rc", r.rc)
    context.ev.attach("output.txt", r.stdout + r.stderr)
    if "rbeval-plugin-ran" in r.stdout:
        context.ev.classify("native")
        return
    raise Unsupported(f"relish does not dispatch relish-<name> executables on PATH: {r.stderr.strip()[:200]}")


def _admission(context):
    hits = {}
    for args in (["--help"], ["apply", "--help"]):
        r = context.log.run([context.cluster.relish.binary, *args], check=False, quiet=True)
        hits[" ".join(args)] = [line.strip() for line in r.stdout.splitlines() if re.search(r"admission|validat", line, re.I)]
    for doc in ("docs/manual/13_reference.md", "docs/manual/10_security.md", "docs/manual/01_deploy-an-app.md"):
        text = context.claims._read(doc) or ""
        hits[doc] = [line.strip() for line in text.splitlines() if re.search(r"admission|validating webhook|mutating", line, re.I)]
    context.ev.attach("search.json", hits)
    context.ev.confidence("medium")
    found = {k: v for k, v in hits.items() if v}
    if found:
        raise AssertionError(f"documentation mentions admission or validation hooks but the harness has no check: {found}")
    raise Unsupported("no admission or validation webhook in relish --help, relish apply --help or the reference, "
                      "security and deploy manuals at the pinned docs ref")


@when("I try to extend the platform with {mechanism}")
def step_extend(context, mechanism):
    table = {"a custom resource kind in the app config": _custom_kind,
             "a relish plugin executable on PATH": _cli_plugin,
             "an admission or validation webhook": _admission}
    if mechanism not in table:
        raise Blocked(f"no mapping for {mechanism!r}")
    context.ev.measure("mechanism", mechanism)
    table[mechanism](context)
    context.s["extended"] = True


@then("the extension should work through a documented interface")
def step_extension_works(context):
    context.ev.expect("extension_works", True)
    context.ev.measure("extension_works", bool(context.s.get("extended")))
    assert context.s.get("extended")


class ExternalController:
    """A minimal operator: keeps desired replicas for its apps and re-applies when the cluster drifts."""

    def __init__(self, context, poll_s: float):
        self.context = context
        self.poll_s = poll_s
        self.spec: dict[str, int] = {}
        self.actions: list[dict] = []
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, daemon=True)

    def _loop(self):
        last_apply: dict[str, float] = {}
        while not self._stop.is_set():
            for name, want in list(self.spec.items()):
                try:
                    have = len(apps.running(self.context.cluster, name))
                except Exception as exc:  # the controller keeps going, like a real one
                    self.actions.append({"ts": time.time(), "app": name, "error": str(exc)[:160]})
                    continue
                if have != want and time.monotonic() - last_apply.get(name, 0) > 10:
                    path = self.context.ev.dir / "controller" / f"{name}.toml"
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(_probe_app_toml(name, want))
                    r = self.context.cluster.relish.run("apply", str(path), check=False, quiet=True)
                    last_apply[name] = time.monotonic()
                    self.actions.append({"ts": time.time(), "app": name, "running": have, "want": want, "rc": r.rc})
            self._stop.wait(self.poll_s)

    def start(self):
        self._thread.start()
        return self

    def stop(self):
        self._stop.set()
        self._thread.join(timeout=30)


@given("an external controller that keeps a custom spec outside the cluster")
def step_controller(context):
    ctl = ExternalController(context, context.cfg.threshold("extension.controller_poll_s")).start()
    context.s["controller"] = ctl

    def stop():
        ctl.stop()
        context.ev.attach("controller-actions.json", ctl.actions)
        for name in ctl.spec:
            apps.delete_app(context.cluster, name)
    context.cleanups.append(stop)
    context.ev.classify("workaround")
    context.ev.limitation("the custom spec lives in the controller, not the cluster: there is no CRD store and no "
                          "watch API, so the controller polls relish status")


@when("the controller is given a spec for {n:d} replicas of a new app")
def step_controller_spec(context, n):
    name = apps.unique("ctrl")
    context.s.update(ctrl_app=name, ctrl_n=n, t_spec=time.monotonic())
    context.s["controller"].spec[name] = n


@then("the app should run with {n:d} replicas within the configured convergence time")
def step_controller_converged(context, n):
    limit = context.cfg.threshold("extension.controller_converge_s")
    name = context.s["ctrl_app"]
    try:
        _, secs = wait_until(lambda: len(apps.running(context.cluster, name)) == n, limit, 2, f"{name} at {n}")
    except TimeoutError:
        secs = None
    context.ev.expect("controller_converge_s", f"<= {limit}")
    context.ev.measure("controller_converge_s", round(secs, 1) if secs is not None else None)
    assert secs is not None, f"{name} not at {n} running replicas within {limit} s"


@then("after a manual scale-down the controller should restore {n:d} replicas within the configured drift time")
def step_controller_drift(context, n):
    limit = context.cfg.threshold("extension.drift_revert_s")
    name = context.s["ctrl_app"]
    path = context.ev.dir / "manual-scale.toml"
    path.write_text(_probe_app_toml(name, 1))
    t = time.monotonic()
    context.cluster.relish.run("apply", str(path), operator="scale the controller's app to 1 by hand")
    try:
        wait_until(lambda: len(apps.running(context.cluster, name)) <= 1, 60, 1, "scale-down visible")
        context.ev.measure("scale_down_observed", True)
    except TimeoutError:
        context.ev.measure("scale_down_observed", False)
    try:
        wait_until(lambda: len(apps.running(context.cluster, name)) == n, limit, 2, f"{name} back at {n}")
        secs = round(time.monotonic() - t, 1)
    except TimeoutError:
        secs = None
    context.ev.expect("drift_revert_s", f"<= {limit}")
    context.ev.measure("drift_revert_s", secs)
    assert secs is not None, f"controller did not restore {n} replicas within {limit} s"


@then("the cluster event API should list events for the app")
def step_events_api(context):
    name = context.s["ctrl_app"]
    status, _, body = nodeconfig.api_request(context, context.lab.nodes[0], "GET", "/v1/events")
    context.ev.attach("events.json", body[:200000])
    hits = body.count(name) if body else 0
    context.ev.measure("events_status", status)
    context.ev.measure("events_naming_app", hits)
    context.ev.expect("events_naming_app", ">= 1")
    assert status == 200, f"GET /v1/events returned {status}"
    assert hits, f"/v1/events has no event naming {name}"


# --------------------------------------------------------------------------
# EXT-007 log export
# --------------------------------------------------------------------------


@given("an app printing a unique log marker")
def step_log_marker_app(context):
    cl = context.cluster
    token = apps.unique("rbeval-export")
    path = apps.render(NATIVE / "log-probe.toml", context.ev.dir / "log-probe.toml", TOKEN=token)
    context.cleanups.append(lambda: apps.delete_app(cl, "logprobe"))
    cl.relish.run("apply", str(path), check=True, timeout=180)
    apps.wait_running(cl, "logprobe", 1, context.cfg.threshold("bootstrap.app_ready_s"))
    wait_until(lambda: token in cl.relish.run("logs", "logprobe", "--grep", token, "--since", "10m", check=False,
                                              quiet=True).stdout, context.cfg.threshold("observability.logs_visible_s"),
               2, "marker in relish logs")
    node = apps.running(cl, "logprobe")[0]["node"]
    context.s.update(marker=token, marker_node=context.lab.node(node))
    context.ev.measure("marker_node", node)


@when("I export the logs of the node running it to the workstation with relish logs-export")
def step_logs_export(context):
    node = context.s["marker_node"]
    dest = context.ev.dir / "export"
    t = time.monotonic()
    r = context.cluster.relish.run("logs-export", "--dest", str(dest), "--node-id", node.name,
                                   endpoint=f"https://{node.ip}:9117", failover=False, check=False, timeout=300)
    files = sorted(str(p.relative_to(dest)) for p in dest.rglob("*") if p.is_file()) if dest.exists() else []
    context.s.update(export_dir=dest, t_export=t)
    context.ev.attach("logs-export.txt", r.stdout + r.stderr)
    context.ev.measure("export_rc", r.rc)
    context.ev.measure("exported_files", files[:50])
    context.ev.measure("export_s", round(time.monotonic() - t, 1))
    assert r.ok and files, f"relish logs-export exit {r.rc}, {len(files)} files: {r.stderr[:300]}"


@then("a standard Parquet reader on the workstation should find the marker")
def step_parquet_reader(context):
    import duckdb  # noqa: PLC0415  (imported here so the dependency is only needed by this scenario)
    dest, marker = context.s["export_dir"], context.s["marker"]
    files = [str(p) for p in Path(dest).rglob("*.parquet")]
    if not files:
        raise AssertionError(f"no .parquet files in the export: {list(Path(dest).rglob('*'))[:10]}")
    con = duckdb.connect()
    cols = [r[0] for r in con.execute("DESCRIBE SELECT * FROM read_parquet(?, union_by_name=true)", [files]).fetchall()]
    rows = con.execute("SELECT * FROM read_parquet(?, union_by_name=true)", [files]).fetchall()
    hits = [r for r in rows if any(marker in str(v) for v in r)]
    context.ev.measure("duckdb_version", duckdb.__version__)
    context.ev.measure("parquet_columns", cols)
    context.ev.measure("parquet_rows", len(rows))
    context.ev.measure("marker_rows", len(hits))
    context.ev.measure("export_to_query_s", round(time.monotonic() - context.s["t_export"], 1))
    context.ev.classify("native")
    assert hits, f"marker {marker} not in {len(rows)} exported rows (columns {cols})"


@then("relish logs-search on the workstation should find the same marker")
def step_logs_search(context):
    dest, marker = context.s["export_dir"], context.s["marker"]
    r = context.log.run([context.cluster.relish.binary, "logs-search", str(dest),
                         f"SELECT count(*) AS n FROM logs WHERE line LIKE '%{marker}%'"], check=False, timeout=120)
    context.ev.attach("logs-search.txt", r.stdout + r.stderr)
    nums = [int(x) for x in re.findall(r"\b(\d+)\b", r.stdout)]
    context.ev.measure("logs_search_rc", r.rc)
    context.ev.measure("logs_search_count", max(nums) if nums else None)
    assert r.ok and nums and max(nums) > 0, f"logs-search exit {r.rc}: {r.stdout[:200]} {r.stderr[:200]}"


# --------------------------------------------------------------------------
# EXT-008 GitOps
# --------------------------------------------------------------------------


class GitHttp:
    """A bare repository served read-only over git's dumb HTTP protocol from the lab gateway."""

    def __init__(self, context):
        self.context = context
        self.root = context.cfg.state_dir / "git"
        self.bare = self.root / "infra.git"
        self.work = self.root / "work"
        port = context.cfg.lab["host_services"]["git_http_port"]
        handler = partial(_QuietFiles, directory=str(self.root))
        self.server = http.server.ThreadingHTTPServer((_gateway(context), port), handler)
        self.url = f"http://{_gateway(context)}:{port}/infra.git"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def git(self, *args: str, cwd: Path | None = None):
        where = ["-C", str(cwd)] if cwd else []
        return self.context.log.run(["git", *where, "-c", "user.name=rbeval", "-c", "user.email=rbeval@lab.invalid",
                                     *args], check=True, quiet=True)

    def start(self):
        if self.root.exists():
            subprocess.run(["rm", "-rf", str(self.root)], check=True)
        self.root.mkdir(parents=True)
        self.git("init", "--bare", "-b", "main", str(self.bare))
        self.git("init", "-b", "main", str(self.work))
        (self.work / "README.md").write_text("rbeval GitOps fixture repository\n")
        self.commit("initial commit")
        self.thread.start()
        return self

    def commit(self, message: str) -> str:
        self.git("add", "-A", cwd=self.work)
        self.git("commit", "-q", "--allow-empty", "-m", message, cwd=self.work)
        self.git("push", "-q", str(self.bare), "HEAD:main", cwd=self.work)
        self.git("update-server-info", cwd=self.bare)
        return self.context.log.run(["git", "-C", str(self.work), "rev-parse", "HEAD"], check=True,
                                    quiet=True).stdout.strip()

    def stop(self):
        self.server.shutdown()
        self.server.server_close()


class _QuietFiles(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass


@given("a git repository served over HTTP from the lab gateway")
def step_git_repo(context):
    repo = GitHttp(context).start()
    context.cleanups.append(repo.stop)
    context.s["repo"] = repo
    context.ev.external(f"git dumb-HTTP server on {repo.url} (harness, stands in for a Git host)")
    context.ev.deviation("the repository is served over plain HTTP from the lab gateway; the manual shows https and ssh URLs")


@given("every node syncs that repository")
def step_gitops_config(context):
    repo = context.s["repo"]
    block = f'[gitops]\nrepo = "{repo.url}"\nbranch = "main"\npath = "/"\npoll_interval_secs = 30\n'
    changed = nodeconfig.set_block(context, "gitops", block)
    context.cleanups.append(lambda: nodeconfig.clear_block(context, "gitops"))
    context.ev.measure("node_toml_changed", changed)
    context.ev.classify("configured")
    context.s["fixture_running"] = {a: len(apps.running(context.cluster, a)) for a in ("backend", "frontend")}


def _write_gitops_app(context, replicas: int | None) -> str:
    repo = context.s["repo"]
    name = context.s.setdefault("gitops_app", apps.unique("gitops"))
    f = repo.work / "apps" / f"{name}.toml"
    if replicas is None:
        f.unlink(missing_ok=True)
        return repo.commit(f"delete {name}")
    f.parent.mkdir(parents=True, exist_ok=True)
    f.write_text(_probe_app_toml(name, replicas))
    return repo.commit(f"{name}: {replicas} replicas")


@when("I commit an app with {n:d} replicas to the repository")
def step_git_commit_app(context, n):
    context.s["t_commit"] = time.monotonic()
    sha = _write_gitops_app(context, n)
    context.ev.measure("commit", sha)
    context.cleanups.append(lambda: apps.delete_app(context.cluster, context.s["gitops_app"]))


@then("the app should run with {n:d} replicas within the configured sync time")
def step_git_synced(context, n):
    limit = context.cfg.threshold("gitops.sync_s")
    name = context.s["gitops_app"]
    try:
        _, _ = wait_until(lambda: len(apps.running(context.cluster, name)) == n, limit, 2, f"{name} synced")
        secs = round(time.monotonic() - context.s["t_commit"], 1)
    except TimeoutError:
        secs = None
    context.ev.expect("sync_s", f"<= {limit}")
    context.ev.measure("sync_s", secs)
    if secs is None:
        r = context.cluster.relish.run("audit", "--cluster", "--since", "15m", check=False)
        context.ev.attach("audit-after-commit.txt", r.stdout + r.stderr)
    assert secs is not None, f"{name} not running with {n} replicas within {limit} s of the commit"


@then("apps applied outside GitOps should be left alone")
def step_git_leaves_others(context):
    time.sleep(35)
    now = {a: len(apps.running(context.cluster, a)) for a in ("backend", "frontend")}
    context.ev.expect("fixture_running_after_sync", context.s["fixture_running"])
    context.ev.measure("fixture_running_after_sync", now)
    assert now == context.s["fixture_running"], f"before {context.s['fixture_running']}, after {now}"


@when("I scale the GitOps app to {n:d} replica by hand")
def step_git_drift(context, n):
    name = context.s["gitops_app"]
    path = context.ev.dir / "drift.toml"
    path.write_text(_probe_app_toml(name, n))
    context.s["t_drift"] = time.monotonic()
    context.cluster.relish.run("apply", str(path), operator=f"scale {name} to {n} by hand")
    try:
        wait_until(lambda: len(apps.running(context.cluster, name)) <= n, 60, 1, "manual scale visible")
        context.ev.measure("manual_scale_observed", True)
    except TimeoutError:
        context.ev.measure("manual_scale_observed", False)


@then("the next sync should restore {n:d} replicas within the configured drift time")
def step_git_drift_reverted(context, n):
    limit = context.cfg.threshold("gitops.drift_revert_s")
    name = context.s["gitops_app"]
    try:
        wait_until(lambda: len(apps.running(context.cluster, name)) == n, limit, 2, f"{name} back at {n}")
        secs = round(time.monotonic() - context.s["t_drift"], 1)
    except TimeoutError:
        secs = None
    context.ev.expect("drift_revert_s", f"<= {limit}")
    context.ev.measure("drift_revert_s", secs)
    assert secs is not None, f"GitOps did not restore {n} replicas within {limit} s"


@when("I delete the app from the repository")
def step_git_delete(context):
    context.s["t_delete"] = time.monotonic()
    context.ev.measure("delete_commit", _write_gitops_app(context, None))


@then("the app should be removed within the configured delete time")
def step_git_deleted(context):
    limit = context.cfg.threshold("gitops.delete_s")
    name = context.s["gitops_app"]
    try:
        wait_until(lambda: not apps.instances(context.cluster, name), limit, 2, f"{name} removed")
        secs = round(time.monotonic() - context.s["t_delete"], 1)
    except TimeoutError:
        secs = None
    context.ev.expect("delete_s", f"<= {limit}")
    context.ev.measure("delete_s", secs)
    assert secs is not None, f"{name} still present {limit} s after it was deleted from the repository"
