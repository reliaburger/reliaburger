"""Fixtures for select-jobs.sh, the script that decides which CI suites a run pays for.

Each test builds a throwaway Git repository, makes the change a pull request
would carry, and reads the decision the script writes to $GITHUB_OUTPUT.
"""
from pathlib import Path
import os
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parent / "select-jobs.sh"

GIT_ENV = {
    "GIT_AUTHOR_NAME": "fixture",
    "GIT_AUTHOR_EMAIL": "fixture@example.com",
    "GIT_COMMITTER_NAME": "fixture",
    "GIT_COMMITTER_EMAIL": "fixture@example.com",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_CONFIG_NOSYSTEM": "1",
}


class Repository:
    """A scratch repository with a `main` branch holding a little of everything."""

    def __init__(self, root):
        self.root = Path(root)
        self.git("init", "-q", "-b", "main")
        for path in ["src/lib.rs", "src/mustard/gossip.rs", "src/bun/task_array_leader.rs",
                     "src/bun/task_executor.rs", "benches/task_arrays.rs", "docs/design/gossip.md",
                     "docs/manual/intro.md", "README.md", "docs/book/07-ship-it.md"]:
            self.write(path, f"{path}\n")
        self.commit("base")

    def git(self, *args):
        result = subprocess.run(["git", *args], cwd=self.root, check=True,
                                capture_output=True, text=True,
                                env={**os.environ, **GIT_ENV})
        return result.stdout.strip()

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def branch(self, name):
        self.git("checkout", "-q", "-b", name)

    def select(self, event="pull_request", base_ref="main", base_sha="", full_ci=False,
               head_ref="feature"):
        with tempfile.TemporaryDirectory() as scratch:
            output = Path(scratch) / "output"
            env = {**os.environ, **GIT_ENV, "GITHUB_OUTPUT": str(output), "EVENT": event,
                   "BASE_REF": base_ref, "BASE_SHA": base_sha, "HEAD_REF": head_ref,
                   "FULL_CI": "true" if full_ci else "false"}
            subprocess.run(["bash", str(SCRIPT)], cwd=self.root, env=env, check=True,
                           capture_output=True, text=True)
            pairs = (line.split("=", 1) for line in output.read_text().split())
            return {key: value == "true" for key, value in pairs}


class SelectJobsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Repository(self.tmp.name)
        self.main = self.repo.git("rev-parse", "HEAD")

    def tearDown(self):
        self.tmp.cleanup()

    def pull_request(self, *changes, delete=(), rename=None):
        self.repo.branch("feature")
        for path in changes:
            self.repo.write(path, "changed\n")
        for path in delete:
            self.repo.git("rm", "-q", path)
        if rename:
            self.repo.git("mv", *rename)
        self.repo.commit("change")

    def stacked_base(self, *changes):
        self.repo.branch("fix/base")
        for path in changes:
            self.repo.write(path, "base fix\n")
        return self.repo.commit("base fix")

    def test_pushes_schedules_and_manual_runs_select_everything(self):
        for event in ["push", "schedule", "workflow_dispatch", "merge_group"]:
            with self.subTest(event=event):
                self.assertEqual(self.repo.select(event=event),
                                 {"code": True, "gossip": True, "heavy": True, "task_arrays": True, "macos": True})

    def test_a_code_change_into_main_runs_the_heavy_suites(self):
        self.pull_request("src/lib.rs")
        self.assertEqual(self.repo.select(base_sha=self.main),
                         {"code": True, "gossip": False, "heavy": True, "task_arrays": False, "macos": True})

    def test_documentation_alone_skips_every_rust_job(self):
        self.pull_request("docs/design/gossip.md", "docs/book/07-ship-it.md")
        self.assertEqual(self.repo.select(base_sha=self.main),
                         {"code": False, "gossip": False, "heavy": False, "task_arrays": False, "macos": False})

    def test_the_website_counts_as_code_because_its_tests_read_it(self):
        self.pull_request("docs/website/assets/tour.cast")
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_docs_the_website_tests_read_count_as_code(self):
        for path in ["docs/roadmap.md", "docs/quickstart.md", "docs/releasing.md"]:
            with self.subTest(path=path):
                self.pull_request(path)
                self.assertTrue(self.repo.select(base_sha=self.main)["code"])
                self.repo.git("checkout", "-q", "main")
                self.repo.git("branch", "-q", "-D", "feature")

    def test_the_manual_counts_as_code_because_relish_compiles_it_in(self):
        self.pull_request("docs/manual/intro.md")
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_the_readme_counts_as_code_because_its_snippets_are_tested(self):
        self.pull_request("README.md")
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_a_gossip_change_runs_the_benchmarks(self):
        self.pull_request("src/mustard/gossip.rs")
        self.assertTrue(self.repo.select(base_sha=self.main)["gossip"])

    def test_a_task_array_change_runs_the_task_array_benchmarks(self):
        for path in ["src/bun/task_array_leader.rs", "src/bun/task_executor.rs",
                     "src/meat/task_array_state.rs", "src/meat/index_set.rs",
                     "benches/task_arrays.rs"]:
            with self.subTest(path=path):
                repo = Repository(tempfile.mkdtemp(dir=self.tmp.name))
                main = repo.git("rev-parse", "HEAD")
                repo.branch("feature")
                repo.write(path, "changed\n")
                repo.commit("change")
                self.assertEqual(repo.select(base_sha=main),
                                 {"code": True, "gossip": False, "heavy": True, "task_arrays": True, "macos": True})

    def test_a_gossip_change_does_not_run_the_task_array_benchmarks(self):
        self.pull_request("src/mustard/gossip.rs")
        self.assertFalse(self.repo.select(base_sha=self.main)["task_arrays"])

    def test_deleting_code_counts_as_code(self):
        self.pull_request(delete=["src/lib.rs"])
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_moving_code_into_the_docs_still_counts_as_code(self):
        # Git's rename detection lists only the destination, a docs path.
        self.pull_request(rename=("src/lib.rs", "docs/lib.md"))
        self.assertEqual(self.repo.select(base_sha=self.main),
                         {"code": True, "gossip": False, "heavy": True, "task_arrays": False, "macos": True})

    def test_moving_gossip_code_still_runs_the_benchmarks(self):
        self.pull_request(rename=("src/mustard/gossip.rs", "src/gossip.rs"))
        self.assertTrue(self.repo.select(base_sha=self.main)["gossip"])

    def test_a_stacked_pull_request_skips_the_heavy_suites(self):
        base = self.stacked_base("src/lib.rs")
        self.pull_request("src/lib.rs")
        self.assertEqual(self.repo.select(base_ref="fix/base", base_sha=base),
                         {"code": True, "gossip": False, "heavy": False, "task_arrays": False, "macos": True})

    def test_a_pull_request_into_a_release_train_runs_only_the_light_set(self):
        # Each release collects its PRs on a `release-*` branch. The PR gets
        # lint, MSRV and portable Linux in about 15 minutes; the merge queue
        # and the push to the train run everything else on the merged result.
        base = self.stacked_base("src/lib.rs")
        self.pull_request("src/lib.rs", "src/mustard/gossip.rs", "benches/task_arrays.rs")
        self.assertEqual(self.repo.select(base_ref="release-1-1-4", base_sha=base),
                         {"code": True, "gossip": False, "heavy": False, "task_arrays": False, "macos": False})

    def test_the_full_ci_label_runs_everything_on_a_release_train_pull_request(self):
        base = self.stacked_base()
        self.pull_request("src/lib.rs", "src/mustard/gossip.rs")
        self.assertEqual(self.repo.select(base_ref="release-1-1-4", base_sha=base, full_ci=True),
                         {"code": True, "gossip": True, "heavy": True, "task_arrays": False, "macos": True})

    def test_a_release_train_heading_anywhere_runs_everything(self):
        # The gate PR (release-* into main) and any PR whose head is a train.
        base = self.stacked_base()
        self.pull_request("src/lib.rs")
        for base_ref in ["main", "release-1-1-5"]:
            with self.subTest(base=base_ref):
                self.assertEqual(self.repo.select(base_ref=base_ref, base_sha=base, head_ref="release-1-1-4"),
                                 {"code": True, "gossip": False, "heavy": True, "task_arrays": False,
                                  "macos": True})

    def test_a_branch_merely_named_like_a_train_is_not_one(self):
        base = self.stacked_base()
        self.pull_request("src/lib.rs")
        for head_ref in ["fix/release-1-1-4", "prerelease-1"]:
            with self.subTest(head=head_ref):
                self.assertFalse(self.repo.select(base_ref="release-1-1-4", base_sha=base,
                                                  head_ref=head_ref)["macos"])

    def test_the_merge_queue_runs_everything(self):
        # merge_group carries no pull_request fields: no base, no label.
        self.assertEqual(self.repo.select(event="merge_group", base_ref="", head_ref=""),
                         {"code": True, "gossip": True, "heavy": True, "task_arrays": True, "macos": True})

    def test_documentation_into_a_release_train_skips_every_rust_job(self):
        base = self.stacked_base()
        self.pull_request("docs/design/gossip.md")
        self.assertEqual(self.repo.select(base_ref="release-1-1-4", base_sha=base),
                         {"code": False, "gossip": False, "heavy": False, "task_arrays": False, "macos": False})

    def test_the_full_ci_label_runs_the_heavy_suites_on_a_stacked_pull_request(self):
        base = self.stacked_base()
        self.pull_request("src/lib.rs")
        self.assertTrue(self.repo.select(base_ref="fix/base", base_sha=base,
                                         full_ci=True)["heavy"])

    def test_the_full_ci_label_does_not_make_documentation_heavy(self):
        self.pull_request("docs/design/gossip.md")
        self.assertFalse(self.repo.select(base_sha=self.main, full_ci=True)["heavy"])

    def test_retargeting_a_stacked_pull_request_to_main_selects_the_heavy_suites(self):
        # The stacked PR changes only docs; the branch under it changed code.
        # Once GitHub retargets it to main, the diff against main carries both.
        base = self.stacked_base("src/lib.rs")
        self.pull_request("docs/design/gossip.md")
        before = self.repo.select(base_ref="fix/base", base_sha=base)
        after = self.repo.select(base_ref="main", base_sha=self.main)
        self.assertEqual(before, {"code": False, "gossip": False, "heavy": False, "task_arrays": False, "macos": False})
        self.assertEqual(after, {"code": True, "gossip": False, "heavy": True, "task_arrays": False, "macos": True})


if __name__ == "__main__":
    unittest.main()
