"""Behave hooks: run directory, claim linking, evidence, status mapping, teardown.

Status mapping (harness statuses, not behave's):
  PASS         every step passed and the scenario recorded measured values and evidence files
  FAIL         an observable product behaviour contradicted a Then/When expectation
  UNSUPPORTED  the product does not offer the capability (Unsupported raised, with evidence)
  BLOCKED      setup failed (any failure in a Given step), a claim tag did not link to a
               verified citation, or the harness itself errored
  NOT_RUN      the scenario was not executed (filtered, dry run, aborted run)
"""

from __future__ import annotations

import datetime as dt
import json
import os
import sys
import traceback
from dataclasses import asdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from harness.claims import Claims  # noqa: E402
from harness.cluster import Cluster  # noqa: E402
from harness.core import (ROOT, Blocked, CommandFailed, Config, ProductFailure, Redactor,  # noqa: E402
                          RunLog, Unsupported, now_iso)
from harness.evidence import Evidence, ScenarioRecord, environment_manifest, write_results  # noqa: E402
from harness.lab import Lab  # noqa: E402


def _tag_values(tags, prefix):
    return [t[len(prefix):] for t in tags if t.startswith(prefix)]


def before_all(context):
    cfg = Config.load()
    run_id = os.environ.get("RBEVAL_RUN_ID") or (
        dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + cfg.product_key)
    run_dir = ROOT / "results" / run_id
    run_dir.mkdir(parents=True, exist_ok=True)
    log = RunLog(run_dir, Redactor(cfg.safety["secrets"]["patterns"]))
    context.cfg = cfg
    context.run_id = run_id
    context.run_dir = run_dir
    context.log = log
    context.lab = Lab(cfg, log)
    context.cluster = Cluster(cfg, log, context.lab)
    context.claims = Claims(cfg)
    context.records = []
    context.shared = {}          # cross-scenario facts (e.g. bootstrap measurements)
    (run_dir / "environment.json").write_text(json.dumps(environment_manifest(cfg), indent=2))
    print(f"[rbeval] run {run_id} -> {run_dir}", file=sys.stderr)


def before_scenario(context, scenario):
    tags = [str(t) for t in scenario.effective_tags]
    req = (_tag_values(tags, "req_") or ["UNTAGGED"])[0]
    row = getattr(scenario, "_row", None)
    if row is not None:
        case = row.get("case") if "case" in row.headings else str(row.index)
        req = f"{req}[{case}]"
    axis = (_tag_values(tags, "axis_") or ["unknown"])[0]
    claim_ids = ["claim_" + c for c in _tag_values(tags, "claim_")]
    cfg = context.cfg
    rec = ScenarioRecord(
        id=req, name=scenario.name, feature=str(scenario.feature.filename), axis=axis,
        claim_ids=claim_ids, source_refs=context.claims.source_refs(claim_ids),
        product_commit=cfg.product["commit"], product_target=cfg.product_key,
        environment="environment.json", tags=tags, hypothesis="hypothesis" in tags,
    )
    rec.start_time = now_iso()
    sdir = context.run_dir / "scenarios" / req.replace("/", "_")
    context.log.scenario_dir = sdir
    context.log.scenario_id = req
    context.ev = Evidence(rec, sdir, context.run_dir, context.log)
    context.record = rec
    context.cleanups = []
    context.s = {}
    context._t0 = dt.datetime.now(dt.timezone.utc)

    problems = []
    if req.startswith("UNTAGGED"):
        problems.append("scenario has no @req_ tag")
    if axis == "unknown":
        problems.append("scenario has no @axis_ tag")
    if not claim_ids:
        problems.append("scenario has no @claim_ tag")
    for cid in claim_ids:
        ok, checks = context.claims.is_verified(cid)
        context.ev.attach(f"claims/{cid}.json", {"claim": context.claims.get(cid),
                                                  "citation_checks": [c.__dict__ for c in checks]})
        if not ok:
            problems.append(f"{cid} does not link to a verified citation")
        claim = context.claims.get(cid) or {}
        if claim.get("kind") == "hypothesis":
            rec.hypothesis = True
    if problems:
        rec.status = "BLOCKED"
        rec.status_reason = "traceability: " + "; ".join(problems)
        scenario.skip("BLOCKED: " + rec.status_reason)


