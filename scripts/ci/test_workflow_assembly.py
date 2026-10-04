"""Synthetic trusted-workflow plan cases; no Rust or workflow qualification."""
import copy
import importlib
from pathlib import Path
import tempfile
import unittest

import contracts as c


# This independent source fixture preserves existing Make argument spelling.
MAKEFILE = '''NEXTEST_PROFILE ?= default
NEXTEST = $(CARGO) nextest run --profile $(NEXTEST_PROFILE) --no-tests=fail
test:
\t$(NEXTEST)
test-cluster:
\tRELIABURGER_CLUSTER_TESTS=1 $(NEXTEST) --run-ignored=only -E 'binary(cluster_failover) | binary(cluster_gossip) '
test-linux:
\t$(CARGO) build --features ebpf --bin bun
\tRELIABURGER_RUNC_TESTS=1 RELIABURGER_BUN_BINARY="$(CURDIR)/target/debug/bun" $(WITH_TEST_IMAGES) $(NEXTEST) --features ebpf --run-ignored=only -E '(binary(owned_runc) | binary(test_storage)) $(LINUX_EXCLUDE)'
test-rootless-runc:
\tRELIABURGER_ROOTLESS_RUNC_TESTS=1 $(NEXTEST) --features ebpf --run-ignored=only -E 'binary(owned_rootless)'
'''


