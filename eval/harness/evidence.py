"""Per-scenario evidence record and run-level result files."""

from __future__ import annotations

import json
import os
import platform
import re
import subprocess
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any
from xml.sax.saxutils import escape, quoteattr

from .core import ROOT, Config, RunLog, now_iso

STATUSES = ("PASS", "FAIL", "UNSUPPORTED", "BLOCKED", "NOT_RUN")
INTEGRATION_CLASSES = ("native", "configured", "workaround", "source_change", "unsupported")


@dataclass
class ScenarioRecord:
    id: str
    name: str
    feature: str
    axis: str
    claim_ids: list[str]
    source_refs: list[str]
    product_commit: str
    product_target: str
    environment: str
    tags: list[str]
    hypothesis: bool = False
    status: str = "NOT_RUN"
    status_reason: str = ""
    start_time: str | None = None
    duration_ms: int | None = None
    observations: list[str] = field(default_factory=list)
    expected: dict[str, Any] = field(default_factory=dict)
    measured: dict[str, Any] = field(default_factory=dict)
    evidence_paths: list[str] = field(default_factory=list)
    manual_steps: list[str] = field(default_factory=list)
    external_dependencies: list[str] = field(default_factory=list)
    limitations: list[str] = field(default_factory=list)
    deviations: list[str] = field(default_factory=list)
    classification: str | None = None
    confidence: str = "high"
    failed_step: str | None = None


class Evidence:
    """What step definitions write to. One instance per scenario."""

    def __init__(self, record: ScenarioRecord, scenario_dir: Path, run_dir: Path, log: RunLog):
        self.r = record
        self.dir = scenario_dir
        self.run_dir = run_dir
        self.log = log
        self.dir.mkdir(parents=True, exist_ok=True)
        self._t0 = time.monotonic()

    def _rel(self, path: Path) -> str:
        return str(path.relative_to(self.run_dir))

    def expect(self, key: str, value: Any) -> None:
        self.r.expected[key] = value

    def measure(self, key: str, value: Any) -> None:
        self.r.measured[key] = value

    def observe(self, text: str) -> None:
        self.r.observations.append(self.log.redact(text))

    def manual_step(self, text: str) -> None:
        self.r.manual_steps.append(text)

    def external(self, dep: str) -> None:
        if dep not in self.r.external_dependencies:
            self.r.external_dependencies.append(dep)

    def limitation(self, text: str) -> None:
        if text not in self.r.limitations:
            self.r.limitations.append(text)

    def deviation(self, text: str) -> None:
        if text not in self.r.deviations:
            self.r.deviations.append(text)

    def classify(self, value: str) -> None:
        assert value in INTEGRATION_CLASSES, value
        self.r.classification = value

    def confidence(self, level: str) -> None:
        self.r.confidence = level

    def attach(self, name: str, data: Any) -> str:
        path = self.dir / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(data, (dict, list)):
            text = json.dumps(data, indent=2, default=str)
        else:
            text = str(data)
        path.write_text(self.log.redact(text))
        rel = self._rel(path)
        if rel not in self.r.evidence_paths:
            self.r.evidence_paths.append(rel)
        return rel

    def attach_path(self, path: Path) -> str:
        rel = self._rel(path)
        if rel not in self.r.evidence_paths:
            self.r.evidence_paths.append(rel)
        return rel

    def has_direct_evidence(self) -> bool:
        return bool(self.r.measured) and bool(self.r.evidence_paths)


def git_commit(path: Path) -> str | None:
    r = subprocess.run(["git", "-C", str(path), "rev-parse", "HEAD"], capture_output=True, text=True)
    if r.returncode != 0:
        return None
    dirty = subprocess.run(["git", "-C", str(path), "status", "--porcelain"], capture_output=True, text=True).stdout.strip()
    return r.stdout.strip() + ("-dirty" if dirty else "")


