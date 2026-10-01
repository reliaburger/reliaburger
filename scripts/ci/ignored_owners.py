#!/usr/bin/env python3
"""Give every ignored test an owner, and prove CI's owners ran theirs.

An `#[ignore]`d test runs only when something selects it. Nextest's
`--no-tests=fail` proves a gate's filter matched *something*, not that it
still matches *this* test, so a rename could drop a test from every gate
without a single red line. Two checks close that gap:

  reasons        Every `#[ignore = "..."]` reason names its owner, and the
                 owner exists:
                   make <target>      a Makefile rule that runs ignored tests
                   scripts/<path>     a script in this repository
                   subprocess fixture a parent test in the same file starts it
                   #<number>          a tracking issue (deferred work)
  evidence DIR   Every test owned by a gate that CI runs (a `make` target or
                 script named in .github/workflows/ci.yml) appears in one of
                 the JUnit reports or libtest logs under DIR. Gates CI doesn't
                 run (test-apple, test-gpu, the reboot scripts) are manual and
                 need no evidence.

Fixtures: scripts/ci/test_ignored_owners.py.
"""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import sys
import xml.etree.ElementTree as ElementTree

SOURCE_DIRS = ("src", "tests", "benches")
IGNORE = re.compile(r'^\s*#\[ignore(?:\s*=\s*"(?P<reason>[^"]*)")?\s*\]')
FUNCTION = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(?P<name>\w+)")
MAKE_OWNER = re.compile(r"\bmake (?P<target>[a-z][a-z0-9-]*)")
SCRIPT_OWNER = re.compile(r"\b(?P<path>scripts/[\w./-]+\.(?:sh|py))")
ISSUE_OWNER = re.compile(r"(?<![\w&])#\d+\b")
FIXTURE_OWNER = "subprocess fixture"
# With --nocapture and one thread, libtest prints "test NAME ... " when a
# test starts and its result after the test's own output.
LIBTEST_STARTED = re.compile(r"^test (?P<name>\S+) \.\.\. ", re.MULTILINE)


@dataclass(frozen=True)
class Ignored:
    """One ignored test: where it is, what it's called and why it's ignored."""

    path: str
    line: int
    name: str
    reason: str | None

    def __str__(self):
        return f"{self.path}:{self.line} {self.name}"

    def binary(self):
        """The nextest binary id that holds this test."""
        parts = Path(self.path).parts
        if parts[0] == "src":
            if parts[1] == "bin":
                return f"reliaburger::bin/{Path(self.path).stem}"
            return "reliaburger"
        if parts[0] == "tests" and len(parts) > 2:
            return f"reliaburger::{parts[1]}"
        return f"reliaburger::{Path(self.path).stem}"


def find_ignored(root):
    """Every `#[ignore]` attribute under the Rust source directories."""
    root = Path(root)
    found = []
    for directory in SOURCE_DIRS:
        for path in sorted((root / directory).rglob("*.rs")):
            lines = path.read_text().splitlines()
            for index, line in enumerate(lines):
                match = IGNORE.match(line)
                if not match:
                    continue
                name = next((m.group("name") for m in map(FUNCTION.match, lines[index + 1:]) if m),
                            "<no function>")
                found.append(Ignored(str(path.relative_to(root)), index + 1, name,
                                     match.group("reason")))
    return found


def make_gates(root):
    """Makefile targets whose recipe runs ignored tests."""
    gates, target = set(), None
    for line in (Path(root) / "Makefile").read_text().splitlines():
        rule = re.match(r"^([a-z][a-z0-9-]*):", line)
        if rule:
            target = rule.group(1)
        elif not line.startswith("\t"):
            target = None
        elif target and "--run-ignored" in line:
            gates.add(target)
    return gates


def ci_owners(root):
    """The make targets and scripts that CI runs."""
    workflow = (Path(root) / ".github" / "workflows" / "ci.yml").read_text()
    targets = {m.group("target") for m in MAKE_OWNER.finditer(workflow)}
    scripts = {m.group("path") for m in SCRIPT_OWNER.finditer(workflow)}
    return targets, scripts


