"""Axis 1: vision and architectural coherence."""

from __future__ import annotations

import time

from behave import given, then, when

from harness import apps, product
from harness.core import Blocked

from harness.cluster import PACKAGES
from harness.steplib import FIXTURE_HOST, NATIVE_FIXTURE, node_ips


def _running_services(context, node) -> list[str]:
    r = context.lab.ssh(node, "systemctl list-units --type=service --state=running --no-legend --plain | awk '{print $1}'",
                        check=True, quiet=True)
    return sorted(r.stdout.split())


def _installed_packages(context, node) -> set[str]:
    r = context.lab.ssh(node, "dpkg-query -W -f '${Package}\\n'", check=True, quiet=True)
    return set(r.stdout.split())


def _classify_services(context, node, added: list[str]) -> dict[str, str]:
    """Name where each service that appeared during provisioning came from."""
    documented = set(PACKAGES.split()) | {"buildah"}
    preinstalled = context.s.get("baseline_packages", {}).get(node.name, set())
    out = {}
    for unit in added:
        if unit == "reliaburger.service":
            out[unit] = "product"
            continue
        r = context.lab.ssh(node, f"dpkg-query -S $(systemctl show -p FragmentPath --value {unit}) 2>/dev/null "
                                  "| cut -d: -f1", check=False, quiet=True)
        pkg = r.stdout.strip().split(",")[0] or None
        if pkg in documented:
            out[unit] = f"documented package {pkg}"
        elif pkg in preinstalled:
            out[unit] = f"preinstalled OS package {pkg} (activated during provisioning)"
        else:
            out[unit] = f"unexpected (package {pkg})"
    return out


@given("a disposable lab with {count:d} clean supported Linux nodes")
def step_fresh_lab(context, count):
    if count != len(context.lab.nodes):
        raise Blocked(f"config/lab.yaml defines {len(context.lab.nodes)} nodes, scenario wants {count}")
    t0 = time.monotonic()
    timings = context.lab.up()
    context.cluster.forget()
    context.ev.measure("vm_provision_s", round(time.monotonic() - t0, 1))
    context.ev.measure("vm_ssh_ready_s", timings["ssh_ready_s"])
    os_info = {}
    for n in context.lab.nodes:
        r = context.lab.ssh(n, ". /etc/os-release; echo $PRETTY_NAME; uname -r; "
                               "stat -fc %T /sys/fs/cgroup; command -v bun relish runc || true", check=True)
        os_info[n.name] = r.stdout.strip().splitlines()
        if any("bun" in line or "relish" in line for line in os_info[n.name][3:]):
            raise Blocked(f"{n.name} is not clean: product binaries already present")
    context.ev.measure("guest_os", os_info)
    context.s["baseline_services"] = {n.name: _running_services(context, n) for n in context.lab.nodes}
    context.s["baseline_packages"] = {n.name: _installed_packages(context, n) for n in context.lab.nodes}


@given("a pinned Reliaburger build and its matching documentation")
def step_pinned_build(context):
    paths = product.fetch(context.cfg, context.log)
    p = context.cfg.product
    context.ev.measure("product", {"target": context.cfg.product_key, "version": p["version"],
                                   "commit": p["commit"], "sha256": p["sha256"], "docs_ref": p["docs_ref"]})
    doc = context.claims._read("docs/linux-servers.md")
    if not doc:
        raise Blocked("docs/linux-servers.md is not readable at the pinned docs ref")
    context.ev.attach("docs/linux-servers.md", doc)
    r = context.log.run([str(paths["relish-linux-x86_64"]), "--version"], check=True)
    if p["commit"][:7] not in r.stdout:
        raise Blocked(f"host relish reports {r.stdout.strip()}, expected commit {p['commit'][:7]}")


