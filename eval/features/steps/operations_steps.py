"""Axis 4: operational credibility."""

from __future__ import annotations

import time

from behave import given, then, when

from harness import apps
from harness.core import Blocked, wait_until
from harness.probes import Prober, Sampler
from harness.resilience import cluster_snapshot, restore_node
from harness.steplib import FIXTURE_HOST, NATIVE_FIXTURE, ensure_fixture, node_ips


@given("a {count:d}-replica fixture application distributed across at least {spread:d} nodes")
def step_fixture_spread(context, count, spread):
    ensure_fixture(context, "frontend", count)
    place = apps.placement(context.cluster, "frontend")
    nodes_with = [n for n, inst in place.items() if any(i.endswith(":running") for i in inst)]
    context.ev.measure("placement_before", place)
    if len(nodes_with) < spread:
        raise Blocked(f"frontend runs on {len(nodes_with)} node(s), need {spread}: {place}")


@given("an external probe recording availability once per second")
def step_probe(context):
    interval = context.cfg.threshold("node_loss.probe_interval_s")
    targets = [{"name": n.name, "ip": n.ip, "port": 80, "host": FIXTURE_HOST, "path": "/healthz"}
               for n in context.lab.nodes]
    prober = Prober(targets, context.ev.dir / "probe.jsonl", interval, context.cfg.threshold("http.probe_timeout_s")).start()
    sampler = Sampler(lambda: cluster_snapshot(context), context.ev.dir / "cluster_state.jsonl", 1.0).start()
    context.s["prober"], context.s["sampler"] = prober, sampler
    context.ev.attach_path(context.ev.dir / "probe.jsonl")
    context.ev.attach_path(context.ev.dir / "cluster_state.jsonl")

    def stop():
        prober.stop()
        sampler.stop()
    context.cleanups.append(stop)
    time.sleep(5)
    pre = Prober.summarise(prober.samples)
    context.ev.measure("probe_before_fault", pre)
    if pre["ratio"] is None or pre["ratio"] < 1.0:
        raise Blocked(f"the application was not fully available before the fault: {pre}")


@when("I forcibly power off a {victim} node hosting at least {n:d} fixture replica")
def step_power_off(context, victim, n):
    cl, lab = context.cluster, context.lab
    health = cl.health()
    leader = (health.get("leaders") or [None])[0]
    place = apps.placement(cl, "frontend")
    hosting = [k for k, v in place.items() if sum(i.endswith(":running") for i in v) >= n]
    if victim == "council-leader":
        candidates = [leader] if leader in hosting else []
    else:
        candidates = [h for h in hosting if h != leader]
    if not candidates:
        raise Blocked(f"no {victim} node hosts {n} replica(s): leader={leader} placement={place}")
    node = lab.node(sorted(candidates)[-1])
    context.s["victim"] = node
    context.s["old_leader"] = leader
    context.s["victim_replicas"] = place[node.name]
    context.ev.measure("victim", {"node": node.name, "role": victim, "leader_before": leader,
                                  "replicas": place[node.name]})
    cl.down_nodes.add(node.name)
    context.cleanups.append(lambda: restore_node(context, node))
    context.s["t_kill"] = time.monotonic()
    r = lab.power_off(node)
    context.ev.measure("power_off", {"command": r.display, "rc": r.rc, "ts": r.start})


@then("the cluster should detect the missing node within the configured detection timeout")
def step_detect(context):
    limit = context.cfg.threshold("node_loss.detection_s")
    node, sampler, t0 = context.s["victim"], context.s["sampler"], context.s["t_kill"]

    def detected():
        s = sampler.first(lambda v: v.get("nodes", {}).get(node.name, "alive") != "alive", t0)
        return s
    try:
        s, _ = wait_until(detected, limit + 60, 1, f"{node.name} not alive in relish nodes")
        secs = round(s["mono"] - t0, 1)
        state = s["value"]["nodes"].get(node.name)
    except TimeoutError:
        secs, state = None, None
    context.ev.expect("detection_s", f"<= {limit}")
    context.ev.measure("detection_s", secs)
    context.ev.measure("detected_state", state)
    if context.s["old_leader"] == node.name:
        new = sampler.first(lambda v: (v.get("leaders") or [node.name])[0] != node.name and len(v.get("leaders") or []) == 1, t0)
        if new is None:
            try:
                new, _ = wait_until(lambda: sampler.first(
                    lambda v: (v.get("leaders") or [node.name])[0] != node.name and len(v.get("leaders") or []) == 1, t0),
                    context.cfg.threshold("leader_loss.writes_resume_s") + 30, 1, "new leader")
            except TimeoutError:
                new = None
        context.ev.measure("new_leader_observed_s", round(new["mono"] - t0, 1) if new else None)
        context.ev.measure("new_leader", new["value"]["leaders"] if new else None)
    assert secs is not None, f"{node.name} still reported alive {limit + 60} s after power-off"
    assert secs <= limit, f"detection took {secs} s > {limit} s"