class WorkflowAssembly(unittest.TestCase):
    def setUp(self):
        self.w = importlib.import_module('workflow_assembly')
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.root.joinpath('Makefile').write_text(MAKEFILE)
        self.expected = dict(commit='a' * 40, run_id='123', attempt='2')

    def tearDown(self):
        self.temp.cleanup()

    def test_portable_plan_waits_for_both_hosts_and_optimized_boundary(self):
        plan = self.w.selected_workflow(self.expected, code='true', heavy='false')
        self.assertEqual(plan['mode'], 'portable')
        self.assertEqual(set(plan['jobs']), {'changes', 'portable', 'macos', 'contract-boundaries'})
        self.assertEqual(set(plan['gates']), {'portable-linux', 'portable-darwin', 'optimized-cron'})

    def test_full_plan_keeps_all_existing_owner_jobs_and_archive_builder(self):
        plan = self.w.selected_workflow(self.expected, code='true', heavy='true')
        self.assertEqual(plan['mode'], 'full')
        self.assertEqual(set(plan['jobs']), {'changes', 'portable', 'macos', 'contract-boundaries', 'linux', 'build-tests', 'cluster', 'acceptance'})
        self.assertIn('oci-interruptions', plan['gates'])
        self.assertEqual(len(plan['gates']), 11)

    def test_selection_never_inferred_from_absent_artifacts(self):
        with self.assertRaises(c.Invalid):
            self.w.selected_workflow(self.expected, code='false', heavy='true')
        with self.assertRaises(c.Invalid):
            self.w.selected_workflow(self.expected, code=True, heavy='false')
        self.assertIsNone(self.w.selected_workflow(self.expected, code='false', heavy='false'))

    def test_context_uses_actual_checkout_not_pr_head_or_late_receipt(self):
        bad = dict(self.expected, commit='PR-head')
        with self.assertRaises(c.Invalid):
            self.w.selected_workflow(bad, code='true', heavy='false')
        plan = self.w.selected_workflow(self.expected, code='true', heavy='false')
        self.assertEqual(plan['commit'], self.expected['commit'])

    def test_cluster_suffix_keeps_trailing_filter_space_and_archive_options(self):
        selectors, environment = self.w.make_selection(self.root, 'test-cluster', archived=True)
        self.assertEqual(selectors, ['--archive-file', 'tests.tar.zst', '--extract-to', '.', '--extract-overwrite', '--workspace-remap', '.', '--profile', 'ci', '--run-ignored=only', '-E', 'binary(cluster_failover) | binary(cluster_gossip) '])
        self.assertEqual(environment, {'RELIABURGER_CLUSTER_TESTS': '1'})

    def test_linux_preserves_ebpf_exclusion_and_image_environment(self):
        selectors, environment = self.w.make_selection(self.root, 'test-linux')
        self.assertEqual(selectors[-1], '(binary(owned_runc) | binary(test_storage)) & not binary(owned_runc) & not binary(owned_network)')
        self.assertIn('--features', selectors)
        self.assertEqual(environment['RELIABURGER_TEST_IMAGE_MIRROR'], '127.0.0.1:5099')
        self.assertEqual(environment['RELIABURGER_BUN_BINARY'], str(self.root.resolve() / 'target/debug/bun'))

    def test_portable_implicit_selection_not_broadened(self):
        selectors, environment = self.w.make_selection(self.root, 'test')
        self.assertEqual(selectors, ['--profile', 'ci'])
        self.assertEqual(environment, {})
        self.assertNotIn('--ignore-default-filter', selectors)

    def test_unknown_or_multiple_invocation_shapes_refuse(self):
        with self.assertRaises(c.Invalid):
            self.w.make_selection(self.root, 'not-a-target')
        self.root.joinpath('Makefile').write_text('test:\n\t$(NEXTEST)\n\t$(NEXTEST)\n')
        with self.assertRaises(c.Invalid):
            self.w.make_selection(self.root, 'test')

    def test_unknown_make_variables_refuse_instead_of_guessing(self):
        self.root.joinpath('Makefile').write_text('test:\n\t$(NEXTEST) --features $(MYSTERY)\n')
        with self.assertRaises(c.Invalid):
            self.w.make_selection(self.root, 'test')

    def test_non_make_shell_prefix_refuses(self):
        self.root.joinpath('Makefile').write_text('test:\n\techo unsafe && $(NEXTEST)\n')
        with self.assertRaises(c.Invalid):
            self.w.make_selection(self.root, 'test')

    def test_job_aggregation_requires_completed_current_selected_jobs(self):
        plan = self.w.selected_workflow(self.expected, code='true', heavy='false')
        needs = {name: {'result': 'success'} for name in plan['jobs']}
        self.w.job_results(plan, needs)
        for status in ['failure', 'cancelled', 'skipped', 'in_progress', '']:
            bad = copy.deepcopy(needs)
            bad['portable']['result'] = status
            with self.assertRaises(c.Invalid):
                self.w.job_results(plan, bad)
        del needs['macos']
        with self.assertRaises(c.Invalid):
            self.w.job_results(plan, needs)

    def test_portable_plan_rejects_accidental_missing_heavy_selection(self):
        plan = self.w.selected_workflow(self.expected, code='true', heavy='true')
        needs = {name: {'result': 'success'} for name in ['changes', 'portable', 'macos', 'contract-boundaries']}
        with self.assertRaises(c.Invalid):
            self.w.job_results(plan, needs)

    def test_genuine_archive_builder_command_is_not_downloaded_metadata(self):
        self.assertEqual(self.w.ARCHIVE_COMMAND, ['cargo', 'nextest', 'archive', '--locked', '--archive-file', 'tests.tar.zst'])
        self.assertEqual(self.w.OCI_BUILD_COMMAND, ['cargo', 'test', '--features', 'ebpf', '--test', 'owned_runc', '--test', 'owned_network', '--test', 'oci_crash', '--no-run', '--message-format=json'])

    def test_original_owner_commands_are_not_late_keep_metadata(self):
        self.assertEqual(self.w.OWNERS['cluster-tests'], ['make', 'test-cluster'])
        self.assertEqual(self.w.OWNERS['oci-interruptions'], ['scripts/release/qualify-oci-interruptions.sh'])
        self.assertNotIn('keep-junit.sh', str(self.w.OWNERS))

    def test_make_entry_preserves_default_filter_and_profile_for_both_operations(self):
        self.root.joinpath('.config').mkdir()
        self.root.joinpath('.config/nextest.toml').write_text('synthetic expected config')
        manifest = {'gates': {'portable-darwin': {'command': ['make', 'test'], 'hosts': ['darwin']}},
                    'contracts': [{'cases': [{'requires': ['portable-darwin'], 'sources': ['src/contract.rs']}]}]}
        entry = self.w.entry_from_make(self.root, manifest, 'portable-darwin', 'darwin')
        run, listed = self.w.gates.commands(entry)
        self.assertEqual(run[3:5], listed[3:5])
        self.assertEqual(entry['selector_context'], {'profile': 'ci', 'ignored': 'default', 'default_filter': 'honor'})
        self.assertNotIn('--ignore-default-filter', run)
        self.assertNotIn('--test-threads=2', run)

    def test_coverage_and_oci_cannot_be_replaced_by_plain_nextest_entry(self):
        for gate in ['portable-linux', 'oci-interruptions']:
            with self.assertRaises(c.Invalid):
                self.w.entry_from_make(self.root, {}, gate, 'linux')

    def test_archive_owner_cannot_invent_a_late_archive_origin(self):
        manifest = {'gates': {'cluster-tests': {'command': ['make', 'test-cluster'], 'hosts': ['linux']}},
                    'contracts': [{'cases': [{'requires': ['cluster-tests'], 'sources': ['tests/cluster_failover.rs']}]}]}
        with self.assertRaises(c.Invalid):
            self.w.entry_from_make(self.root, manifest, 'cluster-tests', 'linux')
