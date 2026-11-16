"""Axis 3: interoperability and adoption (Kubernetes migration)."""

from __future__ import annotations

import difflib
import json
import socket
import subprocess
import time
import tomllib

import yaml
from behave import given, then, when

from harness import apps, migration
from harness.core import ROOT, Blocked, wait_until
from harness.probes import http_request
from harness.migration import parse_report
from harness.services import KindBaseline
from harness.steplib import node_ips

BASELINE = ROOT / "fixtures" / "k8s" / "baseline.yaml"
EXPECT_MESSAGE = "rbeval-config-7f3a"
EXPECT_TOKEN = "rbeval-dummy-secret-91c2"
# The baseline lives in its own namespace and host so the migration never
# touches the native fixture that other scenarios share.
K8S_NS = "rbeval-shop"
K8S_HOST = "shop.rbeval.test"


def functional_probes(get) -> dict:
    """The same four probes for the Kubernetes baseline and the migrated app.
    `get(method, path, body)` returns an HttpResult."""
    out = {}
    r = get("GET", "/healthz", None)
    out["healthz"] = {"ok": r.status == 200, "status": r.status}
    r = get("GET", "/api/info", None)
    try:
        msg = json.loads(r.body).get("message")
    except (json.JSONDecodeError, AttributeError):
        msg = None
    out["configmap_message"] = {"ok": msg == EXPECT_MESSAGE, "got": msg}
    r = get("GET", "/env", None)
    out["secret_env"] = {"ok": f"FIXTURE_TOKEN={EXPECT_TOKEN}" in r.body,
                         "present": "FIXTURE_TOKEN=" in r.body}
    token = apps.unique("migr")
    r = get("POST", "/echo", token.encode())
    out["backend_by_name"] = {"ok": token in r.body, "status": r.status}
    return out


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@given("a validated fixture application defined by Kubernetes Deployment, Service, ConfigMap, and Secret manifests")
def step_baseline(context):
    kind = KindBaseline(context.cfg, context.log)
    kind.ensure()
    if kind not in context.shared.setdefault("services", []):
        context.shared["services"].append(kind)
    context.ev.external(f"kind {context.cfg.lab['baseline_k8s']['node_image']} (baseline only, not used by Reliaburger)")
    kind.kubectl("delete", "-f", str(BASELINE), "--ignore-not-found", "--wait=true", check=False)
    kind.kubectl("apply", "-f", str(BASELINE), check=True)
    for d in ("backend", "frontend"):
        kind.kubectl("rollout", "status", "-n", K8S_NS, f"deployment/{d}", "--timeout=300s", check=True, timeout=330)
    port = _free_port()
    pf = subprocess.Popen(["kubectl", "--kubeconfig", str(kind.kubeconfig), "port-forward", "-n", K8S_NS, "svc/frontend",
                           f"{port}:9898"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_until(lambda: http_request("127.0.0.1", port, "/healthz").ok, 30, 1, "baseline port-forward")
        probes = functional_probes(lambda m, p, b: http_request("127.0.0.1", port, p, method=m, body=b))
    finally:
        pf.terminate()
    kinds = sorted({d["kind"] for d in yaml.safe_load_all(BASELINE.read_text()) if d})
    context.ev.measure("baseline_kinds", kinds)
    context.ev.measure("baseline_probes", probes)
    context.ev.attach("baseline.yaml", BASELINE.read_text())
    context.s["baseline_probes"] = probes
    if not all(p["ok"] for p in probes.values()):
        raise Blocked(f"the Kubernetes baseline itself fails its probes: {probes}")


@when("I import the fixture using the documented Kubernetes migration path")
def step_import(context):
    cl, ev = context.cluster, context.ev
    # Step 1: preview, as docs/manual/09_kubernetes.md shows.
    dry = cl.relish.run("apply", "-f", str(BASELINE), "--dry-run", check=False,
                        operator="relish apply -f baseline.yaml --dry-run (read the migration report)")
    ev.attach("migration-report.txt", dry.stderr)
    ev.attach("dry-run-plan.txt", dry.stdout)
    report = parse_report(dry.stderr)
    context.s["report"] = report
    ev.manual_step("read the migration report and decide what to fix")
    # Step 2: zero-touch attempt, measured before any fix. Start from a clean
    # slate if an earlier run left the migrated apps behind.
    if apps.instances(cl, "frontend", K8S_NS) or apps.instances(cl, "backend", K8S_NS):
        for app in ("frontend", "backend"):
            apps.delete_app(cl, app, K8S_NS)
        wait_until(lambda: not apps.instances(cl, "frontend", K8S_NS) and not apps.instances(cl, "backend", K8S_NS),
                   180, 2, "previous migrated apps removed")
    zero = cl.relish.run("apply", "-f", str(BASELINE), check=False, timeout=180)
    ev.attach("zero-touch-apply.txt", zero.stdout + "\n--- stderr ---\n" + zero.stderr)
    context.cleanups.append(lambda: (apps.delete_app(cl, "frontend", K8S_NS), apps.delete_app(cl, "backend", K8S_NS)))
    apps.wait_running(cl, "frontend", 3, context.cfg.threshold("bootstrap.app_ready_s"), namespace=K8S_NS)
    apps.wait_ingress(node_ips(context), K8S_HOST, "/healthz", 180)
    ip = node_ips(context)[0]
    zero_probes = functional_probes(lambda m, p, b: http_request(ip, 80, p, K8S_HOST, method=m, body=b))
    ev.measure("zero_touch_probes", zero_probes)
    # Step 3: the documented fixes, applied to the imported TOML.
    imp = cl.relish.run("import", "-f", str(BASELINE), check=True,
                        operator="relish import -f baseline.yaml > app.toml")
    converted = imp.stdout
    ev.attach("converted.toml", converted)
    docs = list(yaml.safe_load_all(BASELINE.read_text()))
    cms = {d["metadata"]["name"]: d.get("data", {}) for d in docs if d and d["kind"] == "ConfigMap"}
    secs = {d["metadata"]["name"]: d.get("stringData", {}) for d in docs if d and d["kind"] == "Secret"}
    pubkey = cl.relish.run("secret", "pubkey", check=True, operator="relish secret pubkey").stdout.strip().split()[-1]
    edits: dict[str, list[str]] = {}
    changes = []
    for d in docs:
        if not d or d["kind"] != "Deployment":
            continue
        app = d["metadata"]["name"]
        for c in d["spec"]["template"]["spec"]["containers"][:1]:
            for e in c.get("env", []):
                vf = e.get("valueFrom")
                if not vf:
                    continue
                if "configMapKeyRef" in vf:
                    ref = vf["configMapKeyRef"]
                    value = cms[ref["name"]][ref["key"]]
                    edits.setdefault(app, []).append(f'{e["name"]} = {json.dumps(value)}')
                    changes.append(f"[app.{app}.env] {e['name']}: copy value from ConfigMap/{ref['name']}")
                elif "secretKeyRef" in vf:
                    ref = vf["secretKeyRef"]
                    plain = secs[ref["name"]][ref["key"]]
                    # The fixture secret is a dummy value and stays visible in evidence on purpose.
                    enc = cl.relish.run("secret", "encrypt", "--pubkey", pubkey, plain, check=True,
                                        operator=f"relish secret encrypt for {e['name']}").stdout.strip()
                    edits.setdefault(app, []).append(f'{e["name"]} = {json.dumps(enc)}')
                    changes.append(f"[app.{app}.env] {e['name']}: relish secret encrypt the Secret/{ref['name']} value")
    migrated = converted
    for app, lines in edits.items():
        header = f"[app.{app}.env]"
        block = "\n".join(lines)
        if header in migrated:
            migrated = migrated.replace(header, header + "\n" + block, 1)
        else:
            migrated = migrated.rstrip() + f"\n\n{header}\n{block}\n"
    tomllib.loads(migrated)
    mpath = ev.dir / "migrated.toml"
    mpath.write_text(migrated)
    ev.attach("migrated.toml", migrated)
    ev.attach("manifest.diff", "".join(difflib.unified_diff(converted.splitlines(True), migrated.splitlines(True),
                                                             "converted.toml", "migrated.toml")))
    for ch in changes:
        ev.manual_step(ch)
        context.log.note_operator_action(ch, kind="manifest_edit")
    cl.relish.run("apply", str(mpath), check=True, timeout=180, operator="relish apply app.toml")
    ev.manual_step("relish apply the edited TOML (the YAML is no longer the source of truth)")
    context.s["changes"] = changes
    streak = {"n": 0}

    def rolled_out():
        ok = all(functional_probes(lambda m, p, b, ip=ip: http_request(ip, 80, p, K8S_HOST, method=m, body=b))
                 ["configmap_message"]["ok"] for ip in node_ips(context))
        streak["n"] = streak["n"] + 1 if ok else 0
        return streak["n"] >= 3
    _, secs = wait_until(rolled_out, context.cfg.threshold("rolling_deploy.completion_s"), 3, "migrated rollout")
    ev.measure("migrated_rollout_s", round(secs, 1))


@then("the harness should record accepted and rejected resource kinds")
def step_kinds(context):
    report = context.s["report"]
    docs = [d for d in yaml.safe_load_all(BASELINE.read_text()) if d]
    converted_toml = (context.ev.dir / "converted.toml").read_text()
    accounted, silent = {}, []
    for d in docs:
        key = f"{d['kind']}/{d['metadata']['name']}"
        reported = [s for s, items in report.items() for i in items if f"{i['kind']}/{i['name']}" == key]
        reflected = False
        if d["kind"] == "Service":
            reflected = f"[app.{d['metadata']['name']}]" in converted_toml
        if d["kind"] == "Ingress":
            hosts = [r["host"] for r in d["spec"]["rules"]]
            reflected = all(f'host = "{h}"' in converted_toml for h in hosts)
        accounted[key] = {"reported_in": sorted(set(reported)), "reflected_in_toml": reflected}
        if not reported and not reflected:
            silent.append(key)
    accepted = sorted({k.split("/")[0] for k, v in accounted.items()
                       if "dropped" not in v["reported_in"]})
    rejected = sorted({k.split("/")[0] for k, v in accounted.items() if "dropped" in v["reported_in"]})
    context.ev.measure("resource_accounting", accounted)
    context.ev.measure("accepted_kinds", accepted)
    context.ev.measure("rejected_kinds", rejected)
    context.ev.measure("silently_lost", silent)
    context.ev.expect("silently_lost", [])
    context.ev.attach("migration-report.json", report)
    # Known diagnostic inaccuracy is recorded, not asserted on.
    for item in report["approximated"]:
        if item["kind"] == "ConfigMap" and "not referenced" in item["detail"]:
            context.ev.observe(f"report says {item['kind']}/{item['name']} is 'not referenced by any workload' "
                               "although the frontend references it through configMapKeyRef")
    assert not silent, f"resources neither reported nor converted: {silent}"


@then("every rejected resource should have its diagnostic output preserved")
def step_diagnostics(context):
    report = context.s["report"]
    flagged = report["dropped"] + report["approximated"]
    missing = [i["line"] for i in flagged if not i["detail"]]
    context.ev.measure("diagnostics_count", {"dropped": len(report["dropped"]), "approximated": len(report["approximated"])})
    context.ev.measure("diagnostics_without_explanation", missing)
    assert (context.ev.dir / "migration-report.txt").exists()
    assert not missing, f"report lines without an explanation: {missing}"


@then("the migrated application should pass the same external functional probes as the baseline")
def step_same_probes(context):
    results = {}
    for ip in node_ips(context):
        results[ip] = functional_probes(lambda m, p, b, ip=ip: http_request(ip, 80, p, K8S_HOST, method=m, body=b))
    base = context.s["baseline_probes"]
    context.ev.expect("probes", {k: True for k in base})
    context.ev.measure("migrated_probes", results)
    failing = {ip: [k for k, v in r.items() if not v["ok"]] for ip, r in results.items()}
    failing = {k: v for k, v in failing.items() if v}
    assert not failing, f"migrated app fails probes the baseline passed: {failing}"


@then("the evidence should record every manifest change and manual migration step")
def step_record_changes(context):
    limit = context.cfg.threshold("migration.max_manual_steps")
    changes = context.s["changes"]
    steps = context.record.manual_steps
    zero = context.record.measured.get("zero_touch_probes", {})
    context.ev.measure("manifest_changes", changes)
    context.ev.measure("manual_steps_total", len(steps))
    context.ev.measure("zero_touch_probes_passing", sum(1 for v in zero.values() if v["ok"]))
    context.ev.expect("manual_steps_total", f"<= {limit}")
    assert len(steps) <= limit, f"{len(steps)} manual steps > {limit}"


# --------------------------------------------------------------------------- INT-002

KINDS = ROOT / "fixtures" / "k8s" / "kinds"


@given('the single-kind Kubernetes manifest "{case}"')
def step_kind_manifest(context, case):
    path = KINDS / f"{case}.yaml"
    if not path.exists():
        raise Blocked(f"missing fixture {path}")
    context.s["kind_path"] = path
    context.s["kind_docs"] = [d for d in yaml.safe_load_all(path.read_text()) if d]
    context.ev.attach("input.yaml", path.read_text())


@when("I convert it with relish import")
def step_kind_convert(context):
    from harness import product
    relish = str(product.host_relish(context.cfg, context.log))
    r = context.log.run([relish, "import", "-f", str(context.s["kind_path"])], check=False)
    context.s["kind_toml"] = r.stdout
    context.s["kind_report"] = parse_report(r.stderr)
    context.ev.attach("converted.toml", r.stdout)
    context.ev.attach("report.txt", r.stderr)
    context.ev.measure("import_rc", r.rc)
    context.ev.measure("report", context.s["kind_report"])


@then('the importer outcome should be "{outcome}"')
def step_kind_outcome(context, outcome):
    case = context.s["kind_path"].stem
    target = [d for d in context.s["kind_docs"] if d["kind"] == case or case == "CustomResource"][-1]
    key = f"{target['kind']}/{target['metadata']['name']}"
    rep = context.s["kind_report"]
    where = sorted({s for s, items in rep.items() for i in items if f"{i['kind']}/{i['name']}" == key})
    toml = context.s["kind_toml"]
    if not where and toml.strip():
        where = ["converted"]  # merged into the workload's table without its own report line
    context.ev.expect("outcome", outcome)
    context.ev.measure("outcome_sections", where)
    mapping = {"converted": {"converted"}, "reported": {"approximated"}, "dropped": {"dropped"}}
    assert set(where) & mapping[outcome], f"{key}: expected {outcome}, report sections {where}"
    if outcome == "dropped":
        assert not toml.strip() or f"{target['metadata']['name']}" not in toml, f"{key} dropped yet present in TOML"


@then('the converted configuration should contain "{evidence}"')
def step_kind_evidence(context, evidence):
    if evidence == "-":
        context.ev.measure("toml_evidence", "not applicable")
        return
    found = evidence in context.s["kind_toml"]
    context.ev.expect("toml_contains", evidence)
    context.ev.measure("toml_contains", found)
    assert found, f"converted TOML lacks {evidence!r}"


@then("no resource in the manifest should be silently lost")
def step_kind_not_silent(context):
    rep, toml = context.s["kind_report"], context.s["kind_toml"]
    lost = []
    for d in context.s["kind_docs"]:
        key = f"{d['kind']}/{d['metadata']['name']}"
        reported = any(f"{i['kind']}/{i['name']}" == key for items in rep.values() for i in items)
        reflected = (d["kind"] in ("Service", "Ingress", "HorizontalPodAutoscaler") and "[app." in toml)
        if not reported and not reflected:
            lost.append(key)
    context.ev.measure("silently_lost", lost)
    assert not lost, f"neither reported nor converted: {lost}"


# --------------------------------------------------------------------------
# INT-003 unmodified upstream manifests
# --------------------------------------------------------------------------

UPSTREAM = ROOT / "fixtures" / "k8s" / "upstream"
SETS = {
    "podinfo 6.15.0 deploy/webapp": {
        "file": UPSTREAM / "podinfo-webapp-6.15.0.yaml", "namespace": None, "front": "frontend",
        "host": "webapp.rbeval.test", "expect": ['"version": "6.15.0"'], "echo": True},
    "Online Boutique v0.10.7": {
        "file": UPSTREAM / "online-boutique-v0.10.7.yaml", "namespace": "boutique", "front": "frontend",
        "host": "boutique.rbeval.test", "expect": ["Online Boutique", "Sunglasses"], "echo": False},
}


@given("the unmodified upstream manifests for {name}")
def step_upstream(context, name):
    spec = SETS.get(name)
    if not spec:
        raise Blocked(f"no upstream set {name!r}")
    docs = migration.load_docs(spec["file"])
    context.ev.attach("upstream.yaml", spec["file"].read_text())
    context.ev.measure("upstream_kinds", sorted({d["kind"] for d in docs}))
    context.ev.measure("upstream_documents", len(docs))
    path = spec["file"]
    if spec["namespace"]:
        docs = migration.with_namespace(docs, spec["namespace"])
        path = context.ev.dir / "upstream-namespaced.yaml"
        path.write_text(yaml.safe_dump_all(docs, sort_keys=False))
        context.ev.deviation(f"harness put every document in namespace {spec['namespace']} so the set cannot "
                             "replace the shared fixture's apps of the same name; relish apply has no --namespace flag")
        context.ev.measure("harness_isolation_edits", 1)
    for image in sorted({c["image"] for d in docs if d["kind"] == "Deployment"
                         for c in d["spec"]["template"]["spec"]["containers"]}):
        context.ev.external(f"container image: {image}")
    context.s.update(set=spec, upstream_docs=docs, upstream_path=path)


@when("I apply them with relish apply -f in their own namespace")
def step_upstream_apply(context):
    cl, ev = context.cluster, context.ev
    path = context.s["upstream_path"]
    dry = cl.relish.run("apply", "-f", str(path), "--dry-run", check=False, timeout=120)
    ev.attach("migration-report.txt", dry.stderr)
    context.s["report"] = parse_report(dry.stderr)
    imp = cl.relish.run("import", "-f", str(path), check=False, timeout=120)
    ev.attach("imported.toml", imp.stdout)
    context.s["imported"] = imp.stdout
    targets = migration.imported_apps(imp.stdout) if imp.ok else []
    context.s["targets"] = targets
    context.cleanups.append(lambda: [apps.delete_app(cl, n, ns) for n, ns in targets])
    r = cl.relish.run("apply", "-f", str(path), check=False, timeout=300)
    ev.attach("apply.txt", r.stdout + "\n--- stderr ---\n" + r.stderr)
    ev.measure("zero_touch_apply_rc", r.rc)
    ev.measure("imported_apps", [f"{ns}/{n}" for n, ns in targets])
    context.s["t_apply"] = time.monotonic()
    if not r.ok:
        raise AssertionError(f"relish apply -f refused the upstream manifests: {r.stderr.strip()[:400]}")


@then("the migration report should account for every upstream resource")
def step_upstream_accounting(context):
    report, imported = context.s["report"], context.s["imported"]
    accounted, silent = {}, []
    for d in context.s["upstream_docs"]:
        key = f"{d['kind']}/{d['metadata']['name']}"
        where = [s for s, items in report.items() for i in items if f"{i['kind']}/{i['name']}" == key]
        reflected = d["kind"] in ("Service", "Ingress", "HorizontalPodAutoscaler") and \
            f"[app.{d['metadata']['name']}" in imported
        if where:
            accounted[key] = where[0]
        elif reflected:
            accounted[key] = "merged into the app (no report line)"
        else:
            silent.append(key)
    counts = {}
    for v in accounted.values():
        counts[v] = counts.get(v, 0) + 1
    context.ev.attach("accounting.json", {"accounted": accounted, "silent": silent})
    context.ev.measure("report_counts", counts)
    context.ev.measure("silently_dropped", silent)
    context.ev.expect("silently_dropped", [])
    assert not silent, f"resources neither reported nor reflected in the import: {silent}"


def _workload_progress(context) -> dict:
    out = {}
    doc = tomllib.loads(context.s["imported"] or "")
    for name, ns in context.s["targets"]:
        want = doc["app"][name].get("replicas", 1)
        want = 1 if not isinstance(want, int) else want
        out[f"{ns}/{name}"] = {"want": want, "running": len(apps.running(context.cluster, name, ns))}
    return out


@then("every imported workload should be running within the configured time")
def step_upstream_running(context):
    limit = context.cfg.threshold("interop.realworld_ready_s")
    last = {}

    def all_running():
        last["p"] = _workload_progress(context)
        return all(v["running"] >= v["want"] for v in last["p"].values()) or None
    try:
        wait_until(all_running, limit, 5, "all imported workloads running")
        secs = round(time.monotonic() - context.s["t_apply"], 1)
    except TimeoutError:
        secs = None
    prog = last.get("p", {})
    ok = sum(v["running"] >= v["want"] for v in prog.values())
    context.ev.expect("workloads_running", f"{len(prog)} of {len(prog)} within {limit} s")
    context.ev.measure("workloads_running", f"{ok} of {len(prog)}")
    context.ev.measure("workload_progress", prog)
    context.ev.measure("all_running_s", secs)
    if secs is None:
        st = context.cluster.relish.run("status", check=False)
        context.ev.attach("status-not-running.txt", st.stdout + st.stderr)
        for key, v in prog.items():
            if v["running"] < v["want"]:
                ns, name = key.split("/", 1)
                lg = context.cluster.relish.run("logs", name, "--namespace", ns, "--tail", "40", check=False)
                context.ev.attach(f"logs/{ns}-{name}.txt", lg.stdout + lg.stderr)
    assert secs is not None, f"not running after {limit} s: " + \
        ", ".join(k for k, v in prog.items() if v["running"] < v["want"])


@when("I add an ingress for the front service as the only manual change")
def step_upstream_ingress(context):
    spec, cl = context.s["set"], context.cluster
    toml_text = migration.add_ingress(context.s["imported"], spec["front"], spec["host"])
    path = context.ev.dir / "with-ingress.toml"
    path.write_text(toml_text)
    context.ev.attach("manifest.diff", "".join(difflib.unified_diff(
        context.s["imported"].splitlines(True), toml_text.splitlines(True), "imported.toml", "with-ingress.toml")))
    context.ev.manual_step(f"relish import the manifests and add [app.{spec['front']}.ingress] host {spec['host']}")
    context.ev.manual_step("relish apply the edited TOML")
    cl.relish.run("apply", str(path), check=True, timeout=300, operator=f"add an ingress for {spec['front']}")
    context.s["t_ingress"] = time.monotonic()


@then("the front page should answer through that ingress within the configured time")
def step_upstream_front(context):
    spec = context.s["set"]
    limit = context.cfg.threshold("interop.realworld_ready_s")
    last = {}

    def answers():
        for ip in node_ips(context):
            r = http_request(ip, 80, "/", spec["host"], timeout=5)
            last[ip] = {"status": r.status, "head": r.body[:300]}
            if r.status != 200 or not all(e in r.body for e in spec["expect"]):
                return None
        return True
    try:
        wait_until(answers, limit, 3, "front page on every ingress")
        secs = round(time.monotonic() - context.s["t_ingress"], 1)
    except TimeoutError:
        secs = None
    context.ev.expect("front_page", f"200 containing {spec['expect']} on every ingress")
    context.ev.measure("front_page_s", secs)
    context.ev.measure("front_page_last", last)
    if spec["echo"] and secs is not None:
        token = apps.unique("upstream")
        r = http_request(node_ips(context)[0], 80, "/echo", spec["host"], method="POST", body=token.encode(), timeout=5)
        context.ev.measure("echo_through_backend", token in r.body)
        assert token in r.body, f"frontend could not reach its backend by name: {r.status} {r.body[:200]}"
    assert secs is not None, f"front page not served within {limit} s: {last}"


@then("the manual changes should not exceed the configured maximum")
def step_upstream_manual(context):
    limit = context.cfg.threshold("interop.realworld_max_manual_steps")
    n = len(context.record.manual_steps)
    context.ev.expect("manual_steps", f"<= {limit}")
    context.ev.measure("manual_steps", n)
    assert n <= limit, f"{n} manual steps > {limit}"


# --------------------------------------------------------------------------
# INT-004 export to Kubernetes
# --------------------------------------------------------------------------

EXPORT_NS = "rbeval-export"


@when("I export the native fixture with relish export")
def step_export(context):
    r = context.cluster.relish.run("export", "-f", str(ROOT / "fixtures" / "native" / "two-service.toml"),
                                   check=True)
    path = context.ev.dir / "exported.yaml"
    path.write_text(r.stdout)
    context.ev.attach("export-report.txt", r.stderr)
    context.ev.attach_path(path)
    context.s.update(export_path=path, export_report=r.stderr)
    context.ev.measure("exported_kinds", sorted({d["kind"] for d in migration.load_docs(path)}))


@then("the export report should name what it could not express")
def step_export_report(context):
    rep = context.s["export_report"]
    lines = [line.strip() for line in rep.splitlines() if line.strip().startswith(("!", "-", "~"))]
    context.ev.measure("export_report_lines", lines)
    context.ev.expect("export_report_names", "health checks and metrics, which the fixture uses")
    assert any("health" in line for line in lines) and any("metrics" in line for line in lines), \
        f"report does not mention the fixture's health checks and metrics: {lines}"


def _kind(context) -> KindBaseline:
    kind = KindBaseline(context.cfg, context.log)
    kind.ensure()
    return kind


@then("the exported YAML should pass a server-side dry run on the Kubernetes baseline")
def step_export_dry_run(context):
    kind = _kind(context)
    kind.kubectl("create", "namespace", EXPORT_NS, check=False)
    context.cleanups.append(lambda: kind.kubectl("delete", "namespace", EXPORT_NS, "--wait=false", check=False))
    r = kind.kubectl("apply", "--dry-run=server", "-n", EXPORT_NS, "-f", str(context.s["export_path"]), check=False)
    context.ev.attach("kubectl-dry-run.txt", r.stdout + r.stderr)
    context.ev.measure("server_dry_run_rc", r.rc)
    assert r.ok, f"kubectl apply --dry-run=server failed: {r.stderr[:400]}"


@then("the exported app should roll out on the Kubernetes baseline and answer by service name")
def step_export_rollout(context):
    kind = _kind(context)
    limit = context.cfg.threshold("interop.export_rollout_s")
    kind.kubectl("apply", "-n", EXPORT_NS, "-f", str(context.s["export_path"]), check=True)
    t = time.monotonic()
    for d in ("backend", "frontend"):
        kind.kubectl("rollout", "status", "-n", EXPORT_NS, f"deployment/{d}", f"--timeout={limit}s", check=True,
                     timeout=limit + 30)
    context.ev.measure("rollout_s", round(time.monotonic() - t, 1))
    port = _free_port()
    pf = subprocess.Popen(["kubectl", "--kubeconfig", str(kind.kubeconfig), "port-forward", "-n", EXPORT_NS,
                           "svc/frontend", f"{port}:9898"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_until(lambda: http_request("127.0.0.1", port, "/healthz").ok, 30, 1, "export port-forward")
        token = apps.unique("export")
        r = http_request("127.0.0.1", port, "/echo", method="POST", body=token.encode(), timeout=5)
    finally:
        pf.terminate()
    context.ev.measure("echo_through_backend", token in r.body)
    context.ev.classify("native")
    assert token in r.body, f"exported frontend could not reach the exported backend: {r.status} {r.body[:200]}"
