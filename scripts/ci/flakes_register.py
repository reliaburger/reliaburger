#!/usr/bin/env python3
"""Check the known-flakes register (docs/flakes.md) against GitHub.

The register's rule: a flake that isn't fixed the day it's seen gets an issue,
and its row links that issue. A row whose status starts with "Open" is still
flaky, so it must link at least one issue, and every issue it links must
still be open. An open row that points at a closed issue has lost its owner.

    flakes_register.py check [--register docs/flakes.md] [--offline]

`--offline` checks only that open rows link an issue. Without it, each linked
issue's state comes from `gh issue view` (GH_TOKEN must be set in CI).

Fixtures: scripts/ci/test_flakes_register.py.
"""
import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import subprocess
import sys

REPOSITORY = "reliaburger/reliaburger"
HEADER = ["Test", "First seen", "Cause", "Status"]
ISSUE_LINK = re.compile(r"https://github\.com/reliaburger/reliaburger/issues/(\d+)")
# A cell boundary is a pipe that Markdown hasn't escaped.
CELL_BOUNDARY = re.compile(r"(?<!\\)\|")


@dataclass
class Row:
    """One row of the register's table, with the line it starts on."""

    line: int
    test: str
    first_seen: str
    cause: str
    status: str

    def is_open(self):
        """The flake isn't fixed yet."""
        return self.status.startswith("Open")

    def issues(self):
        """The issue numbers the status links, in order, without repeats."""
        found = []
        for match in ISSUE_LINK.finditer(self.status):
            number = int(match.group(1))
            if number not in found:
                found.append(number)
        return found


def cells(line):
    return [cell.strip() for cell in CELL_BOUNDARY.split(line.strip())[1:-1]]


def parse(text):
    """Every row of every table whose header is Test | First seen | Cause | Status."""
    rows, in_table = [], False
    for number, line in enumerate(text.splitlines(), start=1):
        if not line.startswith("|"):
            in_table = False
            continue
        row = cells(line)
        if row == HEADER:
            in_table = True
        elif in_table and not all(set(cell) <= set("-:") for cell in row) and len(row) == 4:
            rows.append(Row(number, *row))
    return rows


def missing_issues(rows):
    """A message for each open row that links no issue."""
    return [f"docs/flakes.md:{row.line}: {row.test} is open but links no issue"
            for row in rows if row.is_open() and not row.issues()]


def closed_issues(rows, state_of):
    """A message for each open row that links a closed issue.

    `state_of(number)` returns GitHub's state for an issue ("OPEN" or
    "CLOSED"); each issue is asked about once.
    """
    states, problems = {}, []
    for row in rows:
        if not row.is_open():
            continue
        for number in row.issues():
            if number not in states:
                states[number] = state_of(number)
            if states[number] != "OPEN":
                problems.append(f"docs/flakes.md:{row.line}: {row.test} is open but links "
                                f"#{number}, which is {states[number].lower()}")
    return problems


def github_state(number):
    """The issue's state as `gh` reports it."""
    result = subprocess.run(
        ["gh", "issue", "view", str(number), "--repo", REPOSITORY, "--json", "state", "-q", ".state"],
        capture_output=True, text=True, check=True, timeout=60)
    return result.stdout.strip()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("check", help="report open rows without a live issue")
    check.add_argument("--register", type=Path, default=Path("docs/flakes.md"))
    check.add_argument("--offline", action="store_true", help="don't ask GitHub for issue states")
    arguments = parser.parse_args(argv)

    rows = parse(arguments.register.read_text())
    problems = missing_issues(rows)
    if not arguments.offline:
        problems += closed_issues(rows, github_state)
    for problem in problems:
        print(problem, file=sys.stderr)
    open_rows = sum(1 for row in rows if row.is_open())
    if problems:
        print(f"{len(problems)} problem(s) in the flakes register", file=sys.stderr)
        return 1
    checked = "an issue" if arguments.offline else "an open issue"
    print(f"{open_rows} open row(s) of {len(rows)}, each linking {checked}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
