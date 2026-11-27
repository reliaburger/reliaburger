"""Opening a train adds it to the merge-queue ruleset, and a dry run writes nothing (#1053)."""
import contextlib
import io
import json
import unittest

from open_train import GhError, RULESET, main, missing_queue_rules, ruleset_with_train, schedule_only

# The ruleset as `gh api repos/reliaburger/reliaburger/rulesets/25015244`
# returned it on 24 November 2026, trimmed to the fields that matter.
RULESET_GET = {
    "id": RULESET,
    "name": "release trains merge queue",
    "target": "branch",
    "source_type": "Repository",
    "source": "reliaburger/reliaburger",
    "enforcement": "active",
    "conditions": {"ref_name": {"exclude": [], "include": [
        "refs/heads/release-1-3-2", "refs/heads/release-1-3-3"]}},
    "rules": [
        {"type": "deletion"},
        {"type": "merge_queue", "parameters": {"merge_method": "MERGE", "max_entries_to_build": 8}},
        {"type": "required_status_checks", "parameters": {"required_status_checks": [
            {"context": "ignored-test evidence", "integration_id": 15368}]}},
    ],
    "node_id": "RRS_x",
    "created_at": "2026-10-13T17:52:19.217+01:00",
    "updated_at": "2026-11-23T22:20:15.151+00:00",
    "bypass_actors": [{"actor_id": None, "actor_type": "OrganizationAdmin", "bypass_mode": "always"}],
    "current_user_can_bypass": "always",
    "_links": {"self": {"href": "https://api.github.com/x"}},
}


class RulesetTests(unittest.TestCase):
    def test_adds_the_train_after_the_existing_includes(self):
        body = ruleset_with_train(RULESET_GET, "release-1-3-4")
        self.assertEqual(body["conditions"]["ref_name"]["include"], [
            "refs/heads/release-1-3-2", "refs/heads/release-1-3-3", "refs/heads/release-1-3-4"])

    def test_keeps_rules_bypass_and_excludes_as_they_were(self):
        body = ruleset_with_train(RULESET_GET, "release-1-3-4")
        for field in ("name", "target", "enforcement", "rules", "bypass_actors"):
            with self.subTest(field=field):
                self.assertEqual(body[field], RULESET_GET[field])
        self.assertEqual(body["conditions"]["ref_name"]["exclude"], [])

    def test_drops_the_read_only_fields(self):
        body = ruleset_with_train(RULESET_GET, "release-1-3-4")
        for field in ("id", "source", "source_type", "node_id", "created_at", "updated_at",
                      "current_user_can_bypass", "_links"):
            with self.subTest(field=field):
                self.assertNotIn(field, body)

    def test_leaves_the_fetched_ruleset_unchanged(self):
        before = json.dumps(RULESET_GET, sort_keys=True)
        ruleset_with_train(RULESET_GET, "release-1-3-4")
        self.assertEqual(json.dumps(RULESET_GET, sort_keys=True), before)

    def test_a_train_already_named_needs_no_edit(self):
        self.assertIsNone(ruleset_with_train(RULESET_GET, "release-1-3-3"))

    def test_branch_rules_without_the_queue_are_reported(self):
        self.assertEqual(missing_queue_rules([]), ["merge_queue", "required_status_checks"])
        self.assertEqual(missing_queue_rules([{"type": "merge_queue"}]), ["required_status_checks"])
        self.assertEqual(missing_queue_rules(
            [{"type": "deletion"}, {"type": "merge_queue"}, {"type": "required_status_checks"}]), [])


