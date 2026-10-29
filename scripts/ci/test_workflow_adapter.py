"""Workflow trust/aggregation controls with synthetic owner implementations."""
import copy
import importlib
import json
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest.mock import patch

import contracts as c


class WorkflowAdapter(unittest.TestCase):
    def setUp(self):
        self.a = importlib.import_module('workflow_adapter')
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name).resolve()
        self.incoming = self.root / 'incoming'
        self.context = dict(commit='a' * 40, run_id='123', attempt='2')
        self.root.joinpath('Makefile').write_text('coverage:\n\ttrue\ntest:\n\ttrue\ntest-contract-boundaries:\n\ttrue\n')
        self.root.joinpath('src').mkdir()
        self.root.joinpath('src/contract.rs').write_text('synthetic actual source')
        self.manifest = dict(schema_version=1, gates={}, contracts=[])
        for gate, host in [('portable-linux', 'linux'), ('portable-darwin', 'darwin'), ('optimized-cron', 'linux')]:
            self.manifest['gates'][gate] = dict(mode='ci', hosts=[host], command=self.a.w.OWNERS[gate],
                                                required_in=(['portable', 'full'] if gate == 'portable-darwin'
                                                             else ['light', 'portable', 'full']))
            self.manifest['contracts'].append(dict(id=gate, issue=555, promise='synthetic owner contract',
                                                   supported_paths=['success'], refused_paths=['failure'], boundaries=['completion'],
                                                   cases=[dict(id=gate + '-case', binary='reliaburger', test='tests::' + gate,
                                                               requires=[gate], sources=['src/contract.rs'])]))
        self.manifest_path = self.root / 'manifest.json'
        self.manifest_path.write_text(json.dumps(self.manifest))
        self.needs = {job: dict(result='success', outputs={}) for job in ['changes', 'portable', 'macos', 'contract-boundaries']}
        self.needs['changes']['outputs'] = dict(code='true', heavy='false', macos='true')
        # The darwin required case and one other test, split over three shards.
        self.darwin = ('reliaburger', 'tests::portable-darwin')
        self.lists = {1: {self.darwin}, 2: {('reliaburger', 'tests::other')}, 3: {('suite', 'tests::third')}}
        for gate, declaration in self.manifest['gates'].items():
            host = declaration['hosts'][0]
            for shard in ([1, 2, 3] if gate == 'portable-darwin' else [None]):
                self.envelope(gate, host, shard)

    def envelope(self, gate, host, shard, *, label=None, discovery=None):
        """Write one sealed producer envelope and its trusted job output."""
        suffix = '' if shard is None else '-' + str(label or shard)
        directory = self.incoming / ('contract-' + host + '-' + gate + suffix)
        directory.mkdir(parents=True, exist_ok=True)
        (directory / 'approved.json').write_text(json.dumps({'synthetic': gate, 'shard': shard}))
        (directory / 'owner.json').write_text(json.dumps({'synthetic': 'completed'}))
        names = ['approved.json', 'owner.json']
        if shard is not None:
            (directory / 'discovery.json').write_text(json.dumps(discovery or self.discovery(shard)))
            names.append('discovery.json')
        row = dict(schema_version=1, gate=gate, context=dict(self.context, host=host),
                   manifest_sha256=c.digest(self.manifest_path),
                   files={name: c.digest(directory / name) for name in names})
        if shard is not None:
            row['shard'] = [shard, 3]
        (directory / 'gate-seal.json').write_text(json.dumps(row))
        output = self.a.output_name(gate, label or shard)
        self.needs[self.a.JOBS[gate]]['outputs'][output] = c.digest(directory / 'gate-seal.json')
        return directory

    def discovery(self, shard, lists=None):
        """A nextest list for one shard: its own tests match, the rest are other partitions."""
        lists = lists or self.lists
        suites = {}
        for number, cases in lists.items():
            for binary, name in cases:
                suite = suites.setdefault(binary, dict(binary_id=binary, testcases={}))
                status = {'status': 'matches'} if number == shard else {'status': 'mismatch', 'reason': 'partition'}
                if suite['testcases'].get(name, {}).get('filter-match') != {'status': 'matches'}:
                    suite['testcases'][name] = {'ignored': False, 'filter-match': status}
        return {'rust-build-meta': {}, 'test-count': sum(map(len, lists.values())),
                'rust-suites': {binary: {'binary-id': binary, 'status': 'listed', 'testcases': suite['testcases']}
                                for binary, suite in suites.items()}}

    def tearDown(self):
        self.tmp.cleanup()

    def oracle(self, manifest, gate, context, approved, directory, shard=None, archive=None):
        self.assertEqual(approved, {'synthetic': gate, 'shard': shard})
        # Archived owners are checked against the builder's archive digest.
        self.assertEqual(archive, self.a.builder_archive(gate, self.needs))
        required = self.a.required_cases(manifest, gate)
        if shard is None:
            return required
        return required & c.partitioned(c.read_json(Path(directory) / 'discovery.json'))[0]

    def aggregate(self, event='pull_request', release_gate=False):
        with patch.object(self.a.gates, 'candidate_sources'), patch.object(self.a, 'verify_owner', self.oracle):
            return self.a.aggregate(self.root, self.manifest_path, self.incoming, self.context, self.needs,
                                    event=event, release_gate=release_gate)

    def test_selected_portable_hosts_and_optimized_gate_all_complete(self):
        self.assertEqual(len(self.aggregate()), 3)

    def test_three_macos_shards_merge_into_one_complete_owner(self):
        verified = self.aggregate()
        self.assertEqual(verified[('portable-darwin', 'darwin')], {self.darwin})
        self.assertEqual(set(self.a.w.SHARDS), {'portable-darwin'})

    def test_a_missing_or_failed_shard_blocks_aggregation(self):
        for change in ['output', 'envelope']:
            with self.subTest(change=change):
                self.setUp()
                if change == 'output':
                    del self.needs['macos']['outputs'][self.a.output_name('portable-darwin', 3)]
                else:
                    shutil.rmtree(self.incoming / 'contract-darwin-portable-darwin-3')
                with self.assertRaises(c.Invalid):
                    self.aggregate()

    def test_one_shard_cannot_stand_in_for_another(self):
        # Shard 1's sealed envelope, uploaded and output under shard 2's name.
        shutil.rmtree(self.incoming / 'contract-darwin-portable-darwin-2')
        shutil.copytree(self.incoming / 'contract-darwin-portable-darwin-1',
                        self.incoming / 'contract-darwin-portable-darwin-2')
        self.needs['macos']['outputs'][self.a.output_name('portable-darwin', 2)] = \
            self.needs['macos']['outputs'][self.a.output_name('portable-darwin', 1)]
        with self.assertRaisesRegex(c.Invalid, 'wrong current sealed owner'):
            self.aggregate()

    def test_shards_must_run_every_listed_test_exactly_once(self):
        overlap = {1: {self.darwin}, 2: {self.darwin, ('reliaburger', 'tests::other')}, 3: {('suite', 'tests::third')}}
        unlisted = {1: {self.darwin}, 2: {('suite', 'tests::second')}, 3: {('suite', 'tests::third')}}
        for name, lists, message in [('a test in two shards', overlap, 'more than one shard'),
                                     ('a test in no shard', unlisted, 'ran in no shard')]:
            with self.subTest(name):
                self.setUp()
                for shard in (1, 2, 3):
                    discovery = self.discovery(shard, lists)
                    if name == 'a test in no shard':
                        # Listed as another shard's, but no shard ran it.
                        discovery['rust-suites']['reliaburger']['testcases']['tests::other'] = {
                            'ignored': False, 'filter-match': {'status': 'mismatch', 'reason': 'partition'}}
                    self.envelope('portable-darwin', 'darwin', shard, discovery=discovery)
                with self.assertRaisesRegex(c.Invalid, message):
                    self.aggregate()

    def test_shards_together_must_run_every_required_case(self):
        lists = {1: {('reliaburger', 'tests::x')}, 2: {('suite', 'tests::second')}, 3: {('suite', 'tests::third')}}
        for shard in (1, 2, 3):
            self.envelope('portable-darwin', 'darwin', shard, discovery=self.discovery(shard, lists))
        with self.assertRaisesRegex(c.Invalid, 'missed a required case'):
            self.aggregate()

    def test_a_shard_seal_without_its_test_list_is_refused(self):
        directory = self.incoming / 'contract-darwin-portable-darwin-2'
        seal = c.read_json(directory / 'gate-seal.json')
        del seal['files']['discovery.json']
        (directory / 'gate-seal.json').write_text(json.dumps(seal))
        self.needs['macos']['outputs'][self.a.output_name('portable-darwin', 2)] = c.digest(directory / 'gate-seal.json')
        with self.assertRaisesRegex(c.Invalid, 'missing sealed shard test list'):
            self.aggregate()

    def test_shard_names_on_the_command_line_are_exact(self):
        self.assertEqual(self.a.parse_shard('portable-darwin', '2/3'), 2)
        self.assertIsNone(self.a.parse_shard('portable-linux', None))
        for gate, text in [('portable-darwin', '2/4'), ('portable-darwin', '0/3'), ('portable-darwin', '4/3'),
                           ('portable-darwin', '2'), ('portable-darwin', 'x/3'), ('portable-linux', '1/3')]:
            with self.subTest(gate=gate, text=text), self.assertRaises(c.Invalid):
                self.a.parse_shard(gate, text)

    def test_a_split_owner_cannot_produce_without_a_shard_or_a_whole_owner_with_one(self):
        for gate, shard in [('portable-darwin', None), ('portable-linux', 1)]:
            with self.subTest(gate=gate), self.assertRaises(c.Invalid):
                self.a.produce(self.root, self.manifest_path, gate, dict(self.context, host='darwin'),
                               self.root / 'target/contracts/darwin' / gate, shard=shard)

    def test_shards_must_list_the_same_unpartitioned_tests(self):
        lists = dict(self.lists, extra={('suite', 'tests::only-in-shard-3')})
        self.envelope('portable-darwin', 'darwin', 3, discovery=self.discovery(3, lists))
        with self.assertRaisesRegex(c.Invalid, 'different unpartitioned tests'):
            self.aggregate()

    def test_a_light_pull_request_needs_only_linux_and_the_optimized_boundary(self):
        self.needs['changes']['outputs']['macos'] = 'false'
        del self.needs['macos']
        for shard in (1, 2, 3):
            shutil.rmtree(self.incoming / ('contract-darwin-portable-darwin-' + str(shard)))
        verified = self.aggregate()
        self.assertEqual(set(verified), {('portable-linux', 'linux'), ('optimized-cron', 'linux')})

    def test_a_light_run_still_refuses_a_failed_selected_owner(self):
        self.needs['changes']['outputs']['macos'] = 'false'
        self.needs['contract-boundaries']['result'] = 'failure'
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_only_a_pull_request_may_qualify_without_every_owner(self):
        # Pushes to a train or main, merge-queue groups and manual runs
        # always demand the full set, whatever select jobs said.
        for event in ['push', 'merge_group', 'schedule', 'workflow_dispatch']:
            for macos in ['true', 'false']:
                with self.subTest(event=event, macos=macos):
                    self.needs['changes']['outputs']['macos'] = macos
                    with self.assertRaisesRegex(c.Invalid, 'every owner'):
                        self.aggregate(event=event)
        self.needs['changes']['outputs']['macos'] = 'true'
        self.assertEqual(len(self.aggregate(event='pull_request')), 3)

    def test_the_release_gate_pull_request_must_qualify_with_every_owner(self):
        # An ordinary PR may leave macOS out; the release gate PR may not,
        # whatever select jobs said (#1443).
        self.needs['changes']['outputs']['macos'] = 'false'
        del self.needs['macos']
        for shard in (1, 2, 3):
            shutil.rmtree(self.incoming / ('contract-darwin-portable-darwin-' + str(shard)))
        self.assertEqual(len(self.aggregate()), 2)
        with self.assertRaisesRegex(c.Invalid, 'release gate'):
            self.aggregate(release_gate=True)

    def heavy_owners(self, macos):
        """Add every heavy owner, sealed and successful, to the synthetic run.

        Returns the calls the legacy ignored-owner check received, which the
        tests patch out: its own fixtures live in test_oci_legacy_owner.py
        and test_ignored_owners.py.
        """
        self.root.joinpath('Makefile').write_text(''.join(
            f'{command[1]}:\n\ttrue\n' for command in self.a.w.OWNERS.values() if command[0] == 'make'))
        script = self.root / self.a.w.OWNERS['oci-interruptions'][0]
        script.parent.mkdir(parents=True, exist_ok=True)
        script.write_text('#!/bin/sh\n')
        self.root.joinpath('scripts/ci').mkdir(parents=True, exist_ok=True)
        self.root.joinpath('scripts/ci/legacy-oci-aliases.json').write_text('{}')
        self.bindings = self.root / 'ignored-bindings.json'
        self.bindings.write_text('[]')
        for job in ['linux', 'build-tests', 'cluster', 'acceptance']:
            self.needs[job] = dict(result='success', outputs={})
        for gate in self.a.w.HEAVY_GATES:
            self.manifest['gates'][gate] = dict(mode='ci', hosts=['linux'], command=self.a.w.OWNERS[gate],
                                                required_in=['train-full', 'full'])
            self.manifest['contracts'].append(dict(id=gate, issue=555, promise='synthetic heavy owner',
                                                   supported_paths=['success'], refused_paths=['failure'],
                                                   boundaries=['completion'],
                                                   cases=[dict(id=gate + '-case', binary='reliaburger',
                                                               test='tests::' + gate, requires=[gate],
                                                               sources=['src/contract.rs'])]))
        for gate in ['portable-linux', 'optimized-cron']:
            self.manifest['gates'][gate]['required_in'].append('train-full')
        self.manifest_path.write_text(json.dumps(self.manifest))
        for gate, declaration in self.manifest['gates'].items():
            for shard in ([1, 2, 3] if gate == 'portable-darwin' else [None]):
                self.envelope(gate, declaration['hosts'][0], shard)
        self.needs['changes']['outputs'].update(heavy='true', macos=macos)
        if macos == 'false':
            del self.needs['macos']
            for shard in (1, 2, 3):
                shutil.rmtree(self.incoming / ('contract-darwin-portable-darwin-' + str(shard)))
        return []

    def aggregate_heavy(self, legacy, event='pull_request', release_gate=False):
        legacy_owner = importlib.import_module('oci_legacy_owner')
        ignored = importlib.import_module('ignored_owners')

        def problems(root, incoming, bindings, owned):
            legacy.append(set(owned))
            return []

        with patch.object(self.a.gates, 'candidate_sources'), patch.object(self.a, 'verify_owner', self.oracle), \
                patch.object(legacy_owner, 'qualify', return_value=set()), \
                patch.object(ignored, 'evidence_problems', problems):
            return self.a.aggregate(self.root, self.manifest_path, self.incoming, self.context, self.needs,
                                    self.bindings, event=event, release_gate=release_gate)

    def test_a_full_ci_train_pull_request_needs_every_heavy_owner_but_not_macos(self):
        legacy = self.heavy_owners(macos='false')
        verified = self.aggregate_heavy(legacy)
        self.assertEqual(set(verified), {(gate, self.manifest['gates'][gate]['hosts'][0])
                                         for gate in self.a.w.OWNERS if gate != 'portable-darwin'})
        # The heavy owners' ignored cases are still checked case by case.
        self.assertEqual(len(legacy), 1)
        self.assertIn(('make', 'test-cluster'), legacy[0])

    def test_a_full_ci_train_pull_request_still_refuses_a_missing_heavy_owner(self):
        self.heavy_owners(macos='false')
        digest = self.needs['cluster']['outputs'].pop(self.a.output_name('cluster-tests'))
        with self.assertRaisesRegex(c.Invalid, 'missing successful owner step output'):
            self.aggregate_heavy([])
        self.needs['cluster']['outputs'][self.a.output_name('cluster-tests')] = digest
        self.needs['acceptance']['result'] = 'skipped'
        with self.assertRaises(c.Invalid):
            self.aggregate_heavy([])

    def test_push_and_merge_queue_still_demand_the_macos_owners(self):
        # Only a pull request may leave the macOS shards out: the train push,
        # the merge queue, schedules and manual runs need every owner.
        legacy = self.heavy_owners(macos='false')
        for event in ['push', 'merge_group', 'schedule', 'workflow_dispatch']:
            with self.subTest(event=event), self.assertRaisesRegex(c.Invalid, 'every owner'):
                self.aggregate_heavy(legacy, event=event)
        # Claiming macOS ran without its shards' sealed outputs fails too.
        self.needs['changes']['outputs']['macos'] = 'true'
        self.needs['macos'] = dict(result='success', outputs={})
        for event in ['push', 'merge_group']:
            with self.subTest(event=event, macos='claimed'), \
                    self.assertRaisesRegex(c.Invalid, 'missing successful owner step output'):
                self.aggregate_heavy(legacy, event=event)

    def test_the_release_gate_pull_request_passes_with_every_owner(self):
        legacy = self.heavy_owners(macos='true')
        self.assertEqual(len(self.aggregate_heavy(legacy, release_gate=True)), len(self.a.w.OWNERS))

    def test_the_release_gate_pull_request_refuses_the_heavy_set_without_macos(self):
        legacy = self.heavy_owners(macos='false')
        self.assertEqual(len(self.aggregate_heavy(legacy)), len(self.a.w.OWNERS) - 1)
        with self.assertRaisesRegex(c.Invalid, 'release gate'):
            self.aggregate_heavy(legacy, release_gate=True)

    def test_push_and_merge_queue_pass_with_every_owner_macos_included(self):
        legacy = self.heavy_owners(macos='true')
        for event in ['push', 'merge_group']:
            with self.subTest(event=event):
                self.assertEqual(len(self.aggregate_heavy(legacy, event=event)), len(self.a.w.OWNERS))

    def test_missing_job_cannot_be_inferred_completed_from_reports(self):
        del self.needs['macos']
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_failed_skipped_canceled_or_unfinished_job_blocks_aggregation(self):
        for status in ['failure', 'skipped', 'cancelled', 'in_progress']:
            self.needs['portable']['result'] = status
            with self.assertRaises(c.Invalid):
                self.aggregate()

    def test_missing_gate_output_cannot_be_supplied_by_its_own_artifact(self):
        self.needs['portable']['outputs'].clear()
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_a_different_current_context_never_accepts_previous_artifacts(self):
        for field, value in [('attempt', '1'), ('run_id', '999'), ('commit', 'b' * 40)]:
            original = self.context[field]
            self.context[field] = value
            with self.assertRaises(c.Invalid):
                self.aggregate()
            self.context[field] = original

    def reseal(self, gate, shard=None, **context):
        """Rewrite one owner's seal as another producer would, with a matching trusted output."""
        host = self.manifest['gates'][gate]['hosts'][0]
        directory = self.incoming / ('contract-' + host + '-' + self.a.label(gate, shard))
        seal = c.read_json(directory / 'gate-seal.json')
        seal['context'].update(context)
        (directory / 'gate-seal.json').write_text(json.dumps(seal))
        self.needs[self.a.JOBS[gate]]['outputs'][self.a.output_name(gate, shard)] = \
            c.digest(directory / 'gate-seal.json')

    def test_rerun_of_failed_jobs_accepts_owners_sealed_by_an_earlier_attempt_of_this_run(self):
        # Attempt 2 ran every owner; "re-run failed jobs" then re-ran only the
        # failed macOS shard as attempt 3, and GitHub carried the rest over.
        self.context['attempt'] = '3'
        self.reseal('portable-darwin', 2, attempt='3')
        seen = {}

        def oracle(manifest, gate, context, approved, directory, shard=None, archive=None):
            seen[(gate, shard)] = context
            return self.oracle(manifest, gate, context, approved, directory, shard, archive)

        with patch.object(self.a.gates, 'candidate_sources'), patch.object(self.a, 'verify_owner', oracle):
            verified = self.a.aggregate(self.root, self.manifest_path, self.incoming, self.context, self.needs)
        self.assertEqual(len(verified), 3)
        # Each owner's cases are checked against the attempt that produced them;
        # each macOS shard is its own matrix job with its own attempt.
        self.assertEqual(seen[('portable-darwin', 2)], dict(self.context, host='darwin', attempt='3'))
        for shard in (1, 3):
            self.assertEqual(seen[('portable-darwin', shard)], dict(self.context, host='darwin', attempt='2'))
        self.assertEqual(seen[('portable-linux', None)], dict(self.context, host='linux', attempt='2'))
        self.assertEqual(seen[('optimized-cron', None)], dict(self.context, host='linux', attempt='2'))

    def test_earlier_attempt_seal_from_another_run_commit_or_host_is_refused(self):
        for field, value in [('run_id', '999'), ('commit', 'b' * 40), ('host', 'darwin')]:
            with self.subTest(field=field):
                self.tearDown()
                self.setUp()
                self.context['attempt'] = '3'
                self.reseal('portable-linux', **{field: value})
                with self.assertRaisesRegex(c.Invalid, 'wrong current sealed owner'):
                    self.aggregate()

    def test_seal_from_a_later_zero_or_malformed_attempt_is_refused(self):
        for attempt in ['3', '0', '', '1a', 2]:
            with self.subTest(attempt=attempt):
                self.tearDown()
                self.setUp()
                self.reseal('portable-linux', attempt=attempt)
                with self.assertRaisesRegex(c.Invalid, 'wrong current sealed owner'):
                    self.aggregate()

    def test_seal_with_extra_context_fields_is_refused(self):
        self.reseal('portable-linux', authority='forged')
        with self.assertRaisesRegex(c.Invalid, 'wrong current sealed owner'):
            self.aggregate()

    def test_owners_of_one_job_must_come_from_the_same_attempt(self):
        # portable-darwin and optimized-cron belong to different jobs; put two
        # owners in one job so that a mixed set is possible.
        self.a.JOBS['optimized-cron'] = 'portable'
        self.addCleanup(self.a.JOBS.__setitem__, 'optimized-cron', 'contract-boundaries')
        self.needs['portable']['outputs'][self.a.output_name('optimized-cron')] = (
            self.needs['contract-boundaries']['outputs'].pop(self.a.output_name('optimized-cron')))
        self.context['attempt'] = '3'
        self.reseal('optimized-cron', attempt='3')
        with self.assertRaisesRegex(c.Invalid, 'one job sealed owners in different attempts'):
            self.aggregate()

    def test_same_named_report_in_wrong_host_gate_does_not_count(self):
        shutil.rmtree(self.incoming / 'contract-darwin-portable-darwin-1')
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_sealed_expected_plan_changed_after_producer_cannot_authorize_itself(self):
        directory = self.incoming / 'contract-linux-portable-linux'
        (directory / 'approved.json').write_text(json.dumps({'synthetic': 'forged'}))
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_seal_cannot_replace_the_trusted_job_output_digest(self):
        directory = self.incoming / 'contract-linux-portable-linux'
        seal = c.read_json(directory / 'gate-seal.json')
        seal['context']['run_id'] = '999'
        (directory / 'gate-seal.json').write_text(json.dumps(seal))
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_payload_path_traversal_is_refused_even_with_matching_trusted_digest(self):
        directory = self.incoming / 'contract-linux-portable-linux'
        seal = c.read_json(directory / 'gate-seal.json')
        seal['files']['../escape'] = '0' * 64
        (directory / 'gate-seal.json').write_text(json.dumps(seal))
        self.needs['portable']['outputs']['portable_linux_sha256'] = c.digest(directory / 'gate-seal.json')
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_current_manifest_changed_cannot_reuse_previous_success(self):
        self.manifest['contracts'][0]['promise'] = 'changed current contract'
        self.manifest_path.write_text(json.dumps(self.manifest))
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_actual_case_verifier_is_required_after_seal_hash_validation(self):
        with patch.object(self.a.gates, 'candidate_sources'), patch.object(self.a, 'verify_owner', side_effect=c.Invalid('failed real case')):
            with self.assertRaises(c.Invalid):
                self.a.aggregate(self.root, self.manifest_path, self.incoming, self.context, self.needs)

    def test_full_selection_requires_original_heavy_jobs_instead_of_smaller_plan(self):
        self.needs['changes']['outputs']['heavy'] = 'true'
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_failed_archive_builder_does_not_authorize_downloaded_archive(self):
        needs = {'build-tests': {'result': 'failure', 'outputs': {'archive_sha256': 'a' * 64, 'archive_origin_sha256': 'b' * 64}}}
        with self.assertRaises(c.Invalid):
            self.a.archive_origin(dict(self.context, host='linux'), needs)

    def test_archive_expected_command_and_identity_are_not_taken_from_artifact(self):
        needs = {'build-tests': {'result': 'success', 'outputs': {'archive_sha256': 'a' * 64, 'archive_origin_sha256': 'b' * 64}}}
        origin = self.a.archive_origin(dict(self.context, host='linux'), needs)
        self.assertEqual(origin['builder']['command'], self.a.w.ARCHIVE_COMMAND)
        self.assertEqual(origin['builder']['commit'], self.context['commit'])
        self.assertEqual(origin['sha256'], 'b' * 64)

    def test_failed_case_cannot_publish_a_successful_step_digest(self):
        directory = self.root / 'failed-producer'
        directory.mkdir()
        with patch.object(self.a, 'verify_owner', side_effect=c.Invalid('skipped testcase')):
            with self.assertRaises(c.Invalid):
                self.a.seal(self.manifest_path, self.manifest, 'portable-linux', dict(self.context, host='linux'), {}, directory)
        self.assertFalse((directory / 'gate-seal.json').exists())

    def test_step_output_is_emitted_only_for_a_valid_producer_digest(self):
        path = self.root / 'github-output'
        with self.assertRaises(c.Invalid):
            self.a.write_output(path, 'portable_linux_sha256', 'not-a-digest')
        self.assertFalse(path.exists())
        self.a.write_output(path, 'portable_linux_sha256', 'a' * 64)
        self.assertEqual(path.read_text(), 'portable_linux_sha256=' + 'a' * 64 + '\n')

    def test_archived_runtime_is_bound_to_current_builder_outputs_and_tools(self):
        import test_owner_tools
        fixture = test_owner_tools.OwnerTools()
        fixture.setUp()
        try:
            runtime = dict(context=copy.deepcopy(self.context), tools=fixture.tools,
                           command=[fixture.tools['nextest']['path'], 'nextest', 'archive'] + self.a.w.ARCHIVE_COMMAND[3:],
                           exit_code=0, origin_sha256='b' * 64)
            path = self.root / 'archive-runtime.json'
            path.write_text(json.dumps(runtime))
            needs = {'build-tests': {'result': 'success', 'outputs': {'archive_runtime_sha256': c.digest(path),
                                                                       'archive_origin_sha256': 'b' * 64}}}
            self.assertEqual(self.a.verify_archive_runtime(self.root, dict(self.context, host='linux'), needs), runtime)
            runtime['context']['attempt'] = '99'
            path.write_text(json.dumps(runtime))
            needs['build-tests']['outputs']['archive_runtime_sha256'] = c.digest(path)
            with self.assertRaises(c.Invalid):
                self.a.verify_archive_runtime(self.root, dict(self.context, host='linux'), needs)
        finally:
            fixture.tearDown()

    def test_rerun_owner_accepts_an_archive_built_by_an_earlier_attempt_of_this_run(self):
        # build-tests passed in attempt 1; the cluster job failed and was
        # re-run alone as attempt 2, so it consumes the attempt-1 archive.
        import test_owner_tools
        fixture = test_owner_tools.OwnerTools()
        fixture.setUp()
        try:
            builder_context = dict(self.context, attempt='1')
            runtime = dict(context=builder_context, tools=fixture.tools,
                           command=[fixture.tools['nextest']['path'], 'nextest', 'archive'] + self.a.w.ARCHIVE_COMMAND[3:],
                           exit_code=0, origin_sha256='b' * 64)
            path = self.root / 'archive-runtime.json'
            path.write_text(json.dumps(runtime))
            needs = {'build-tests': {'result': 'success', 'outputs': {
                'archive_sha256': 'a' * 64, 'archive_origin_sha256': 'b' * 64,
                'archive_runtime_sha256': c.digest(path)}}}
            current = dict(self.context, host='linux')
            self.assertEqual(self.a.verify_archive_runtime(self.root, current, needs), runtime)
            origin = self.a.archive_origin(current, needs, runtime)
            self.assertEqual(origin['builder']['attempt'], '1')
            self.assertEqual(origin['builder']['run_id'], self.context['run_id'])
            self.assertEqual(origin['builder']['commit'], self.context['commit'])
            for field, value in [('run_id', '999'), ('commit', 'b' * 40), ('attempt', '0')]:
                with self.subTest(field=field):
                    path.write_text(json.dumps(dict(runtime, context=dict(builder_context, **{field: value}))))
                    needs['build-tests']['outputs']['archive_runtime_sha256'] = c.digest(path)
                    with self.assertRaises(c.Invalid):
                        self.a.verify_archive_runtime(self.root, current, needs)
        finally:
            fixture.tearDown()

    def test_failed_archive_builder_runtime_is_refused(self):
        path = self.root / 'archive-runtime.json'
        path.write_text('{}')
        needs = {'build-tests': {'result': 'failure', 'outputs': {
            'archive_runtime_sha256': c.digest(path), 'archive_origin_sha256': 'b' * 64}}}
        with self.assertRaisesRegex(c.Invalid, 'archive build job did not succeed'):
            self.a.verify_archive_runtime(self.root, dict(self.context, host='linux'), needs)

    def test_cron_metadata_refuses_feature_drift_before_tests(self):
        from types import SimpleNamespace
        calls = []
        def run(command, **kwargs):
            calls.append(command)
            features = ['std'] if '--manifest-path' not in command else ['std', 'extra']
            data = {'version': 1, 'packages': [{'id': 't', 'name': 'time', 'version': '0.3.47'},
                                               {'id': 'e', 'name': 'thiserror', 'version': '2.0.18'}],
                    'resolve': {'root': 'root', 'nodes': [
                        {'id': 'root', 'features': [], 'deps': [
                            {'name': 'time', 'pkg': 't', 'dep_kinds': [{'kind': None, 'target': None}]},
                            {'name': 'thiserror', 'pkg': 'e', 'dep_kinds': [{'kind': None, 'target': None}]}]},
                        {'id': 't', 'features': features}, {'id': 'e', 'features': ['std']}]}}
            return SimpleNamespace(returncode=0, stdout=json.dumps(data))
        with self.assertRaises(c.Invalid):
            self.a.cron_dependencies(self.root, run=run)
        self.assertEqual(len(calls), 2)

    def test_cron_metadata_schema_bool_or_new_version_refuses_before_resolution(self):
        from types import SimpleNamespace
        for version in [True, 2]:
            with self.assertRaises(c.Invalid):
                self.a.cron_dependencies(self.root, run=lambda *a, **k: SimpleNamespace(returncode=0, stdout=json.dumps({'version': version})))


