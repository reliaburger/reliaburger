"""The CI workflow keeps evidence for every suite, and its triggers and gates say what they do."""
from pathlib import Path
import re
import shlex
import tempfile
import subprocess
import unittest

from workflow_assembly import SHARDS

REPO = Path(__file__).resolve().parents[2]
WORKFLOWS = REPO / ".github" / "workflows"

# Make targets that run nextest and so write a JUnit report under the ci profile.
NEXTEST_TARGETS = re.compile(r"\bmake (coverage|test(?:-[a-z-]+)?)(?![\w-])")
NOT_NEXTEST = {"test-doc", "test-images", "test-ci-scripts"}

# The no-op job a closed pull request's run starts instead of CI.
CLOSED_JOB = "closed-pull-request"


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
            # A split owner is one matrix job; each shard sets only its own output.
            count = SHARDS.get(gate)
            suffixes = [''] if count is None else ['_' + str(shard) for shard in range(1, count + 1)]
            for suffix in suffixes:
                self.assertIn(identity + suffix + '_sha256: ${{ steps.' + identity
                              + '.outputs.' + identity + suffix + '_sha256 }}', job)
            label = gate if count is None else gate + '-${{ matrix.shard }}'
            if count is not None:
                self.assertIn('        shard: [' + ', '.join(map(str, range(1, count + 1))) + ']\n', job)
                self.assertIn('--shard "${{ matrix.shard }}/' + str(count) + '"', producer)
                self.assertIn('--directory "target/contracts/' + host + '/' + label + '"', producer)
            if gate != 'oci-interruptions':
                for argument in ('--host ' + host, '--commit "$EXPECTED_CHECKOUT_COMMIT"',
                                 '--run-id "$GITHUB_RUN_ID"', '--attempt "$GITHUB_RUN_ATTEMPT"',
                                 '--github-output "$GITHUB_OUTPUT"'):
                    self.assertIn(argument, producer)
            retains = [step for step in blocks[index + 1:]
                       if 'actions/upload-artifact@' in step
                       and 'name: contract-' + host + '-' + label + '\n' in step]
            self.assertEqual(len(retains), 1, gate)
            retain = retains[0]
            self.assertIn('if: always()', retain)
            self.assertIn('path: target/contracts/' + host + '/' + label + '\n', retain)
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

    def assert_oci_interruptions_bounded(self, workflow_jobs):
        """A hung OCI test fails its own step and leaves evidence behind (#800).

        The step ends well inside the job's limit, so its log and envelope
        are kept. A watcher records free disk and memory throughout, and a
        report afterwards prints the low-water marks, plus every process the
        step left running with its kernel stacks when it didn't succeed.
        """
        job = workflow_jobs['linux']
        job_limit = re.search(r'^    timeout-minutes: (\d+)$', job, re.M)
        self.assertIsNotNone(job_limit)
        blocks = steps(job)

        def only(predicate):
            found = [index for index, step in enumerate(blocks) if predicate(step)]
            self.assertEqual(len(found), 1)
            return found[0]

        producer = only(lambda step: 'run: scripts/release/qualify-oci-interruptions.sh' in step)
        watch = only(lambda step: 'name: Watch disk and memory during the OCI interruptions' in step)
        report = only(lambda step: "name: Report the OCI interruptions' headroom and leftovers" in step)
        upload = only(lambda step: 'name: contract-linux-oci-interruptions\n' in step)
        self.assertLess(watch, producer)
        self.assertLess(producer, report)
        self.assertLess(report, upload)
        step_limit = re.search(r'^        timeout-minutes: (\d+)$', blocks[producer], re.M)
        self.assertIsNotNone(step_limit)
        # The steps before it take about nine minutes; the rest of the margin
        # is for the report and the evidence upload.
        self.assertLessEqual(int(step_limit.group(1)) + 15, int(job_limit.group(1)))
        # The watcher must not hold the step's output open, or the step
        # itself would never finish.
        for text in ('nohup ', '< /dev/null', 'MemAvailable', 'df ', '.pid'):
            self.assertIn(text, blocks[watch])
        for text in ('if: always()', 'timeout-minutes:', '.pid', 'steps.oci_interruptions.outcome',
                     'ps -eo', '/stack'):
            self.assertIn(text, blocks[report])

    def test_a_hung_oci_interruption_test_fails_its_step_and_keeps_its_evidence(self):
        self.assert_oci_interruptions_bounded(self.jobs)

    def test_an_unbounded_or_unwatched_oci_step_is_detected(self):
        self.assert_oci_interruptions_bounded(self.jobs)
        original = self.jobs['linux']
        blocks = steps(original)
        producer = next(step for step in blocks
                        if 'run: scripts/release/qualify-oci-interruptions.sh' in step)
        watch = next(step for step in blocks
                     if 'name: Watch disk and memory during the OCI interruptions' in step)
        report = next(step for step in blocks
                      if "name: Report the OCI interruptions' headroom and leftovers" in step)
        unbounded = re.sub(r'\n        timeout-minutes: \d+', '', producer)
        changes = [
            original.replace(producer, unbounded),
            original.replace(producer, re.sub(r'timeout-minutes: \d+', 'timeout-minutes: 39', producer)),
            original.replace(watch, ''),
            original.replace(watch, watch.replace(' < /dev/null', '')),
            original.replace(report, ''),
            original.replace(report, report.replace('if: always()', 'if: failure()')),
            original.replace(watch + '\n', '').replace(report, report + '\n' + watch),
        ]
        for changed in changes:
            with self.subTest(change=changed), self.assertRaises(AssertionError):
                self.assert_oci_interruptions_bounded(dict(self.jobs, linux=changed))

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

    def test_the_standard_clients_run_on_a_prepared_host(self):
        acceptance = self.jobs["acceptance"]
        provision = 'sudo scripts/standard-clients-host.sh --provision'
        self.assertIn(provision, acceptance)
        self.assertLess(acceptance.index(provision),
                        acceptance.index('--gate standard-clients --host linux'))


