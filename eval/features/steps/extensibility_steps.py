"""Axis 2: completeness versus extensibility."""

from __future__ import annotations

import json
import re
import secrets
import shlex
import time

from behave import given, then, when

from harness import apps, product
from harness.core import ROOT, Blocked, Unsupported, wait_until
from harness.services import Collector
from harness.steplib import ensure_fixture, node_ips

OTEL_FIXTURE = ROOT / "fixtures" / "native" / "otel-fixture.toml"
OTEL_HOST = "otel.rbeval.test"
PODINFO = "ghcr.io/stefanprodan/podinfo@sha256:ec73780a8425f59ea49f5bc8cdff0d598805a224fbaa1f86c67a244f250fa9da"


def _collector(context) -> Collector:
    col = context.shared.get("collector")
    if col is None:
        col = Collector(context.cfg, context.log, context.run_dir / "services" / "collector")
        col.start()
        context.shared["collector"] = col
        context.shared.setdefault("services", []).append(col)
    return col


@given("a reachable external telemetry collector in the test lab")
def step_collector(context):
    col = _collector(context)
    ip = context.cfg.lab["network"]["gateway"]
    reach = {}
    for n in context.lab.nodes:
        r = context.lab.ssh(n, f"timeout 3 bash -c '</dev/tcp/{ip}/4317' && echo open", check=False)
        reach[n.name] = "open" in r.stdout
    context.ev.measure("collector_reachable_from_nodes", reach)
    context.ev.external(f"OpenTelemetry Collector {col.image} on {ip}:4317 (outside the cluster)")
    if not all(reach.values()):
        raise Blocked(f"collector not reachable from every node: {reach}")


@given("an instrumented fixture application")
def step_instrumented_fixture(context):
    """Prove the fixture emits OTLP on its own, outside Reliaburger, so a later
    absence of spans can only come from the platform path."""
    col = _collector(context)
    name = "rbeval-otel-selftest"
    log = context.log
    log.run(["docker", "rm", "-f", name], check=False, quiet=True)
    log.run(["docker", "run", "-d", "--name", name, "-p", "127.0.0.1:19899:9898",
             "-e", f"OTEL_EXPORTER_OTLP_ENDPOINT=http://{context.cfg.lab['network']['gateway']}:4317",
             "-e", "OTEL_EXPORTER_OTLP_INSECURE=true", PODINFO, "./podinfo", "--port=9898",
             "--otel-service-name=rbeval-otel-selftest"], check=True, timeout=300)
    context.cleanups.append(lambda: log.run(["docker", "rm", "-f", name], check=False))
    from harness.probes import http_request
    wait_until(lambda: http_request("127.0.0.1", 19899, "/readyz").ok, 60, 1, "selftest podinfo")
    tid = secrets.token_hex(16)
    http_request("127.0.0.1", 19899, "/api/info", headers={"traceparent": f"00-{tid}-{secrets.token_hex(8)}-01"})
    _, secs = wait_until(lambda: tid in col.trace_ids(), 45, 1, "self-test span at collector")
    context.ev.measure("fixture_selftest_span_s", round(secs, 1))
    context.ev.observe("fixture image exports OTLP spans when run directly in Docker against the same collector")


@when("I configure telemetry export using documented supported interfaces")
def step_configure_export(context):
    ev = context.ev
    paths = {}
    # 1. Platform-native export: bounded experiment against the shipped CLI.
    relish = str(product.host_relish(context.cfg, context.log))
    r = context.log.run([relish, "--help"], check=True)
    commands = re.findall(r"^\s{2}([a-z][a-z-]+)\s", r.stdout, re.M)
    hits = {}
    for c in commands:
        h = context.log.run([relish, c, "--help"], check=False, quiet=True)
        found = sorted(set(re.findall(r"(?i)\b(otlp|opentelemetry|otel|remote[_-]write|syslog|loki)\b", h.stdout)))
        if found:
            hits[c] = found
    paths["platform_native_otlp"] = {
        "classification": "unsupported",
        "evidence": "docs state no OTLP/remote_write (claim_no_otlp_opentelemetry_export); "
                    f"`relish <cmd> --help` across {len(commands)} commands mentions: {hits or 'nothing'}",
    }
    # 2. Application SDK export, which the whitepaper documents (whitepaper.md:991).
    col_ip = context.cfg.lab["network"]["gateway"]
    rendered = apps.render(OTEL_FIXTURE, context.ev.dir / "otel-fixture.toml", COLLECTOR=col_ip)
    context.cluster.relish.run("apply", str(rendered), timeout=180)
    context.cleanups.append(lambda: apps.delete_app(context.cluster, "otelfix"))
    apps.wait_running(context.cluster, "otelfix", 2, context.cfg.threshold("bootstrap.app_ready_s"))
    apps.wait_ingress(node_ips(context), OTEL_HOST, "/readyz", 120)
    ev.attach("otel-fixture.toml", rendered.read_text())
    ev.manual_step("set OTEL_EXPORTER_OTLP_ENDPOINT and OTEL_EXPORTER_OTLP_INSECURE in [app.otelfix.env]")
    paths["application_sdk_otlp"] = {"classification": "pending", "config": "two env vars in the app config"}
    context.s["paths"] = paths
    ev.measure("export_paths", paths)


