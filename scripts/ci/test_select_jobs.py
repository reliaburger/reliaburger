"""Fixtures for select-jobs.sh, the script that decides which CI suites a run pays for.

Each test builds a throwaway Git repository, makes the change a pull request
would carry, and reads the decision the script writes to $GITHUB_OUTPUT.
"""
from pathlib import Path
import os
import re
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
                     "docs/manual/intro.md", "README.md", "docs/book/07-ship-it.md",
                     "docs/plans/README.md", "docs/talks/talk.md", "src/grill/apple.rs"]:
            self.write(path, f"{path}\n")
        # A file with macOS-only code behind a cfg, as rustfmt writes it.
        self.write("src/grill/process.rs", '#[cfg(target_os = "macos")]\nfn mac() {}\n')
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

    def outputs(self, event="pull_request", base_ref="main", base_sha="", full_ci=False,
                head_ref="feature", release_gate=False, macos=False, ref="refs/pull/1/merge",
                queue_run="", main_ref="main", draft=False):
        """Every output the script writes, as the strings it wrote."""
        with tempfile.TemporaryDirectory() as scratch:
            output = Path(scratch) / "output"
            env = {**os.environ, **GIT_ENV, "GITHUB_OUTPUT": str(output), "EVENT": event,
                   "BASE_REF": base_ref, "BASE_SHA": base_sha, "HEAD_REF": head_ref,
                   "FULL_CI": "true" if full_ci else "false",
                   "RELEASE_GATE": "true" if release_gate else "false",
                   "MACOS_LABEL": "true" if macos else "false",
                   "GITHUB_REF": ref, "QUEUE_RUN": queue_run, "MAIN_REF": main_ref,
                   "DRAFT": "true" if draft else "false"}
            subprocess.run(["bash", str(SCRIPT)], cwd=self.root, env=env, check=True,
                           capture_output=True, text=True)
            return dict(line.split("=", 1) for line in output.read_text().splitlines())

    def select(self, **kwargs):
        """The six job groups, as booleans. Without a queue run, portable and lint follow code."""
        found = self.outputs(**kwargs)
        assert found["portable"] == found["code"] == found["lint"] and found["tested_by"] == "", found
        return {key: found[key] == "true" for key in ("code", "gossip", "heavy", "task_arrays", "macos", "s3")}


REPO = SCRIPT.parents[2]


def docs_only(repo, path):
    """Whether select-jobs.sh's `is_docs_only` calls `path` documentation."""
    script = SCRIPT.read_text()
    start = script.index("is_docs_only() {")
    function = script[start:script.index("\n}\n", start) + 3]
    done = subprocess.run(["bash", "-c", function + 'is_docs_only "$1"', "is_docs_only", path],
                          cwd=repo.root, capture_output=True, text=True)
    return done.returncode == 0