class TrainAndQueueTests(unittest.TestCase):
    """Light checks on train PRs; everything on the merge queue and the train."""

    def setUp(self):
        self.ci = (WORKFLOWS / "ci.yml").read_text()
        self.jobs = jobs(self.ci)
        self.triggers = self.ci[:self.ci.index("\nconcurrency:")]

    def test_the_merge_queue_and_pushes_to_a_train_run_ci(self):
        self.assertRegex(self.triggers, r"\n  merge_group:\n(?:\s+#.*\n)*\s+types: \[checks_requested\]\n")
        self.assertRegex(self.triggers, r"\n  push:\n(?:\s+#.*\n)*\s+branches: \[main, \"release-\*\"\]\n")

    def test_selection_sees_the_head_branch_and_the_full_ci_label(self):
        changes = self.jobs["changes"]
        self.assertIn("HEAD_REF: ${{ github.event.pull_request.head.ref }}", changes)
        self.assertIn("FULL_CI: ${{ contains(github.event.pull_request.labels.*.name, 'full-ci') }}", changes)
        self.assertIn("macos: ${{ steps.select.outputs.macos }}", changes)
        self.assertIn("RELEASE_GATE: ${{ contains(github.event.pull_request.labels.*.name, 'release-gate') }}",
                      changes)
        self.assertIn("MACOS_LABEL: ${{ contains(github.event.pull_request.labels.*.name, 'macos') }}", changes)

    def test_evidence_binds_to_the_merge_groups_own_commit(self):
        self.assertIn("  EXPECTED_CHECKOUT_COMMIT: ${{ github.event.merge_group.head_sha || github.sha }}\n",
                      self.ci)

    def test_a_newer_push_to_a_pull_request_or_a_train_cancels_the_older_run(self):
        group = re.search(r"concurrency:\n(?:#.*\n)*\s+group: (.*)", self.ci).group(1)
        cancel = re.search(r"\n  cancel-in-progress: (.*)", self.ci).group(1)
        self.assertIn("github.event.pull_request.number", group)
        self.assertIn("startsWith(github.ref, 'refs/heads/release-') && github.ref", group)
        self.assertIn("github.event_name == 'pull_request'", cancel)
        self.assertIn("github.event_name == 'push' && startsWith(github.ref, 'refs/heads/release-')", cancel)
        # Main and merge-queue runs fall back to their own run id.
        self.assertTrue(group.rstrip().endswith("|| github.run_id }}"), group)

    def test_closing_a_pull_request_starts_a_run_that_cancels_its_older_one(self):
        types = re.search(r"\n  pull_request:\n(?:\s+#.*\n|\s+branches: .*\n)*\s+types: \[(.*)\]\n",
                          self.triggers).group(1).split(", ")
        self.assertIn("closed", types)
        # The closed run joins the pull request's group, so cancel-in-progress
        # ends the run that was still working on the closed PR.
        cancel = re.search(r"\n  cancel-in-progress: (.*)", self.ci).group(1)
        self.assertNotIn("github.event.action", cancel)
        closed = self.jobs[CLOSED_JOB]
        self.assertIn("    if: github.event.action == 'closed'\n", closed)
        self.assertNotIn("needs:", closed)
        # A no-op: it neither checks out nor builds anything.
        self.assertEqual([step for step in steps(closed) if "uses:" in step], [])
        self.assertLessEqual(int(re.search(r"timeout-minutes: (\d+)", closed).group(1)), 5)

    def test_a_closed_pull_requests_run_does_no_other_work(self):
        guard = "github.event.action != 'closed'"
        for name, job in self.jobs.items():
            if name == CLOSED_JOB:
                continue
            condition = re.search(r"\n    if: (.*)\n", "\n" + job + "\n")
            if condition and guard in condition.group(1):
                continue
            # Without the guard, a job may only run after select jobs ran,
            # and only when that succeeded: a status function such as
            # always() would start it on a closed run too.
            needs = re.search(r"\n    needs: (.*)\n", "\n" + job + "\n")
            self.assertIsNotNone(needs, name)
            self.assertIn("changes", needs.group(1), name)
            if condition:
                self.assertNotRegex(condition.group(1), r"always\(\)|failure\(\)|cancelled\(\)", name)
        for name in ["changes", "ci-policy", "dependency-audit", "ignored-evidence"]:
            self.assertIn(guard, self.jobs[name], name)

    def test_the_final_gate_waits_for_every_other_job_and_always_reports(self):
        gate = self.jobs["ignored-evidence"]
        needs = set(re.search(r"needs: \[([^\]]*)\]", gate).group(1).split(", "))
        # The closed-PR job never runs alongside the gate.
        self.assertEqual(needs, set(self.jobs) - {"ignored-evidence", CLOSED_JOB})
        self.assertIn("    name: ignored-test evidence\n", gate)
        # It reports whatever the other jobs did, except on a closed PR's
        # run, where there is nothing to report.
        self.assertIn("    if: ${{ always() && github.event.action != 'closed' }}\n", gate)
        blocks = steps(gate)
        first = next(index for index, step in enumerate(blocks) if "every_job.py" in step)
        aggregate = next(index for index, step in enumerate(blocks) if "workflow_adapter.py aggregate" in step)
        self.assertLess(first, aggregate)
        self.assertNotIn("if:", blocks[first])
        self.assertIn('--event "$GITHUB_EVENT_NAME"', blocks[first])
        self.assertIn('--event "$GITHUB_EVENT_NAME"', blocks[aggregate])
        # The release gate PR qualifies only with every owner, macOS included (#1443).
        self.assertIn("RELEASE_GATE: ${{ contains(github.event.pull_request.labels.*.name, 'release-gate') }}",
                      blocks[aggregate])
        self.assertIn('--release-gate "$RELEASE_GATE"', blocks[aggregate])
        # The owner check still reports when another job failed, so one red
        # job doesn't hide whether the evidence was complete.
        self.assertIn("if: ${{ !cancelled() && needs.changes.outputs.code == 'true' }}", blocks[aggregate])

    def test_macos_runs_when_selected_and_splits_the_once_only_steps(self):
        macos = self.jobs["macos"]
        self.assertIn("    if: needs.changes.outputs.macos == 'true'\n", macos)
        self.assertIn("      fail-fast: false\n", macos)
        self.assertIn("    name: portable macOS (${{ matrix.shard }}/" + str(SHARDS["portable-darwin"]) + ")\n", macos)
        for name in ["Lint every target, with all features", "Doctests"]:
            block = next(step for step in steps(macos) if "name: " + name in step)
            self.assertRegex(block, r"if: matrix\.shard == \d\n")

    def test_the_heavy_jobs_wait_for_their_own_selection(self):
        for name in ["linux", "build-tests", "cluster", "acceptance"]:
            self.assertIn("if: needs.changes.outputs.heavy == 'true'", self.jobs[name])
        for name in ["lint", "minimum-rust", "portable", "contract-boundaries", "demo-app"]:
            self.assertIn("if: needs.changes.outputs.code == 'true'", self.jobs[name])

    def test_the_portable_suite_finishes_on_a_slow_runner_with_headroom(self):
        # About 25 minutes normally and 35 to 41 on a slow runner (#1421).
        # nextest's per-test timeouts catch a hung test, so the job limit
        # only has to outlast a slow runner by half again.
        limit = re.search(r"\n    timeout-minutes: (\d+)\n", self.jobs["portable"])
        self.assertIsNotNone(limit)
        self.assertGreaterEqual(int(limit.group(1)), 41 * 3 // 2)

    def test_the_nightly_signing_build_s_own_key_test_runs(self):
        # The nightly build trusts the nightly key instead of every release
        # key, and only a build with the feature compiles the test that says
        # so (#1442).
        portable = self.jobs["portable"]
        block = next(step for step in steps(portable) if "--features nightly-signing" in step)
        self.assertIn("cargo test --locked --lib --features nightly-signing upgrade::keys::", block)
        self.assertNotIn("if:", block)
        keys = (REPO / "src" / "upgrade" / "keys.rs").read_text()
        self.assertIn('#[cfg(feature = "nightly-signing")]\n    #[test]', keys)


class BenchmarkTests(unittest.TestCase):
    def setUp(self):
        self.ci = (WORKFLOWS / "ci.yml").read_text()
        self.jobs = jobs(self.ci)

    def test_bench_task_arrays_is_run_by_a_workflow(self):
        job = self.jobs["task-array-benchmarks"]
        # select-jobs.sh sets task_arrays on schedules, so it runs nightly.
        self.assertIn("needs.changes.outputs.task_arrays == 'true'", job)
        self.assertIn("task_arrays: ${{ steps.select.outputs.task_arrays }}",
                      self.jobs["changes"])
        self.assertIn("run: make bench-task-arrays", job)
        compare = next(step for step in steps(job) if "bench_baseline.py compare" in step)
        self.assertIn("--max-ratio", compare)

    def test_task_array_benchmarks_over_the_limit_are_measured_again_once(self):
        # A disk stall during one benchmark is not a regression (#936): the
        # compare step re-runs just the benchmarks over the limit, once.
        job = self.jobs["task-array-benchmarks"]
        compare = next(step for step in steps(job) if "bench_baseline.py compare" in step)
        self.assertIn("--remeasure 'cargo bench --bench task_arrays --'", compare)

    def test_the_on_disk_ledger_benchmark_is_reported_not_gated(self):
        # Its speed is the runner's disk, which varies several-fold within one
        # job (#1038); the tmpfs variant beside it is gated instead.
        job = self.jobs["task-array-benchmarks"]
        compare = next(step for step in steps(job) if "bench_baseline.py compare" in step)
        self.assertEqual(re.findall(r"--report-only (\S+)", compare),
                         ["storage/ledger_and_index_64k"])

    def test_every_report_only_benchmark_has_a_gated_variant_in_the_bench(self):
        job = self.jobs["task-array-benchmarks"]
        compare = next(step for step in steps(job) if "bench_baseline.py compare" in step)
        bench = (REPO / "benches" / "task_arrays.rs").read_text()
        names = re.findall(r"--report-only (\S+)", compare)
        self.assertTrue(names)
        for name in names:
            with self.subTest(benchmark=name):
                group, function = name.split("/", 1)
                self.assertIn(f'benchmark_group("{group}")', bench)
                self.assertIn(f'"{function}"', bench)
                self.assertIn(f'"{function}_tmpfs"', bench)

    def test_only_main_saves_the_task_array_baseline(self):
        job = self.jobs["task-array-benchmarks"]
        restore = next(step for step in steps(job) if "actions/cache/restore@" in step)
        self.assertIn("restore-keys: task-array-bench-", restore)
        for marker in ("bench_baseline.py save", "actions/cache/save@"):
            step = next(step for step in steps(job) if marker in step)
            self.assertIn("if: github.ref == 'refs/heads/main'", step)

    def test_the_task_array_criterion_output_is_kept_as_an_artefact(self):
        job = self.jobs["task-array-benchmarks"]
        upload = next(step for step in steps(job) if "actions/upload-artifact@" in step)
        self.assertIn("if: always()", upload)
        self.assertIn("target/criterion", upload)
        self.assertIn("name: criterion-task-arrays", upload)

    def test_every_benchmark_target_is_run_by_a_workflow(self):
        makefile = (REPO / "Makefile").read_text()
        targets = re.findall(r"(?m)^(bench[a-z-]*):", makefile)
        self.assertIn("bench-task-arrays", targets)
        workflows = "\n".join(path.read_text() for path in WORKFLOWS.glob("*.yml"))
        for target in targets:
            with self.subTest(target=target):
                self.assertRegex(workflows, rf"(?m)run: make {re.escape(target)}$")


class ArtifactDownloadTests(unittest.TestCase):
    """A stalled artefact download fails fast and is retried once (#977).

    `actions/download-artifact` normally takes a minute or two, but one run
    took 13 minutes and pushed `acceptance` past its 40-minute limit. Each
    download gets its own short limit and one retry, so a stall costs at most
    two short limits instead of the job's whole budget. That's enough for the
    small evidence envelopes; ci.yml's test archive is fetched by
    `fetch_artifact.py` instead (ArchiveFetchTests).
    """

    # The longest any one attempt may take; downloads normally take 1 to 2.
    LIMIT_MINUTES = 5
    WORKFLOW_FILES = ("ci.yml", "v02-loops.yml")

    def setUp(self):
        self.workflows = {name: jobs((WORKFLOWS / name).read_text())
                          for name in self.WORKFLOW_FILES}

    @staticmethod
    def with_block(step):
        """The step's `with:` inputs, as text."""
        found = re.search(r"\n        with:\n((?:          .*\n?)*)", step)
        return found.group(1).rstrip() if found else ""

    def assert_downloads_retried(self, workflow_jobs):
        """Every download is a bounded attempt followed by one bounded retry.

        No subTest here: the detection test expects the first failure to raise.
        """
        pairs = 0
        for name, job in workflow_jobs.items():
            blocks = steps(job)
            downloads = [index for index, step in enumerate(blocks)
                         if "uses: actions/download-artifact@" in step]
            remaining = list(downloads)
            while remaining:
                first = remaining.pop(0)
                attempt = blocks[first]
                self.assertIn("\n        continue-on-error: true", attempt, f"{name}: the first attempt must not fail the job")
                step_id = re.search(r"\n        id: ([a-z_]+)\n", attempt)
                self.assertIsNotNone(step_id, "the first attempt needs an id")
                self.assertTrue(remaining and remaining[0] == first + 1,
                                "the first attempt must be followed by its retry")
                retry = blocks[remaining.pop(0)]
                self.assertNotIn("continue-on-error", retry)
                condition = re.search(r"\n        if: (.*)", retry)
                self.assertIsNotNone(condition, "the retry runs only when the first attempt failed")
                # !cancelled(): an earlier failed step in the job must not
                # skip the retry of a download that runs regardless.
                self.assertIn("!cancelled()", condition.group(1))
                self.assertIn(f"steps.{step_id.group(1)}.outcome == 'failure'", condition.group(1))
                self.assertEqual(self.with_block(attempt), self.with_block(retry))
                self.assertNotEqual(self.with_block(attempt), "")
                for step in (attempt, retry):
                    limit = re.search(r"\n        timeout-minutes: (\d+)\n", step + "\n")
                    self.assertIsNotNone(limit, "each attempt needs its own timeout-minutes")
                    self.assertLessEqual(int(limit.group(1)), self.LIMIT_MINUTES)
                pairs += 1
        return pairs

    def test_a_stalled_artefact_download_fails_fast_and_is_retried_once(self):
        for name, workflow_jobs in self.workflows.items():
            with self.subTest(workflow=name):
                self.assertGreater(self.assert_downloads_retried(workflow_jobs), 0)

    def test_the_jobs_that_download_the_test_archive_keep_their_own_limits(self):
        ci = self.workflows["ci.yml"]
        for name, minutes in (("cluster", 30), ("acceptance", 40), ("ignored-evidence", 10)):
            with self.subTest(job=name):
                self.assertIn(f"\n    timeout-minutes: {minutes}\n", ci[name])

    def test_an_unbounded_unretried_or_skippable_download_is_detected(self):
        ci = self.workflows["ci.yml"]
        self.assert_downloads_retried(ci)
        original = ci["ignored-evidence"]
        blocks = steps(original)
        downloads = [step for step in blocks if "uses: actions/download-artifact@" in step]
        self.assertEqual(len(downloads), 2)
        attempt, retry = downloads
        changes = [
            original.replace(retry + "\n", ""),
            original.replace(attempt, re.sub(r"\n        timeout-minutes: \d+", "", attempt)),
            original.replace(retry, re.sub(r"timeout-minutes: \d+", "timeout-minutes: 15", retry)),
            original.replace(attempt, attempt.replace("\n        continue-on-error: true", "")),
            original.replace(retry, retry.replace("!cancelled() && ", "")),
            original.replace(retry, retry.replace("outcome == 'failure'", "outcome == 'success'")),
            original.replace(retry, retry.replace("pattern: contract-*", "pattern: something-else")),
            original.replace(retry, retry + "\n" + retry),
        ]
        for changed in changes:
            with self.subTest(change=changed), self.assertRaises(AssertionError):
                self.assert_downloads_retried(dict(ci, **{"ignored-evidence": changed}))


class ArchiveFetchTests(unittest.TestCase):
    """The test archive is fetched by a transfer that resumes (#977).

    With `actions/download-artifact`, the 3.0 GB archive took 33 s to almost
    5 minutes in passing runs, against a 5-minute limit per attempt, and a
    retry started again from the first byte, so a slow runner failed both
    attempts. `fetch_artifact.py` resumes where a transfer stopped and stops
    at its own deadline, inside the step's limit, inside the job's.
    """

    CONSUMERS = {"cluster": 30, "acceptance": 40}
    # The fetch, then everything else the job does, must fit the job's limit.
    LIMIT_MINUTES = 15

    def setUp(self):
        self.text = (WORKFLOWS / "ci.yml").read_text()
        self.ci = jobs(self.text)

    def assert_fetches_once(self, job, job_minutes):
        """No subTest here: the detection test expects the first failure to raise."""
        fetches = [step for step in steps(job) if "fetch_artifact.py" in step]
        self.assertEqual(len(fetches), 1, "exactly one fetch of the archive per consumer job")
        fetch = fetches[0]
        self.assertIn("--name nextest-archive", fetch)
        self.assertNotIn("continue-on-error", fetch, "a failed fetch must fail the job")
        self.assertNotIn("\n        if:", fetch, "a skipped fetch must fail the job")
        self.assertIn("GH_TOKEN: ${{ github.token }}", fetch)
        limit = re.search(r"\n        timeout-minutes: (\d+)\n", fetch + "\n")
        self.assertIsNotNone(limit, "the fetch needs its own timeout-minutes")
        self.assertLessEqual(int(limit.group(1)), self.LIMIT_MINUTES)
        self.assertLess(int(limit.group(1)), job_minutes)
        deadline = re.search(r"--deadline (\d+)", fetch)
        self.assertIsNotNone(deadline, "the fetch stops itself before the step's limit")
        self.assertLess(int(deadline.group(1)), int(limit.group(1)) * 60)
        self.assertRegex(fetch, r"--stall \d+")
        self.assertNotIn("actions/download-artifact@", job, "the archive has one fetch, not two")
        self.assertRegex(job, r"\n    permissions:\n      contents: read\n      actions: read\n",
                         "listing this run's artefacts needs actions: read")

    def test_each_archive_consumer_fetches_the_archive_once_with_a_deadline(self):
        for name, minutes in self.CONSUMERS.items():
            with self.subTest(job=name):
                self.assert_fetches_once(self.ci[name], minutes)

    def test_no_job_downloads_the_archive_with_the_action(self):
        for name, job in self.ci.items():
            for step in steps(job):
                if "actions/download-artifact@" in step:
                    with self.subTest(job=name):
                        self.assertNotIn("nextest-archive", step)

    def test_every_caller_of_ci_grants_what_the_fetch_needs(self):
        callers = 0
        for path in WORKFLOWS.glob("*.yml"):
            for name, job in jobs(path.read_text()).items():
                if "uses: ./.github/workflows/ci.yml" not in job:
                    continue
                callers += 1
                with self.subTest(workflow=path.name, job=name):
                    self.assertRegex(job, r"\n    permissions:\n      contents: read\n      actions: read\n")
        self.assertGreater(callers, 0)

    def test_an_unbounded_skippable_or_doubled_fetch_is_detected(self):
        original = self.ci["acceptance"]
        fetch = next(step for step in steps(original) if "fetch_artifact.py" in step)
        changes = [
            original.replace(fetch + "\n", ""),
            original.replace(fetch, fetch + "\n" + fetch),
            original.replace(fetch, re.sub(r"\n        timeout-minutes: \d+", "", fetch)),
            original.replace(fetch, re.sub(r"timeout-minutes: \d+", "timeout-minutes: 40", fetch)),
            original.replace(fetch, re.sub(r"--deadline \d+", "--deadline 900", fetch)),
            original.replace(fetch, re.sub(r" --deadline \d+", "", fetch)),
            original.replace(fetch, re.sub(r" --stall \d+", "", fetch)),
            original.replace(fetch, fetch.replace("timeout-minutes:", "continue-on-error: true\n        timeout-minutes:")),
            original.replace(fetch, fetch.replace("timeout-minutes:", "if: ${{ !cancelled() }}\n        timeout-minutes:")),
            original.replace(fetch, fetch.replace("--name nextest-archive", "--name something-else")),
            original.replace(fetch, fetch.replace("\n          GH_TOKEN: ${{ github.token }}", "")),
            original.replace("      actions: read\n", ""),
        ]
        for changed in changes:
            with self.subTest(change=changed), self.assertRaises(AssertionError):
                self.assert_fetches_once(changed, 40)


class ArchiveExtractionTests(unittest.TestCase):
    """Each archive consumer extracts the test archive once (#1050).

    `acceptance` has four owners and `cluster` one, and each owner used to
    extract the 2.9 GB archive twice, for its discovery and for its run. Now
    one step per job extracts it, and every owner names that step's receipt
    digest, so its evidence is bound to the archive the step checked.
    """

    CONSUMERS = ("cluster", "acceptance")

    def setUp(self):
        self.ci = jobs((WORKFLOWS / "ci.yml").read_text())

    def assert_extracts_once(self, job):
        """No subTest here: the detection test expects the first failure to raise."""
        blocks = steps(job)
        extracts = [index for index, step in enumerate(blocks) if "workflow_adapter.py extract" in step]
        self.assertEqual(len(extracts), 1, "exactly one extraction per consumer job")
        extract = blocks[extracts[0]]
        step_id = re.search(r"\n        id: ([a-z_]+)\n", extract)
        self.assertIsNotNone(step_id, "the extraction needs an id for its output")
        self.assertNotIn("\n        if:", extract, "a skipped extraction must fail the owners")
        fetches = [index for index, step in enumerate(blocks) if "fetch_artifact.py" in step]
        self.assertTrue(fetches and max(fetches) < extracts[0], "extract after the fetch")
        producers = [index for index, step in enumerate(blocks) if "workflow_adapter.py produce" in step]
        self.assertTrue(producers and extracts[0] < min(producers), "extract before every owner")
        reference = ('--archive-extraction "${{ steps.' + step_id.group(1)
                     + '.outputs.archive_extraction_sha256 }}"')
        for index in producers:
            self.assertIn(reference, blocks[index], "every owner names the job's extraction")
        for option in ("--archive-file", "--extract-to", "--extract-overwrite"):
            self.assertNotIn(option, job, "an owner must not open the archive again")
        return len(producers)

    def test_each_archive_consumer_extracts_once_before_its_owners(self):
        owners = {name: self.assert_extracts_once(self.ci[name]) for name in self.CONSUMERS}
        self.assertEqual(owners, {"cluster": 1, "acceptance": 4})

    def test_a_missing_repeated_late_or_unreferenced_extraction_is_detected(self):
        original = self.ci["acceptance"]
        blocks = steps(original)
        extract = next(step for step in blocks if "workflow_adapter.py extract" in step)
        first = next(step for step in blocks if "workflow_adapter.py produce" in step)
        last = [step for step in blocks if "workflow_adapter.py produce" in step][-1]
        changes = [
            original.replace(extract + "\n", ""),
            original.replace(extract, extract + "\n" + extract.replace("id: ", "id: again_")),
            original.replace(extract + "\n", "").replace(first, first + "\n" + extract),
            original.replace(extract, extract.replace("\n        id: ", "\n        if: ${{ !cancelled() }}\n        id: ")),
            original.replace(last, last.replace(" --archive-extraction", " --unrelated")),
            original.replace(last, last.replace("outputs.archive_extraction_sha256", "outputs.something_else")),
            original + "\n    env:\n      ARCHIVED: cargo nextest run --archive-file tests.tar.zst",
        ]
        for changed in changes:
            with self.subTest(change=changed), self.assertRaises(AssertionError):
                self.assert_extracts_once(changed)


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

    def test_every_x86_64_lab_build_makes_a_broken_version_signed_with_the_runs_key(self):
        # #406: the fallback test, and the Wyse lab's, need a broken version
        # the run's own images trust, sorting above the next version.
        build = self.step("Build a broken version (x86-64 lab builds)")
        self.assertIn("if: env.NEXT_VERSION != ''", build)
        self.assertIn("+ 2 ))", build)
        self.assertIn('--extra-tree "$GITHUB_WORKSPACE/image/tests/broken-bun"', build)
        self.assertIn('"$RUNNER_TEMP"/broken/*.SHA256SUMS', self.step("Sign (throwaway key)"))
        lay_out = self.step("Lay out the next version as a release")
        self.assertIn('"$RUNNER_TEMP/broken" "$BROKEN_VERSION"', lay_out)

    def test_both_kinds_of_broken_build_break_bun_the_same_way(self):
        self.assertIn("image/tests/broken-bun/", self.step("Break bun on purpose (spike S4)"))
        conf = (REPO / "image/tests/broken-bun/usr/lib/systemd/system"
                / "reliaburger.service.d/50-broken.conf").read_text()
        self.assertIn("ExecStart=\nExecStart=/bin/false", conf)

    def test_the_fallback_test_runs_on_every_x86_64_lab_build(self):
        # Every pull request run, so the release train and its gate run it.
        fallback = self.step("OS fallback (x86-64, KVM)")
        condition = fallback.split("if:", 1)[1].split("\n", 1)[0]
        self.assertIn("env.BROKEN_VERSION != ''", condition)
        self.assertNotIn("event_name", condition)
        self.assertIn('"$BROKEN_VERSION" source-bin/relish-linux-x86_64 --fallback', fallback)
        self.assertIn("timeout-minutes:", fallback)
        branches = re.search(r"pull_request:\n(?:\s+#.*\n)*\s+branches: (\[.*\])", self.text)
        self.assertIn('"release-*"', branches.group(1))

    def test_the_boot_test_uses_the_wyse_sized_disk(self):
        boot = self.step("Boot test")
        self.assertIn(". image/tests/disk.sh", boot)
        self.assertIn("disk_from_image", boot)


if __name__ == "__main__":
    unittest.main()