def after_step(context, step):
    if step.status.name in ("failed", "error"):
        context.record.failed_step = f"{step.keyword} {step.name}"
        try:
            capture_diagnostics(context)
        except Exception as exc:  # diagnostics must never mask the failure
            context.ev.observe(f"diagnostics capture failed: {exc}")


def capture_diagnostics(context):
    ev = context.ev
    cl = context.cluster
    if not cl.token_path.exists():
        return
    for name, args in (("wtf", ("wtf",)), ("status", ("status",)), ("council", ("council",)), ("nodes", ("nodes",))):
        r = cl.relish.run(*args, output="json", check=False, timeout=45)
        ev.attach(f"diagnostics/{name}.json", r.stdout or r.stderr)
    for n in context.lab.nodes:
        if context.lab.domain_state(n) != "running" or n.name in cl.down_nodes:
            continue
        r = context.lab.ssh(n, "sudo journalctl -u reliaburger.service --no-pager -n 300 --since '-15 min'",
                            check=False, timeout=30)
        ev.attach(f"diagnostics/journal-{n.name}.txt", r.stdout + r.stderr)


def _classify(context, scenario) -> tuple[str, str]:
    rec, ev = context.record, context.ev
    if rec.status == "BLOCKED":
        return "BLOCKED", rec.status_reason
    st = scenario.status.name
    if st == "skipped":
        reason = getattr(scenario, "skip_reason", "") or ""
        for s in ("UNSUPPORTED", "BLOCKED"):
            if reason.startswith(s + ":"):
                return s, reason[len(s) + 1:].strip()
        return "NOT_RUN", reason or "skipped"
    if st == "untested":
        return "NOT_RUN", "not executed"
    if st == "passed":
        if ev.has_direct_evidence():
            return "PASS", "all steps passed with recorded measurements"
        return "BLOCKED", "steps passed but no measured values or evidence files were recorded"
    failed = next((s for s in scenario.steps if s.status.name in ("failed", "error")), None)
    exc = failed.exception if failed else None
    msg = f"{failed.keyword} {failed.name}: {type(exc).__name__}: {exc}" if failed else st
    msg = context.log.redact(msg)[:1500]
    if isinstance(exc, Unsupported):
        return "UNSUPPORTED", str(exc)[:1500]
    if isinstance(exc, Blocked):
        return "BLOCKED", msg
    if failed is not None and failed.step_type == "given":
        return "BLOCKED", "setup: " + msg
    if isinstance(exc, (AssertionError, ProductFailure, CommandFailed, TimeoutError)):
        return "FAIL", msg
    if failed is not None and failed.error_message:
        context.ev.attach("diagnostics/harness_traceback.txt", failed.error_message)
    return "BLOCKED", "harness error: " + msg


def after_scenario(context, scenario):
    rec = context.record
    for fn in reversed(context.cleanups):
        try:
            fn()
        except Exception as exc:
            context.ev.observe(f"cleanup failed: {type(exc).__name__}: {exc}")
            context.ev.attach("diagnostics/cleanup_traceback.txt", traceback.format_exc())
    status, reason = _classify(context, scenario)
    rec.status, rec.status_reason = status, reason
    rec.duration_ms = int((dt.datetime.now(dt.timezone.utc) - context._t0).total_seconds() * 1000)
    sdir = context.log.scenario_dir
    (sdir / "result.json").write_text(json.dumps(asdict(rec), indent=2, default=str))
    if (sdir / "commands.jsonl").exists():
        context.ev.attach_path(sdir / "commands.jsonl")
        (sdir / "result.json").write_text(json.dumps(asdict(rec), indent=2, default=str))
    context.records.append(rec)
    context.log.scenario_dir = None
    context.log.scenario_id = None
    print(f"[rbeval] {rec.id}: {status} ({reason[:160]})", file=sys.stderr)


def after_all(context):
    summary = write_results(context.run_dir, context.records, context.cfg, context.run_id)
    (context.run_dir / "operator_actions.json").write_text(json.dumps(context.log.operator_actions, indent=2))
    print(f"[rbeval] summary: {json.dumps(summary['counts'])}", file=sys.stderr)
    keep = os.environ.get("RBEVAL_KEEP_LAB") == "1" or context.cfg.lab.get("teardown_after_run") == "keep"
    from harness import services  # noqa: PLC0415
    services.teardown_all(context)
    if not keep and context.lab.touched:
        try:
            context.lab.down()
            context.cluster.forget()
        except Exception as exc:
            print(f"[rbeval] teardown failed: {exc}", file=sys.stderr)