ALL = {"code": True, "gossip": True, "heavy": True, "task_arrays": True, "macos": True, "s3": True}
NOTHING = {key: False for key in ALL}
# What a merge-queue group with code runs: everything but portable macOS.
QUEUE = dict(ALL, macos=False)


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
                                 {"code": True, "gossip": True, "heavy": True, "task_arrays": True, "macos": True, "s3": True})

    def test_a_code_change_into_main_runs_the_heavy_suites_but_not_macos(self):
        # macOS runs only where it adds something (#1443): the heavy suites
        # still run on every code PR into main.
        self.pull_request("src/lib.rs")
        self.assertEqual(self.repo.select(base_sha=self.main),
                         {"code": True, "gossip": False, "heavy": True, "task_arrays": False, "macos": False, "s3": False})

    def test_documentation_alone_skips_every_rust_job(self):
        self.pull_request("docs/plans/README.md", "docs/talks/talk.md")
        self.assertEqual(self.repo.select(base_sha=self.main),
                         {"code": False, "gossip": False, "heavy": False, "task_arrays": False, "macos": False, "s3": False})

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

    def test_docs_the_suite_or_the_binary_reads_count_as_code(self):
        # The suite reads these, or relish's source bundle packs them; a
        # change to one ran no Rust test until the push after it (#2485).
        for path in ["docs/testing/concept-budget.json", "docs/release-log.md", "docs/testing.md",
                     "docs/book/17-a-million-jobs.md", "docs/book/07-ship-it.md",
                     "docs/qualification/2026-11-06-v0.2.7-exit-test.md", "docs/flakes.md",
                     "docs/design/gossip.md", "CLAUDE.md"]:
            with self.subTest(path=path):
                self.pull_request(path)
                code = self.repo.select(base_sha=self.main)["code"]
                self.repo.git("checkout", "-q", "main")
                self.repo.git("branch", "-q", "-D", "feature")
                self.assertTrue(code)

    def test_no_docs_path_named_by_a_test_is_docs_only(self):
        # Every "docs/..." literal in tests/ and src/ that names a file or
        # directory in the repository, so a new reader can't miss the list.
        literals = set()
        for root in ("tests", "src"):
            for source in (REPO / root).rglob("*.rs"):
                literals.update(re.findall(r'"(docs/[A-Za-z0-9_./-]*)', source.read_text(errors="replace")))
        named = set()
        for literal in literals:
            target = REPO / literal
            if target.is_file():
                named.add(literal)
            elif target.is_dir():
                found = next((path for path in sorted(target.rglob("*")) if path.is_file()), None)
                if found is not None:
                    named.add(found.relative_to(REPO).as_posix())
        self.assertGreater(len(named), 20, "the scan found the docs the suite reads")
        for path in sorted(named):
            with self.subTest(path=path):
                self.assertFalse(docs_only(self.repo, path), f"{path} is read by a test or the binary")

    def test_plans_talks_and_research_stay_documentation(self):
        for path in ["docs/plans/README.md", "docs/talks/kubecon.md", "docs/research/batch.md",
                     "docs/_quarto/book.yml", "CHANGELOG.md"]:
            with self.subTest(path=path):
                self.assertTrue(docs_only(self.repo, path), path)

    def test_the_policy_files_count_as_code_because_the_issue_form_tests_read_them(self):
        for path in ["CONTRIBUTING.md", "SECURITY.md", "AGENTS.md"]:
            with self.subTest(path=path):
                self.pull_request(path)
                self.assertTrue(self.repo.select(base_sha=self.main)["code"])
                self.repo.git("checkout", "-q", "main")
                self.repo.git("branch", "-q", "-D", "feature")

    def test_the_feature_catalogue_counts_as_code_because_the_page_test_reads_it(self):
        self.pull_request("docs/features.toml")
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_the_skill_counts_as_code_because_the_agent_docs_test_reads_it(self):
        self.pull_request("plugins/reliaburger/skills/reliaburger/SKILL.md")
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_the_manual_counts_as_code_because_relish_compiles_it_in(self):
        self.pull_request("docs/manual/intro.md")
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_the_readme_counts_as_code_because_its_snippets_are_tested(self):
        self.pull_request("README.md")
        self.assertTrue(self.repo.select(base_sha=self.main)["code"])

    def test_a_gossip_change_runs_the_benchmarks(self):
        self.pull_request("src/mustard/gossip.rs")
        self.assertTrue(self.repo.select(base_sha=self.main)["gossip"])

    def test_a_scheduler_or_policy_bench_change_runs_the_benchmarks(self):
        # make bench runs the scheduler and policy benchmarks beside gossip's,
        # so a change to them or to what they measure runs it (#2493).
        for path in ["benches/scheduler.rs", "benches/policy.rs", "src/cluster/orchestrate.rs",
                     "src/meat/scheduler.rs", "src/meat/filter.rs", "src/meat/score.rs",
                     "src/meat/quota.rs", "src/meat/cluster_state.rs", "src/policy/check.rs"]:
            with self.subTest(path=path):
                self.pull_request(path)
                gossip = self.repo.select(base_sha=self.main)["gossip"]
                self.repo.git("checkout", "-q", "main")
                self.repo.git("branch", "-q", "-D", "feature")
                self.assertTrue(gossip)

    def test_a_task_array_change_runs_the_task_array_benchmarks(self):
        for path in ["src/bun/task_array_leader.rs", "src/bun/task_executor.rs",
                     "src/meat/task_array_state.rs", "src/meat/index_set.rs",
                     "benches/task_arrays.rs", "scripts/ci/bench_baseline.py"]:
            with self.subTest(path=path):
                repo = Repository(tempfile.mkdtemp(dir=self.tmp.name))
                main = repo.git("rev-parse", "HEAD")
                repo.branch("feature")
                repo.write(path, "changed\n")
                repo.commit("change")
                self.assertEqual(repo.select(base_sha=main),
                                 {"code": True, "gossip": False, "heavy": True, "task_arrays": True, "macos": False, "s3": False})

    def test_an_s3_client_change_runs_the_s3_round_trip(self):
        # #1534: what `make test-s3` sends to its server, and the script
        # that starts the server.
        for path in ["src/ketchup/export.rs", "src/council/backup.rs",
                     "src/object_storage.rs", "scripts/test-s3.sh"]:
            with self.subTest(path=path):
                repo = Repository(tempfile.mkdtemp(dir=self.tmp.name))
                main = repo.git("rev-parse", "HEAD")
                repo.branch("feature")
                repo.write(path, "changed\n")
                repo.commit("change")
                self.assertEqual(repo.select(base_sha=main),
                                 {"code": True, "gossip": False, "heavy": True, "task_arrays": False,
                                  "macos": False, "s3": True})

    def test_an_s3_change_into_a_train_waits_for_the_push_unless_labelled_full_ci(self):
        base = self.stacked_base()
        self.pull_request("src/object_storage.rs")
        self.assertFalse(self.repo.select(base_ref="release-1-1-4", base_sha=base)["s3"])
        self.assertTrue(self.repo.select(base_ref="release-1-1-4", base_sha=base, full_ci=True)["s3"])

    def test_other_code_does_not_run_the_s3_round_trip(self):
        self.pull_request("src/lib.rs", "src/ketchup/mod.rs")
        self.assertFalse(self.repo.select(base_sha=self.main)["s3"])

    def manifests(self, criterion, tokio):
        self.repo.write("Cargo.toml",
                        f'[dev-dependencies]\ncriterion = "{criterion}"\ntokio = "{tokio}"\n')
        self.repo.write("Cargo.lock",
                        f'[[package]]\nname = "criterion"\nversion = "{criterion}"\n\n'
                        f'[[package]]\nname = "criterion-plot"\nversion = "0.5.0"\n\n'
                        f'[[package]]\nname = "tokio"\nversion = "{tokio}"\n')

    def test_a_criterion_bump_runs_both_benchmarks(self):
        # #1151: the benchmarks' output format and filter syntax are
        # criterion's, so bumping it must run the jobs that parse them.
        self.manifests("0.5.1", "1.40.0")
        main = self.repo.commit("manifests")
        self.repo.branch("feature")
        self.manifests("0.8.0", "1.40.0")
        self.repo.commit("bump criterion")
        selected = self.repo.select(base_sha=main)
        self.assertTrue(selected["gossip"])
        self.assertTrue(selected["task_arrays"])
        # A full-ci PR into a train selects them too.
        selected = self.repo.select(base_ref="release-0-2-3", base_sha=main, full_ci=True)
        self.assertTrue(selected["gossip"])
        self.assertTrue(selected["task_arrays"])

    def test_a_criterion_bump_in_the_lockfile_alone_runs_both_benchmarks(self):
        self.manifests("0.5.1", "1.40.0")
        main = self.repo.commit("manifests")
        self.repo.branch("feature")
        self.repo.write("Cargo.lock",
                        '[[package]]\nname = "criterion"\nversion = "0.5.2"\n\n'
                        '[[package]]\nname = "criterion-plot"\nversion = "0.5.0"\n\n'
                        '[[package]]\nname = "tokio"\nversion = "1.40.0"\n')
        self.repo.commit("update criterion")
        selected = self.repo.select(base_sha=main)
        self.assertTrue(selected["gossip"])
        self.assertTrue(selected["task_arrays"])

    def test_another_dependency_bump_does_not_run_the_benchmarks(self):
        self.manifests("0.5.1", "1.40.0")
        main = self.repo.commit("manifests")
        self.repo.branch("feature")
        self.manifests("0.5.1", "1.41.0")
        self.repo.commit("bump tokio")
        selected = self.repo.select(base_sha=main)
        self.assertFalse(selected["gossip"])
        self.assertFalse(selected["task_arrays"])

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
                         {"code": True, "gossip": False, "heavy": True, "task_arrays": False, "macos": False, "s3": False})

    def test_moving_gossip_code_still_runs_the_benchmarks(self):
        self.pull_request(rename=("src/mustard/gossip.rs", "src/gossip.rs"))
        self.assertTrue(self.repo.select(base_sha=self.main)["gossip"])

    def test_a_stacked_pull_request_skips_the_heavy_suites_and_macos(self):
        base = self.stacked_base("src/lib.rs")
        self.pull_request("src/lib.rs")
        self.assertEqual(self.repo.select(base_ref="fix/base", base_sha=base),
                         {"code": True, "gossip": False, "heavy": False, "task_arrays": False, "macos": False, "s3": False})

    def test_a_pull_request_into_a_release_train_runs_only_the_light_set(self):
        # Each release collects its PRs on a `release-*` branch. The PR gets
        # lint, MSRV and portable Linux in about 15 minutes; the merge queue
        # and the push to the train run everything else on the merged result.
        base = self.stacked_base("src/lib.rs")
        self.pull_request("src/lib.rs", "src/mustard/gossip.rs", "benches/task_arrays.rs")
        self.assertEqual(self.repo.select(base_ref="release-1-1-4", base_sha=base),
                         {"code": True, "gossip": False, "heavy": False, "task_arrays": False, "macos": False, "s3": False})

    def test_the_full_ci_label_on_a_release_train_pull_request_runs_the_heavy_suites_but_not_macos(self):
        # Most train PRs need full-ci for privileged Linux, cluster and
        # acceptance; three macOS runners each time queued for over an hour.
        # The merge queue and the push to the train still run macOS.
        base = self.stacked_base()
        self.pull_request("src/lib.rs", "src/mustard/gossip.rs")
        self.assertEqual(self.repo.select(base_ref="release-1-1-4", base_sha=base, full_ci=True),
                         {"code": True, "gossip": True, "heavy": True, "task_arrays": False, "macos": False, "s3": False})

    def test_the_full_ci_label_runs_the_heavy_suites_but_not_macos_anywhere(self):
        base = self.stacked_base()
        self.pull_request("src/lib.rs")
        for base_ref, base_sha in [("main", self.main), ("fix/base", base)]:
            with self.subTest(base=base_ref):
                self.assertEqual(self.repo.select(base_ref=base_ref, base_sha=base_sha, full_ci=True),
                                 {"code": True, "gossip": False, "heavy": True, "task_arrays": False,
                                  "macos": False, "s3": False})

    def test_the_gate_pr_gets_the_light_set_on_each_push(self):
        # The push to the train has just run everything on the same code;
        # the gate PR's synchronize doesn't run it again (#1443).
        base = self.stacked_base()
        self.pull_request("src/lib.rs", "src/grill/apple.rs", "src/mustard/gossip.rs")
        for base_ref in ["main", "release-1-1-5"]:
            with self.subTest(base=base_ref):
                self.assertEqual(self.repo.select(base_ref=base_ref, base_sha=base, head_ref="release-1-1-4"),
                                 {"code": True, "gossip": False, "heavy": False, "task_arrays": False,
                                  "macos": False, "s3": False})

    def test_the_gate_pr_labelled_release_gate_runs_everything(self):
        base = self.stacked_base()
        self.pull_request("src/lib.rs")
        self.assertEqual(self.repo.select(base_ref="main", base_sha=base, head_ref="release-1-1-4",
                                          release_gate=True),
                         {"code": True, "gossip": True, "heavy": True, "task_arrays": True, "macos": True, "s3": True})

    def test_the_gate_pr_labelled_full_ci_runs_the_heavy_suites_without_macos(self):
        base = self.stacked_base()
        self.pull_request("src/lib.rs")
        self.assertEqual(self.repo.select(base_ref="main", base_sha=base, head_ref="release-1-1-4", full_ci=True),
                         {"code": True, "gossip": False, "heavy": True, "task_arrays": False, "macos": False, "s3": False})

    def test_a_pull_request_touching_macos_code_runs_macos(self):
        # The Apple runtime, any Rust file with macOS-only code, the
        # installers and the demo scripts.
        paths = ["src/grill/apple.rs", "src/grill/process.rs", "docs/website/install.sh",
                 "scripts/release/install.sh.in", "scripts/demo/tour.sh"]
        for base_ref in ["main", "release-1-1-4", "fix/base"]:
            for path in paths:
                with self.subTest(base=base_ref, path=path):
                    repo = Repository(tempfile.mkdtemp(dir=self.tmp.name))
                    main = repo.git("rev-parse", "HEAD")
                    repo.branch("feature")
                    existing = repo.root / path
                    repo.write(path, (existing.read_text() if existing.exists() else "") + "changed\n")
                    repo.commit("change")
                    self.assertTrue(repo.select(base_ref=base_ref, base_sha=main)["macos"])

    def test_removing_macos_code_runs_macos_too(self):
        self.repo.branch("feature")
        self.repo.write("src/grill/process.rs", "fn mac() {}\n")
        self.repo.commit("drop the cfg")
        self.assertTrue(self.repo.select(base_sha=self.main)["macos"])

    def test_a_train_pull_request_touching_macos_code_keeps_the_light_set_otherwise(self):
        base = self.stacked_base()
        self.pull_request("src/grill/apple.rs")
        self.assertEqual(self.repo.select(base_ref="release-1-1-4", base_sha=base),
                         {"code": True, "gossip": False, "heavy": False, "task_arrays": False, "macos": True, "s3": False})

    def test_the_macos_label_runs_macos_on_any_code_pull_request(self):
        base = self.stacked_base()
        self.pull_request("src/lib.rs")
        for base_ref, base_sha, head_ref in [("main", self.main, "feature"), ("release-1-1-4", base, "feature"),
                                             ("fix/base", base, "feature"), ("main", base, "release-1-1-4"),
                                             ("main", self.main, "dependabot/cargo/tokio-1.41.0")]:
            with self.subTest(base=base_ref, head=head_ref):
                self.assertTrue(self.repo.select(base_ref=base_ref, base_sha=base_sha, head_ref=head_ref,
                                                 macos=True)["macos"])

    def test_the_macos_label_does_not_make_documentation_run_rust_jobs(self):
        self.pull_request("docs/plans/README.md")
        self.assertFalse(self.repo.select(base_sha=self.main, macos=True)["macos"])

    def test_dependabot_never_gets_macos(self):
        # The nightly on main covers it (#1443).
        self.repo.branch("dependabot/cargo/tokio-1.41.0")
        self.repo.write("Cargo.lock", "changed\n")
        self.repo.write("src/grill/apple.rs", "changed\n")
        self.repo.commit("bump")
        self.assertEqual(self.repo.select(base_sha=self.main, head_ref="dependabot/cargo/tokio-1.41.0"),
                         {"code": True, "gossip": False, "heavy": True, "task_arrays": False, "macos": False, "s3": False})

    def test_a_branch_merely_named_like_a_train_is_not_one(self):
        base = self.stacked_base()
        self.pull_request("src/lib.rs")
        for head_ref in ["fix/release-1-1-4", "prerelease-1"]:
            with self.subTest(head=head_ref):
                self.assertFalse(self.repo.select(base_ref="release-1-1-4", base_sha=base,
                                                  head_ref=head_ref)["macos"])

    def test_the_merge_queue_runs_everything_but_macos(self):
        # merge_group carries no pull_request fields: no base ref, no label.
        # Its base is the commit the group sits on. Portable macOS took 50
        # minutes to over 2 hours there and set every group's length; the
        # nightly on the newest train runs it instead (macos-nightly.yml).
        self.pull_request("src/lib.rs")
        self.assertEqual(self.repo.select(event="merge_group", base_ref="", head_ref="", base_sha=self.main),
                         QUEUE)

    def test_the_merge_queue_skips_macos_even_for_macos_code(self):
        # The macos label and the macOS-code rule are pull request rules.
        self.pull_request("src/grill/process.rs", "src/grill/apple.rs")
        self.assertEqual(self.repo.select(event="merge_group", base_sha=self.main, macos=True), QUEUE)

    def test_the_merge_queue_runs_every_benchmark_whatever_the_group_touches(self):
        self.pull_request("src/grill/process.rs")
        found = self.repo.select(event="merge_group", base_sha=self.main)
        self.assertTrue(found["gossip"] and found["task_arrays"] and found["s3"], found)

    def test_a_documentation_only_merge_group_runs_no_code_job(self):
        # Its code is its base's, which the run before it tests (#1723).
        self.pull_request("docs/plans/README.md", "docs/talks/talk.md")
        self.assertEqual(self.repo.select(event="merge_group", base_ref="", head_ref="", base_sha=self.main),
                         NOTHING)

    def test_a_merge_group_with_documentation_that_tests_read_runs_everything_but_macos(self):
        self.pull_request("docs/manual/intro.md")
        self.assertEqual(self.repo.select(event="merge_group", base_sha=self.main), QUEUE)

    def test_a_merge_group_without_a_base_runs_everything_but_macos(self):
        self.pull_request("docs/plans/README.md")
        self.assertEqual(self.repo.select(event="merge_group", base_sha=""), QUEUE)

    def test_a_draft_runs_only_the_cheap_set(self):
        # Lint and format (with CI policy and select jobs, which always run);
        # no portable suite, no heavy suite, no macOS. Marking it ready for
        # review starts a run with the full set.
        self.pull_request("src/lib.rs", "src/grill/apple.rs", "src/mustard/gossip.rs")
        for base_ref, labels in [("main", {}), ("release-1-3-3", dict(full_ci=True, macos=True))]:
            with self.subTest(base=base_ref):
                found = self.repo.outputs(base_ref=base_ref, base_sha=self.main, draft=True, **labels)
                self.assertEqual(found, dict({key: "false" for key in ALL}, portable="false", tested_by="",
                                             lint="true"))
        self.assertEqual(self.repo.select(base_sha=self.main, draft=False)["heavy"], True)

    def test_a_0_4_0_branch_runs_only_the_cheap_set_until_its_release_opens(self):
        self.pull_request("src/lib.rs")
        for head_ref in ["0-4-0/gpu-scheduling", "0-4-0/x"]:
            with self.subTest(head=head_ref):
                found = self.repo.outputs(base_sha=self.main, head_ref=head_ref, full_ci=True)
                self.assertEqual(found, dict({key: "false" for key in ALL}, portable="false", tested_by="",
                                             lint="true"))
        # Only the prefix counts.
        self.assertTrue(self.repo.select(base_sha=self.main, head_ref="fix/0-4-0/x")["heavy"])

    def test_a_0_3_4_batch_branch_runs_only_the_cheap_set(self):
        # 0.3.4's batch branches are folded into integration PRs and never
        # get one of their own; one opened by mistake runs the cheap set.
        self.pull_request("src/lib.rs")
        found = self.repo.outputs(base_sha=self.main, head_ref="0-3-4/ci-workflows",
                                  base_ref="release-1-3-4", full_ci=True)
        self.assertEqual(found, dict({key: "false" for key in ALL}, portable="false", tested_by="",
                                     lint="true"))
        self.assertTrue(self.repo.select(base_sha=self.main, head_ref="0-3-4-x")["heavy"])

    def test_a_documentation_only_draft_skips_lint_too(self):
        self.pull_request("docs/plans/README.md")
        found = self.repo.outputs(base_sha=self.main, draft=True)
        self.assertEqual(found["lint"], "false")
        self.assertEqual(found["code"], "false")

    def test_the_merge_queue_ignores_the_draft_rule(self):
        # A merge group has no pull request: DRAFT and HEAD_REF are empty.
        self.pull_request("src/lib.rs")
        self.assertEqual(self.repo.select(event="merge_group", base_sha=self.main, head_ref="", draft=False),
                         QUEUE)


    def test_documentation_into_a_release_train_skips_every_rust_job(self):
        base = self.stacked_base()
        self.pull_request("docs/plans/README.md")
        self.assertEqual(self.repo.select(base_ref="release-1-1-4", base_sha=base),
                         {"code": False, "gossip": False, "heavy": False, "task_arrays": False, "macos": False, "s3": False})

    def test_the_full_ci_label_runs_the_heavy_suites_on_a_stacked_pull_request(self):
        base = self.stacked_base()
        self.pull_request("src/lib.rs")
        self.assertTrue(self.repo.select(base_ref="fix/base", base_sha=base,
                                         full_ci=True)["heavy"])

    def test_the_full_ci_label_does_not_make_documentation_heavy(self):
        self.pull_request("docs/plans/README.md")
        self.assertFalse(self.repo.select(base_sha=self.main, full_ci=True)["heavy"])

    def test_retargeting_a_stacked_pull_request_to_main_selects_the_heavy_suites(self):
        # The stacked PR changes only docs; the branch under it changed code.
        # Once GitHub retargets it to main, the diff against main carries both.
        base = self.stacked_base("src/lib.rs")
        self.pull_request("docs/plans/README.md")
        before = self.repo.select(base_ref="fix/base", base_sha=base)
        after = self.repo.select(base_ref="main", base_sha=self.main)
        self.assertEqual(before, {"code": False, "gossip": False, "heavy": False, "task_arrays": False, "macos": False, "s3": False})
        self.assertEqual(after, {"code": True, "gossip": False, "heavy": True, "task_arrays": False, "macos": False, "s3": False})