@then("the desired healthy replica count should be restored within the configured recovery timeout")
def step_recover(context):
    limit = context.cfg.threshold("node_loss.recovery_s")
    node, t0 = context.s["victim"], context.s["t_kill"]
    try:
        wait_until(lambda: len(apps.running(context.cluster, "frontend", exclude_nodes={node.name})) >= 3,
                   limit + 120, 2, "3 running frontends on survivors")
        secs = round(time.monotonic() - t0, 1)
    except TimeoutError:
        secs = None
    context.s["t_recovered"] = time.monotonic() if secs is not None else None
    context.ev.expect("recovery_s", f"<= {limit}")
    context.ev.measure("recovery_s", secs)
    context.ev.measure("placement_after", apps.placement(context.cluster, "frontend"))
    assert secs is not None, f"replicas not restored on survivors within {limit + 120} s"
    assert secs <= limit, f"recovery took {secs} s > {limit} s"


@then("the application should satisfy the configured availability threshold during recovery")
def step_availability(context):
    need = context.cfg.threshold("node_loss.availability_min_ratio")
    tail = context.cfg.threshold("node_loss.post_recovery_observe_s")
    node, prober, t0 = context.s["victim"], context.s["prober"], context.s["t_kill"]
    t_rec = context.s.get("t_recovered") or time.monotonic()
    end = t_rec + tail
    while time.monotonic() < end:
        time.sleep(1)
    per = {}
    survivors = []
    for n in context.lab.nodes:
        w = prober.window(t0, end, n.name)
        per[n.name] = Prober.summarise(w)
        if n.name != node.name:
            survivors.extend(w)
    overall = Prober.summarise(survivors)
    context.ev.expect("availability_ratio_via_surviving_ingresses", f">= {need}")
    context.ev.measure("availability_via_survivors", overall)
    context.ev.measure("availability_per_ingress", per)
    context.ev.limitation(f"probes through {node.name}'s own ingress fail while it is off; the lab has no "
                          "load balancer or floating address in front of the nodes, so they are excluded from the ratio")
    assert overall["ratio"] is not None and overall["ratio"] >= need, \
        f"availability {overall['ratio']} < {need} ({overall})"


@then("the evidence should contain replica placements, probe results, and cluster state transitions")
def step_evidence(context):
    sampler = context.s["sampler"]
    transitions, last = [], None
    for s in sampler.samples:
        v = s["value"]
        key = (tuple(sorted((v.get("nodes") or {}).items())), tuple(v.get("leaders") or []),
               tuple(v.get("frontend") or []))
        if key != last:
            transitions.append({"ts": s["ts"], "t_rel_s": round(s["mono"] - context.s["t_kill"], 1), **v})
            last = key
    context.ev.attach("state_transitions.json", transitions)
    wtf = context.cluster.relish.run("wtf", check=False, timeout=60)
    context.ev.attach("wtf-after.txt", wtf.stdout + wtf.stderr)
    context.ev.measure("state_transitions_count", len(transitions))
    assert transitions and context.record.measured.get("placement_before") and context.record.measured.get("placement_after")


FIXTURE_APPS = {"backend": 2, "frontend": 3}


def _fixture_rows(context) -> list[dict]:
    return [r for r in apps.instances(context.cluster) if r.get("app_name") in FIXTURE_APPS]


def _converged(context) -> bool:
    rows = _fixture_rows(context)
    for app, want in FIXTURE_APPS.items():
        mine = [r for r in rows if r["app_name"] == app]
        if sum(r["state"] == "running" for r in mine) < want or any(r["state"] != "running" for r in mine):
            return False
    return all(apps.ingress_get(ip, FIXTURE_HOST, "/healthz").ok for ip in node_ips(context))


def _delete_fixture(context) -> None:
    for app in FIXTURE_APPS:
        apps.delete_app(context.cluster, app)


def recover_fixture(context) -> dict:
    """Bring the shared fixture back after a wedged cycle, recording what it cost."""
    cl, lab, ev = context.cluster, context.lab, context.ev
    out = {"restarted_nodes": []}
    _delete_fixture(context)
    try:
        wait_until(lambda: not _fixture_rows(context), context.cfg.threshold("redeploy.retire_s"), 3, "fixture retired")
    except TimeoutError:
        stuck = sorted({r["node"] for r in _fixture_rows(context)})
        for name in stuck:
            lab.ssh(name, "sudo systemctl restart reliaburger.service",
                    operator=f"restart reliaburger.service on {name} to clear instances that never retire")
            out["restarted_nodes"].append(name)
        cl.wait_healthy(180)
        wait_until(lambda: not _fixture_rows(context), 120, 3, "fixture retired after restart")
    ensure_fixture(context, "frontend", FIXTURE_APPS["frontend"])
    ev.measure("recovery", out)
    return out


