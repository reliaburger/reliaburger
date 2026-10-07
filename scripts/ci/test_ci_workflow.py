"""The CI workflow keeps evidence for every suite, and its triggers and gates say what they do."""
from pathlib import Path
import re
import shlex
import tempfile
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

    EXPECTED_OWNERS = {
        'portable-linux': ('portable', 'linux'),
        'portable-darwin': ('macos', 'darwin'),
        'optimized-cron': ('contract-boundaries', 'linux'),
        'rootless-linux': ('linux', 'linux'),
        'linux-root-storage': ('linux', 'linux'),
        'oci-interruptions': ('linux', 'linux'),
        'cluster-tests': ('cluster', 'linux'),
        'slow-tests': ('acceptance', 'linux'),
        'upgrade-node': ('acceptance', 'linux'),
        'upgrade-cluster': ('acceptance', 'linux'),
        'standard-clients': ('acceptance', 'linux'),
    }

    def assert_owner_artifacts(self, workflow_jobs):
        """Every fixed owner keeps completion and JUnit under its own identity."""
        for gate, (job_name, host) in self.EXPECTED_OWNERS.items():
            job = workflow_jobs[job_name]
            blocks = steps(job)
            producers = [index for index, step in enumerate(blocks)
                         if ('workflow_adapter.py produce' in step and '--gate ' + gate + ' ' in step)
                         or (gate == 'oci-interruptions'
                             and 'run: scripts/release/qualify-oci-interruptions.sh' in step)]
            self.assertEqual(len(producers), 1, gate)
            index = producers[0]
            producer = blocks[index]
            identity = gate.replace('-', '_')
            self.assertIn('id: ' + identity, producer)
            self.assertIn(identity + '_sha256: ${{ steps.' + identity
                          + '.outputs.' + identity + '_sha256 }}', job)
            if gate != 'oci-interruptions':
                for argument in ('--host ' + host, '--commit "$EXPECTED_CHECKOUT_COMMIT"',
                                 '--run-id "$GITHUB_RUN_ID"', '--attempt "$GITHUB_RUN_ATTEMPT"',
                                 '--github-output "$GITHUB_OUTPUT"'):
                    self.assertIn(argument, producer)
            retains = [step for step in blocks[index + 1:]
                       if 'actions/upload-artifact@' in step
                       and 'name: contract-' + host + '-' + gate + '\n' in step]
            self.assertEqual(len(retains), 1, gate)
            retain = retains[0]
            self.assertIn('if: always()', retain)
            self.assertIn('path: target/contracts/' + host + '/' + gate, retain)
            self.assertIn('retention-days: 14', retain)
            self.assertIn('if-no-files-found: error', retain)
        # Original test owners must be intercepted rather than executed again.
        for job in workflow_jobs.values():
            for step in steps(job):
                self.assertEqual(set(NEXTEST_TARGETS.findall(step)) - NOT_NEXTEST, set())

    def assert_cached_owners_clear_restored_envelopes(self, workflow_jobs):
        """Clear cached generated directories once before any fresh producer."""
        matched = set()
        for name, job in workflow_jobs.items():
            blocks = steps(job)
            caches = [index for index, step in enumerate(blocks)
                      if 'uses: Swatinem/rust-cache@' in step]
            producers = [index for index, step in enumerate(blocks)
                         if 'workflow_adapter.py produce' in step
                         or 'run: scripts/release/qualify-oci-interruptions.sh' in step]
            if not caches or not producers:
                continue
            matched.add(name)
            cleanup = [index for index, step in enumerate(blocks)
                       if 'name: Clear restored execution envelopes' in step]
            self.assertEqual(len(cleanup), 1, name)
            index = cleanup[0]
            self.assertGreater(index, max(caches), name)
            self.assertLess(index, min(producers), name)
            self.assertNotIn('if:', blocks[index], name)
            commands = re.findall(r'^        run: (.+)$', blocks[index], re.M)
            self.assertEqual(commands, ['rm -rf -- target/contracts'], name)
        self.assertEqual(matched, {'portable', 'linux', 'macos'})

    def test_cached_evidence_owners_clear_only_restored_envelopes_before_production(self):
        self.assert_cached_owners_clear_restored_envelopes(self.jobs)
        # rust-cache removes nonartifact files but retains generic directories.
        # Execute the actual workflow's bounded cleanup against that shape.
        block = next(step for step in steps(self.jobs['portable'])
                     if 'name: Clear restored execution envelopes' in step)
        command = re.search(r'^        run: (.+)$', block, re.M).group(1)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stale = root / 'target/contracts/linux/portable-linux'
            stale.mkdir(parents=True)
            artifact = root / 'target/debug/deps/keep-artifact'
            artifact.parent.mkdir(parents=True)
            artifact.write_text('compiled dependency')
            subprocess.run(shlex.split(command), cwd=root, check=True)
            self.assertFalse((root / 'target/contracts').exists())
            self.assertEqual(artifact.read_text(), 'compiled dependency')
            stale.mkdir(parents=True)
            # Within-job duplicate owner admission remains exclusive: there
            # is no cleanup between producers, nor adopted old completion.
            with self.assertRaises(FileExistsError):
                stale.mkdir(parents=True, exist_ok=False)

    def test_missing_early_late_duplicate_conditional_or_broad_cache_cleanup_is_detected(self):
        self.assert_cached_owners_clear_restored_envelopes(self.jobs)
        original = self.jobs['portable']
        blocks = steps(original)
        cleanup = next(step for step in blocks
                       if 'name: Clear restored execution envelopes' in step)
        cache = next(step for step in blocks if 'uses: Swatinem/rust-cache@' in step)
        producer = next(step for step in blocks if 'workflow_adapter.py produce' in step)
        removed = original.replace(cleanup, '')
        changes = [
            removed,
            removed.replace(cache, cleanup + '\n' + cache),
            removed.replace(producer, producer + '\n' + cleanup),
            original.replace(cleanup, cleanup + '\n' + cleanup),
            original.replace('run: rm -rf -- target/contracts', 'run: rm -rf -- target'),
            original.replace('run: rm -rf -- target/contracts',
                             'if: steps.cache.outputs.cache-hit == \'true\'\n        run: rm -rf -- target/contracts'),
        ]
        for changed in changes:
            with self.subTest(change=changed), self.assertRaises(AssertionError):
                self.assert_cached_owners_clear_restored_envelopes(dict(self.jobs, portable=changed))

    def assert_root_storage_evidence_restored_for_upload(self, workflow_jobs):
        """Privileged producer stays private; its completed receipts are readable."""
        blocks = steps(workflow_jobs['linux'])
        producers = [index for index, step in enumerate(blocks)
                     if 'workflow_adapter.py produce' in step
                     and '--gate linux-root-storage ' in step]
        self.assertEqual(len(producers), 1)
        self.assertIn('sudo -E env PATH=', blocks[producers[0]])
        restores = [index for index, step in enumerate(blocks)
                    if 'name: Restore root-storage evidence ownership' in step]
        self.assertEqual(len(restores), 1)
        index = restores[0]
        uploads = [number for number, step in enumerate(blocks)
                   if 'name: contract-linux-linux-root-storage\n' in step]
        self.assertEqual(len(uploads), 1)
        self.assertGreater(index, producers[0])
        self.assertLess(index, uploads[0])
        self.assertIn('if: always()', blocks[index])
        expected = ('        run: |\n'
                    '          if [ -d target/contracts/linux/linux-root-storage ]; then\n'
                    '            sudo chown -R "$(id -u):$(id -g)" -- target/contracts/linux/linux-root-storage\n'
                    '          fi')
        self.assertIn(expected, blocks[index])
        self.assertNotIn('continue-on-error', blocks[producers[0]])
        # The OCI producer/build/receipt parent runs as the runner; sudo is
        # inside only the actual Rust namespace wrapper. Do not broaden chown.
        for name, job in workflow_jobs.items():
            restores = [step for step in steps(job)
                        if 'name: Restore root-storage evidence ownership' in step]
            self.assertEqual(len(restores), 1 if name == 'linux' else 0)

    def test_privileged_private_evidence_is_restored_before_always_upload(self):
        self.assert_root_storage_evidence_restored_for_upload(self.jobs)
        owner = (REPO / 'scripts/ci/make_owner.py').read_text()
        self.assertIn('directory.mkdir(mode=0o700, parents=True, exist_ok=False)', owner)

    def test_missing_early_late_conditional_or_broad_root_evidence_restore_is_detected(self):
        self.assert_root_storage_evidence_restored_for_upload(self.jobs)
        original = self.jobs['linux']
        blocks = steps(original)
        restore = next(step for step in blocks
                       if 'name: Restore root-storage evidence ownership' in step)
        producer = next(step for step in blocks
                        if 'workflow_adapter.py produce' in step
                        and '--gate linux-root-storage ' in step)
        upload = next(step for step in blocks
                      if 'name: contract-linux-linux-root-storage\n' in step)
        removed = original.replace(restore, '')
        changes = [
            removed,
            removed.replace(producer, restore + '\n' + producer),
            removed.replace(upload, upload + '\n' + restore),
            original.replace(restore, restore.replace('if: always()', 'if: success()')),
            original.replace(restore, restore.replace(' -- target/contracts/linux/linux-root-storage',
                                                       ' -- target/contracts')),
            original.replace(restore, restore + '\n' + restore),
        ]
        for changed in changes:
            with self.subTest(change=changed), self.assertRaises(AssertionError):
                self.assert_root_storage_evidence_restored_for_upload(dict(self.jobs, linux=changed))

    def test_every_suite_keeps_its_own_junit_report(self):
        self.assert_owner_artifacts(self.jobs)

    def test_every_job_that_keeps_reports_uploads_them_even_on_failure(self):
        retains = [step for job in self.jobs.values() for step in steps(job)
                   if 'actions/upload-artifact@' in step and 'name: contract-' in step]
        self.assertEqual(len(retains), len(self.EXPECTED_OWNERS))
        for retain in retains:
            self.assertIn('if: always()', retain)
            self.assertIn('retention-days: 14', retain)
            self.assertIn('if-no-files-found: error', retain)

    def test_missing_failed_or_misnamed_owner_upload_is_detected(self):
        original = self.jobs['portable']
        for changed in (original.replace('if: always()', 'if: success()'),
                        original.replace('name: contract-linux-portable-linux', 'name: wrong-owner'),
                        original.replace('portable_linux_sha256: ${{ steps.portable_linux.outputs.portable_linux_sha256 }}',
                                         'portable_linux_sha256: late-unchecked-metadata')):
            with self.subTest(change=changed), self.assertRaises(AssertionError):
                self.assert_owner_artifacts(dict(self.jobs, portable=changed))

    def test_the_old_shared_report_path_is_not_uploaded(self):
        for name, job in self.jobs.items():
            with self.subTest(job=name):
                self.assertNotIn("path: target/nextest/ci/junit.xml", job)

    def test_ci_checks_that_every_ci_owned_ignored_test_ran(self):
        evidence = self.jobs["ignored-evidence"]
        for job in ["linux", "cluster", "acceptance"]:
            self.assertIn(job, re.search(r"needs: \[([^\]]*)\]", evidence).group(1))
        for job in ('portable', 'macos', 'contract-boundaries', 'build-tests', 'changes'):
            self.assertIn(job, re.search(r"needs: \[([^\]]*)\]", evidence).group(1))
        self.assertIn("pattern: contract-*", evidence)
        self.assertIn("merge-multiple: false", evidence)
        self.assertIn("workflow_adapter.py aggregate", evidence)
        for argument in ('--manifest tests/contracts/manifest.json',
                         '--ignored-bindings tests/contracts/ignored-bindings.json',
                         '--needs target/workflow-needs.json', '--commit "$EXPECTED_CHECKOUT_COMMIT"',
                         '--run-id "$GITHUB_RUN_ID"', '--attempt "$GITHUB_RUN_ATTEMPT"'):
            self.assertIn(argument, evidence)
        self.assertIn("${{ toJSON(needs) }}", evidence)
        self.assertIn("always()", evidence)
        self.assertNotIn("scripts/ci/ignored_owners.py evidence", evidence)

    def test_ci_runs_the_policy_checks_on_every_pull_request(self):
        policy = self.jobs["ci-policy"]
        self.assertNotIn("needs:", policy)
        self.assertIn("make test-ci-scripts", policy)
        self.assertIn("make check-ignored", policy)

    def test_the_crane_client_has_a_provisioned_job(self):
        acceptance = self.jobs["acceptance"]
        self.assertIn("--gate standard-clients --host linux", acceptance)
        self.assertLess(acceptance.index('name: Install crane'),
                        acceptance.index('--gate standard-clients --host linux'))
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

    def test_nothing_publishes_on_a_schedule_until_v0_3_0_is_promoted(self):
        # Maintainer, 7 October 2026: the weekly publish stays off until
        # v0.3.0 is promoted, so the train landing on main publishes nothing
        # by itself. A dispatch with `publish` is the only way to publish.
        triggers = self.text.split("\njobs:", 1)[0]
        self.assertNotRegex(triggers, r"(?m)^\s+schedule:")
        self.assertNotRegex(triggers, r"(?m)^\s+- cron:")

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
