"""Offline tests for the harness's own logic: nothing here touches the lab."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from harness import migration  # noqa: E402
from harness.claims import Claims  # noqa: E402
from harness.core import Blocked, CommandFailed, CmdResult, ProductFailure, Redactor, Unsupported  # noqa: E402
from harness.probes import Prober  # noqa: E402


def _environment():
    spec = importlib.util.spec_from_file_location("rbeval_environment", ROOT / "features" / "environment.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


# -- redaction ---------------------------------------------------------------


def test_redactor_hides_token_patterns_and_registered_literals():
    r = Redactor([r"rbrg_[A-Za-z0-9_\-]{16,}"])
    r.add("hunter2-secret")
    out = r("token rbrg_abcdefghijklmnop1234 and hunter2-secret")
    assert "rbrg_" not in out and "hunter2" not in out
    assert out.count("«redacted»") == 2


def test_redactor_ignores_literals_too_short_to_be_secrets():
    r = Redactor([])
    r.add("abc")
    assert r("abc") == "abc"


# -- migration report --------------------------------------------------------

REPORT = """Converted:
  + Namespace/rbeval-shop
  + Deployment/backend → [app.backend]

Approximated (review recommended):
  ~ Deployment/frontend — env FIXTURE_TOKEN uses valueFrom
  ~ ConfigMap/fixture-config — not referenced by any workload; import manually if needed

Dropped (no Reliaburger equivalent):
  - NetworkPolicy/deny-all — no equivalent
"""


def test_parse_report_sorts_lines_into_sections():
    rep = migration.parse_report(REPORT)
    assert [f"{i['kind']}/{i['name']}" for i in rep["converted"]] == ["Namespace/rbeval-shop", "Deployment/backend"]
    assert [i["kind"] for i in rep["approximated"]] == ["Deployment", "ConfigMap"]
    assert rep["dropped"][0]["name"] == "deny-all"


def test_with_namespace_moves_namespaced_kinds_and_adds_the_namespace():
    docs = [{"kind": "Deployment", "metadata": {"name": "a"}}, {"kind": "ClusterRole", "metadata": {"name": "r"}}]
    out = migration.with_namespace(docs, "shop")
    assert out[0] == {"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "shop"}}
    assert out[1]["metadata"] == {"name": "a", "namespace": "shop"}
    assert all(d["kind"] != "ClusterRole" for d in out)
    assert docs[0]["metadata"] == {"name": "a"}, "input must not be mutated"


def test_add_ingress_is_idempotent_and_keeps_the_app_parseable():
    toml_text = '[app.frontend]\nnamespace = "b"\nreplicas = 1\n'
    once = migration.add_ingress(toml_text, "frontend", "b.test")
    assert migration.add_ingress(once, "frontend", "b.test") == once
    assert migration.imported_apps(once) == [("frontend", "b")]


# -- probes ------------------------------------------------------------------


def test_summarise_reports_ratio_and_longest_failure_streak():
    s = [{"ok": True, "latency_ms": 5, "error": None, "status": 200}] * 3 + \
        [{"ok": False, "latency_ms": None, "error": "timeout", "status": None}] * 2 + \
        [{"ok": True, "latency_ms": 7, "error": None, "status": 200}]
    out = Prober.summarise(s)
    assert out["ratio"] == round(4 / 6, 4)
    assert out["longest_failure_streak"] == 2
    assert out["errors"] == ["timeout"]


def test_summarise_of_nothing_has_no_ratio():
    assert Prober.summarise([])["ratio"] is None


# -- citations ---------------------------------------------------------------


def _claims_reading(text: str) -> Claims:
    c = Claims.__new__(Claims)
    c._read = lambda path: text
    return c


def test_citation_found_near_the_cited_lines():
    c = _claims_reading("intro\nLose a node and the scheduler\nmoves its workloads.\n")
    chk = c._verify_one("x", {"ref": "README.md:2-3", "quote": "Lose a node and the scheduler moves its workloads"})
    assert chk.ok, chk.detail


def test_citation_with_an_ellipsis_checks_each_segment():
    c = _claims_reading("alpha beta gamma\ndelta epsilon\n")
    assert c._verify_one("x", {"ref": "a.md:1-2", "quote": "alpha beta ... delta epsilon"}).ok
    assert not c._verify_one("x", {"ref": "a.md:1-2", "quote": "alpha beta ... zeta eta"}).ok


def test_citation_far_from_the_cited_lines_fails():
    c = _claims_reading("\n".join(["filler"] * 20 + ["the quoted sentence lives here"]))
    assert not c._verify_one("x", {"ref": "a.md:2-2", "quote": "the quoted sentence lives here"}).ok


# -- status mapping ----------------------------------------------------------


def _scenario(status, step_type="then", exc=None, skip_reason=""):
    step = SimpleNamespace(status=SimpleNamespace(name="failed" if exc else "passed"), exception=exc,
                           keyword=step_type.capitalize(), name="something", step_type=step_type, error_message="tb")
    return SimpleNamespace(status=SimpleNamespace(name=status), steps=[step], skip_reason=skip_reason)


def _context(evidence=True):
    rec = SimpleNamespace(status=None, status_reason="")
    ev = SimpleNamespace(has_direct_evidence=lambda: evidence, attach=lambda *a, **k: None)
    return SimpleNamespace(record=rec, ev=ev, log=SimpleNamespace(redact=lambda s: s))


@pytest.mark.parametrize("exc,step_type,want", [
    (AssertionError("x"), "then", "FAIL"),
    (ProductFailure("x"), "when", "FAIL"),
    (TimeoutError("x"), "then", "FAIL"),
    (CommandFailed(CmdResult(argv=["relish"], rc=1, stdout="", stderr="no", start="", duration_ms=1), "boom"),
     "when", "FAIL"),
    (Unsupported("no such thing"), "then", "UNSUPPORTED"),
    (Blocked("lab down"), "then", "BLOCKED"),
    (AssertionError("x"), "given", "BLOCKED"),
    (KeyError("bug"), "then", "BLOCKED"),
])
def test_failures_map_to_harness_statuses(exc, step_type, want):
    env = _environment()
    status, _ = env._classify(_context(), _scenario("failed", step_type, exc))
    assert status == want


def test_a_pass_without_evidence_is_blocked():
    env = _environment()
    assert env._classify(_context(evidence=False), _scenario("passed"))[0] == "BLOCKED"
    assert env._classify(_context(evidence=True), _scenario("passed"))[0] == "PASS"


@pytest.mark.parametrize("reason,want", [("UNSUPPORTED: no api", "UNSUPPORTED"), ("BLOCKED: no claim", "BLOCKED"),
                                         ("filtered out", "NOT_RUN")])
def test_skips_map_by_their_reason_prefix(reason, want):
    env = _environment()
    assert env._classify(_context(), _scenario("skipped", skip_reason=reason))[0] == want


def test_every_node_admits_the_operator_to_its_api():
    from harness.cluster import node_toml
    cfg = SimpleNamespace(lab={"cluster_name": "rbeval", "network": {"gateway": "10.77.0.1"}})
    nodes = [SimpleNamespace(name=f"node-0{i}", ip=f"10.77.0.1{i}") for i in (1, 2, 3)]
    for node in nodes:
        assert 'operator_cidrs = ["10.77.0.1/32"]' in node_toml(cfg, node, nodes[0], nodes)
