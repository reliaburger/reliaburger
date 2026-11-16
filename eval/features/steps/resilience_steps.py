"""Axis 4, resilience: leader loss, partition, power cuts, whole-cluster power loss, rolling deploys."""

from __future__ import annotations

import json
import re
import threading
import time

from behave import then, when

from harness import apps
from harness.core import Blocked, CommandFailed, wait_until
from harness.probes import Prober
from harness.resilience import (apply_write_probe, delete_write_probes, heal, partition, restore_node,
                                writes_resume)
from harness.steplib import FIXTURE_HOST, NATIVE_FIXTURE, node_ips

CORRUPTION = re.compile(r"corrupt|panick|panic:|refusing to start|failed to open .*(store|database|redb)|"
                        r"invalid (database|file)|checksum mismatch", re.I)


def _leader(context) -> str | None:
    return (context.cluster.health().get("leaders") or [None])[0]


def _survivor_endpoint(context, exclude: set[str]) -> str:
    n = next(n for n in context.lab.nodes if n.name not in exclude)
    return f"https://{n.ip}:9117"


def _message_everywhere(context, want: str, per_node: int = 3) -> bool:
    for ip in node_ips(context):
        for _ in range(per_node):
            r = apps.ingress_get(ip, FIXTURE_HOST, "/api/info")
            try:
                if json.loads(r.body).get("message") != want:
                    return False
            except (json.JSONDecodeError, AttributeError):
                return False
    return True


def _availability(context, t0: float, exclude: set[str] | None = None) -> dict:
    prober = context.s["prober"]
    exclude = exclude or set()
    samples = []
    for n in context.lab.nodes:
        if n.name not in exclude:
            samples.extend(prober.window(t0, time.monotonic(), n.name))
    return Prober.summarise(samples)


# --------------------------------------------------------------------------
# OPS-006 leader loss
# --------------------------------------------------------------------------


@when("I cut the power to the council leader")
def step_cut_leader(context):
    cl, lab = context.cluster, context.lab
    leader = _leader(context)
    if not leader:
        raise Blocked("no council leader before the fault")
    node = lab.node(leader)
    context.s.update(victim=node, old_leader=leader)
    cl.down_nodes.add(node.name)
    context.cleanups.append(lambda: delete_write_probes(context))
    context.cleanups.append(lambda: restore_node(context, node))
    context.s["t_kill"] = time.monotonic()
    r = lab.power_off(node)
    context.ev.measure("power_off", {"node": node.name, "rc": r.rc, "ts": r.start})


@then("a new leader should be elected within the configured election target")
def step_new_leader(context):
    limit = context.cfg.threshold("leader_loss.election_s")
    old, t0 = context.s["old_leader"], context.s["t_kill"]
    ep = _survivor_endpoint(context, {old})
    seen = []

    def elected():
        try:
            s = context.cluster.relish.json("council", endpoint=ep, failover=False, timeout=5, quiet=True)["summary"]
        except (CommandFailed, KeyError) as exc:
            seen.append({"t_rel_s": round(time.monotonic() - t0, 1), "error": str(exc)[:120]})
            return None
        leaders = s.get("leaders") or []
        seen.append({"t_rel_s": round(time.monotonic() - t0, 1), "leaders": leaders})
        return leaders[0] if len(leaders) == 1 and leaders[0] != old else None
    try:
        new, _ = wait_until(elected, limit + 60, 0.5, "new council leader")
        secs = round(time.monotonic() - t0, 1)
    except TimeoutError:
        new, secs = None, None
    context.ev.attach("council-samples.json", seen)
    context.ev.expect("election_s", f"<= {limit}")
    context.ev.measure("election_s", secs)
    context.ev.measure("new_leader", new)
    context.ev.limitation("election time is sampled every 0.5 s from the workstation and includes CLI round trips")
    assert secs is not None, f"no new leader within {limit + 60} s"
    assert secs <= limit, f"election took {secs} s > {limit} s"


@then("a newly applied app should be committed and running within the configured write target")
def step_writes_resume(context):
    limit = context.cfg.threshold("leader_loss.writes_resume_s")
    out = writes_resume(context, context.s["t_kill"], limit)
    context.ev.attach("write-attempts.json", out)
    context.ev.expect("writes_placed_s", f"<= {limit}")
    context.ev.measure("writes_commit_s", out.get("commit_s"))
    context.ev.measure("writes_placed_s", out.get("placed_s"))
    assert out.get("placed_s") is not None, f"no probe app committed and running within {limit + 60} s"
    assert out["placed_s"] <= limit, f"writes resumed (placed) after {out['placed_s']} s > {limit} s"