class FakeGitHub:
    """Answers `gh` calls for a repo where `branches` exist, and records every call."""

    def __init__(self, branches=("main",), milestones=("0.3.3",), ruleset=RULESET_GET,
                 ahead=0, branch_rules=None, main_workflows=("ci.yml", "fuzz.yml")):
        self.main_workflows = list(main_workflows)
        self.branches = set(branches)
        self.milestones = list(milestones)
        self.ruleset = json.loads(json.dumps(ruleset))
        self.ahead = ahead
        self.branch_rules = branch_rules
        self.calls = []

    def writes(self):
        return [args for args, _ in self.calls if "-X" in args or args[:2] == ["pr", "create"]]

    def __call__(self, args, input_text=None):
        self.calls.append((args, input_text))
        if args[:2] == ["pr", "list"]:
            return ""
        if args[:2] == ["pr", "create"]:
            return "https://github.com/reliaburger/reliaburger/pull/1\n"
        path = next(arg for arg in args[1:] if arg.startswith("repos/"))
        method = args[args.index("-X") + 1] if "-X" in args else "GET"
        if "/branches/" in path and "/rules/" not in path:
            name = path.rsplit("/", 1)[1]
            if name not in self.branches:
                raise GhError("Branch not found")
            return name + "\n"
        if "/commits/" in path:
            return "abc123\n"
        if path.endswith("/git/refs"):
            ref = next(arg for arg in args if arg.startswith("ref="))
            self.branches.add(ref.removeprefix("ref=refs/heads/"))
            return "{}"
        if path.endswith(f"/rulesets/{RULESET}"):
            if method == "PUT":
                self.ruleset.update(json.loads(input_text))
            return json.dumps(self.ruleset)
        if "/rules/branches/" in path:
            if self.branch_rules is not None:
                return json.dumps(self.branch_rules)
            train = "refs/heads/" + path.rsplit("/", 1)[1]
            named = train in self.ruleset["conditions"]["ref_name"]["include"]
            types = ["merge_queue", "required_status_checks"] if named else []
            return json.dumps([{"type": kind} for kind in types])
        if "/milestones" in path:
            if method == "POST":
                self.milestones.append(next(a for a in args if a.startswith("title=")).removeprefix("title="))
                return "{}"
            return "\n".join(self.milestones) + "\n"
        if "/contents/.github/workflows" in path:
            self.assertion_ref = path
            return "\n".join(self.main_workflows) + "\n"
        if "/compare/" in path:
            return f"{self.ahead}\n"
        raise AssertionError(f"unexpected gh call {args}")


# The local tree's workflows, as {file name: text}: one that runs on pushes and
# pull requests, and one that GitHub only ever runs from main.
LOCAL_WORKFLOWS = {
    "ci.yml": "name: CI\non:\n  push:\n  pull_request:\n  schedule:\n    - cron: '0 1 * * *'\njobs: {}\n",
    "fuzz.yml": "name: Fuzz\non:\n  schedule:\n    - cron: '29 2 * * *'\n  workflow_dispatch:\njobs: {}\n",
}


def run(argv, github, workflows=None):
    out = io.StringIO()
    workflows = LOCAL_WORKFLOWS if workflows is None else workflows
    return main(argv, run=github, out=out, workflows=workflows), out.getvalue()


class OpenTrainTests(unittest.TestCase):
    def test_dry_run_makes_no_writes_and_prints_the_ruleset_body(self):
        github = FakeGitHub()
        code, out = run(["release-1-3-4", "--milestone", "0.3.4", "--dry-run"], github)
        self.assertEqual(code, 0)
        self.assertEqual(github.writes(), [])
        self.assertIn('"refs/heads/release-1-3-4"', out)
        self.assertIn(f"would run: gh api -X PUT repos/reliaburger/reliaburger/rulesets/{RULESET}", out)
        self.assertIn("would run: gh api -X POST repos/reliaburger/reliaburger/milestones -f title=0.3.4", out)

    def test_opens_the_branch_queue_and_milestone_in_order(self):
        github = FakeGitHub()
        code, out = run(["release-1-3-4", "--milestone", "0.3.4"], github)
        self.assertEqual(code, 0)
        self.assertEqual([call[3] for call in github.writes()], [
            "repos/reliaburger/reliaburger/git/refs",
            f"repos/reliaburger/reliaburger/rulesets/{RULESET}",
            "repos/reliaburger/reliaburger/milestones",
        ])
        self.assertIn("refs/heads/release-1-3-4", github.ruleset["conditions"]["ref_name"]["include"])
        self.assertIn("0.3.4", github.milestones)
        # No commits yet, so no gate PR: the script says what to run later.
        self.assertIn("once the version bump merges", out)

    def test_from_starts_the_branch_at_the_given_base(self):
        github = FakeGitHub(branches=("main", "release-1-3-3"))
        run(["release-1-3-4", "--milestone", "0.3.4", "--from", "release-1-3-3"], github)
        self.assertIn(["api", "repos/reliaburger/reliaburger/commits/release-1-3-3", "--jq", ".sha"],
                      [args for args, _ in github.calls])

    def test_a_second_run_writes_only_the_gate_pr(self):
        github = FakeGitHub(branches=("main", "release-1-3-3"), milestones=("0.3.3",), ahead=1)
        code, out = run(["release-1-3-3", "--milestone", "0.3.3"], github)
        self.assertEqual(code, 0)
        writes = github.writes()
        self.assertEqual(len(writes), 1)
        self.assertEqual(writes[0][:2], ["pr", "create"])
        self.assertIn("--milestone", writes[0])
        # Ready without the release-gate label, it would fail its required check.
        self.assertIn("--draft", writes[0])

    def test_fails_when_the_branch_still_has_no_queue(self):
        github = FakeGitHub(branch_rules=[{"type": "deletion"}])
        code, _ = run(["release-1-3-4", "--milestone", "0.3.4"], github)
        self.assertEqual(code, 1)
        # It stops before the milestone.
        self.assertNotIn("0.3.4", github.milestones)

    def test_refuses_a_name_that_is_not_a_train(self):
        for argv in (["release-*", "--milestone", "0.3.4"], ["main", "--milestone", "0.3.4"],
                     ["release-1-3-4", "--milestone", "v0.3.4"]):
            with self.subTest(argv=argv), self.assertRaises(SystemExit), \
                    contextlib.redirect_stderr(io.StringIO()):
                run(argv, FakeGitHub())