@given("the fixture application is running with 2 backend and 3 frontend replicas")
def step_fixture_running(context):
    ensure_fixture(context, "frontend", FIXTURE_APPS["frontend"])
    if not _converged(context):
        raise Blocked(f"the fixture is not cleanly running before the first cycle: {apps.placement(context.cluster, 'backend')} "
                      f"{apps.placement(context.cluster, 'frontend')}")
    context.ev.measure("placement_before", {a: apps.placement(context.cluster, a) for a in FIXTURE_APPS})


@when("I delete and re-apply the fixture for the configured number of cycles, {timing}")
def step_redeploy_cycles(context, timing):
    cl, ev = context.cluster, context.ev
    settle = timing.startswith("after")
    cycles = context.cfg.threshold("redeploy.cycles")
    converge = context.cfg.threshold("redeploy.converge_s")
    retire = context.cfg.threshold("redeploy.retire_s")
    context.s["t_start"] = time.monotonic()
    context.s["since"] = time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime())
    results = []
    for i in range(1, cycles + 1):
        cyc: dict = {"cycle": i, "settle": settle}
        t0 = time.monotonic()
        _delete_fixture(context)
        cyc["delete_returned_s"] = round(time.monotonic() - t0, 1)
        if settle:
            try:
                _, secs = wait_until(lambda: not _fixture_rows(context), retire, 2, "deleted fixture retired")
                cyc["retired_s"] = round(secs, 1)
            except TimeoutError:
                cyc["retired_s"] = None
                cyc["left_after_delete"] = sorted(f"{r['node']}:{r['id']}:{r['state']}" for r in _fixture_rows(context))
        else:
            cyc["rows_at_apply"] = len(_fixture_rows(context))
        t1 = time.monotonic()
        cl.relish.run("apply", str(NATIVE_FIXTURE), timeout=180)
        try:
            _, secs = wait_until(lambda: _converged(context), converge, 2, f"cycle {i} converged")
            cyc["converged_s"] = round(secs, 1)
        except TimeoutError:
            cyc["converged_s"] = None
        cyc["since_delete_s"] = round(time.monotonic() - t0, 1)
        cyc["apply_to_converged_s"] = round(time.monotonic() - t1, 1) if cyc["converged_s"] is not None else None
        cyc["instances"] = sorted(f"{r['node']}:{r['id']}:{r['state']}" for r in _fixture_rows(context))
        results.append(cyc)
        if cyc["converged_s"] is None:
            ev.observe(f"cycle {i} did not converge within {converge} s; later cycles were not attempted")
            break
    context.s["cycles"] = results
    ev.attach("cycles.json", results)
    ev.measure("cycles_attempted", len(results))
    ev.measure("cycles_converged", sum(c["converged_s"] is not None for c in results))
    ev.measure("converged_s_per_cycle", [c["converged_s"] for c in results])
    if any(c["converged_s"] is None for c in results):
        context.cleanups.append(lambda: recover_fixture(context))


@then("every cycle should reach the desired replicas within the configured convergence timeout")
def step_cycles_converged(context):
    cycles = context.s["cycles"]
    want = context.cfg.threshold("redeploy.cycles")
    limit = context.cfg.threshold("redeploy.converge_s")
    context.ev.expect("cycles_converged", f"{want} of {want}, each <= {limit} s")
    bad = [c for c in cycles if c["converged_s"] is None]
    assert not bad and len(cycles) == want, \
        f"cycle {bad[0]['cycle']} of {want} never converged; instances: {bad[0]['instances']}" if bad else "too few cycles"


@then("no stopped fixture instances should remain after the last cycle")
def step_no_stopped(context):
    rows = _fixture_rows(context)
    left = sorted(f"{r['node']}:{r['id']}:{r['state']}" for r in rows if r["state"] != "running")
    context.ev.expect("non_running_fixture_instances", 0)
    context.ev.measure("non_running_fixture_instances", left)
    assert not left, f"instances left behind: {left}"


@then("the evidence should record each cycle's timings and the node logs")
def step_cycle_evidence(context):
    lab, ev = context.lab, context.ev
    for n in lab.nodes:
        r = lab.ssh(n, f"sudo journalctl -u reliaburger.service --no-pager --since '{context.s['since']} UTC'",
                    check=False, timeout=60, quiet=True)
        ev.attach(f"journal-{n.name}.txt", r.stdout + r.stderr)
        retries = sum("retrying in" in line for line in r.stdout.splitlines())
        ev.measure(f"deploy_retry_lines_{n.name}", retries)
    wtf = context.cluster.relish.run("wtf", check=False, timeout=60)
    ev.attach("wtf-after.txt", wtf.stdout + wtf.stderr)
    assert context.s["cycles"], "no cycles recorded"