@then("the fixture should keep serving through the surviving ingresses")
def step_survivors_serving(context):
    need = context.cfg.threshold("node_loss.availability_min_ratio")
    victim = context.s["victim"]
    s = _availability(context, context.s["t_kill"], {victim.name})
    context.ev.expect("availability_via_survivors", f">= {need}")
    context.ev.measure("availability_via_survivors", s)
    assert s["ratio"] is not None and s["ratio"] >= need, f"availability {s}"


# --------------------------------------------------------------------------
# OPS-007 partition
# --------------------------------------------------------------------------


@when("I partition one non-leader node from the other two for the configured isolation time")
def step_partition(context):
    cl = context.cluster
    leader = _leader(context)
    place = apps.placement(cl, "frontend")
    hosting = sorted(k for k, v in place.items() if k != leader and any(i.endswith(":running") for i in v))
    if not hosting:
        raise Blocked(f"no non-leader node runs a frontend: leader={leader} placement={place}")
    node = context.lab.node(hosting[-1])
    peers = [n.ip for n in context.lab.nodes if n.name != node.name]
    context.s.update(victim=node, old_leader=leader)
    # The operator still reaches the minority node, so name it explicitly when
    # we want it; everything else goes to the majority.
    cl.down_nodes.add(node.name)
    context.cleanups.append(lambda: delete_write_probes(context))
    context.cleanups.append(lambda: (heal(context, node), cl.down_nodes.discard(node.name)))
    context.s["t_part"] = time.monotonic()
    partition(context, node, peers)
    context.ev.measure("partitioned", {"node": node.name, "from": peers, "leader": leader})


@then("the majority side should commit and run a newly applied app")
def step_majority_writes(context):
    limit = context.cfg.threshold("leader_loss.writes_resume_s")
    out = writes_resume(context, time.monotonic(), limit)
    context.ev.attach("majority-write.json", out)
    context.ev.expect("majority_write_placed_s", f"<= {limit}")
    context.ev.measure("majority_write_placed_s", out.get("placed_s"))
    if out.get("app"):
        context.ev.measure("majority_write_placement", apps.placement(context.cluster, out["app"]))
    assert out.get("placed_s") is not None and out["placed_s"] <= limit, f"majority write: {out}"


@then("the minority node should refuse to commit an apply sent to it")
def step_minority_refuses(context):
    node = context.s["victim"]
    name = apps.unique("minority")
    t = time.monotonic()
    r = apply_write_probe(context, name, endpoint=f"https://{node.ip}:9117", timeout=60)
    context.s["minority_app"], context.s["minority_rc"] = name, r.rc
    context.ev.expect("minority_apply", "refused")
    context.ev.measure("minority_apply", {"rc": r.rc, "secs": round(time.monotonic() - t, 1),
                                          "stdout": r.stdout.strip()[:300], "stderr": r.stderr.strip()[:300]})
    assert r.rc != 0, f"the partitioned node accepted an apply: {r.stdout[:300]}"


@then("the minority node should keep serving the fixture through its own ingress")
def step_minority_serves(context):
    need = context.cfg.threshold("partition.minority_serving_min_ratio")
    node = context.s["victim"]
    ok = 0
    for _ in range(20):
        ok += apps.ingress_get(node.ip, FIXTURE_HOST, "/healthz").ok
        time.sleep(0.5)
    ratio = ok / 20
    context.ev.expect("minority_ingress_ratio", f">= {need}")
    context.ev.measure("minority_ingress_ratio", ratio)
    assert ratio >= need, f"minority ingress served {ok}/20"


@then("after the partition heals the node should rejoin within the configured rejoin time")
def step_partition_heals(context):
    isolation = context.cfg.threshold("partition.isolation_s")
    limit = context.cfg.threshold("partition.rejoin_s")
    node, cl = context.s["victim"], context.cluster
    remaining = context.s["t_part"] + isolation - time.monotonic()
    if remaining > 0:
        time.sleep(remaining)
    context.ev.measure("isolated_for_s", round(time.monotonic() - context.s["t_part"], 1))
    view = cl.relish.run("nodes", check=False, timeout=30)
    context.ev.attach("nodes-during-partition.txt", view.stdout + view.stderr)
    heal(context, node)
    cl.down_nodes.discard(node.name)
    t = time.monotonic()
    try:
        cl.wait_healthy(limit)
        secs = round(time.monotonic() - t, 1)
    except TimeoutError:
        secs = None
    context.ev.expect("rejoin_s", f"<= {limit}")
    context.ev.measure("rejoin_s", secs)
    assert secs is not None, f"{node.name} not healthy {limit} s after the partition healed"


