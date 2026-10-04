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
            self.manifest['gates'][gate] = dict(mode='ci', hosts=[host], command=self.a.w.OWNERS[gate])
            self.manifest['contracts'].append(dict(id=gate, issue=555, promise='synthetic owner contract',
                                                   supported_paths=['success'], refused_paths=['failure'], boundaries=['completion'],
                                                   cases=[dict(id=gate + '-case', binary='reliaburger', test='tests::' + gate,
                                                               requires=[gate], sources=['src/contract.rs'])]))
        self.manifest_path = self.root / 'manifest.json'
        self.manifest_path.write_text(json.dumps(self.manifest))
        self.needs = {job: dict(result='success', outputs={}) for job in ['changes', 'portable', 'macos', 'contract-boundaries']}
        self.needs['changes']['outputs'] = dict(code='true', heavy='false')
        for gate, declaration in self.manifest['gates'].items():
            host = declaration['hosts'][0]
            directory = self.incoming / ('contract-' + host + '-' + gate)
            directory.mkdir(parents=True)
            (directory / 'approved.json').write_text(json.dumps({'synthetic': gate}))
            (directory / 'owner.json').write_text(json.dumps({'synthetic': 'completed'}))
            row = dict(schema_version=1, gate=gate, context=dict(self.context, host=host),
                       manifest_sha256=c.digest(self.manifest_path),
                       files={name: c.digest(directory / name) for name in ['approved.json', 'owner.json']})
            (directory / 'gate-seal.json').write_text(json.dumps(row))
            self.needs[self.a.JOBS[gate]]['outputs'][self.a.output_name(gate)] = c.digest(directory / 'gate-seal.json')

    def tearDown(self):
        self.tmp.cleanup()

    def oracle(self, manifest, gate, context, approved, directory):
        self.assertEqual(approved, {'synthetic': gate})
        return self.a.required_cases(manifest, gate)

    def aggregate(self):
        with patch.object(self.a.gates, 'candidate_sources'), patch.object(self.a, 'verify_owner', self.oracle):
            return self.a.aggregate(self.root, self.manifest_path, self.incoming, self.context, self.needs)

    def test_selected_portable_hosts_and_optimized_gate_all_complete(self):
        self.assertEqual(len(self.aggregate()), 3)

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

    def test_same_named_report_in_wrong_host_gate_does_not_count(self):
        shutil.rmtree(self.incoming / 'contract-darwin-portable-darwin')
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
