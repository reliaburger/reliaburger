"""Lab and cluster preconditions shared by every axis."""

from __future__ import annotations


from behave import given, then

from harness.cluster import DEVIATIONS, PACKAGES
from harness.core import Blocked
from harness.steplib import NATIVE_FIXTURE, ensure_cluster

@given("a healthy disposable Reliaburger cluster")
def step_healthy_cluster(context):
    ensure_cluster(context)


@given("a healthy disposable 3-node Reliaburger cluster with sufficient spare capacity")
def step_healthy_cluster_capacity(context):
    ensure_cluster(context)
    free = {}
    for n in context.lab.nodes:
        r = context.lab.ssh(n, "free -m | awk '/Mem:/{print $7}'; nproc", check=True, quiet=True)
        avail, cpus = r.stdout.split()
        free[n.name] = {"available_mib": int(avail), "cpus": int(cpus)}
    context.ev.measure("node_capacity", free)
    # One node must be able to absorb a lost node's fixture replicas.
    low = [k for k, v in free.items() if v["available_mib"] < 1024]
    if low:
        raise Blocked(f"nodes without 1 GiB available memory: {low}")


@then("the evidence should record external platform dependencies and manual interventions")
def step_record_deps(context):
    ev = context.ev
    boot = context.shared.get("bootstrap", {})
    for pkg in PACKAGES.split():
        ev.external(f"distro package: {pkg}")
    ev.external("distro package: buildah (node-01 only, optional, for relish build)")
    ev.external(f"release download: {context.cfg.product['base_url']} (bun, relish, SHA256SUMS)")
    for line in NATIVE_FIXTURE.read_text().splitlines():
        if line.startswith("image = "):
            ev.external("container registry: " + line.split('"')[1].split("/")[0])
    for d in DEVIATIONS:
        ev.deviation(d)
    actions = [a for a in context.log.operator_actions if a.get("scenario") == context.record.id]
    ev.measure("operator_actions_count", len(actions))
    ev.measure("operator_actions", [a["description"] for a in actions])
    ev.attach("operator_actions.json", actions)
    origins = boot.get("service_origins")
    if origins is not None:
        ev.measure("services_added_by_origin", origins)
        unexpected = {u: o for u, o in origins.items() if o.startswith("unexpected")}
        assert "reliaburger.service" in origins, f"the product's own unit is not running: {origins}"
        assert not unexpected, f"services outside the product and its documented packages: {unexpected}"
    assert actions, "no operator actions were recorded for the documented procedure"
