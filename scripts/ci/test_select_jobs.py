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
        for path in ["src/lib.rs", "src/mustard/gossip.rs", "docs/roadmap.md",
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

    def select(self, event="pull_request", base_ref="main", base_sha="", full_ci=False):
        with tempfile.TemporaryDirectory() as scratch:
            output = Path(scratch) / "output"
            env = {**os.environ, **GIT_ENV, "GITHUB_OUTPUT": str(output), "EVENT": event,
                   "BASE_REF": base_ref, "BASE_SHA": base_sha,
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
        for event in ["push", "schedule", "workflow_dispatch"]:
            with self.subTest(event=event):
                self.assertEqual(self.repo.select(event=event),
                                 {"code": True, "gossip": True, "heavy": True})

    def test_a_code_change_into_main_runs_the_heavy_suites(self):
        self.pull_request("src/lib.rs")
        self.assertEqual(self.repo.select(base_sha=self.main),
                         {"code": True, "gossip": False, "heavy": True})

    def test_documentation_alone_skips_every_rust_job(self):
        self.pull_request("docs/roadmap.md", "docs/book/07-ship-it.md")
        self.assertEqual(self.repo.select(base_sha=self.main),
                         {"code": False, "gossip": False, "heavy": False})

    def test_the_manual_counts_as_code_because_relish_compiles_it_in(self):
        self.pull_request("docs/manual/intro.md")
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_the_readme_counts_as_code_because_its_snippets_are_tested(self):
        self.pull_request("README.md")
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_a_gossip_change_runs_the_benchmarks(self):
        self.pull_request("src/mustard/gossip.rs")
        self.assertTrue(self.repo.select(base_sha=self.main)["gossip"])

    def test_deleting_code_counts_as_code(self):
        self.pull_request(delete=["src/lib.rs"])
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_moving_code_into_the_docs_still_counts_as_code(self):
        # Git's rename detection lists only the destination, a docs path.
        self.pull_request(rename=("src/lib.rs", "docs/lib.md"))
        self.assertEqual(self.repo.select(base_sha=self.main),
                         {"code": True, "gossip": False, "heavy": True})

    def test_moving_gossip_code_still_runs_the_benchmarks(self):
        self.pull_request(rename=("src/mustard/gossip.rs", "src/gossip.rs"))
        self.assertTrue(self.repo.select(base_sha=self.main)["gossip"])

    def test_a_stacked_pull_request_skips_the_heavy_suites(self):
        base = self.stacked_base("src/lib.rs")
        self.pull_request("src/lib.rs")
        self.assertEqual(self.repo.select(base_ref="fix/base", base_sha=base),
                         {"code": True, "gossip": False, "heavy": False})

    def test_the_full_ci_label_runs_the_heavy_suites_on_a_stacked_pull_request(self):
        base = self.stacked_base()
        self.pull_request("src/lib.rs")
        self.assertTrue(self.repo.select(base_ref="fix/base", base_sha=base,
                                         full_ci=True)["heavy"])

    def test_the_full_ci_label_does_not_make_documentation_heavy(self):
        self.pull_request("docs/roadmap.md")
        self.assertFalse(self.repo.select(base_sha=self.main, full_ci=True)["heavy"])

    def test_retargeting_a_stacked_pull_request_to_main_selects_the_heavy_suites(self):
        # The stacked PR changes only docs; the branch under it changed code.
        # Once GitHub retargets it to main, the diff against main carries both.
        base = self.stacked_base("src/lib.rs")
        self.pull_request("docs/roadmap.md")
        before = self.repo.select(base_ref="fix/base", base_sha=base)
        after = self.repo.select(base_ref="main", base_sha=self.main)
        self.assertEqual(before, {"code": False, "gossip": False, "heavy": False})
        self.assertEqual(after, {"code": True, "gossip": False, "heavy": True})


if __name__ == "__main__":
    unittest.main()
