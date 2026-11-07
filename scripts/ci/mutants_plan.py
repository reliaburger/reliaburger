"""Plan a cargo-mutants run that fits a CI budget, and report it (#1584).

`plan` reads `cargo mutants --list` output and says how many mutants there
are, which shard of them to test, and the nextest filter that runs only the
unit tests of the top-level modules they're in.

A mutant's test rebuilds the whole crate and then runs the tests, a few
minutes on a hosted runner, so a run tests at most `--max` mutants: a
round-robin shard, spread over every file the diff touches. The filter keeps
the test part short. A unit test lives beside its code, so a mutant in
`src/council/network.rs` runs `council::`'s tests, not all of them. A mutant
that only an integration test would catch then shows as missed, and the
summary says so.

`summary` turns a `mutants.out` directory into the job summary: how many
mutants finished, whether the time budget stopped the run, and one row per
missed mutant, each with a link that opens a pre-filled `missed-mutant`
issue. A person files it; the job never does.
"""
import argparse
import math
import re
import sys
from pathlib import Path
from urllib.parse import quote, urlencode

# "src/council/network.rs:12:5: replace f -> bool with true"
LINE = re.compile(r"^(?P<path>[^:]+\.rs):(?P<line>\d+):\d+: (?P<what>.+)$")
# More rows than this and the summary is a wall nobody reads.
SUMMARY_ROWS = 50
# What a Rust regex treats as syntax; cargo-mutants' --re is one.
REGEX_SYNTAX = set("\\.+*?()|[]{}^$")


def mutants(text):
    """(path, line, description) for each mutant line of cargo-mutants output."""
    found = []
    for line in text.splitlines():
        match = LINE.match(line.strip())
        if match:
            found.append((match.group("path"), int(match.group("line")), match.group("what")))
    return found


def module(path):
    """The top-level library module a source file belongs to, or None.

    None means the file isn't under one module (`src/lib.rs`, a binary), so
    no module filter can be narrower than the whole library.
    """
    parts = Path(path).parts
    if len(parts) < 2 or parts[0] != "src" or parts[1] in ("lib.rs", "bin"):
        return None
    return parts[1].removesuffix(".rs")


def test_filter(paths):
    """A nextest filter for the unit tests of every module in `paths`.

    Empty when any path has no module: then every library test runs.
    """
    modules = {module(path) for path in paths}
    if not modules or None in modules:
        return ""
    names = "|".join(sorted(modules))
    return f"test(/^({names})::/)"


def plan(text, maximum):
    """The count, shard count, tested count and filter for at most `maximum`."""
    found = mutants(text)
    shards = max(1, math.ceil(len(found) / maximum))
    return {
        "count": len(found),
        "shards": shards,
        "tested": math.ceil(len(found) / shards),
        "filter": test_filter(path for path, _, _ in found),
    }


def shell_regex(text):
    """`text` as a single-quoted shell word holding a Rust regex that matches it."""
    escaped = "".join("\\" + char if char in REGEX_SYNTAX else char for char in text)
    return "'" + escaped.replace("'", "'\\''") + "'"


def issue_link(repository, commit, path, line, what):
    """A link to a new `missed-mutant` issue, title and body filled in."""
    source = f"https://github.com/{repository}/blob/{commit}/{path}#L{line}"
    body = "\n".join([
        f"cargo-mutants changed [`{path}` line {line}]({source}) and no test failed:",
        "",
        f"> {what}",
        "",
        "Write a test that fails with this change and passes without it.",
        "Check first whether an integration test in `tests/` already catches it:",
        "CI's mutants job runs only the module's unit tests.",
        "",
        "To check a new test kills it:",
        "",
        "```sh",
        f"cargo mutants --file {path} --re {shell_regex(what)}",
        "```",
        "",
        "See docs/testing.md, \"Mutation testing\".",
    ])
    query = urlencode({"labels": "missed-mutant", "title": f"Missed mutant: {what} ({path})", "body": body},
                      quote_via=quote)
    return f"https://github.com/{repository}/issues/new?{query}"


# The files cargo-mutants appends one line to as each mutant finishes.
OUTCOMES = ("caught", "missed", "timeout", "unviable")


def results(directory):
    """Each outcome's mutants from a `mutants.out` directory; a missing file is none."""
    found = {}
    for outcome in OUTCOMES:
        path = Path(directory) / f"{outcome}.txt"
        found[outcome] = mutants(path.read_text()) if path.is_file() else []
    return found


def summary(outcomes, planned, repository, commit, stopped=False):
    """The job summary for a run, as Markdown.

    `outcomes` maps each of OUTCOMES to its mutants, as `results` reads
    them. `stopped` says the run hit its time budget before the last one.
    """
    missed = outcomes["missed"]
    lines = ["## Mutation testing", ""]
    if planned["count"] == 0:
        lines.append("No mutants in this change.")
        return "\n".join(lines) + "\n"
    finished = sum(len(outcomes[outcome]) for outcome in OUTCOMES)
    counts = ", ".join(f"{len(outcomes[outcome])} {outcome}" for outcome in OUTCOMES)
    lines.append(f"Tested {finished} of {planned['count']} mutants ({counts}); the plan was "
                 f"{planned['tested']}, shard 1 of {planned['shards']}, round-robin.")
    if stopped:
        lines += ["", f"The run stopped at its time budget with {planned['tested'] - finished} "
                  "of the planned mutants untested."]
    if planned["filter"]:
        lines += ["", f"Tests run per mutant: the library's unit tests matching `{planned['filter']}`. "
                  "A mutant only an integration test catches shows as missed here."]
    if not missed:
        return "\n".join(lines) + "\n"
    lines += ["", "| Mutant | Where | |", "| --- | --- | --- |"]
    for path, line, what in missed[:SUMMARY_ROWS]:
        link = issue_link(repository, commit, path, line, what)
        lines.append(f"| `{what}` | `{path}:{line}` | [file an issue]({link}) |")
    if len(missed) > SUMMARY_ROWS:
        lines += ["", f"And {len(missed) - SUMMARY_ROWS} more in the `mutants-out` artefact's `missed.txt`."]
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    planning = commands.add_parser("plan", help="size a run from `cargo mutants --list`")
    planning.add_argument("--list", required=True, type=Path)
    planning.add_argument("--max", required=True, type=int, help="most mutants to test")
    planning.add_argument("--github-output", type=Path, help="append the plan here as key=value lines")
    reporting = commands.add_parser("summary", help="Markdown for the job summary")
    reporting.add_argument("--list", required=True, type=Path)
    reporting.add_argument("--max", required=True, type=int)
    reporting.add_argument("--results", required=True, type=Path, help="the mutants.out directory")
    reporting.add_argument("--stopped", action="store_true", help="the run hit its time budget")
    reporting.add_argument("--repository", required=True, help="owner/name")
    reporting.add_argument("--commit", required=True)
    arguments = parser.parse_args(argv)
    if arguments.max < 1:
        parser.error("--max must be at least 1")
    planned = plan(arguments.list.read_text(), arguments.max)
    if arguments.command == "plan":
        lines = "".join(f"{key}={value}\n" for key, value in planned.items())
        if arguments.github_output:
            with arguments.github_output.open("a") as output:
                output.write(lines)
        sys.stdout.write(lines)
        return
    sys.stdout.write(summary(results(arguments.results), planned, arguments.repository, arguments.commit,
                             arguments.stopped))


if __name__ == "__main__":
    main()