@then("the cluster should have exactly one leader and no app that only the minority accepted")
def step_no_split_brain(context):
    h = context.cluster.health()
    name = context.s.get("minority_app")
    exists = bool(name and apps.instances(context.cluster, name))
    context.ev.measure("leaders_after_heal", h.get("leaders"))
    context.ev.measure("minority_app_exists_after_heal", exists)
    assert len(h.get("leaders") or []) == 1, f"leaders after heal: {h.get('leaders')}"
    if context.s.get("minority_rc") != 0:
        assert not exists, f"{name} was refused on the minority but exists after the heal"


# --------------------------------------------------------------------------
# OPS-008 power cut under write load
# --------------------------------------------------------------------------


class WriteLoad:
    """Apply a fresh probe app and delete the previous one, back to back, through the majority."""

    def __init__(self, context):
        self.context = context
        self.stats = {"applies_ok": 0, "applies_failed": 0, "deletes": 0}
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, daemon=True)

    def _loop(self):
        previous = None
        while not self._stop.is_set():
            name = apps.unique("load")
            r = apply_write_probe(self.context, name, timeout=20)
            self.stats["applies_ok" if r.ok else "applies_failed"] += 1
            if previous:
                apps.delete_app(self.context.cluster, previous)
                self.stats["deletes"] += 1
            previous = name if r.ok else previous
            self._stop.wait(0.5)

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._stop.set()
        self._thread.join(timeout=60)


@when("I cut the power to one non-leader voter under write load for the configured number of cycles")
def step_power_cut_cycles(context):
    cl, lab = context.cluster, context.lab
    cycles = context.cfg.threshold("power_cut.cycles")
    before = context.cfg.threshold("power_cut.load_before_cut_s")
    limit = context.cfg.threshold("power_cut.rejoin_s")
    leader = _leader(context)
    voters = cl.health().get("voters_answering") or []
    candidates = sorted(v for v in voters if v != leader)
    if not candidates:
        raise Blocked(f"no non-leader voter: leader={leader} voters={voters}")
    node = lab.node(candidates[-1])
    context.s.update(victim=node, since=time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime()))
    context.cleanups.append(lambda: delete_write_probes(context))
    context.cleanups.append(lambda: restore_node(context, node))
    results = []
    for i in range(1, cycles + 1):
        cyc: dict = {"cycle": i, "node": node.name}
        with WriteLoad(context) as load:
            time.sleep(before)
            cl.down_nodes.add(node.name)
            lab.power_off(node)
            cyc["cut_at"] = time.strftime("%H:%M:%S", time.gmtime())
            time.sleep(10)
            lab.power_on(node)
            t_on = time.monotonic()
        cyc["load"] = dict(load.stats)
        cl.down_nodes.discard(node.name)
        try:
            cl.wait_healthy(limit)
            cyc["rejoin_s"] = round(time.monotonic() - t_on, 1)
        except TimeoutError as exc:
            cyc["rejoin_s"] = None
            cyc["health"] = str(exc)[:400]
        results.append(cyc)
        if cyc["rejoin_s"] is None:
            context.ev.observe(f"cycle {i}: {node.name} did not rejoin within {limit} s; later cycles skipped")
            break
    context.s["cut_cycles"] = results
    context.ev.attach("power-cut-cycles.json", results)
    context.ev.measure("rejoin_s_per_cycle", [c["rejoin_s"] for c in results])
    context.ev.measure("load_per_cycle", [c["load"] for c in results])


@then("the voter should rejoin after every cycle within the configured rejoin time")
def step_rejoined_every_cycle(context):
    limit = context.cfg.threshold("power_cut.rejoin_s")
    want = context.cfg.threshold("power_cut.cycles")
    res = context.s["cut_cycles"]
    context.ev.expect("cycles_rejoined", f"{want} of {want}, each <= {limit} s")
    context.ev.measure("cycles_rejoined", sum(c["rejoin_s"] is not None for c in res))
    assert len(res) == want and all(c["rejoin_s"] is not None for c in res), f"cycles: {res}"