def owners(test):
    """The owners a reason names, as ("make", target) or ("script", path)."""
    reason = test.reason or ""
    return ([("make", m.group("target")) for m in MAKE_OWNER.finditer(reason)]
            + [("script", m.group("path")) for m in SCRIPT_OWNER.finditer(reason)])


def owner_problems(root):
    """Ignored tests whose reason names no owner, or an owner that doesn't exist."""
    root = Path(root)
    gates = make_gates(root)
    problems = []
    for test in find_ignored(root):
        if test.reason is None:
            problems.append(f"{test}: #[ignore] has no reason; name the gate that runs it")
            continue
        named = owners(test)
        for kind, owner in named:
            if kind == "make" and owner not in gates:
                problems.append(f"{test}: `make {owner}` is not a Makefile target "
                                "that runs ignored tests")
            if kind == "script" and not (root / owner).is_file():
                problems.append(f"{test}: {owner} does not exist")
        if named or ISSUE_OWNER.search(test.reason):
            continue
        if FIXTURE_OWNER in test.reason:
            source = (root / test.path).read_text()
            if source.count(test.name) < 2:
                problems.append(f"{test}: a subprocess fixture, but nothing in "
                                f"{test.path} starts it")
            continue
        problems.append(f"{test}: reason {test.reason!r} names no owner (`make <gate>`, "
                        "`scripts/<path>`, `subprocess fixture` or `#<issue>`)")
    return problems


def executed(reports):
    """(binary id, test name) for every test in the JUnit reports and libtest logs."""
    seen, problems = set(), []
    reports = Path(reports)
    xml_files = sorted(reports.rglob("*.xml"))
    if not xml_files:
        problems.append(f"no JUnit reports under {reports}")
    for path in xml_files:
        try:
            tree = ElementTree.parse(path)
        except ElementTree.ParseError as error:
            problems.append(f"{path.relative_to(reports)}: not a JUnit report ({error})")
            continue
        for suite in tree.iter("testsuite"):
            for case in suite.iter("testcase"):
                seen.add((suite.get("name"), case.get("name")))
    for path in sorted(reports.rglob("*.log")):
        # Cargo names test executables <target>-<hash>.
        binary = "reliaburger::" + re.sub(r"-[0-9a-f]+$", "", path.stem)
        for match in LIBTEST_STARTED.finditer(path.read_text(errors="replace")):
            seen.add((binary, match.group("name")))
    return seen, problems


def ran(test, seen):
    binary = test.binary()
    return any(suite == binary and (case == test.name or case.endswith("::" + test.name))
               for suite, case in seen)


def evidence_problems(root, reports):
    """CI-owned ignored tests that no report shows running."""
    targets, scripts = ci_owners(root)
    seen, problems = executed(reports)
    for test in find_ignored(root):
        ci = [owner for kind, owner in owners(test)
              if (kind == "make" and owner in targets) or (kind == "script" and owner in scripts)]
        if ci and not ran(test, seen):
            names = ", ".join(f"make {o}" if not o.startswith("scripts/") else o for o in ci)
            problems.append(f"{test}: owned by {names}, but no CI report shows it running")
    return problems


def main(argv):
    if argv[1:2] == ["reasons"] and len(argv) == 2:
        problems = owner_problems(Path.cwd())
    elif argv[1:2] == ["evidence"] and len(argv) == 3:
        problems = evidence_problems(Path.cwd(), Path(argv[2]))
    else:
        print(f"usage: {argv[0]} reasons | evidence REPORT_DIR", file=sys.stderr)
        return 2
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        print(f"{len(problems)} ignored test(s) without a working owner; "
              "see docs/testing.md#who-runs-an-ignored-test", file=sys.stderr)
        return 1
    print(f"every ignored test has an owner ({argv[1]})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