class TrainPushTests(unittest.TestCase):
    """A train push the merge queue already tested runs nothing (#1723)."""

    TRAIN = "refs/heads/release-1-3-1"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Repository(self.tmp.name)
        self.repo.write("Cargo.lock", "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n")
        self.repo.write("Cargo.toml", "[package]\nname = \"reliaburger\"\n")
        self.repo.commit("dependencies")
        self.repo.branch("release-1-3-1")
        self.repo.write("src/lib.rs", "fn train() {}\n")
        self.repo.commit("a train merge")

    def tearDown(self):
        self.tmp.cleanup()

    def push(self, **kwargs):
        return self.repo.outputs(**dict(dict(event="push", base_ref="", head_ref="", ref=self.TRAIN), **kwargs))

    def test_a_push_the_queue_passed_runs_no_job(self):
        found = self.push(queue_run="44820647332")
        self.assertEqual(found, dict({key: "false" for key in ALL}, portable="false", tested_by="44820647332",
                                     lint="false"))

    def test_a_train_whose_dependencies_differ_from_mains_still_saves_its_cache(self):
        self.repo.write("Cargo.lock", "[[package]]\nname = \"serde\"\nversion = \"1.0.1\"\n")
        self.repo.commit("bump serde")
        found = self.push(queue_run="7")
        self.assertEqual((found["code"], found["portable"], found["tested_by"]), ("false", "true", "7"))

    def test_the_cargo_config_counts_as_part_of_the_dependency_graph(self):
        self.repo.write(".cargo/config.toml", "[build]\n")
        self.repo.commit("config")
        self.assertEqual(self.push(queue_run="7")["portable"], "true")

    def test_an_unreadable_main_counts_as_different(self):
        self.assertEqual(self.push(queue_run="7", main_ref="origin/absent")["portable"], "true")

    def test_a_push_the_queue_did_not_test_runs_everything(self):
        for queue_run in ["", "not-a-run"]:
            with self.subTest(queue_run=queue_run):
                found = self.push(queue_run=queue_run)
                self.assertEqual(found, dict({key: "true" for key in ALL}, portable="true", tested_by="",
                                             lint="true"))

    def test_a_push_to_main_runs_everything_whatever_it_is_told(self):
        found = self.push(queue_run="7", ref="refs/heads/main")
        self.assertEqual((found["code"], found["tested_by"]), ("true", ""))


if __name__ == "__main__":
    unittest.main()
