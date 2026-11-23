"""Which merge-queue runs queue_sweep.py cancels, and the workflow that runs it (#1723)."""
from datetime import datetime, timezone
from pathlib import Path
import re
import unittest

import queue_sweep

REPO = Path(__file__).resolve().parents[2]
WORKFLOW = REPO / ".github" / "workflows" / "queue-sweep.yml"
NOW = datetime(2026, 11, 20, 21, 40, tzinfo=timezone.utc)
REPOSITORY = "reliaburger/reliaburger"

# Two of 20 November's groups: 2154 on 09de86c7 was dropped at 21:28 when the
# queue re-formed, and its run kept going until 23:15; 2154 on 4f1aebbe
# replaced it.
DROPPED = "gh-readonly-queue/release-1-3-1/pr-2154-09de86c7f60a32215127ed3c9e2b5de5d9ddae6b"
LIVE = "gh-readonly-queue/release-1-3-1/pr-2154-4f1aebbe6287bf7c01394aa287449f7e4eb43313"


def run(number, branch, status="in_progress", created="2026-11-20T21:23:00Z", event="merge_group"):
    return dict(id=number, head_branch=branch, status=status, created_at=created, event=event)


class FakeApi:
    """The three API calls the sweep makes, over a fixed set of runs and branches."""

    def __init__(self, runs, branches, broken_branches=(), refuse_cancel=()):
        self.runs, self.branches = runs, set(branches)
        self.broken, self.refuse = set(broken_branches), set(refuse_cancel)
        self.cancelled = []

    def __call__(self, path, method="GET"):
        if path.startswith(f"repos/{REPOSITORY}/actions/workflows/ci.yml/runs?"):
            self.query = path
            return dict(total_count=len(self.runs), workflow_runs=self.runs)
        ref = re.fullmatch(rf"repos/{REPOSITORY}/git/ref/heads/(.+)", path)
        if ref:
            if ref.group(1) in self.broken:
                raise RuntimeError("HTTP 502")
            if ref.group(1) not in self.branches:
                raise queue_sweep.NotFound(path)
            return dict(ref="refs/heads/" + ref.group(1))
        cancel = re.fullmatch(rf"repos/{REPOSITORY}/actions/runs/(\d+)/cancel", path)
        if cancel and method == "POST":
            if int(cancel.group(1)) in self.refuse:
                raise RuntimeError("HTTP 409")
            self.cancelled.append(int(cancel.group(1)))
            return None
        raise AssertionError(f"unexpected call {method} {path}")


def sweep(api, own=None):
    log = []
    cancelled = queue_sweep.sweep(REPOSITORY, api, NOW, own, log=log.append)
    return cancelled, log


class Superseded(unittest.TestCase):
    def test_a_running_group_whose_branch_is_gone_is_cancelled(self):
        api = FakeApi([run(1, DROPPED), run(2, LIVE, created="2026-11-20T21:28:00Z")], [LIVE])
        cancelled, log = sweep(api)
        self.assertEqual(cancelled, [1])
        self.assertEqual(api.cancelled, [1])
        self.assertIn("cancelled run 1", log[0])

    def test_it_asks_only_for_ci_merge_group_runs(self):
        api = FakeApi([], [])
        sweep(api)
        self.assertIn("event=merge_group", api.query)

    def test_a_queued_group_whose_branch_is_gone_is_cancelled_before_it_takes_a_runner(self):
        api = FakeApi([run(1, DROPPED, status="queued")], [])
        self.assertEqual(sweep(api)[0], [1])

    def test_groups_still_in_the_queue_keep_running(self):
        api = FakeApi([run(2, LIVE)], [LIVE])
        cancelled, log = sweep(api)
        self.assertEqual(cancelled, [])
        self.assertIn("still in the queue", log[-1])

    def test_a_finished_run_is_left_alone_even_when_its_branch_is_gone(self):
        # A group that merged loses its branch too.
        api = FakeApi([run(1, DROPPED, status="completed")], [])
        self.assertEqual(sweep(api)[0], [])
        self.assertEqual(api.cancelled, [])

    def test_a_run_younger_than_a_minute_is_left_alone(self):
        api = FakeApi([run(1, DROPPED, created="2026-11-20T21:39:30Z")], [])
        self.assertEqual(sweep(api)[0], [])

    def test_only_merge_queue_branches_count(self):
        api = FakeApi([run(1, "release-1-3-1", event="push"),
                       run(2, "0-3-1/some-fix", event="merge_group")], [])
        self.assertEqual(sweep(api)[0], [])

    def test_an_unknown_answer_about_a_branch_keeps_its_run(self):
        api = FakeApi([run(1, DROPPED)], [], broken_branches=[DROPPED])
        cancelled, log = sweep(api)
        self.assertEqual(cancelled, [])
        self.assertTrue(any("could not look up" in line for line in log))

    def test_a_refused_cancel_is_reported_and_the_sweep_goes_on(self):
        other = DROPPED.replace("pr-2154", "pr-2155")
        api = FakeApi([run(1, DROPPED), run(3, other)], [], refuse_cancel=[1])
        cancelled, log = sweep(api)
        self.assertEqual(cancelled, [3])
        self.assertTrue(any("could not cancel run 1" in line for line in log))

    def test_its_own_run_is_never_cancelled(self):
        api = FakeApi([run(7, DROPPED)], [])
        self.assertEqual(sweep(api, own=7)[0], [])


class Command(unittest.TestCase):
    def test_an_api_failure_never_fails_the_workflow(self):
        def broken(path, method="GET"):
            raise RuntimeError("HTTP 500")
        self.assertEqual(queue_sweep.main(["--repository", REPOSITORY], api=broken, now=NOW), 0)

    def test_the_command_cancels_through_the_api(self):
        api = FakeApi([run(1, DROPPED)], [])
        self.assertEqual(queue_sweep.main(["--repository", REPOSITORY], api=api, now=NOW), 0)
        self.assertEqual(api.cancelled, [1])


class Workflow(unittest.TestCase):
    def setUp(self):
        self.text = WORKFLOW.read_text()

    def test_it_sweeps_when_the_queue_starts_a_group_and_when_a_train_moves(self):
        self.assertRegex(self.text, r"\n  merge_group:\n\s+types: \[checks_requested\]\n")
        self.assertRegex(self.text, r"\n  push:\n\s+branches: \[\"release-\*\"\]\n")
        self.assertNotIn("pull_request", self.text)

    def test_only_this_workflow_holds_the_permission_to_cancel(self):
        self.assertRegex(self.text, r"\npermissions:\n  actions: write\n  contents: read\n")
        ci = (REPO / ".github" / "workflows" / "ci.yml").read_text()
        self.assertNotIn("actions: write", ci)

    def test_it_runs_the_sweep_with_the_runs_token_and_a_short_limit(self):
        self.assertIn("GH_TOKEN: ${{ github.token }}", self.text)
        self.assertIn('scripts/ci/queue_sweep.py --repository "$GITHUB_REPOSITORY"', self.text)
        self.assertLessEqual(int(re.search(r"timeout-minutes: (\d+)", self.text).group(1)), 5)


if __name__ == "__main__":
    unittest.main()
