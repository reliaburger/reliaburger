"""A candidate build reuses a CI run only when that run passed every job on its exact commit (#1363)."""
import contextlib
import io
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

from ci_reuse import FULL_EVENTS, REQUIRED_JOBS, SKIPPED_JOB, decide, main

REPO = Path(__file__).resolve().parents[2]
CI = REPO / ".github" / "workflows" / "ci.yml"
COMMIT = "a" * 40
OTHER = "b" * 40
REPOSITORY = "reliaburger/reliaburger"


def run(run_id=1, commit=COMMIT, event="push", conclusion="success", repository=REPOSITORY):
    return dict(id=run_id, head_sha=commit, event=event, status="completed", conclusion=conclusion,
                path=".github/workflows/ci.yml", run_attempt=1,
                repository=dict(full_name=repository), head_repository=dict(full_name=repository))


def green_jobs():
    return [dict(name=name, conclusion="success") for name in REQUIRED_JOBS] + [
        dict(name=SKIPPED_JOB, conclusion="skipped")]


def with_job(name, conclusion):
    return [dict(job, conclusion=conclusion) if job["name"] == name else job for job in green_jobs()]


def ci_job_names():
    """Every check name ci.yml's jobs report, with matrices expanded."""
    names = []
    text = CI.read_text()
    jobs = re.split(r"^  (?=[a-z][a-z0-9-]*:\s*$)", text.split("\njobs:\n", 1)[1], flags=re.M)
    for job in filter(None, jobs):
        match = re.search(r"^    name: (.+)$", job, re.M)
        if match is None:
            raise AssertionError(f"ci.yml job has no name: {job.splitlines()[0]}")
        name = match.group(1).strip()
        shards = re.search(r"^        shard: \[([0-9, ]+)\]$", job, re.M)
        if "${{ matrix.shard }}" in name:
            if shards is None:
                raise AssertionError(f"{name} names a shard but has no shard matrix")
            names += [name.replace("${{ matrix.shard }}", shard.strip()) for shard in shards.group(1).split(",")]
        else:
            names.append(name)
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
                run_id, reasons = self.decide([run()], with_job("portable macOS (2/3)", conclusion))
                self.assertIsNone(run_id)
                self.assertIn("portable macOS (2/3)", reasons[0])

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

    def test_one_green_run_among_failed_ones_is_enough(self):
        runs = [run(1, conclusion="failure"), run(2)]
        self.assertEqual(self.decide(runs)[0], 2)


class MainTests(unittest.TestCase):
    def main(self, runs_for, jobs_for=lambda repository, run_id: green_jobs(), commit=COMMIT):
        with tempfile.NamedTemporaryFile("r", suffix=".out") as output, \
                contextlib.redirect_stderr(io.StringIO()):
            status = main(["--repository", REPOSITORY, "--commit", commit, "--github-output", output.name],
                          runs_for=runs_for, jobs_for=jobs_for)
            return status, output.read()

    def test_a_green_run_is_written_out(self):
        self.assertEqual(self.main(lambda repository, commit: [run(7)]), (0, "reuse=true\nci_run=7\n"))

    def test_an_api_error_runs_the_suite(self):
        def broken(repository, commit):
            raise subprocess.CalledProcessError(1, ["gh", "api"])
        self.assertEqual(self.main(broken), (0, "reuse=false\nci_run=\n"))

    def test_an_error_reading_jobs_runs_the_suite(self):
        def broken(repository, run_id):
            raise ValueError("run 7 has 120 jobs; only 100 were read")
        self.assertEqual(self.main(lambda repository, commit: [run(7)], broken),
                         (0, "reuse=false\nci_run=\n"))

    def test_a_short_commit_runs_the_suite(self):
        self.assertEqual(self.main(lambda repository, commit: [run(7, commit="aaaa")], commit="aaaa"),
                         (0, "reuse=false\nci_run=\n"))


if __name__ == "__main__":
    unittest.main()
