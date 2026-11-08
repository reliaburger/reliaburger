"""A candidate build reuses a CI run only when that run passed every job on its exact commit (#1363).

It waits for a run on the commit that's still going (#1722), and still runs
the suite on any doubt.
"""
import contextlib
import io
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

import ci_reuse
from ci_reuse import FULL_EVENTS, REQUIRED_JOBS, SKIPPED_JOB, decide, main, wait_for_evidence

REPO = Path(__file__).resolve().parents[2]
CI = REPO / ".github" / "workflows" / "ci.yml"
COMMIT = "a" * 40
OTHER = "b" * 40
REPOSITORY = "reliaburger/reliaburger"


def run(run_id=1, commit=COMMIT, event="push", conclusion="success", repository=REPOSITORY, attempt=1,
        status="completed"):
    return dict(id=run_id, head_sha=commit, event=event, status=status,
                conclusion=conclusion if status == "completed" else None,
                path=".github/workflows/ci.yml", run_attempt=attempt,
                repository=dict(full_name=repository), head_repository=dict(full_name=repository))


def green_jobs():
    return [dict(name=name, conclusion="success") for name in REQUIRED_JOBS] + [
        dict(name=SKIPPED_JOB, conclusion="skipped")]


def with_job(name, conclusion):
    return [dict(job, conclusion=conclusion) if job["name"] == name else job for job in green_jobs()]


def ci_job_names():
    """Every check name ci.yml's jobs report.

    No job has a matrix: one would report a check per entry, and a name
    with `${{ matrix.* }}` in it would never match REQUIRED_JOBS.
    """
    names = []
    text = CI.read_text()
    jobs = re.split(r"^  (?=[a-z][a-z0-9-]*:\s*$)", text.split("\njobs:\n", 1)[1], flags=re.M)
    for job in filter(None, jobs):
        match = re.search(r"^    name: (.+)$", job, re.M)
        if match is None:
            raise AssertionError(f"ci.yml job has no name: {job.splitlines()[0]}")
        names.append(match.group(1).strip())
    return names


class RequiredJobsTests(unittest.TestCase):
    def test_required_jobs_are_every_job_ci_yml_runs(self):
        # A new CI job must be added here, or the candidate could skip it.
        names = ci_job_names()
        self.assertIn(SKIPPED_JOB, names)
        self.assertEqual(sorted(name for name in names if name != SKIPPED_JOB), sorted(REQUIRED_JOBS))

    def test_ignored_test_evidence_is_required(self):
        self.assertIn("ignored-test evidence", REQUIRED_JOBS)

    def test_every_accepted_event_selects_every_job(self):
        for event in FULL_EVENTS:
            with self.subTest(event=event):
                result = subprocess.run(["bash", str(REPO / "scripts/ci/select-jobs.sh")], cwd=REPO,
                                        env=dict(os.environ, EVENT=event, GITHUB_OUTPUT="/dev/null"),
                                        capture_output=True, text=True, check=True)
                self.assertIn("code=true gossip=true heavy=true task_arrays=true macos=true", result.stderr)

    def test_pull_request_runs_are_never_accepted(self):
        # A PR's run tests a merge commit and may skip jobs.
        self.assertNotIn("pull_request", FULL_EVENTS)


