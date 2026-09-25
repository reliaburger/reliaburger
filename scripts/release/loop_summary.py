#!/usr/bin/env python3
"""Summarise nextest stress loops for the V02 qualification record.

Per job:  loop_summary.py --lane NAME --junit junit.xml [--log loop.log] --output summary.md
Combined: loop_summary.py --combine DIRECTORY --output summary.md

A job writes summary.md plus summary.json beside it. The combined mode reads
every summary.json below DIRECTORY and renders one table. Each row reports
runs, failures and, when nothing failed, the rule-of-three bound: zero
failures in n independent runs bounds the true failure rate below 3/n at 95 %
confidence.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
import xml.etree.ElementTree as ElementTree

# nextest 0.9.145 ends a stress run with, for example,
# "Summary [   0.143s] 3/3 stress run iterations: 3 passed"; the first number
# is the iterations that ran. Its JUnit report holds one testsuite per
# iteration ("binary@stress-N"), so counting testcases counts every run.
ITERATIONS = re.compile(r"Summary \[[^\]]*\]\s+(\d+)(?:/\d+)? stress run iterations?")


def count_junit(path: pathlib.Path) -> dict[str, dict[str, int]]:
    """Runs and failures per test, from every testcase element in a JUnit file."""
    tests: dict[str, dict[str, int]] = {}
    if not path.is_file():
        return tests
    root = ElementTree.parse(path).getroot()
    for case in root.iter("testcase"):
        name = case.get("name", "?")
        binary = case.get("classname", "")
        key = f"{binary}::{name}" if binary else name
        entry = tests.setdefault(key, {"runs": 0, "failures": 0})
        entry["runs"] += 1
        if case.find("failure") is not None or case.find("error") is not None:
            entry["failures"] += 1
    return tests


def stress_iterations(log: pathlib.Path | None) -> int | None:
    """The iteration count nextest reported for the stress run, if any."""
    if log is None or not log.is_file():
        return None
    found = ITERATIONS.findall(log.read_text(errors="replace"))
    return int(found[-1]) if found else None


def bound(runs: int, failures: int) -> str:
    """The 95 % upper bound on the failure rate, or why there isn't one."""
    if runs == 0:
        return "no runs"
    if failures:
        return f"FAILED ({failures}/{runs})"
    return f"< {min(300 / runs, 100):.2f}%"


def render(rows: list[dict]) -> str:
    lines = [
        "| Lane | Test | Runs | Failures | 95% failure-rate bound |",
        "|---|---|---|---|---|",
    ]
    for row in rows:
        lines.append(
            f"| {row['lane']} | `{row['test']}` | {row['runs']} | {row['failures']} | "
            f"{bound(row['runs'], row['failures'])} |"
        )
    if not rows:
        lines.append("| (none) | | 0 | 0 | no runs |")
    return "\n".join(lines) + "\n"


def summarise_job(lane: str, junit: pathlib.Path, log: pathlib.Path | None) -> list[dict]:
    tests = count_junit(junit)
    rows = [
        {"lane": lane, "test": test, "runs": counts["runs"], "failures": counts["failures"]}
        for test, counts in sorted(tests.items())
    ]
    if not rows:
        # A missing JUnit file is never a pass: report the lane with no runs.
        rows.append({"lane": lane, "test": "(no JUnit results)", "runs": 0, "failures": 0})
    iterations = stress_iterations(log)
    for row in rows:
        row["iterations_reported"] = iterations
    return rows


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane")
    parser.add_argument("--junit", type=pathlib.Path)
    parser.add_argument("--log", type=pathlib.Path)
    parser.add_argument("--combine", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    arguments = parser.parse_args(argv)
    arguments.output.parent.mkdir(parents=True, exist_ok=True)

    if arguments.combine:
        rows = []
        for path in sorted(arguments.combine.rglob("summary.json")):
            rows.extend(json.loads(path.read_text()))
        heading = "# V02 loop lanes\n\n"
    elif arguments.lane and arguments.junit:
        rows = summarise_job(arguments.lane, arguments.junit, arguments.log)
        arguments.output.with_suffix(".json").write_text(json.dumps(rows, indent=2) + "\n")
        heading = f"# {arguments.lane}\n\n"
    else:
        parser.error("pass --lane and --junit, or --combine")

    failed = any(row["failures"] or not row["runs"] for row in rows)
    verdict = "FAIL" if failed or not rows else "PASS"
    arguments.output.write_text(
        heading
        + render(rows)
        + f"\nVerdict: **{verdict}**. The bound is the rule of three: 0 failures in n runs "
        "means a failure rate below 3/n at 95% confidence.\n"
    )
    print(arguments.output.read_text())
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
