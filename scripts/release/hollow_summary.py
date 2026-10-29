#!/usr/bin/env python3
"""Summarise hollow-node fleet runs (#1401) as one Markdown table.

Each run of `tests/hollow_nodes.rs`'s `hollow_fleet_measurement` writes a
JSON report. This prints one row per report, smallest fleet first, for the
qualification record. A value the run didn't reach reads "not reached" and a
phase it never got to reads "not measured", so a stopped run can't pass for
a measured one.

Usage: scripts/release/hollow_summary.py DIR

Fixtures: scripts/release/test_hollow_summary.py.
"""
from __future__ import annotations

import json
import sys
from pathlib import Path

NOT_REACHED = "not reached"
NOT_MEASURED = "not measured"


class HollowSummaryError(Exception):
    """A report directory that can't be summarised."""


def seconds(milliseconds):
    if milliseconds is None:
        return NOT_REACHED
    return f"{milliseconds / 1000:.1f} s"


def phase(entries, name):
    return next((entry for entry in entries if entry.get("phase") == name), None)


def pair(latency):
    """`p50/p99 ms` of a latency summary, or "not measured"."""
    if not latency or latency.get("p50") is None:
        return NOT_MEASURED
    return f"{latency['p50']:.0f}/{latency['p99']:.0f} ms"


def row(report):
    """One report's table cells, keyed by column."""
    steady = phase(report.get("resources", []), "steady")
    commit = phase(report.get("raft_commit", []), "steady")
    churn = report.get("churn")
    outcome = "complete"
    if report.get("stopped"):
        outcome = f"stopped: {report['stopped']}"
    if report.get("hollow_voters"):
        outcome += f"; {report['hollow_voters']} hollow voters"
    return {
        "nodes": str(report["plan"]["hollow_nodes"]),
        "gossip converged": seconds(report.get("gossip_converged_ms")),
        "council sees all": seconds(report.get("council_sees_all_ms")),
        "all replicas": seconds(report.get("all_replicas_ms")),
        "council CPU (steady)": (f"{steady['council_cpu_percent']:.0f}%" if steady else NOT_MEASURED),
        "council RSS (steady)": (f"{steady['council_rss_mb_max']:.0f} MB" if steady else NOT_MEASURED),
        "fleet RSS (steady)": (f"{steady['fleet_rss_mb_max']:.0f} MB" if steady else NOT_MEASURED),
        "Raft commit p50/p99 (steady)": pair(commit["latency"] if commit else None),
        "apply p50/p99": pair(report.get("small_applies")),
        "dead detected": seconds(churn["detected_ms"]) if churn else NOT_MEASURED,
        "rescheduled": seconds(churn["rescheduled_ms"]) if churn else NOT_MEASURED,
        "outcome": outcome,
    }


def render_table(reports):
    rows = [row(report) for report in sorted(reports, key=lambda r: r["plan"]["hollow_nodes"])]
    columns = list(rows[0])
    lines = ["| " + " | ".join(columns) + " |", "|" + "---|" * len(columns)]
    lines += ["| " + " | ".join(cells[column] for column in columns) + " |" for cells in rows]
    return "\n".join(lines)


def load_reports(directory):
    paths = sorted(Path(directory).glob("hollow-*.json"))
    if not paths:
        raise HollowSummaryError(f"no hollow-*.json reports in {directory}")
    reports = []
    for path in paths:
        try:
            reports.append(json.loads(path.read_text()))
        except json.JSONDecodeError as error:
            raise HollowSummaryError(f"{path}: {error}") from error
    return reports


def main(argv):
    if len(argv) != 2:
        print(__doc__.split("\n\n")[2], file=sys.stderr)
        return 2
    try:
        print(render_table(load_reports(argv[1])))
    except HollowSummaryError as error:
        print(f"hollow_summary: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
