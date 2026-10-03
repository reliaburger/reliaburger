"""The CI workflow keeps evidence for every suite, and its triggers and gates say what they do."""
from pathlib import Path
import re
import subprocess
import unittest

REPO = Path(__file__).resolve().parents[2]
WORKFLOWS = REPO / ".github" / "workflows"

# Make targets that run nextest and so write a JUnit report under the ci profile.
NEXTEST_TARGETS = re.compile(r"\bmake (coverage|test(?:-[a-z-]+)?)(?![\w-])")
NOT_NEXTEST = {"test-doc", "test-images", "test-ci-scripts"}


def jobs(text):
    """{job id: the job's lines}, for the top-level jobs of a workflow."""
    found, current = {}, None
    in_jobs = False
    for line in text.splitlines():
        if line.startswith("jobs:"):
            in_jobs = True
            continue
        if not in_jobs:
            continue
        header = re.match(r"^  ([a-z][a-z0-9-]*):\s*$", line)
        if header:
            current = header.group(1)
            found[current] = []
        elif current is not None:
            found[current].append(line)
    return {name: "\n".join(lines) for name, lines in found.items()}


def steps(job):
    """Each step of a job as its block of text."""
    blocks = re.split(r"\n(?=      - )", job)
    return [block for block in blocks if block.lstrip().startswith("- ")]


def dry_run(target):
    result = subprocess.run(["make", "-n", target], cwd=REPO, capture_output=True, text=True)
    return result.returncode, result.stdout.splitlines()


class LocalGateTests(unittest.TestCase):
    def test_ci_bench_runs_every_step_of_ci(self):
        _, ci = dry_run("ci")
        status, bench = dry_run("ci-bench")
        self.assertEqual(status, 0)
        self.assertEqual([line for line in ci if line not in bench], [])
        self.assertIn("cargo bench --bench gossip", bench)

    def test_ci_full_is_gone(self):
        # It ran fewer checks than `make ci` and none of the suites its name
        # promised.
        status, _ = dry_run("ci-full")
        self.assertNotEqual(status, 0)

    def test_make_ci_checks_the_ci_scripts_and_ignored_test_owners(self):
        _, ci = dry_run("ci")
        self.assertTrue(any("unittest discover -s scripts/ci" in line for line in ci), ci)
        self.assertTrue(any("ignored_owners.py reasons" in line for line in ci), ci)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.jobs = jobs((WORKFLOWS / "ci.yml").read_text())

    def test_every_suite_keeps_its_own_junit_report(self):
        suites = 0
        for name, job in self.jobs.items():
            job_steps = steps(job)
            for index, step in enumerate(job_steps):
                if "keep-junit.sh" in step:
                    continue
                for target in set(NEXTEST_TARGETS.findall(step)) - NOT_NEXTEST:
                    suites += 1
                    with self.subTest(job=name, suite=target):
                        keeps = [later for later in job_steps[index + 1:]
                                 if f"scripts/ci/keep-junit.sh {target} " in later]
                        self.assertTrue(keeps, f"{name}: nothing keeps make {target}'s report")
                        self.assertRegex(keeps[0], r"if: \$\{\{ !cancelled\(\) \}\}|if: always\(\)")
        self.assertGreaterEqual(suites, 8)

    def test_every_job_that_keeps_reports_uploads_them_even_on_failure(self):
        for name, job in self.jobs.items():
            if "scripts/ci/keep-junit.sh" not in job:
                continue
            with self.subTest(job=name):
                uploads = [step for step in steps(job)
                           if "actions/upload-artifact@" in step and "target/junit" in step]
                self.assertEqual(len(uploads), 1, name)
                self.assertIn("if: always()", uploads[0])
                self.assertIn(f"name: junit-{name}", uploads[0])
                self.assertIn("retention-days: 14", uploads[0])
                self.assertIn("if-no-files-found: error", uploads[0])

    def test_the_old_shared_report_path_is_not_uploaded(self):
        for name, job in self.jobs.items():
            with self.subTest(job=name):
                self.assertNotIn("path: target/nextest/ci/junit.xml", job)

    def test_ci_checks_that_every_ci_owned_ignored_test_ran(self):
        evidence = self.jobs["ignored-evidence"]
        for job in ["linux", "cluster", "acceptance"]:
            self.assertIn(job, re.search(r"needs: \[([^\]]*)\]", evidence).group(1))
        self.assertIn("pattern: junit-*", evidence)
        self.assertIn("name: oci-interruption-evidence", evidence)
        self.assertIn("scripts/ci/ignored_owners.py evidence", evidence)

    def test_ci_runs_the_policy_checks_on_every_pull_request(self):
        policy = self.jobs["ci-policy"]
        self.assertNotIn("needs:", policy)
        self.assertIn("make test-ci-scripts", policy)
        self.assertIn("make check-ignored", policy)

    def test_the_crane_client_has_a_provisioned_job(self):
        acceptance = self.jobs["acceptance"]
        self.assertIn("make test-standard-clients", acceptance)
        install = next(step for step in steps(acceptance) if "go-containerregistry" in step)
        self.assertIn("sha256sum --check", install)


