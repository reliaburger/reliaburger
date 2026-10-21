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
\t$(CARGO) test --no-run --features ebpf
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
        plan = self.w.selected_workflow(self.expected, code='true', heavy='false', macos='true')
        self.assertEqual(plan['mode'], 'portable')
        self.assertEqual(set(plan['jobs']), {'changes', 'portable', 'macos', 'contract-boundaries'})
        self.assertEqual(set(plan['gates']), {'portable-linux', 'portable-darwin', 'optimized-cron'})

    def test_full_plan_keeps_all_existing_owner_jobs_and_archive_builder(self):
        plan = self.w.selected_workflow(self.expected, code='true', heavy='true', macos='true')
        self.assertEqual(plan['mode'], 'full')
        self.assertEqual(set(plan['jobs']), {'changes', 'portable', 'macos', 'contract-boundaries', 'linux', 'build-tests', 'cluster', 'acceptance'})
        self.assertIn('oci-interruptions', plan['gates'])
        self.assertEqual(len(plan['gates']), 11)

    def test_selection_never_inferred_from_absent_artifacts(self):
        with self.assertRaises(c.Invalid):
            self.w.selected_workflow(self.expected, code='false', heavy='true', macos='true')
        with self.assertRaises(c.Invalid):
            self.w.selected_workflow(self.expected, code=True, heavy='false', macos='true')
        self.assertIsNone(self.w.selected_workflow(self.expected, code='false', heavy='false', macos='false'))

    def test_light_plan_waits_only_for_linux_and_the_optimized_boundary(self):
        plan = self.w.selected_workflow(self.expected, code='true', heavy='false', macos='false')
        self.assertEqual(plan['mode'], 'light')
        self.assertEqual(set(plan['jobs']), {'changes', 'portable', 'contract-boundaries'})
        self.assertEqual(set(plan['gates']), {'portable-linux', 'optimized-cron'})

    def test_train_full_plan_waits_for_every_heavy_owner_but_not_macos(self):
        # A full-ci pull request into a merge train: everything except the
        # three portable macOS shards, which the queue and the train push run.
        plan = self.w.selected_workflow(self.expected, code='true', heavy='true', macos='false')
        self.assertEqual(plan['mode'], 'train-full')
        self.assertEqual(set(plan['jobs']), {'changes', 'portable', 'contract-boundaries', 'linux',
                                             'build-tests', 'cluster', 'acceptance'})
        self.assertEqual(set(plan['gates']), set(self.w.OWNERS) - {'portable-darwin'})

    def test_an_unknown_macos_value_is_refused(self):
        for heavy in ['true', 'false']:
            for value in [None, True, '', 'yes']:
                with self.subTest(heavy=heavy, macos=value), self.assertRaises(c.Invalid):
                    self.w.selected_workflow(self.expected, code='true', heavy=heavy, macos=value)

    def test_the_portable_macos_owner_is_split_into_three_hash_partitions(self):
        self.assertEqual(self.w.SHARDS, {'portable-darwin': 3})
        self.assertEqual([self.w.partition('portable-darwin', shard) for shard in (1, 2, 3)],
                         ['hash:1/3', 'hash:2/3', 'hash:3/3'])
        self.assertIsNone(self.w.partition('portable-linux', None))
        for gate, shard in [('portable-darwin', None), ('portable-darwin', 0), ('portable-darwin', 4),
                            ('portable-darwin', '1'), ('portable-linux', 1)]:
            with self.subTest(gate=gate, shard=shard), self.assertRaises(c.Invalid):
                self.w.partition(gate, shard)

    def test_a_shard_lists_and_runs_with_its_partition(self):
        self.root.joinpath('.config').mkdir()
        self.root.joinpath('.config/nextest.toml').write_text('synthetic expected config')
        manifest = {'gates': {'portable-darwin': {'command': ['make', 'test'], 'hosts': ['darwin']}},
                    'contracts': [{'cases': [{'requires': ['portable-darwin'], 'sources': ['src/contract.rs']}]}]}
        entry = self.w.entry_from_make(self.root, manifest, 'portable-darwin', 'darwin', shard=2)
        self.assertEqual(entry['partition'], 'hash:2/3')
        self.assertEqual(entry['selectors'], ['--profile', 'ci', '--partition', 'hash:2/3'])
        run, listed = self.w.gates.commands(entry)
        self.assertIn('hash:2/3', run)
        self.assertIn('hash:2/3', listed)
        unsharded = self.w.entry_from_make(self.root, manifest, 'portable-darwin', 'darwin')
        self.assertNotIn('partition', unsharded)
        self.assertNotIn('--partition', unsharded['selectors'])

    def test_context_uses_actual_checkout_not_pr_head_or_late_receipt(self):
        bad = dict(self.expected, commit='PR-head')
        with self.assertRaises(c.Invalid):
            self.w.selected_workflow(bad, code='true', heavy='false', macos='true')
        plan = self.w.selected_workflow(self.expected, code='true', heavy='false', macos='true')
        self.assertEqual(plan['commit'], self.expected['commit'])

    def test_cluster_suffix_keeps_trailing_filter_space_and_reuses_the_extracted_build(self):
        selectors, environment = self.w.make_selection(self.root, 'test-cluster', archived=True)
        self.assertEqual(selectors, ['--binaries-metadata', 'target/nextest/binaries-metadata.json',
                                     '--cargo-metadata', 'target/nextest/cargo-metadata.json',
                                     '--target-dir-remap', 'target', '--workspace-remap', '.',
                                     '--profile', 'ci', '--run-ignored=only', '-E',
                                     'binary(cluster_failover) | binary(cluster_gossip) '])
        self.assertEqual(environment, {'RELIABURGER_CLUSTER_TESTS': '1'})

    def test_an_archived_owner_never_opens_the_archive_itself(self):
        # The job extracts once; an owner that named the archive would
        # extract it again for both its discovery and its run (#1050).
        selectors, _ = self.w.make_selection(self.root, 'test-cluster', archived=True)
        for option in ('--archive-file', '--extract-to', '--extract-overwrite'):
            self.assertFalse(any(word == option or word.startswith(option + '=') for word in selectors), option)
        self.assertEqual(self.w.ARCHIVE_METADATA, ('target/nextest/binaries-metadata.json',
                                                   'target/nextest/cargo-metadata.json'))

    def test_the_job_extracts_the_archive_once_by_listing_only_its_binaries(self):
        # Listing binaries only extracts without running every test binary
        # to enumerate its tests; the owners list their own selections later.
        self.assertEqual(self.w.extraction_command('/runner/bin/cargo-nextest'), [
            '/runner/bin/cargo-nextest', 'nextest', 'list', '--archive-file', 'tests.tar.zst',
            '--extract-to', '.', '--extract-overwrite', '--workspace-remap', '.',
            '--list-type', 'binaries-only', '--message-format', 'json'])

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
        plan = self.w.selected_workflow(self.expected, code='true', heavy='false', macos='true')
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
        plan = self.w.selected_workflow(self.expected, code='true', heavy='true', macos='true')
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

    def archived_inputs(self):
        self.root.joinpath('.config').mkdir(exist_ok=True)
        self.root.joinpath('.config/nextest.toml').write_text('synthetic expected config')
        manifest = {'gates': {'cluster-tests': {'command': ['make', 'test-cluster'], 'hosts': ['linux']}},
                    'contracts': [{'cases': [{'requires': ['cluster-tests'], 'sources': ['tests/cluster_failover.rs']}]}]}
        builder = dict(schema_version=1, commit='a' * 40, run_id='123', attempt='2', host='linux',
                       command=self.w.ARCHIVE_COMMAND, exit_code=0, archive_sha256='1' * 64)
        origin = dict(path='archive-origin.json', sha256='2' * 64, builder=builder)
        extraction = dict(schema_version=1, context=dict(commit='a' * 40, run_id='123', attempt='2', host='linux'),
                          archive_sha256='1' * 64, archive_origin_sha256='2' * 64,
                          command=self.w.extraction_command('/runner/bin/cargo-nextest'), exit_code=0,
                          metadata={'target/nextest/binaries-metadata.json': '3' * 64,
                                    'target/nextest/cargo-metadata.json': '4' * 64})
        return manifest, origin, extraction

    def test_an_archived_owner_runs_the_metadata_its_job_extracted(self):
        manifest, origin, extraction = self.archived_inputs()
        entry = self.w.entry_from_make(self.root, manifest, 'cluster-tests', 'linux',
                                       archive_origin=origin, archive_extraction=extraction)
        self.assertEqual(entry['archive_extraction'], extraction)
        self.assertEqual(entry['selectors'][:len(self.w.ARCHIVED_SELECTORS)], self.w.ARCHIVED_SELECTORS)
        # The owner checks these before discovery: the metadata it runs is
        # the metadata extracted from the builder's archive.
        self.assertEqual(entry['inputs']['target/nextest/binaries-metadata.json'], '3' * 64)
        self.assertEqual(entry['inputs']['target/nextest/cargo-metadata.json'], '4' * 64)
        # The archive itself was hashed once, by the extraction, not per owner.
        self.assertNotIn('tests.tar.zst', entry['inputs'])

    def test_an_archived_owner_needs_an_extraction_of_its_builders_archive(self):
        manifest, origin, extraction = self.archived_inputs()
        with self.assertRaises(c.Invalid):
            self.w.entry_from_make(self.root, manifest, 'cluster-tests', 'linux', archive_origin=origin)
        with self.assertRaises(c.Invalid):
            self.w.entry_from_make(self.root, manifest, 'cluster-tests', 'linux', archive_extraction=extraction)
        other = dict(extraction, archive_sha256='9' * 64)
        with self.assertRaises(c.Invalid):
            self.w.entry_from_make(self.root, manifest, 'cluster-tests', 'linux',
                                   archive_origin=origin, archive_extraction=other)
