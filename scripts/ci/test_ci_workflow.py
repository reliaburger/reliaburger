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

    def test_archive_jobs_prepare_once_and_all_owners_use_the_trusted_receipt(self):
        for job in ('cluster', 'acceptance'):
            with self.subTest(job=job):
                blocks = steps(self.jobs[job])
                prepares = [i for i, step in enumerate(blocks) if 'workflow_adapter.py extract ' in step]
                self.assertEqual(len(prepares), 1)
                self.assertIn('id: extract', blocks[prepares[0]])
                owners = [i for i, step in enumerate(blocks) if 'workflow_adapter.py produce ' in step]
                self.assertTrue(owners)
                for index in owners:
                    self.assertLess(prepares[0], index)
                    self.assertIn('--archive-extraction-sha256 "$EXTRACTION_SHA256"', blocks[index])
                    self.assertIn('EXTRACTION_SHA256: ${{ steps.extract.outputs.archive_extraction_sha256 }}', blocks[index])
                self.assertNotIn('ARCHIVED:', self.jobs[job])

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

    def test_the_final_gate_waits_for_every_other_job_and_always_reports(self):
        gate = self.jobs["ignored-evidence"]
        needs = set(re.search(r"needs: \[([^\]]*)\]", gate).group(1).split(", "))
        self.assertEqual(needs, set(self.jobs) - {"ignored-evidence"})
        self.assertIn("    name: ignored-test evidence\n", gate)
        self.assertIn("    if: ${{ always() }}\n", gate)
        blocks = steps(gate)
        first = next(index for index, step in enumerate(blocks) if "every_job.py" in step)
        aggregate = next(index for index, step in enumerate(blocks) if "workflow_adapter.py aggregate" in step)
        self.assertLess(first, aggregate)
        self.assertNotIn("if:", blocks[first])
        self.assertIn('--event "$GITHUB_EVENT_NAME"', blocks[first])
        self.assertIn('--event "$GITHUB_EVENT_NAME"', blocks[aggregate])
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


if __name__ == "__main__":
    unittest.main()
