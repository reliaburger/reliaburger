"""Axis 4, security boundaries: token roles and scopes, the network perimeter, chaos safety rails."""

from __future__ import annotations

import ipaddress
import re
import shlex
import time

from behave import given, then, when

from harness import apps
from harness.core import ROOT, Blocked, wait_until
from harness.probes import tcp_probe
from harness.steplib import ensure_fixture

NATIVE = ROOT / "fixtures" / "native"
RBAC_NS = "rbeval-rbac"
REFUSAL = re.compile(r"\b(401|403)\b|forbidden|unauthori[sz]ed|not (allowed|permitted)|refus|scope|permission", re.I)


def make_token(context, role: str, namespaces: str | None = None) -> str:
    """Create a token with the admin token, register it for redaction, revoke it after the scenario."""
    cl = context.cluster
    name = apps.unique(f"rbeval-{role}")
    args = ["token", "create", "--name", name, "--role", role]
    if namespaces:
        args += ["--namespaces", namespaces]
    r = cl.relish.run(*args, check=True, operator=f"relish token create --role {role}"
                      + (f" --namespaces {namespaces}" if namespaces else ""))
    secret = next((w for w in r.stdout.split() if w.startswith("rbrg_")), None)
    if not secret:
        raise Blocked(f"relish token create printed no rbrg_ token: {r.stdout[:200]!r}")
    context.log.redact.add(secret)
    context.cleanups.append(lambda: cl.relish.run("token", "revoke", name, check=False))
    context.s.setdefault("tokens", {})[role] = name
    context.ev.measure("token", {"name": name, "role": role, "namespaces": namespaces})
    return secret


def as_token(context, secret: str | None, *args: str, **kw):
    prefix = ["--token", secret] if secret else []
    return context.cluster.relish.run(*prefix, *args, check=False, **kw)


def audit_text(context, action: str) -> str:
    r = context.cluster.relish.run("audit", "--cluster", "--action", action, "--since", "15m", check=False, timeout=60)
    return r.stdout + r.stderr


def audit_containing(context, action: str, needle: str) -> str | None:
    text = audit_text(context, action)
    return text if needle in text else None


# --------------------------------------------------------------------------
# OPS-003 token roles and scopes
# --------------------------------------------------------------------------


@given("the namespace rbeval-rbac exists")
def step_rbac_namespace(context):
    context.cluster.relish.run("apply", str(NATIVE / "rbac-namespace.toml"), check=True)


@given("a {role} API token scoped to {scope}")
def step_scoped_token(context, role, scope):
    context.s["token_secret"] = make_token(context, role, None if scope == "all" else scope)
    context.s["role"] = role


ACTIONS = {
    "read cluster status": lambda: ["status"],
    "apply an app in rbeval-rbac": lambda: ["apply", str(NATIVE / "rbac-app.toml")],
    "apply an app in default": lambda: ["apply", str(NATIVE / "rbac-app-default.toml")],
    "inspect an app in default": lambda: ["inspect", "frontend"],
    "create an API token": lambda: ["token", "create", "--name", apps.unique("rbeval-escalated"), "--role", "read-only"],
    "apply a namespace block": lambda: ["apply", str(NATIVE / "rbac-namespace.toml")],
}


@when("the token is used to {action}")
def step_use_token(context, action):
    cl = context.cluster
    if action not in ACTIONS:
        raise Blocked(f"no mapping for action {action!r}")
    if action == "inspect an app in default":
        ensure_fixture(context, "frontend", 3)
    args = ACTIONS[action]()
    context.cleanups.append(lambda: (apps.delete_app(cl, "rbacprobe", RBAC_NS), apps.delete_app(cl, "rbacprobe-default")))
    if args[:2] == ["token", "create"]:
        context.cleanups.append(lambda: cl.relish.run("token", "revoke", args[3], check=False))
    context.s["t_action"] = time.monotonic()
    r = as_token(context, context.s["token_secret"], *args, timeout=120)
    secret = next((w for w in r.stdout.split() if w.startswith("rbrg_")), None)
    if secret:
        context.log.redact.add(secret)
    context.s["result"] = r
    context.ev.measure("command", f"relish {shlex.join(args)}")
    context.ev.measure("exit_code", r.rc)
    context.ev.attach("stdout.txt", r.stdout)
    context.ev.attach("stderr.txt", r.stderr)