@then("its journal should show no store corruption or refused start")
def step_no_corruption(context):
    node = context.s["victim"]
    r = context.lab.ssh(node, f"sudo journalctl -u reliaburger.service --no-pager --since '{context.s['since']} UTC'",
                        check=False, timeout=60, quiet=True)
    context.ev.attach(f"journal-{node.name}.txt", r.stdout + r.stderr)
    hits = [line for line in r.stdout.splitlines() if CORRUPTION.search(line)]
    context.ev.expect("corruption_lines", 0)
    context.ev.measure("corruption_lines", hits[:20])
    starts = sum("Started reliaburger.service" in line for line in r.stdout.splitlines())
    context.ev.measure("service_starts", starts)
    assert not hits, f"{len(hits)} suspicious journal lines, first: {hits[0][:200]}"


# --------------------------------------------------------------------------
# OPS-009 whole-cluster power loss
# --------------------------------------------------------------------------


@when("I cut the power to every node at once and power them all on again")
def step_power_cycle_all(context):
    lab, cl = context.lab, context.cluster
    context.s["fixture_before"] = {a: apps.placement(cl, a) for a in ("backend", "frontend")}
    context.cleanups.append(lambda: [lab.power_on(n) for n in lab.nodes if lab.domain_state(n) != "running"])
    threads = [threading.Thread(target=lab.power_off, args=(n,)) for n in lab.nodes]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    context.ev.measure("powered_off", {n.name: lab.domain_state(n) for n in lab.nodes})
    time.sleep(10)
    for n in lab.nodes:
        lab.power_on(n)
    context.s["t_on"] = time.monotonic()
    cl.down_nodes.clear()


@then("the council should be healthy within the configured recovery time")
def step_council_back(context):
    limit = context.cfg.threshold("power_cycle.recovery_s")
    try:
        health, _ = context.cluster.wait_healthy(limit)
        secs = round(time.monotonic() - context.s["t_on"], 1)
    except TimeoutError as exc:
        health, secs = {"error": str(exc)[:600]}, None
    context.ev.expect("council_healthy_s", f"<= {limit}")
    context.ev.measure("council_healthy_s", secs)
    context.ev.measure("health_after", health)
    assert secs is not None, f"council not healthy {limit} s after power-on"


@then("the fixture should serve on every ingress within the configured recovery time")
def step_fixture_back(context):
    limit = context.cfg.threshold("power_cycle.recovery_s")
    remaining = max(5.0, context.s["t_on"] + limit - time.monotonic())
    try:
        apps.wait_ingress(node_ips(context), FIXTURE_HOST, "/healthz", remaining)
        secs = round(time.monotonic() - context.s["t_on"], 1)
    except TimeoutError:
        secs = None
    context.ev.expect("fixture_serving_s", f"<= {limit}")
    context.ev.measure("fixture_serving_s", secs)
    assert secs is not None, f"fixture not serving on every ingress {limit} s after power-on"


@then("the applied apps should still be defined")
def step_apps_defined(context):
    cl = context.cluster
    after = {a: apps.placement(cl, a) for a in ("backend", "frontend")}
    running = {a: len(apps.running(cl, a)) for a in ("backend", "frontend")}
    context.ev.measure("fixture_before", context.s["fixture_before"])
    context.ev.measure("fixture_after", after)
    context.ev.expect("running_after", {"backend": 2, "frontend": 3})
    context.ev.measure("running_after", running)
    assert running == {"backend": 2, "frontend": 3}, f"running after power cycle: {running}"


# --------------------------------------------------------------------------
# OPS-010 / OPS-011 rolling deploys
# --------------------------------------------------------------------------


def _variant(context, message: str, health_path: str | None = None):
    text = NATIVE_FIXTURE.read_text().replace('PODINFO_UI_MESSAGE = "rbeval-frontend"',
                                              f'PODINFO_UI_MESSAGE = "{message}"')
    if health_path:
        head, sep, tail = text.partition("[app.frontend.health]\npath = \"/readyz\"")
        if not sep:
            raise Blocked("fixture layout changed: no [app.frontend.health] path to replace")
        text = head + f'[app.frontend.health]\npath = "{health_path}"' + tail
    path = context.ev.dir / "frontend-variant.toml"
    path.write_text(text)
    context.ev.attach_path(path)
    return path


def _restore_fixture(context):
    context.cluster.relish.run("apply", str(NATIVE_FIXTURE), check=False, timeout=180)
    try:
        wait_until(lambda: _message_everywhere(context, "rbeval-frontend", 2), 300, 5, "original fixture back")
    except TimeoutError:
        context.ev.observe("cleanup: the original frontend message was not back on every ingress within 300 s")