class ScheduleOnlyWorkflowTests(unittest.TestCase):
    """GitHub runs a scheduled or dispatched workflow from main's copy only (#2481)."""

    NIGHTLY = ("name: macOS nightly\non:\n  schedule:\n    - cron: \"41 1 * * *\"\n"
               "  workflow_dispatch:\n    inputs:\n      ref:\n        type: string\njobs: {}\n")

    def test_a_workflow_with_only_schedule_or_dispatch_triggers_is_schedule_only(self):
        self.assertTrue(schedule_only(self.NIGHTLY))
        self.assertTrue(schedule_only("on:\n  workflow_dispatch:\njobs: {}\n"))
        self.assertTrue(schedule_only("on: [schedule, workflow_dispatch]\njobs: {}\n"))

    def test_a_workflow_a_push_pull_request_or_queue_starts_is_not(self):
        for text in (LOCAL_WORKFLOWS["ci.yml"], "on:\n  merge_group:\n  schedule:\n",
                     "on: push\n", "on:\n  pull_request:\n    branches: [main]\n"):
            with self.subTest(text=text):
                self.assertFalse(schedule_only(text))

    def test_opening_a_train_refuses_a_schedule_only_workflow_missing_from_main(self):
        github = FakeGitHub()
        workflows = {**LOCAL_WORKFLOWS, "macos-nightly.yml": self.NIGHTLY}
        with contextlib.redirect_stderr(io.StringIO()) as err:
            code, out = run(["release-1-3-4", "--milestone", "0.3.4"], github, workflows)
        self.assertEqual(code, 1)
        self.assertIn("macos-nightly.yml", err.getvalue())
        self.assertEqual(github.writes(), [])
        self.assertIn("ref=main", github.assertion_ref)

    def test_a_dry_run_reports_the_missing_workflow_and_goes_on(self):
        github = FakeGitHub()
        workflows = {**LOCAL_WORKFLOWS, "macos-nightly.yml": self.NIGHTLY}
        code, out = run(["release-1-3-4", "--milestone", "0.3.4", "--dry-run"], github, workflows)
        self.assertEqual(code, 0)
        self.assertIn("macos-nightly.yml", out)
        self.assertIn("would run: gh api -X POST repos/reliaburger/reliaburger/milestones", out)
        self.assertEqual(github.writes(), [])

    def test_a_schedule_only_workflow_on_main_passes(self):
        github = FakeGitHub(main_workflows=("ci.yml", "fuzz.yml", "macos-nightly.yml"))
        workflows = {**LOCAL_WORKFLOWS, "macos-nightly.yml": self.NIGHTLY}
        code, out = run(["release-1-3-4", "--milestone", "0.3.4"], github, workflows)
        self.assertEqual(code, 0)
        self.assertIn("every schedule-only workflow is on main", out)

    def test_the_repository_s_own_workflows_are_read_by_default(self):
        from open_train import local_workflows
        self.assertIn("ci.yml", local_workflows())


if __name__ == "__main__":
    unittest.main()