@then("the API should {verdict} the request")
def step_api_verdict(context, verdict):
    r = context.s["result"]
    refused = r.rc != 0
    reason = (REFUSAL.search(r.stderr) or REFUSAL.search(r.stdout))
    context.ev.expect("verdict", verdict)
    context.ev.measure("verdict", "refuse" if refused else "allow")
    context.ev.measure("refusal_text", r.stderr.strip()[:300] if refused else None)
    if verdict == "allow":
        assert not refused, f"refused with exit {r.rc}: {r.stderr[:300]}"
    else:
        assert refused, f"allowed (exit 0): {r.stdout[:300]}"
        assert reason, f"failed, but not with an authorisation refusal: {r.stderr[:300]}"


@then("a refused request should appear in the audit log as api.refused")
def step_audit_refused(context):
    name = context.s["tokens"][context.s["role"]]
    refused = context.s["result"].rc != 0
    limit = context.cfg.threshold("chaos.audit_visible_s")
    text = ""
    if refused:
        try:
            text, secs = wait_until(lambda: audit_containing(context, "api.refused", name),
                                    limit, 3, "api.refused audit record")
            context.ev.measure("audit_visible_s", round(secs, 1))
        except TimeoutError:
            text = audit_text(context, "api.refused")
            context.ev.measure("audit_visible_s", None)
    else:
        text = audit_text(context, "api.refused")
    context.ev.attach("audit-api-refused.txt", text)
    hits = [line for line in text.splitlines() if name in line]
    context.ev.measure("audit_lines_for_token", len(hits))
    if refused:
        context.ev.expect("audit_visible_s", f"<= {limit}")
        assert hits, f"no api.refused audit record names token {name} within {limit} s"


# --------------------------------------------------------------------------
# OPS-004 network perimeter
# --------------------------------------------------------------------------


@given("a probe source address for {source}")
def step_probe_source(context, source):
    if source == "the operator":
        context.s["source"] = None
        context.ev.measure("source_address", context.cfg.lab["network"]["gateway"])
        return
    addr = context.cfg.safety["host"]["perimeter_probe_address"]
    net = ipaddress.ip_network(context.cfg.lab["network"]["cidr"])
    if ipaddress.ip_address(addr) not in net:
        raise Blocked(f"perimeter probe address {addr} is outside the lab network {net}")
    bridge = context.cfg.lab["network"]["bridge"]
    have = context.log.run(["ip", "-br", "addr", "show", bridge], check=True, quiet=True).stdout
    if f"{addr}/" not in have:
        context.log.run(["sudo", "-n", "ip", "addr", "add", f"{addr}/32", "dev", bridge], check=True)
        context.cleanups.append(lambda: context.log.run(["sudo", "-n", "ip", "addr", "del", f"{addr}/32", "dev", bridge],
                                                        check=False))
    context.s["source"] = addr
    context.ev.measure("source_address", addr)


def _ports(context, port_text: str) -> dict[str, int]:
    """Which port to probe on which node."""
    m = re.search(r"\b(\d{2,5})\b", port_text)
    if m:
        return {n.name: int(m.group(1)) for n in context.lab.nodes}
    if "host port" in port_text:
        rows = [r for r in apps.instances(context.cluster, "backend") if r.get("state") == "running"]
        ports = {r["node"]: int(r["host_port"]) for r in rows if r.get("host_port")}
        if not ports:
            raise Blocked(f"relish status shows no host port for a running backend: {rows}")
        return ports
    raise Blocked(f"cannot map {port_text!r} to a port")


