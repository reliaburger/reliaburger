"""Add or remove a marked block in every node's node.toml, then restart nodes one at a time.

Scenarios that test a node.toml interface (alert destinations, GitOps) use this
so the change is recorded as operator actions and undone afterwards.
"""

from __future__ import annotations

import json
import re
import ssl
import urllib.error
import urllib.request

NODE_TOML = "/etc/reliaburger/node.toml"


def _strip(text: str, name: str) -> str:
    return re.sub(rf"\n?# rbeval:{re.escape(name)} begin\n.*?# rbeval:{re.escape(name)} end\n", "\n", text, flags=re.S)


def _restart_all(context, why: str) -> None:
    cl, lab = context.cluster, context.lab
    leader = (cl.health().get("leaders") or [None])[0]
    order = sorted(lab.nodes, key=lambda n: n.name == leader)
    for n in order:
        lab.ssh(n, "sudo systemctl restart reliaburger.service", operator=f"restart bun on {n.name} ({why})")
        cl.wait_healthy(240)


def set_block(context, name: str, block: str) -> bool:
    """Make every node.toml end with `block`; returns True if anything changed."""
    cl, lab = context.cluster, context.lab
    changed = False
    for n in lab.nodes:
        current = lab.ssh(n, f"sudo cat {NODE_TOML}", quiet=True).stdout
        wanted = _strip(current, name).rstrip() + f"\n\n# rbeval:{name} begin\n{block.strip()}\n# rbeval:{name} end\n"
        if wanted != current:
            cl.write_file(n, NODE_TOML, wanted, f"edit {NODE_TOML} on {n.name}: add {name}", mode="0644")
            changed = True
    if changed:
        _restart_all(context, f"pick up {name}")
    return changed


def clear_block(context, name: str) -> None:
    cl, lab = context.cluster, context.lab
    changed = False
    for n in lab.nodes:
        current = lab.ssh(n, f"sudo cat {NODE_TOML}", quiet=True).stdout
        wanted = _strip(current, name)
        if wanted != current:
            cl.write_file(n, NODE_TOML, wanted.rstrip() + "\n", f"edit {NODE_TOML} on {n.name}: remove {name}")
            changed = True
    if changed:
        _restart_all(context, f"drop {name}")


def api_request(context, node, method: str, path: str, body: bytes | None = None,
                headers: dict | None = None, timeout: float = 10) -> tuple[int | None, dict, str]:
    """Call a node's HTTPS API as the admin token, trusting the cluster root CA."""
    cl = context.cluster
    ctx = ssl.create_default_context(cafile=str(cl.ca_path))
    req = urllib.request.Request(f"https://{node.ip}:9117{path}", data=body, method=method,
                                 headers={"Authorization": f"Bearer {cl.token()}", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=timeout, context=ctx) as resp:
            return resp.status, dict(resp.headers), resp.read().decode(errors="replace")
    except urllib.error.HTTPError as exc:
        return exc.code, dict(exc.headers or {}), exc.read().decode(errors="replace")
    except (urllib.error.URLError, OSError) as exc:
        return None, {}, f"{type(exc).__name__}: {exc}"


def as_json(text: str):
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return None
