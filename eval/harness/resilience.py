"""Helpers for fault scenarios: cluster snapshots, node restore, write probes, partitions."""

from __future__ import annotations

import time

from . import apps
from .core import ROOT, CommandFailed, wait_until

WRITE_PROBE = ROOT / "fixtures" / "native" / "write-probe.toml"


def cluster_snapshot(context) -> dict:
    """What an operator sees from the workstation: gossip, council and frontend placement."""
    cl = context.cluster
    snap: dict = {}
    try:
        snap["nodes"] = {n["node_id"]: n["state"] for n in cl.relish.json("nodes", timeout=15, quiet=True)}
    except CommandFailed as exc:
        snap["nodes_error"] = str(exc)[:200]
    try:
        s = cl.relish.json("council", timeout=15, quiet=True)["summary"]
        snap["leaders"] = s.get("leaders")
        snap["voters_answering"] = s.get("voters_answering")
        snap["unanswered"] = s.get("unanswered")
    except (CommandFailed, KeyError) as exc:
        snap["council_error"] = str(exc)[:200]
    try:
        rows = apps.instances(cl, "frontend")
        snap["frontend"] = sorted(f"{r['node']}:{r['id']}:{r['state']}" for r in rows)
    except CommandFailed as exc:
        snap["status_error"] = str(exc)[:200]
    return snap


def restore_node(context, node) -> None:
    lab, cl = context.lab, context.cluster
    if lab.domain_state(node) != "running":
        lab.power_on(node)
    cl.down_nodes.discard(node.name)
    t0 = time.monotonic()
    try:
        cl.wait_healthy(300)
        context.ev.measure(f"{node.name}_rejoined_after_power_on_s", round(time.monotonic() - t0, 1))
    except TimeoutError as exc:
        context.ev.observe(f"{node.name} did not rejoin within 300 s of power-on: {exc}")
        context.ev.measure(f"{node.name}_rejoined_after_power_on_s", None)
        r = lab.ssh(node, "sudo journalctl -u reliaburger.service --no-pager -n 200", check=False, timeout=30)
        context.ev.attach(f"diagnostics/journal-{node.name}-after-power-on.txt", r.stdout + r.stderr)


def apply_write_probe(context, name: str, endpoint: str | None = None, timeout: float = 15):
    """Apply a one-replica app with a fresh name; returns the CmdResult."""
    path = apps.render(WRITE_PROBE, context.ev.dir / "write-probes" / f"{name}.toml", NAME=name)
    context.s.setdefault("write_probe_names", []).append(name)
    return context.cluster.relish.run("apply", str(path), endpoint=endpoint, check=False, timeout=timeout,
                                      failover=endpoint is None, quiet=True)


def delete_write_probes(context) -> None:
    for name in context.s.get("write_probe_names", []):
        apps.delete_app(context.cluster, name)


def writes_resume(context, t0: float, limit: float, interval: float = 1.0) -> dict:
    """Apply fresh probe apps until one commits, then wait for it to run. Times are from t0."""
    out: dict = {"attempts": []}
    deadline = t0 + limit + 60
    committed = None
    while time.monotonic() < deadline:
        name = apps.unique("wprobe")
        t = time.monotonic()
        r = apply_write_probe(context, name)
        out["attempts"].append({"t_rel_s": round(t - t0, 1), "rc": r.rc, "err": r.stderr.strip()[:160]})
        if r.ok:
            committed = name
            out["commit_s"] = round(time.monotonic() - t0, 1)
            break
        time.sleep(max(0.0, interval - (time.monotonic() - t)))
    if committed is None:
        out["commit_s"] = out["placed_s"] = None
        return out
    try:
        wait_until(lambda: apps.running(context.cluster, committed), max(5.0, deadline - time.monotonic()), 1,
                   f"{committed} running")
        out["placed_s"] = round(time.monotonic() - t0, 1)
    except TimeoutError:
        out["placed_s"] = None
    out["app"] = committed
    return out


PARTITION_TABLE = "rbeval_partition"


def partition(context, node, peers: list[str]) -> None:
    """Drop traffic between `node` and its peers at the node, leaving the operator path open."""
    ips = ", ".join(peers)
    rules = (f"table inet {PARTITION_TABLE} {{\n"
             f"  chain input {{ type filter hook input priority -300; policy accept; ip saddr {{ {ips} }} drop; }}\n"
             f"  chain output {{ type filter hook output priority -300; policy accept; ip daddr {{ {ips} }} drop; }}\n"
             "}\n")
    context.lab.ssh(node, f"sudo nft -f - <<'EOF'\n{rules}EOF",
                    operator=f"partition {node.name} from {ips} with an nftables table")


def heal(context, node) -> None:
    context.lab.ssh(node, f"sudo nft delete table inet {PARTITION_TABLE}", check=False,
                    operator=f"remove the partition on {node.name}")