class DecideTests(unittest.TestCase):
    def decide(self, runs, jobs=None):
        return decide(COMMIT, REPOSITORY, runs, lambda run_id: green_jobs() if jobs is None else jobs)

    def test_a_green_run_on_the_commit_skips_the_suite(self):
        self.assertEqual(self.decide([run()]), (1, []))

    def test_merge_queue_and_nightly_runs_count(self):
        for event in ("merge_group", "schedule"):
            with self.subTest(event=event):
                self.assertEqual(self.decide([run(event=event)])[0], 1)

    def test_a_failed_cancelled_or_skipped_required_job_runs_the_suite(self):
        for conclusion in ("failure", "cancelled", "skipped", "timed_out", None):
            with self.subTest(conclusion=conclusion):
                run_id, reasons = self.decide([run()], with_job("portable macOS", conclusion))
                self.assertIsNone(run_id)
                self.assertIn("portable macOS", reasons[0])

    def test_a_missing_ignored_test_evidence_runs_the_suite(self):
        jobs = [job for job in green_jobs() if job["name"] != "ignored-test evidence"]
        run_id, reasons = self.decide([run()], jobs)
        self.assertIsNone(run_id)
        self.assertIn("'ignored-test evidence' is missing", reasons[0])

    def test_an_unknown_job_that_did_not_pass_runs_the_suite(self):
        # A job ci.yml gained after this list was written still has to pass.
        run_id, _ = self.decide([run()], green_jobs() + [dict(name="new suite", conclusion="skipped")])
        self.assertIsNone(run_id)

    def test_a_closed_pr_job_that_ran_is_suspicious(self):
        run_id, _ = self.decide([run()], with_job(SKIPPED_JOB, "success"))
        self.assertIsNone(run_id)

    def test_a_failed_run_runs_the_suite(self):
        self.assertIsNone(self.decide([run(conclusion="failure")])[0])

    def test_no_run_runs_the_suite(self):
        run_id, reasons = self.decide([])
        self.assertIsNone(run_id)
        self.assertIn(COMMIT, reasons[0])

    def test_a_run_on_a_different_commit_runs_the_suite(self):
        self.assertIsNone(self.decide([run(commit=OTHER)])[0])

    def test_a_pull_request_run_runs_the_suite(self):
        self.assertIsNone(self.decide([run(event="pull_request")])[0])

    def test_a_run_from_a_fork_runs_the_suite(self):
        self.assertIsNone(self.decide([run(repository="someone/fork")])[0])

    def test_a_run_that_went_green_only_on_a_re_run_runs_the_suite(self):
        # Retries are 0: a failure re-run until it passes is not evidence (#1436).
        run_id, reasons = self.decide([run(attempt=2)])
        self.assertIsNone(run_id)
        self.assertIn("attempt 2", reasons[0])

    def test_a_run_without_an_attempt_number_runs_the_suite(self):
        record = run()
        del record["run_attempt"]
        self.assertIsNone(self.decide([record])[0])

    def test_one_green_run_among_failed_ones_is_enough(self):
        runs = [run(1, conclusion="failure"), run(2)]
        self.assertEqual(self.decide(runs)[0], 2)


class Clock:
    """A fake clock: `sleep` moves time on, and `runs_for` answers from a script of polls."""

    def __init__(self, polls):
        self.polls = list(polls)
        self.time = 0.0
        self.calls = 0

    def now(self):
        return self.time

    def sleep(self, seconds):
        self.time += seconds

    def runs_for(self, repository, commit):
        self.calls += 1
        answer = self.polls[min(self.calls, len(self.polls)) - 1]
        if isinstance(answer, Exception):
            raise answer
        return answer