class CronDirectDependencies(unittest.TestCase):
    """Use root edges rather than similarly named transitive packages."""

    def setUp(self):
        self.adapter = importlib.import_module('workflow_adapter')
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.main = self.metadata()
        self.tiny = self.metadata()

    def tearDown(self):
        self.tmp.cleanup()

    @staticmethod
    def metadata():
        return {
            'version': 1,
            'packages': [
                {'id': 'time-direct', 'name': 'time', 'version': '0.3.47'},
                {'id': 'error-direct', 'name': 'thiserror', 'version': '2.0.18'},
            ],
            'resolve': {
                'root': 'root',
                'nodes': [
                    {'id': 'root', 'features': [], 'deps': [
                        {'name': 'time', 'pkg': 'time-direct',
                         'dep_kinds': [{'kind': None, 'target': None}]},
                        {'name': 'thiserror', 'pkg': 'error-direct',
                         'dep_kinds': [{'kind': None, 'target': None}]},
                    ]},
                    {'id': 'time-direct', 'features': ['std', 'formatting']},
                    {'id': 'error-direct', 'features': ['std']},
                ],
            },
        }

    def run_metadata(self, command, **kwargs):
        from types import SimpleNamespace
        data = self.tiny if '--manifest-path' in command else self.main
        return SimpleNamespace(returncode=0, stdout=json.dumps(data))

    def inspect(self):
        return self.adapter.cron_dependencies(self.root, run=self.run_metadata)

    def test_another_transitive_version_does_not_obscure_direct_production_dependency(self):
        self.main['packages'].append(
            {'id': 'error-transitive', 'name': 'thiserror', 'version': '1.0.69'})
        self.main['resolve']['nodes'].append(
            {'id': 'error-transitive', 'features': ['std']})
        observations = self.inspect()
        self.assertEqual(observations[0], observations[1])
        self.assertEqual(observations[0]['thiserror'],
                         {'version': '2.0.18', 'features': ['std']})
        self.assertEqual(observations[0]['time'],
                         {'version': '0.3.47', 'features': ['formatting', 'std']})

    def test_missing_or_unknown_root_is_refused(self):
        for root in (None, 'not-a-node'):
            with self.subTest(root=root):
                self.main['resolve']['root'] = root
                with self.assertRaises(c.Invalid):
                    self.inspect()

    def test_missing_direct_dependency_cannot_fall_back_to_transitive_package(self):
        self.main['resolve']['nodes'][0]['deps'].pop()
        with self.assertRaises(c.Invalid):
            self.inspect()

    def test_duplicate_default_direct_edge_is_refused(self):
        edges = self.main['resolve']['nodes'][0]['deps']
        edges.append(copy.deepcopy(edges[-1]))
        with self.assertRaises(c.Invalid):
            self.inspect()

    def test_wrong_edge_name_is_refused(self):
        self.main['resolve']['nodes'][0]['deps'][-1]['name'] = 'unrelated'
        with self.assertRaises(c.Invalid):
            self.inspect()

    def test_edge_package_id_must_resolve_to_the_named_package(self):
        self.main['resolve']['nodes'][0]['deps'][-1]['pkg'] = 'time-direct'
        with self.assertRaises(c.Invalid):
            self.inspect()

    def test_missing_edge_package_id_is_refused(self):
        self.main['resolve']['nodes'][0]['deps'][-1]['pkg'] = 'missing'
        with self.assertRaises(c.Invalid):
            self.inspect()

    def test_development_build_or_target_only_dependencies_are_not_default_production(self):
        edge = self.main['resolve']['nodes'][0]['deps'][-1]
        for kind, target in [('dev', None), ('build', None), (None, 'cfg(unix)')]:
            with self.subTest(kind=kind, target=target):
                edge['dep_kinds'] = [{'kind': kind, 'target': target}]
                with self.assertRaises(c.Invalid):
                    self.inspect()

    def test_direct_node_features_and_version_must_match(self):
        self.tiny['resolve']['nodes'][-1]['features'].append('other')
        with self.assertRaises(c.Invalid):
            self.inspect()
        self.tiny = self.metadata()
        self.tiny['packages'][-1]['version'] = '2.0.19'
        with self.assertRaises(c.Invalid):
            self.inspect()

    def test_direct_resolved_node_must_exist_once(self):
        for nodes in (self.main['resolve']['nodes'][:-1],
                      self.main['resolve']['nodes'] + [copy.deepcopy(self.main['resolve']['nodes'][-1])]):
            with self.subTest(count=len(nodes)):
                original = self.main['resolve']['nodes']
                self.main['resolve']['nodes'] = nodes
                with self.assertRaises(c.Invalid):
                    self.inspect()
                self.main['resolve']['nodes'] = original