def environment_manifest(cfg: Config, extra: dict | None = None) -> dict:
    def cmd(*argv):
        try:
            return subprocess.run(argv, capture_output=True, text=True, timeout=20).stdout.strip()
        except (OSError, subprocess.TimeoutExpired):
            return None
    mem_kib = None
    try:
        mem_kib = int(re.search(r"MemTotal:\s+(\d+)", Path("/proc/meminfo").read_text()).group(1))
    except (OSError, AttributeError):
        pass
    import behave  # noqa: PLC0415
    return {
        "generated": now_iso(),
        "harness_commit": git_commit(ROOT),
        "product": {"target": cfg.product_key, **{k: cfg.product[k] for k in ("version", "commit", "channel", "release_tag", "docs_ref")},
                    "sha256": cfg.product["sha256"]},
        "host": {
            "kernel": platform.release(),
            "os": cmd("lsb_release", "-ds"),
            "cpus": os.cpu_count(),
            "memory_mib": mem_kib // 1024 if mem_kib else None,
            "qemu": cmd("qemu-system-x86_64", "--version"),
            "libvirt": cmd("virsh", "--version"),
            "python": platform.python_version(),
            "behave": behave.__version__,
        },
        "lab": {"image": cfg.lab["image"], "node_spec": cfg.lab["node_spec"],
                "nodes": cfg.lab["nodes"], "network": cfg.lab["network"]},
        **(extra or {}),
    }


def write_results(run_dir: Path, records: list[ScenarioRecord], cfg: Config, run_id: str) -> dict:
    data = [asdict(r) for r in records]
    (run_dir / "results.json").write_text(json.dumps(data, indent=2, default=str))
    counts = {s: sum(1 for r in records if r.status == s) for s in STATUSES}
    executed = counts["PASS"] + counts["FAIL"]
    summary = {
        "run_id": run_id,
        "product_target": cfg.product_key,
        "product_commit": cfg.product["commit"],
        "total": len(records),
        "counts": counts,
        "denominators": {
            "executed (PASS+FAIL)": executed,
            "pass_rate_of_executed": round(counts["PASS"] / executed, 3) if executed else None,
            "unsupported": counts["UNSUPPORTED"],
            "blocked": counts["BLOCKED"],
            "not_run": counts["NOT_RUN"],
        },
        "by_axis": {},
    }
    for r in records:
        a = summary["by_axis"].setdefault(r.axis, {s: 0 for s in STATUSES})
        a[r.status] += 1
    (run_dir / "summary.json").write_text(json.dumps(summary, indent=2))
    write_junit(run_dir / "junit-rbeval.xml", records, run_id)
    return summary


def write_junit(path: Path, records: list[ScenarioRecord], run_id: str) -> None:
    """JUnit view of the harness statuses: FAIL is a failure, BLOCKED an error,
    UNSUPPORTED and NOT_RUN are skipped. Nothing but PASS counts as passed."""
    by_axis: dict[str, list[ScenarioRecord]] = {}
    for r in records:
        by_axis.setdefault(r.axis, []).append(r)
    parts = ['<?xml version="1.0" encoding="UTF-8"?>', f"<testsuites name={quoteattr('rbeval ' + run_id)}>"]
    for axis, recs in sorted(by_axis.items()):
        fails = sum(r.status == "FAIL" for r in recs)
        errs = sum(r.status == "BLOCKED" for r in recs)
        skips = sum(r.status in ("UNSUPPORTED", "NOT_RUN") for r in recs)
        parts.append(f'<testsuite name={quoteattr(axis)} tests="{len(recs)}" failures="{fails}" errors="{errs}" skipped="{skips}">')
        for r in recs:
            t = (r.duration_ms or 0) / 1000
            parts.append(f'<testcase classname={quoteattr(axis)} name={quoteattr(r.id + " " + r.name)} time="{t:.3f}">')
            msg = quoteattr(r.status_reason[:500])
            if r.status == "FAIL":
                parts.append(f"<failure message={msg}>{escape(json.dumps(r.measured, default=str)[:4000])}</failure>")
            elif r.status == "BLOCKED":
                parts.append(f"<error message={msg}/>")
            elif r.status in ("UNSUPPORTED", "NOT_RUN"):
                parts.append(f"<skipped message={quoteattr(r.status + ': ' + r.status_reason[:480])}/>")
            parts.append("</testcase>")
        parts.append("</testsuite>")
    parts.append("</testsuites>")
    path.write_text("\n".join(parts))