class WaitTests(unittest.TestCase):
    """The candidate is dispatched seconds after the push to main, while that push's CI runs (#1722)."""

    def wait(self, clock, jobs=None, wait_seconds=7200, poll_seconds=60, grace_seconds=180):
        return wait_for_evidence(COMMIT, REPOSITORY, lambda: clock.runs_for(REPOSITORY, COMMIT),
                                 lambda run_id: green_jobs() if jobs is None else jobs,
                                 wait_seconds=wait_seconds, poll_seconds=poll_seconds,
                                 grace_seconds=grace_seconds, now=clock.now, sleep=clock.sleep)

    def test_a_green_run_already_there_is_reused_without_waiting(self):
        clock = Clock([[run(7)]])
        self.assertEqual(self.wait(clock), (7, [], 0))
        self.assertEqual(clock.calls, 1)

    def test_a_running_push_run_is_waited_for_and_then_reused(self):
        clock = Clock([[run(7, status="in_progress")]] * 30 + [[run(7)]])
        run_id, reasons, waited = self.wait(clock)
        self.assertEqual((run_id, reasons), (7, []))
        self.assertEqual(waited, 30 * 60)

    def test_a_queued_run_counts_as_still_going(self):
        for status in ("queued", "waiting", "pending", "requested", "in_progress"):
            with self.subTest(status=status):
                clock = Clock([[run(7, status=status)], [run(7)]])
                self.assertEqual(self.wait(clock)[0], 7)

    def test_a_run_that_fails_while_waited_for_runs_the_suite_at_once(self):
        clock = Clock([[run(7, status="in_progress")], [run(7, conclusion="failure")]])
        run_id, reasons, waited = self.wait(clock)
        self.assertIsNone(run_id)
        self.assertIn("completed/failure", reasons[0])
        self.assertEqual(clock.calls, 2)

    def test_a_run_that_skipped_a_required_job_runs_the_suite(self):
        clock = Clock([[run(7, status="in_progress")], [run(7)]])
        run_id, reasons, _ = self.wait(clock, jobs=with_job("acceptance", "skipped"))
        self.assertIsNone(run_id)
        self.assertIn("'acceptance' finished as skipped", reasons[0])

    def test_waiting_has_a_deadline_after_which_the_suite_runs(self):
        clock = Clock([[run(7, status="in_progress")]])
        run_id, reasons, waited = self.wait(clock, wait_seconds=600)
        self.assertIsNone(run_id)
        self.assertIn("still in_progress after 10 minutes", reasons[-1])
        self.assertGreaterEqual(waited, 600)
        self.assertLessEqual(waited, 660)

    def test_a_running_pull_request_run_is_not_waited_for(self):
        # A pull request's run may skip jobs and tests a merge commit, so its
        # result could never be reused: waiting for it would only cost time.
        clock = Clock([[run(7, event="pull_request", status="in_progress")]])
        run_id, _, waited = self.wait(clock, grace_seconds=0)
        self.assertIsNone(run_id)
        self.assertEqual(waited, 0)

    def test_a_running_run_on_another_commit_is_not_waited_for(self):
        clock = Clock([[run(7, commit=OTHER, status="in_progress")]])
        self.assertEqual(self.wait(clock, grace_seconds=0)[::2], (None, 0))

    def test_a_push_run_not_created_yet_gets_a_short_grace(self):
        # A build dispatched the moment a merge lands can ask before GitHub
        # has created the push's CI run.
        clock = Clock([[], [], [run(7, status="queued")], [run(7)]])
        self.assertEqual(self.wait(clock)[0], 7)

    def test_no_run_at_all_runs_the_suite_after_the_grace(self):
        clock = Clock([[]])
        run_id, reasons, waited = self.wait(clock, grace_seconds=180)
        self.assertIsNone(run_id)
        self.assertIn(COMMIT, reasons[0])
        self.assertGreaterEqual(waited, 180)
        self.assertLess(waited, 300)

    def test_a_failed_run_ends_the_search_even_inside_the_grace(self):
        clock = Clock([[run(7, conclusion="cancelled")]])
        self.assertIsNone(self.wait(clock)[0])
        self.assertEqual(clock.calls, 1)

    def test_a_failed_run_beside_a_running_one_waits_for_the_running_one(self):
        # A train push cancelled by the next one, and main's push still going.
        clock = Clock([[run(6, conclusion="cancelled"), run(7, status="in_progress")],
                       [run(6, conclusion="cancelled"), run(7)]])
        self.assertEqual(self.wait(clock)[0], 7)

    def test_an_api_error_while_waiting_runs_the_suite(self):
        clock = Clock([[run(7, status="in_progress")], RuntimeError("HTTP 502")])
        with self.assertRaises(RuntimeError):
            self.wait(clock)

    def test_jobs_are_read_only_for_a_run_that_succeeded(self):
        read = []

        def jobs_for(run_id):
            read.append(run_id)
            return green_jobs()
        clock = Clock([[run(6, conclusion="failure"), run(7)]])
        result = wait_for_evidence(COMMIT, REPOSITORY, lambda: clock.runs_for(REPOSITORY, COMMIT), jobs_for,
                                   wait_seconds=60, poll_seconds=60, grace_seconds=0,
                                   now=clock.now, sleep=clock.sleep)
        self.assertEqual(result[0], 7)
        self.assertEqual(read, [7])