@when("I open a TCP connection to {port_text} on every node from that address")
def step_tcp_probe(context, port_text):
    timeout = context.cfg.threshold("security.perimeter_probe_timeout_s")
    targets = _ports(context, port_text)
    out = {}
    for n in context.lab.nodes:
        if n.name not in targets:
            continue
        out[n.name] = {"port": targets[n.name], "result": tcp_probe(n.ip, targets[n.name], timeout, context.s["source"])}
    context.s["probe"] = out
    context.ev.measure("probe", out)


@then("every node should answer {expected}")
def step_probe_expect(context, expected):
    out = context.s["probe"]
    context.ev.expect("result_on_every_node", expected)
    wrong = {k: v for k, v in out.items() if v["result"] != expected}
    assert out and not wrong, f"expected {expected}: {wrong}"


# --------------------------------------------------------------------------
# OPS-005 chaos safety rails
# --------------------------------------------------------------------------


@when('I request the fault "{attempt}" with a {role} token')
def step_request_fault(context, attempt, role):
    cl = context.cluster
    leader = (cl.health().get("leaders") or [None])[0]
    args = shlex.split(attempt.replace("LEADER", leader or "node-01"))
    secret = None if role == "admin" else make_token(context, role)
    context.cleanups.append(lambda: cl.relish.run("fault", "clear", "--acknowledge", check=False))
    if args[0] == "pause":
        context.cleanups.append(lambda: cl.relish.run("fault", "resume", args[1], "--acknowledge", check=False))
    context.s["placement_before"] = apps.placement(cl, "frontend")
    r = as_token(context, secret, "fault", *args, "--reason", f"rbeval {context.record.id}", timeout=120)
    context.s["result"] = r
    context.ev.measure("command", "relish fault " + shlex.join(args))
    context.ev.measure("leader", leader)
    context.ev.measure("exit_code", r.rc)
    context.ev.attach("stdout.txt", r.stdout)
    context.ev.attach("stderr.txt", r.stderr)
    lst = cl.relish.run("fault", "list", check=False)
    context.ev.attach("fault-list-after.txt", lst.stdout + lst.stderr)


@then("the fault injector should {verdict} it")
def step_fault_verdict(context, verdict):
    r = context.s["result"]
    accepted = r.rc == 0
    context.ev.expect("verdict", verdict)
    context.ev.measure("verdict", "accept" if accepted else "refuse")
    context.ev.measure("refusal_text", None if accepted else r.stderr.strip()[:400])
    if verdict == "accept":
        assert accepted, f"refused with exit {r.rc}: {r.stderr[:300]}"
    else:
        assert not accepted, f"accepted (exit 0): {r.stdout[:300]}"


@then("the fixture should be back to 3 running frontend replicas within the configured recovery time")
def step_fault_recovered(context):
    limit = context.cfg.threshold("chaos.kill_recovery_s")
    time.sleep(3)
    try:
        secs = apps.wait_running(context.cluster, "frontend", 3, limit)
    except TimeoutError:
        secs = None
    context.ev.expect("frontend_running_again_s", f"<= {limit}")
    context.ev.measure("frontend_running_again_s", round(secs, 1) if secs is not None else None)
    context.ev.measure("placement_after", apps.placement(context.cluster, "frontend"))
    assert secs is not None, f"frontend not back to 3 running within {limit} s"


@then("an accepted fault should appear in the audit log")
def step_fault_audit(context):
    accepted = context.s["result"].rc == 0
    limit = context.cfg.threshold("chaos.audit_visible_s")
    text = audit_text(context, "fault.*")
    if accepted:
        try:
            text, _ = wait_until(lambda: audit_containing(context, "fault.*", "fault.injected"),
                                 limit, 3, "fault.injected audit record")
        except TimeoutError:
            pass
    context.ev.attach("audit-fault.txt", text)
    context.ev.measure("fault_injected_lines", sum("fault.injected" in line for line in text.splitlines()))
    if accepted:
        assert "fault.injected" in text, f"no fault.injected audit record within {limit} s"
