"""Helpers shared by step definitions (behave step files should not import each other)."""

from __future__ import annotations

from . import apps
from .core import ROOT

NATIVE_FIXTURE = ROOT / "fixtures" / "native" / "two-service.toml"
FIXTURE_HOST = "fixture.rbeval.test"


def node_ips(context, exclude: set[str] | None = None) -> list[str]:
    exclude = exclude or set()
    return [n.ip for n in context.lab.nodes if n.name not in exclude]


def ensure_cluster(context) -> dict:
    """Reuse a healthy cluster for this product build, or build one with the documented procedure."""
    cl = context.cluster
    context.lab.touched = True
    if cl.is_reusable():
        context.ev.observe("reused the healthy lab cluster built earlier in this or a previous run")
        st = cl.state()
    else:
        context.ev.observe("no reusable cluster: building fresh VMs and following docs/linux-servers.md")
        context.lab.up()
        st = cl.provision()
    health, _ = cl.wait_healthy(120)
    context.ev.measure("cluster_health_at_start", health)
    context.ev.attach("cluster_state.json", st)
    return st


def ensure_fixture(context, app: str = "frontend", replicas: int = 3) -> None:
    cl = context.cluster
    if len(apps.running(cl, app)) < replicas or len(apps.running(cl, "backend")) < 2:
        cl.relish.run("apply", str(NATIVE_FIXTURE), timeout=180)
        apps.wait_running(cl, "backend", 2, context.cfg.threshold("bootstrap.app_ready_s"))
        apps.wait_running(cl, app, replicas, context.cfg.threshold("bootstrap.app_ready_s"))
    apps.wait_ingress(node_ips(context), FIXTURE_HOST, "/healthz", 120)
