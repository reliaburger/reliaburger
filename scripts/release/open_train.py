#!/usr/bin/env python3
"""Open a release train in one step, merge queue included (#1053).

    open_train.py release-1-3-4 --milestone 0.3.4 --dry-run
    open_train.py release-1-3-4 --milestone 0.3.4 --from release-1-3-3

A train is a branch `release-X-Y-Z` that the release's PRs merge into
(docs/releasing.md, "Opening a train"). Opening one takes four writes to
GitHub, in this order, and each is skipped when it's already done, so a
second run finishes what a failed first run started:

1. Create the branch from `--from` (default `main`).
2. Add `refs/heads/<train>` to the merge-queue ruleset's include list, then
   check that `rules/branches/<train>` lists `merge_queue` and
   `required_status_checks`. GitHub refuses a pattern such as
   `refs/heads/release-*` in a ruleset with a merge queue ("Wildcard ref
   names are not supported when merge queue is enabled", HTTP 422), so
   every train is named. Only a repository admin can edit a ruleset.
3. Create the milestone.
4. Open the gate PR from the train into `main`. GitHub won't open a PR with
   no commits, so on a branch that's still at its base this step prints the
   command to run once the version bump has merged, and the script exits 0.

`--dry-run` reads what it needs, prints every write it would make (the
ruleset's new body included) and makes none.
"""
import argparse
import json
import re
import shlex
import subprocess
import sys

REPOSITORY = "reliaburger/reliaburger"
RULESET = 25015244
TRAIN = re.compile(r"^release-\d+-\d+-\d+$")
MILESTONE = re.compile(r"^\d+\.\d+\.\d+$")
# The fields a ruleset PUT takes; the rest of a GET (id, links, timestamps)
# is read-only.
RULESET_FIELDS = ("name", "target", "enforcement", "conditions", "rules", "bypass_actors")
# What `rules/branches/<train>` must list once the ruleset names the train.
QUEUE_RULES = ("merge_queue", "required_status_checks")


class GhError(Exception):
    """A `gh` call failed."""


def gh(args, input_text=None):
    """Run `gh` with `args` and return its stdout."""
    done = subprocess.run(["gh", *args], input=input_text, capture_output=True, text=True)
    if done.returncode != 0:
        raise GhError(f"gh {' '.join(args)}: {done.stderr.strip()}")
    return done.stdout


def check_train(name):
    """Refuse a branch name that isn't `release-X-Y-Z`."""
    if not TRAIN.match(name):
        raise ValueError(f"a train is named release-X-Y-Z, not {name!r}")


def ruleset_with_train(ruleset, train):
    """The PUT body that adds `train` to the ruleset, or None if it's there.

    Keeps every other include, the excludes, the rules and the bypass
    actors exactly as they were, and drops the GET's read-only fields.
    """
    ref = f"refs/heads/{train}"
    include = ruleset["conditions"]["ref_name"]["include"]
    if ref in include:
        return None
    body = {field: ruleset[field] for field in RULESET_FIELDS if field in ruleset}
    conditions = json.loads(json.dumps(body["conditions"]))
    conditions["ref_name"]["include"] = [*include, ref]
    body["conditions"] = conditions
    return body


def missing_queue_rules(branch_rules):
    """The queue rules `rules/branches/<train>` lacks (empty when it has them)."""
    present = {rule["type"] for rule in branch_rules}
    return [rule for rule in QUEUE_RULES if rule not in present]


def gate_pr_body(train, milestone):
    """The gate PR's first body: what it is, and an empty closing list."""
    return (
        f"The {milestone} merge train, as docs/releasing.md describes it. "
        f"{milestone}'s PRs merge into `{train}`, and this PR takes the train "
        f"into `main` at freeze. Milestone: {milestone}.\n\n"
        "**Closes**, one per issue, added as each PR merges into the train:\n"
    )