class FetchTests(unittest.TestCase):
    def fetch(self, page):
        asked = []
        original = ci_reuse.gh_json
        ci_reuse.gh_json = lambda path: asked.append(path) or page
        try:
            return ci_reuse.fetch_runs(REPOSITORY, COMMIT), asked
        finally:
            ci_reuse.gh_json = original

    def test_runs_still_going_are_listed_too(self):
        # Asking only for completed runs is what made 0.2.6's and 0.2.7's
        # candidates re-run the suite (#1722).
        runs, asked = self.fetch(dict(total_count=1, workflow_runs=[run(7, status="in_progress")]))
        self.assertEqual([r["id"] for r in runs], [7])
        self.assertEqual(asked, [f"repos/{REPOSITORY}/actions/workflows/ci.yml/runs?head_sha={COMMIT}&per_page=100"])
        self.assertNotIn("status=", asked[0])

    def test_runs_beyond_the_first_page_are_an_error(self):
        with self.assertRaises(ValueError):
            self.fetch(dict(total_count=101, workflow_runs=[run(n) for n in range(100)]))


class MainTests(unittest.TestCase):
    def main(self, runs_for, jobs_for=lambda repository, run_id: green_jobs(), commit=COMMIT, clock=None):
        clock = clock or Clock([[]])
        with tempfile.NamedTemporaryFile("r", suffix=".out") as output, \
                tempfile.NamedTemporaryFile("r", suffix=".md") as summary, \
                contextlib.redirect_stderr(io.StringIO()):
            status = main(["--repository", REPOSITORY, "--commit", commit, "--github-output", output.name,
                           "--step-summary", summary.name],
                          runs_for=runs_for, jobs_for=jobs_for, now=clock.now, sleep=clock.sleep)
            self.summary = summary.read()
            return status, output.read()

    def test_a_green_run_is_written_out(self):
        self.assertEqual(self.main(lambda repository, commit: [run(7)]),
                         (0, "reuse=true\nci_run=7\nci_run_attempt=1\n"))

    def test_the_summary_links_the_reused_run(self):
        self.main(lambda repository, commit: [run(7)])
        self.assertIn("reused ci.yml run [7](https://github.com/reliaburger/reliaburger/actions/runs/7), "
                      "attempt 1", self.summary)

    def test_a_run_waited_for_is_reused_and_the_wait_is_reported(self):
        clock = Clock([[run(7, status="in_progress")]] * 5 + [[run(7)]])
        self.assertEqual(self.main(clock.runs_for, clock=clock),
                         (0, "reuse=true\nci_run=7\nci_run_attempt=1\n"))
        self.assertIn("after waiting 5 minutes", self.summary)

    def test_the_summary_says_why_the_suite_runs(self):
        self.main(lambda repository, commit: [run(7, conclusion="failure")])
        self.assertIn("ran in this build", self.summary)
        self.assertIn("completed/failure", self.summary)

    def test_an_api_error_while_waiting_runs_the_suite(self):
        clock = Clock([[run(7, status="in_progress")], subprocess.CalledProcessError(1, ["gh", "api"])])
        self.assertEqual(self.main(clock.runs_for, clock=clock),
                         (0, "reuse=false\nci_run=\nci_run_attempt=\n"))
        self.assertIn("GitHub API error", self.summary)

    def test_an_api_error_runs_the_suite(self):
        def broken(repository, commit):
            raise subprocess.CalledProcessError(1, ["gh", "api"])
        self.assertEqual(self.main(broken), (0, "reuse=false\nci_run=\nci_run_attempt=\n"))

    def test_an_error_reading_jobs_runs_the_suite(self):
        def broken(repository, run_id):
            raise ValueError("run 7 has 120 jobs; only 100 were read")
        self.assertEqual(self.main(lambda repository, commit: [run(7)], broken),
                         (0, "reuse=false\nci_run=\nci_run_attempt=\n"))

    def test_a_short_commit_runs_the_suite(self):
        self.assertEqual(self.main(lambda repository, commit: [run(7, commit="aaaa")], commit="aaaa"),
                         (0, "reuse=false\nci_run=\nci_run_attempt=\n"))


if __name__ == "__main__":
    unittest.main()
