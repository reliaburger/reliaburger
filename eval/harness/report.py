"""Generated reports: per-run summary markdown plus copies of the JSON and JUnit."""

from __future__ import annotations

import json
import shutil
from pathlib import Path

from .core import ROOT

AXES = ["vision", "extensibility", "interoperability", "operations"]


def _fmt(v) -> str:
    s = json.dumps(v, default=str) if not isinstance(v, str) else v
    return s if len(s) <= 90 else s[:87] + "..."


def build_reports(run_dir: Path) -> Path:
    results = json.loads((run_dir / "results.json").read_text())
    summary = json.loads((run_dir / "summary.json").read_text())
    env = json.loads((run_dir / "environment.json").read_text())
    out = ROOT / "reports" / "runs" / run_dir.name
    out.mkdir(parents=True, exist_ok=True)
    shutil.copy(run_dir / "results.json", out / "results.json")
    shutil.copy(run_dir / "summary.json", out / "summary.json")
    shutil.copy(run_dir / "junit-rbeval.xml", out / "junit-rbeval.xml")
    junit_dir = run_dir / "junit"
    if junit_dir.exists():
        shutil.copytree(junit_dir, out / "junit-behave", dirs_exist_ok=True)

    c = summary["counts"]
    d = summary["denominators"]
    lines = [
        f"# Run {run_dir.name}",
        "",
        f"Product: {env['product']['target']} (`{env['product']['commit'][:12]}`, {env['product']['channel']}). "
        f"Harness: `{(env.get('harness_commit') or 'unknown')[:12]}`. Generated from `results/{run_dir.name}/`.",
        "",
        "## Totals",
        "",
        "| PASS | FAIL | UNSUPPORTED | BLOCKED | NOT_RUN | total |",
        "|---|---|---|---|---|---|",
        f"| {c['PASS']} | {c['FAIL']} | {c['UNSUPPORTED']} | {c['BLOCKED']} | {c['NOT_RUN']} | {summary['total']} |",
        "",
        f"Executed (PASS+FAIL): {d['executed (PASS+FAIL)']}; pass rate of executed: {d['pass_rate_of_executed']}. "
        "UNSUPPORTED and BLOCKED are never counted as passes.",
        "",
    ]
    for axis in AXES:
        rows = [r for r in results if r["axis"] == axis]
        if not rows:
            continue
        lines += [f"## {axis.capitalize()}", "", "| id | status | scenario | reason |", "|---|---|---|---|"]
        for r in sorted(rows, key=lambda r: r["id"]):
            lines.append(f"| {r['id']} | {r['status']} | {r['name']} | {_fmt(r['status_reason']).replace('|', '/')} |")
        lines.append("")
        for r in sorted(rows, key=lambda r: r["id"]):
            lines += [f"### {r['id']}: {r['name']}", "",
                      f"Status {r['status']}, {round((r['duration_ms'] or 0) / 1000)} s, confidence {r['confidence']}"
                      + (f", integration class `{r['classification']}`" if r.get("classification") else "")
                      + (", derived hypothesis" if r.get("hypothesis") else "") + ".",
                      "", f"Claims: {', '.join(r['claim_ids'])}.", ""]
            if r["expected"]:
                lines.append("| measure | expected | measured |")
                lines.append("|---|---|---|")
                for k, v in r["expected"].items():
                    lines.append(f"| {k} | {_fmt(v)} | {_fmt(r['measured'].get(k, '—'))} |".replace("—", "n/a"))
                lines.append("")
            for o in r["observations"]:
                lines.append(f"- {o}")
            if r["observations"]:
                lines.append("")
    trace: dict[str, list[str]] = {}
    for r in results:
        for cid in r["claim_ids"]:
            trace.setdefault(cid, []).append(f"{r['id']} ({r['status']})")
    lines += ["## Claim traceability", "", "| claim | scenarios |", "|---|---|"]
    for cid in sorted(trace):
        lines.append(f"| {cid} | {', '.join(trace[cid])} |")
    (out / "summary.md").write_text("\n".join(lines) + "\n")
    return out
