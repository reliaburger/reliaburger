"""Kubernetes migration helpers shared by the interoperability steps."""

from __future__ import annotations

import re
import tomllib

import yaml


def parse_report(stderr: str) -> dict:
    """Parse relish's migration report into converted / approximated / dropped entries."""
    sections = {"converted": [], "approximated": [], "dropped": [], "other": []}
    current = "other"
    for line in stderr.splitlines():
        low = line.strip().lower()
        if low.startswith("converted"):
            current = "converted"; continue
        if low.startswith("approximated"):
            current = "approximated"; continue
        if low.startswith("dropped") or low.startswith("not applied") or low.startswith("skipped"):
            current = "dropped"; continue
        m = re.match(r"\s*([+~\-x!])\s+([A-Za-z]+)/([\w.\-]+)\s*(?:[—→-]+\s*(.*))?$", line)
        if m:
            sections[current].append({"kind": m.group(2), "name": m.group(3), "detail": (m.group(4) or "").strip(),
                                       "marker": m.group(1), "line": line.strip()})
    return sections


def load_docs(path) -> list[dict]:
    return [d for d in yaml.safe_load_all(open(path)) if d]


def with_namespace(docs: list[dict], namespace: str) -> list[dict]:
    """Put every namespaced document in `namespace` and add the Namespace itself."""
    out = [{"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": namespace}}]
    for d in docs:
        if d["kind"] in ("Namespace", "ClusterRole", "ClusterRoleBinding", "CustomResourceDefinition"):
            continue
        d = {**d, "metadata": {**d.get("metadata", {}), "namespace": namespace}}
        out.append(d)
    return out


def imported_apps(toml_text: str) -> list[tuple[str, str]]:
    """(name, namespace) for every [app.*] in an imported TOML."""
    doc = tomllib.loads(toml_text)
    return [(name, spec.get("namespace", "default")) for name, spec in doc.get("app", {}).items()]


def add_ingress(toml_text: str, app: str, host: str) -> str:
    """Append an ingress block for `app` unless the import already produced one."""
    if re.search(rf"^\[app\.{re.escape(app)}\.ingress\]", toml_text, re.M):
        return toml_text
    return toml_text.rstrip() + f'\n\n[app.{app}.ingress]\nhost = "{host}"\npath = "/"\n'