@when("I generate {count:d} uniquely identifiable application requests")
def step_generate(context, count):
    ids, statuses = [], {}
    ips = node_ips(context)
    for i in range(count):
        tid = secrets.token_hex(16)
        r = apps.ingress_get(ips[i % len(ips)], OTEL_HOST, f"/api/info?rbeval={tid}",
                             headers={"traceparent": f"00-{tid}-{secrets.token_hex(8)}-01"})
        ids.append(tid)
        statuses[str(r.status)] = statuses.get(str(r.status), 0) + 1
    context.s["trace_ids"] = ids
    context.s["sent_mono"] = time.monotonic()
    context.ev.measure("requests_sent", count)
    context.ev.measure("request_statuses", statuses)
    context.ev.attach("trace_ids.txt", "\n".join(ids))


@then("the external collector should receive telemetry attributable to the fixture")
def step_collector_received(context):
    col: Collector = context.shared["collector"]
    ids = set(context.s["trace_ids"])
    need = context.cfg.threshold("observability.telemetry_min_attributed_ratio")
    deadline = context.cfg.threshold("observability.webhook_delivery_s")
    try:
        _, secs = wait_until(lambda: len(ids & col.trace_ids()) / len(ids) >= need, deadline, 2,
                             "spans for the generated requests")
    except TimeoutError:
        secs = None
    got = ids & col.trace_ids()
    ratio = round(len(got) / len(ids), 3)
    services = sorted(col.services())
    context.ev.expect("attributed_ratio", f">= {need} within {deadline} s")
    context.ev.measure("attributed_ratio", ratio)
    context.ev.measure("attributed_count", len(got))
    context.ev.measure("first_full_attribution_s", None if secs is None else round(secs, 1))
    context.ev.measure("collector_service_names", services)
    context.ev.attach_path(col.out / "traces.jsonl") if (col.out / "traces.jsonl").exists() else None
    ok = ratio >= need and "rbeval-otel-fixture" in services
    context.s["paths"]["application_sdk_otlp"]["classification"] = "configured" if ok else "unsupported"
    context.ev.measure("export_paths", context.s["paths"])
    assert ok, f"collector attributed {len(got)}/{len(ids)} requests (services seen: {services})"


@then("the evidence should classify the integration as native, configured, workaround, source_change, or unsupported")
def step_classify(context):
    paths = context.s["paths"]
    order = ["native", "configured", "workaround", "source_change", "unsupported"]
    best = min((p["classification"] for p in paths.values() if p["classification"] in order), key=order.index)
    context.ev.classify(best)
    context.ev.measure("integration_classification", best)
    context.ev.observe(f"best available path is '{best}'; platform-native OTLP export is "
                       f"{paths['platform_native_otlp']['classification']}")
    if best == "unsupported":
        raise Unsupported("no documented interface delivered telemetry to the external collector")


@then("the evidence should record all required product source modifications")
def step_source_mods(context):
    # The node binaries must still be the pinned release: any source change
    # would show up as a different digest.
    pin = context.cfg.product["sha256"]["bun-linux-x86_64"]
    digests = {}
    for n in context.lab.nodes:
        r = context.lab.ssh(n, 'sha256sum "$(readlink -f /usr/local/bin/bun)" | cut -d" " -f1', check=True, quiet=True)
        digests[n.name] = r.stdout.strip()
    mods = [] if all(d == pin for d in digests.values()) else [f"bun digest differs from release: {digests}"]
    context.ev.measure("node_bun_sha256", digests)
    context.ev.measure("product_source_modifications", mods)
    context.ev.expect("product_source_modifications", "recorded; none for a configured integration")
    assert not mods, mods[0]


@given("the native fixture application is running")
def step_native_fixture(context):
    ensure_fixture(context, "frontend", 3)


@when("I run relish {command} with --output json from the workstation")
def step_relish_json(context, command):
    args = shlex.split(command)
    r = context.cluster.relish.run(*args, output="json", check=False, timeout=120)
    context.s["json_cmd"] = r
    context.ev.attach("stdout.txt", r.stdout)
    context.ev.attach("stderr.txt", r.stderr)
    context.ev.measure("exit_code", r.rc)
    context.ev.measure("stdout_bytes", len(r.stdout))
    context.ev.observe(f"command: relish --output json {command}")


@then("the command should exit 0")
def step_exit_zero(context):
    r = context.s["json_cmd"]
    context.ev.expect("exit_code", 0)
    assert r.rc == 0, f"exit {r.rc}: {r.stderr[:300]}"


@then("its standard output should parse as JSON")
def step_parses_json(context):
    r = context.s["json_cmd"]
    context.ev.expect("stdout_is_json", True)
    try:
        doc = json.loads(r.stdout)
    except json.JSONDecodeError as exc:
        context.ev.measure("stdout_is_json", False)
        context.ev.measure("stdout_head", r.stdout[:200])
        raise AssertionError(f"stdout is not JSON ({exc}); it starts {r.stdout[:120]!r}") from None
    context.ev.measure("stdout_is_json", True)
    context.ev.measure("json_top_level", type(doc).__name__ + (f" keys {sorted(doc)[:12]}" if isinstance(doc, dict) else
                                                               f" of {len(doc)}" if isinstance(doc, list) else ""))