@when("I provision a {count:d}-node cluster using the documented procedure")
def step_provision(context, count):
    st = context.cluster.provision()
    t = st["timings"]
    context.ev.measure("bootstrap_timings_s", t)
    context.ev.attach("cluster_state.json", st)
    extra = {}
    for n in context.lab.nodes:
        before = set(context.s.get("baseline_services", {}).get(n.name, []))
        added = sorted(set(_running_services(context, n)) - before)
        extra[n.name] = _classify_services(context, n, added)
    origins = {u: o for v in extra.values() for u, o in v.items()}
    context.shared["bootstrap"] = {"timings": t, "service_origins": origins}
    context.ev.measure("services_added_per_node", extra)
    context.ev.measure("node_versions", context.cluster.node_versions())


@when("I deploy the fixture application with {count:d} communicating services")
def step_deploy_fixture(context, count):
    cl = context.cluster
    t0 = time.monotonic()
    cl.relish.run("apply", str(NATIVE_FIXTURE), timeout=180, operator="deploy fixture with relish apply")
    ready = context.cfg.threshold("bootstrap.app_ready_s")
    apps.wait_running(cl, "backend", 2, ready)
    apps.wait_running(cl, "frontend", 3, ready)
    apps.wait_ingress(node_ips(context), FIXTURE_HOST, "/healthz", ready)
    context.ev.measure("app_ready_s", round(time.monotonic() - t0, 1))
    context.ev.expect("app_ready_s", f"<= {ready}")
    context.ev.measure("placement", {"frontend": apps.placement(cl, "frontend"), "backend": apps.placement(cl, "backend")})
    context.ev.attach("fixture.toml", NATIVE_FIXTURE.read_text())


@then("all {count:d} nodes should report healthy within the configured bootstrap timeout")
def step_nodes_healthy(context, count):
    limit = context.cfg.threshold("bootstrap.cluster_formation_s")
    t = context.shared["bootstrap"]["timings"]
    health = context.cluster.health()
    context.ev.expect("formation_after_last_start_s", f"<= {limit}")
    context.ev.measure("health_after_bootstrap", health)
    alive = [k for k, v in health.get("nodes", {}).items() if v == "alive"]
    assert len(alive) == count, f"only {len(alive)} nodes alive: {health}"
    assert len(health["voters_answering"]) == count and len(health["leaders"]) == 1, health
    assert t["formation_after_last_start_s"] <= limit, f"formation took {t['formation_after_last_start_s']} s > {limit} s"


@then("the application should pass its external HTTP health probe")
def step_health_probe(context):
    results = {}
    for ip in node_ips(context):
        r = apps.ingress_get(ip, FIXTURE_HOST, "/healthz", timeout=context.cfg.threshold("http.probe_timeout_s"))
        results[ip] = {"status": r.status, "latency_ms": round(r.latency_ms, 1), "error": r.error}
    context.ev.expect("ingress /healthz on every node", 200)
    context.ev.measure("ingress_health", results)
    bad = {k: v for k, v in results.items() if v["status"] != 200}
    assert not bad, f"ingress health probe failed: {bad}"


@then("inter-service name resolution should pass its connectivity probe")
def step_name_resolution(context):
    cl = context.cluster
    resolve = cl.relish.run("resolve", "backend", check=False)
    context.ev.attach("resolve-backend.txt", resolve.stdout + resolve.stderr)
    trials = []
    for i in range(10):
        ip = node_ips(context)[i % 3]
        token = apps.unique("echo")
        r = apps.echo_through_frontend(ip, FIXTURE_HOST, token)
        trials.append({"via": ip, "status": r.status, "echoed": token in r.body, "body": r.body[:200]})
    ok = sum(t["echoed"] for t in trials)
    context.ev.expect("frontend -> http://backend:9898/echo by name", "10/10 echoed")
    context.ev.measure("echo_trials", trials)
    context.ev.measure("echo_ok", f"{ok}/10")
    assert resolve.ok, f"relish resolve backend failed: {resolve.stderr[-300:]}"
    assert ok == 10, f"only {ok}/10 requests reached the backend by name"
