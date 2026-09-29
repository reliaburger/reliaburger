#!/usr/bin/env python3
"""Summarise nextest stress loops for the V02 qualification record.

Per job:  loop_summary.py --lane NAME --junit junit.xml --log loop.log
                          --candidate SHA [--expected-iterations N] --output summary.md
Combined: loop_summary.py --combine DIRECTORY --manifest lanes.json
                          --candidate SHA --output summary.md

A job writes summary.md plus summary.json beside it. The combined mode reads
every summary.json below DIRECTORY and checks it against the lane manifest:
a lane or test the manifest expects but no job reported is incomplete, a lane
the manifest doesn't name is a failure, and so is a row for another candidate.

Every row has a status: pass, fail, skip (only skipped results) or incomplete
(missing results, or fewer runs than the loop's iterations). Only a record
where every row passes is a PASS, and the exit status says the same thing as
the verdict: 0 for PASS, 1 otherwise. A passing row reports the rule-of-three
bound: zero failures in n independent runs bounds the true failure rate below
3/n at 95 % confidence.
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

PASS, FAIL, SKIP, INCOMPLETE = "pass", "fail", "skip", "incomplete"


def count_junit(path: pathlib.Path) -> dict[str, dict[str, int]]:
    """Executed runs, failures and skips per test, from a JUnit file."""
    tests: dict[str, dict[str, int]] = {}
    if not path.is_file():
        return tests
    root = ElementTree.parse(path).getroot()
    for case in root.iter("testcase"):
        name = case.get("name", "?")
        binary = case.get("classname", "")
        key = f"{binary}::{name}" if binary else name
        entry = tests.setdefault(key, {"runs": 0, "failures": 0, "skipped": 0})
        # A skipped testcase never ran, so it isn't a run of any kind.
        if case.find("skipped") is not None:
            entry["skipped"] += 1
            continue
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


def classify(counts: dict[str, int], iterations: int | None, expected: int | None) -> tuple[str, str]:
    """A test's status and, unless it passed, why."""
    runs, failures, skipped = counts["runs"], counts["failures"], counts["skipped"]
    if failures:
        return FAIL, f"{failures} of {runs} runs failed"
    if runs == 0 and skipped:
        return SKIP, f"skipped {skipped} times, never ran"
    if runs == 0:
        return INCOMPLETE, "no runs"
    if skipped:
        return INCOMPLETE, f"skipped in {skipped} iterations"
    if iterations is None:
        return INCOMPLETE, "no stress iteration count in the log"
    if runs < iterations:
        return INCOMPLETE, f"{runs} runs in {iterations} reported iterations"
    if expected is not None and runs < expected:
        return INCOMPLETE, f"{runs} runs, expected {expected}"
    return PASS, ""


def summarise_job(
    lane: str,
    junit: pathlib.Path,
    log: pathlib.Path | None,
    candidate: str,
    expected: int | None,
) -> list[dict]:
    tests = count_junit(junit)
    iterations = stress_iterations(log)
    rows = []
    for test, counts in sorted(tests.items()):
        status, note = classify(counts, iterations, expected)
        rows.append({"lane": lane, "test": test, **counts, "status": status, "note": note})
    if not rows:
        # A missing JUnit file is never a pass: report the lane with no runs.
        rows.append({
            "lane": lane, "test": "(no JUnit results)", "runs": 0, "failures": 0,
            "skipped": 0, "status": INCOMPLETE, "note": f"no JUnit results at {junit}",
        })
    for row in rows:
        row["iterations_reported"] = iterations
        row["candidate"] = candidate
    return rows


def missing(lane: str, test: str, note: str) -> dict:
    return {
        "lane": lane, "test": test, "runs": 0, "failures": 0, "skipped": 0,
        "status": INCOMPLETE, "note": note,
    }


def check_against_manifest(rows: list[dict], manifest: dict, candidate: str) -> list[dict]:
    """Rows plus an incomplete row for every expected lane or test with no
    evidence; rows for unknown lanes or other candidates become failures."""
    expected = {entry["lane"]: entry.get("tests", []) for entry in manifest["lanes"]}
    checked = []
    for row in rows:
        row = dict(row)
        if row["lane"] not in expected:
            row["status"], row["note"] = FAIL, "lane is not in the lane manifest"
        elif row.get("candidate") != candidate:
            row["status"] = FAIL
            row["note"] = f"candidate {row.get('candidate')} is not {candidate}"
        checked.append(row)
    for lane, tests in expected.items():
        reported = {row["test"] for row in rows if row["lane"] == lane}
        if not reported:
            checked.append(missing(lane, "(lane missing)", "no summary for this lane"))
            continue
        for test in tests:
            if test not in reported:
                checked.append(missing(lane, test, "expected test has no results"))
    return checked


def render(rows: list[dict]) -> str:
    lines = [
        "| Lane | Test | Status | Runs | Failures | 95% failure-rate bound | Note |",
        "|---|---|---|---|---|---|---|",
    ]
    for row in rows:
        test = row["test"] if row["test"].startswith("(") else f"`{row['test']}`"
        lines.append(
            f"| {row['lane']} | {test} | {row['status']} | {row['runs']} | {row['failures']} | "
            f"{bound(row['runs'], row['failures'])} | {row.get('note', '')} |"
        )
    if not rows:
        lines.append("| (none) | | incomplete | 0 | 0 | no runs | no lanes |")
    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--lane")
    parser.add_argument("--junit", type=pathlib.Path)
    parser.add_argument("--log", type=pathlib.Path)
    parser.add_argument("--expected-iterations", type=int)
    parser.add_argument("--combine", type=pathlib.Path)
    parser.add_argument("--manifest", type=pathlib.Path)
    parser.add_argument("--candidate", required=True, help="the commit every record must belong to")
    parser.add_argument("--output", type=pathlib.Path, required=True)
    arguments = parser.parse_args(argv)
    arguments.output.parent.mkdir(parents=True, exist_ok=True)

    if arguments.combine:
        if not arguments.manifest:
            parser.error("--combine needs --manifest: the lanes the record must contain")
        rows = []
        for path in sorted(arguments.combine.rglob("summary.json")):
            rows.extend(json.loads(path.read_text()))
        manifest = json.loads(arguments.manifest.read_text())
        rows = check_against_manifest(rows, manifest, arguments.candidate)
        heading = f"# V02 loop lanes\n\nCandidate: `{arguments.candidate}`\n\n"
    elif arguments.lane and arguments.junit:
        rows = summarise_job(
            arguments.lane, arguments.junit, arguments.log,
            arguments.candidate, arguments.expected_iterations,
        )
        arguments.output.with_suffix(".json").write_text(json.dumps(rows, indent=2) + "\n")
        heading = f"# {arguments.lane}\n\nCandidate: `{arguments.candidate}`\n\n"
    else:
        parser.error("pass --lane and --junit, or --combine")

    passed = bool(rows) and all(row["status"] == PASS for row in rows)
    verdict = "PASS" if passed else "FAIL"
    arguments.output.write_text(
        heading
        + render(rows)
        + f"\nVerdict: **{verdict}**. The bound is the rule of three: 0 failures in n runs "
        "means a failure rate below 3/n at 95% confidence.\n"
    )
    print(arguments.output.read_text())
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