class Opener:
    """Opens one train, through `run` (a `gh` caller), writing only when `apply`."""

    def __init__(self, run, train, milestone, base, apply, out=sys.stdout):
        self.run = run
        self.train = train
        self.milestone = milestone
        self.base = base
        self.apply = apply
        self.out = out

    def say(self, text):
        print(text, file=self.out)

    def write(self, args, input_text=None):
        if not self.apply:
            self.say("  would run: " + shlex.join(["gh", *args]))
            if input_text is not None:
                self.say(input_text)
            return None
        return self.run(args, input_text)

    def branch_exists(self, name):
        try:
            self.run(["api", f"repos/{REPOSITORY}/branches/{name}", "--jq", ".name"])
            return True
        except GhError:
            return False

    def create_branch(self):
        if self.branch_exists(self.train):
            self.say(f"1. branch {self.train} exists")
            return
        sha = self.run(["api", f"repos/{REPOSITORY}/commits/{self.base}", "--jq", ".sha"]).strip()
        self.say(f"1. create {self.train} from {self.base} at {sha}")
        self.write(["api", "-X", "POST", f"repos/{REPOSITORY}/git/refs",
                    "-f", f"ref=refs/heads/{self.train}", "-f", f"sha={sha}"])

    def add_to_ruleset(self):
        ruleset = json.loads(self.run(["api", f"repos/{REPOSITORY}/rulesets/{RULESET}"]))
        body = ruleset_with_train(ruleset, self.train)
        if body is None:
            self.say(f"2. ruleset {RULESET} names {self.train}")
        else:
            self.say(f"2. add refs/heads/{self.train} to ruleset {RULESET}")
            self.write(["api", "-X", "PUT", f"repos/{REPOSITORY}/rulesets/{RULESET}", "--input", "-"],
                       json.dumps(body, indent=2))
        if not self.apply:
            return
        rules = json.loads(self.run(["api", f"repos/{REPOSITORY}/rules/branches/{self.train}"]))
        missing = missing_queue_rules(rules)
        if missing:
            raise GhError(f"{self.train} still lacks {', '.join(missing)} after the ruleset edit")
        self.say(f"   {self.train} has {' and '.join(QUEUE_RULES)}")

    def create_milestone(self):
        titles = self.run(["api", "--paginate", f"repos/{REPOSITORY}/milestones?state=all&per_page=100",
                           "--jq", ".[].title"]).splitlines()
        if self.milestone in titles:
            self.say(f"3. milestone {self.milestone} exists")
            return
        self.say(f"3. create milestone {self.milestone}")
        self.write(["api", "-X", "POST", f"repos/{REPOSITORY}/milestones", "-f", f"title={self.milestone}"])

    def open_gate_pr(self):
        existing = self.run(["pr", "list", "-R", REPOSITORY, "--base", "main", "--head", self.train,
                             "--state", "open", "--json", "number", "--jq", ".[].number"]).split()
        if existing:
            self.say(f"4. gate PR #{existing[0]} is open")
            return
        create = ["pr", "create", "-R", REPOSITORY, "--base", "main", "--head", self.train,
                  "--title", f"Release {self.milestone}", "--milestone", self.milestone,
                  "--body", gate_pr_body(self.train, self.milestone)]
        ahead = 0
        if self.branch_exists(self.train):
            ahead = int(self.run(["api", f"repos/{REPOSITORY}/compare/main...{self.train}",
                                  "--jq", ".ahead_by"]).strip())
        if ahead == 0:
            self.say(f"4. {self.train} has nothing beyond main yet: once the version bump merges, run")
            self.say("   " + shlex.join(["gh", *create]))
            return
        self.say(f"4. open the gate PR {self.train} -> main")
        self.write(create)

    def open(self):
        self.create_branch()
        self.add_to_ruleset()
        self.create_milestone()
        self.open_gate_pr()


def main(argv=None, run=gh, out=sys.stdout):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("train", help="the branch, release-X-Y-Z")
    parser.add_argument("--milestone", required=True, help="the release's milestone, e.g. 0.3.4")
    parser.add_argument("--from", dest="base", default="main",
                        help="branch or commit to start from (default main)")
    parser.add_argument("--dry-run", action="store_true", help="print the writes, make none")
    args = parser.parse_args(argv)
    try:
        check_train(args.train)
    except ValueError as error:
        parser.error(str(error))
    if not MILESTONE.match(args.milestone):
        parser.error(f"a milestone is X.Y.Z, not {args.milestone!r}")
    try:
        Opener(run, args.train, args.milestone, args.base, not args.dry_run, out).open()
    except GhError as error:
        print(f"open_train: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