@when("I apply a changed frontend message")
def step_apply_good(context):
    msg = apps.unique("rbeval-frontend-v2")
    path = _variant(context, msg)
    context.cleanups.append(lambda: _restore_fixture(context))
    context.s.update(message=msg, t_deploy=time.monotonic(), epoch=time.time())
    context.cluster.relish.run("apply", str(path), timeout=180)


@then("every ingress should serve the new message within the configured completion time")
def step_good_complete(context):
    limit = context.cfg.threshold("rolling_deploy.completion_s")
    streak = {"n": 0}

    def done():
        streak["n"] = streak["n"] + 1 if _message_everywhere(context, context.s["message"]) else 0
        return streak["n"] >= 3
    try:
        wait_until(done, limit, 2, "new message on every ingress")
        secs = round(time.monotonic() - context.s["t_deploy"], 1)
    except TimeoutError:
        secs = None
    context.ev.expect("rollout_complete_s", f"<= {limit}")
    context.ev.measure("rollout_complete_s", secs)
    hist = context.cluster.relish.run("history", "frontend", check=False)
    context.ev.attach("history-after.txt", hist.stdout + hist.stderr)
    assert secs is not None, f"new message not on every ingress within {limit} s"


@then("availability during the rollout should meet the configured threshold")
@then("availability during the failed rollout should meet the configured threshold")
def step_rollout_availability(context):
    need = context.cfg.threshold("rolling_deploy.availability_min_ratio")
    s = _availability(context, context.s["t_deploy"])
    context.ev.expect("availability_during_rollout", f">= {need}")
    context.ev.measure("availability_during_rollout", s)
    assert s["ratio"] is not None and s["ratio"] >= need, f"availability {s}"


@when("I apply a frontend version whose health check never passes")
def step_apply_bad(context):
    path = _variant(context, "rbeval-frontend-bad", health_path="/status/503")
    context.cleanups.append(lambda: _restore_fixture(context))
    context.s.update(t_deploy=time.monotonic(), epoch=time.time())
    context.cluster.relish.run("apply", str(path), timeout=180)


def _new_history(context) -> list[dict]:
    rows = context.cluster.relish.json("history", "frontend", timeout=30, quiet=True)
    return [r for r in rows if r.get("created_at", {}).get("secs_since_epoch", 0) >= int(context.s["epoch"]) - 1]


@then("the deploy should be rolled back within the configured rollback time")
def step_rolled_back(context):
    limit = context.cfg.threshold("rolling_deploy.rollback_s")
    seen: list = []

    def rolled_back():
        rows = _new_history(context)
        seen.append({"t_rel_s": round(time.monotonic() - context.s["t_deploy"], 1),
                     "results": sorted({f"{r['node']}:{r['result']}" for r in rows})})
        finished = [r for r in rows if not re.search(r"progress|running|pending", str(r["result"]), re.I)]
        return bool(rows) and len(finished) == len(rows) and any(
            re.search(r"roll|fail", str(r["result"]), re.I) for r in rows)
    try:
        wait_until(rolled_back, limit + 120, 5, "rollback recorded in history")
        secs = round(time.monotonic() - context.s["t_deploy"], 1)
    except TimeoutError:
        secs = None
    context.ev.attach("history-samples.json", seen)
    context.ev.expect("rollback_s", f"<= {limit}")
    context.ev.measure("rollback_s", secs)
    context.ev.measure("history_results", seen[-1]["results"] if seen else None)
    assert secs is not None, f"no failed or rolled-back result in relish history within {limit + 120} s"
    assert secs <= limit, f"rollback took {secs} s > {limit} s"


@then("the previous version should keep serving on every ingress")
def step_previous_serves(context):
    ok = _message_everywhere(context, "rbeval-frontend")
    bad_seen = any(json.loads(apps.ingress_get(ip, FIXTURE_HOST, "/api/info").body or "{}").get("message")
                   == "rbeval-frontend-bad" for ip in node_ips(context))
    running = len(apps.running(context.cluster, "frontend"))
    context.ev.measure("previous_message_everywhere", ok)
    context.ev.measure("bad_message_seen", bad_seen)
    context.ev.measure("frontend_running", running)
    assert ok and not bad_seen, f"previous message everywhere={ok}, bad version served={bad_seen}"
