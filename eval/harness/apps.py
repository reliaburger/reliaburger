"""Workload helpers shared by step definitions."""

from __future__ import annotations

import uuid
from pathlib import Path

from .cluster import Cluster
from .core import ROOT, wait_until
from .probes import http_request

FIXTURES = ROOT / "fixtures"


def render(template: Path, out: Path, **subs: str) -> Path:
    text = template.read_text()
    for k, v in subs.items():
        text = text.replace("{{" + k + "}}", v)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(text)
    return out


def instances(cluster: Cluster, app: str | None = None, namespace: str = "default") -> list[dict]:
    rows = cluster.relish.json("status", timeout=60, quiet=True)
    return [r for r in rows if (app is None or r.get("app_name") == app) and r.get("namespace") == namespace]


def running(cluster: Cluster, app: str, namespace: str = "default", exclude_nodes: set[str] | None = None) -> list[dict]:
    exclude_nodes = exclude_nodes or set()
    return [r for r in instances(cluster, app, namespace)
            if r.get("state") == "running" and r.get("node") not in exclude_nodes]


def placement(cluster: Cluster, app: str, namespace: str = "default") -> dict[str, list[str]]:
    out: dict[str, list[str]] = {}
    for r in instances(cluster, app, namespace):
        out.setdefault(r["node"], []).append(f"{r['id']}:{r['state']}")
    return out


def wait_running(cluster: Cluster, app: str, count: int, timeout: float, namespace: str = "default",
                 exclude_nodes: set[str] | None = None) -> float:
    _, secs = wait_until(lambda: len(running(cluster, app, namespace, exclude_nodes)) >= count,
                         timeout, 2, f"{count} running {app}")
    return secs


def ingress_get(ip: str, host: str, path: str = "/", timeout: float = 2.0, headers: dict | None = None):
    return http_request(ip, 80, path, host, timeout=timeout, headers=headers)


def wait_ingress(ips: list[str], host: str, path: str, timeout: float, expect: str | None = None) -> float:
    def ok():
        for ip in ips:
            r = ingress_get(ip, host, path)
            if not r.ok or (expect and expect not in r.body):
                return False
        return True
    _, secs = wait_until(ok, timeout, 2, f"ingress {host}{path} on {ips}")
    return secs


def delete_app(cluster: Cluster, app: str, namespace: str | None = None) -> None:
    args = ["delete", app] + (["--namespace", namespace] if namespace else [])
    cluster.relish.run(*args, check=False, timeout=120)


def echo_through_frontend(ip: str, host: str, token: str, timeout: float = 3.0):
    """POST /echo to the frontend, which forwards to http://backend:9898/echo by name."""
    return http_request(ip, 80, "/echo", host, method="POST", body=token.encode(), timeout=timeout)


def unique(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex[:12]}"