class RetargetTests(unittest.TestCase):
    def setUp(self):
        self.ci = (WORKFLOWS / "ci.yml").read_text()
        self.retarget = (WORKFLOWS / "ci-retarget.yml").read_text()

    def test_retargeting_reruns_selection_without_a_push(self):
        self.assertRegex(self.retarget, r"types: \[edited\]")
        # Title and body edits fire `edited` too; only a base change reruns CI.
        self.assertIn("if: github.event.changes.base", self.retarget)
        self.assertIn("uses: ./.github/workflows/ci.yml", self.retarget)

    def test_retargeting_watches_the_same_base_branches_as_ci(self):
        branches = re.compile(r"pull_request:\n(?:\s+#.*\n)*\s+branches: (\[.*\])")
        self.assertEqual(branches.search(self.retarget).group(1),
                         branches.search(self.ci).group(1))

    def test_a_retarget_run_does_not_cancel_the_pull_requests_own_run(self):
        # A called workflow's github.workflow is the caller's name, so the
        # two runs land in different concurrency groups.
        group = re.search(r"concurrency:\n\s+group: (.*)", self.ci).group(1)
        self.assertIn("github.workflow", group)


class ApplianceTests(unittest.TestCase):
    """appliance.yml: lab builds carry a next version and a lab channel for
    the Wyse lab, without loosening how a published build is signed."""

    def setUp(self):
        self.text = (WORKFLOWS / "appliance.yml").read_text()
        self.jobs = jobs(self.text)
        self.image = steps(self.jobs["image"])

    def step(self, name):
        found = [s for s in self.image if f"- name: {name}" in s]
        self.assertEqual(len(found), 1, name)
        return found[0]

    def test_only_main_publishes(self):
        plan = self.jobs["plan"]
        self.assertIn('if [ "$GITHUB_REF" = refs/heads/main ]', plan)
        self.assertIn("needs.plan.outputs.publish == 'true'", self.jobs["sign"])

    def test_the_release_key_stays_in_the_sign_job_which_runs_only_first_party_actions(self):
        for name, job in self.jobs.items():
            with self.subTest(job=name):
                if name == "sign":
                    self.assertEqual(job.count("secrets.RELIABURGER_RELEASE_KEY"), 1)
                else:
                    self.assertNotIn("RELIABURGER_RELEASE_KEY", job)
        actions = re.findall(r"uses: ([^@\s]+)@", self.jobs["sign"])
        self.assertTrue(actions)
        for action in actions:
            self.assertTrue(action.startswith("actions/"), action)

    def test_lab_builds_sign_a_lab_channel_for_the_next_version_with_the_runs_key(self):
        sign = self.step("Sign (throwaway key)")
        self.assertIn("if: env.PUBLISH != 'true'", sign)
        self.assertRegex(sign, r"scripts/release/os_release\.py\"? lab-channel")
        self.assertIn('--version "$NEXT_VERSION"', sign)
        # The channel is checked with the run's public key, and the private
        # key is gone before any test runs.
        self.assertIn("os-channel.json.sig", sign)
        self.assertLess(sign.index("lab-channel"), sign.index('rm -f "$RUNNER_TEMP/spike.key"'))
        self.assertIn('"$RUNNER_TEMP/spike.der"', sign.split("rm -f", 1)[1])

    def test_the_next_version_is_built_for_every_x86_64_lab_build(self):
        build = self.step("Build the next version (x86-64 lab builds)")
        self.assertIn("env.PUBLISH != 'true'", build)
        # Not only when relish was built from source: a dispatch naming a
        # bun release gets a next version too.
        self.assertNotIn("source-bin", build.split("run:")[0])

    def test_the_next_version_is_uploaded_as_a_release_tree(self):
        upload = self.step("Upload the next version")
        self.assertIn("name: appliance-x86_64-next", upload)
        self.assertIn("lab-release", upload)
        self.assertIn("if: env.NEXT_VERSION != ''", upload)
        self.assertIn("lab-release.sh", self.step("Lay out the next version as a release"))

    def test_dispatched_lab_builds_keep_their_artefacts_a_week(self):
        for name in ["Upload the image", "Upload the next version"]:
            with self.subTest(step=name):
                self.assertIn("github.event_name == 'workflow_dispatch' && 7",
                              self.step(name))
        # A published build's artefacts still last three days: they go into
        # its releases within the run.
        self.assertIn("env.PUBLISH == 'true' && 3", self.step("Upload the image"))

    def test_the_boot_test_uses_the_wyse_sized_disk(self):
        boot = self.step("Boot test")
        self.assertIn(". image/tests/disk.sh", boot)
        self.assertIn("disk_from_image", boot)


if __name__ == "__main__":
    unittest.main()
