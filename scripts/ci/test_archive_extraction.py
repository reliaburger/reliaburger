"""One archive extraction per consumer job, bound into every owner's evidence (#1050).

Synthetic controls: a fake nextest writes the two metadata files that
`cargo nextest list --archive-file ... --extract-to` writes. They prove the
trust chain, not hosted timing or a real archive layout.
"""
import copy
import importlib
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

import contracts as c
import test_owner_tools


class ArchiveExtraction(unittest.TestCase):
    def setUp(self):
        self.a = importlib.import_module('workflow_adapter')
        self.w = self.a.w
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name).resolve()
        self.context = dict(commit='a' * 40, run_id='123', attempt='2', host='linux')
        fixture = test_owner_tools.OwnerTools()
        fixture.setUp()
        self.addCleanup(fixture.tearDown)
        self.tools = copy.deepcopy(fixture.tools)
        for observed in self.tools.values():
            Path(observed['path']).write_text('synthetic actual tool')
            observed['sha256'] = c.digest(observed['path'])
        archive = self.root / 'tests.tar.zst'
        archive.write_bytes(b'synthetic archive bytes')
        builder = dict(schema_version=1, commit=self.context['commit'], run_id=self.context['run_id'],
                       attempt=self.context['attempt'], host='linux', command=self.w.ARCHIVE_COMMAND,
                       exit_code=0, archive_sha256=c.digest(archive))
        (self.root / 'archive-origin.json').write_text(json.dumps(builder))
        runtime = dict(context={key: self.context[key] for key in ('commit', 'run_id', 'attempt')},
                       tools=self.tools,
                       command=[self.tools['nextest']['path'], 'nextest', 'archive'] + self.w.ARCHIVE_COMMAND[3:],
                       exit_code=0, origin_sha256=c.digest(self.root / 'archive-origin.json'))
        (self.root / 'archive-runtime.json').write_text(json.dumps(runtime))
        self.needs = {'build-tests': {'result': 'success', 'outputs': {
            'archive_sha256': builder['archive_sha256'],
            'archive_origin_sha256': c.digest(self.root / 'archive-origin.json'),
            'archive_runtime_sha256': c.digest(self.root / 'archive-runtime.json')}}}
        self.calls = []
        self.status = 0
        self.writes = True

    def tearDown(self):
        self.tmp.cleanup()

    def fake_nextest(self, command, **kwargs):
        self.calls.append(list(command))
        if self.writes:
            for name in self.w.ARCHIVE_METADATA:
                path = self.root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('extracted ' + name)
        kwargs['stdout'].write(b'{"rust-binaries": {}}')
        return SimpleNamespace(returncode=self.status)

    def extract(self):
        return self.a.extract(self.root, self.context, self.needs, self.tools, environment={}, run=self.fake_nextest)

    def origin(self):
        runtime = self.a.verify_archive_runtime(self.root, self.context, self.needs)
        return self.a.archive_origin(self.context, self.needs, runtime)

    def test_the_job_checks_the_archive_extracts_it_once_and_records_its_metadata(self):
        digest = self.extract()
        self.assertEqual(self.calls, [self.w.extraction_command(self.tools['nextest']['path'])])
        receipt = self.root / self.a.EXTRACTION
        self.assertEqual(c.digest(receipt), digest)
        row = c.read_json(receipt)
        self.assertEqual(row['archive_sha256'], self.needs['build-tests']['outputs']['archive_sha256'])
        self.assertEqual(row['metadata'], {name: c.digest(self.root / name) for name in self.w.ARCHIVE_METADATA})
        self.assertEqual(row['context'], self.context)
        # Every owner of the job loads this same receipt by its step output.
        self.assertEqual(self.a.load_extraction(self.root, digest, self.context, self.origin()), row)

    def test_an_archive_that_differs_from_its_builder_is_never_extracted(self):
        (self.root / 'tests.tar.zst').write_bytes(b'another archive')
        with self.assertRaisesRegex(c.Invalid, 'archive differs from successful builder'):
            self.extract()
        self.assertEqual(self.calls, [])
        self.assertFalse((self.root / self.a.EXTRACTION).exists())

    def test_a_failed_builder_authorises_no_extraction(self):
        self.needs['build-tests']['result'] = 'failure'
        with self.assertRaises(c.Invalid):
            self.extract()
        self.assertEqual(self.calls, [])

    def test_a_failed_extraction_leaves_no_receipt(self):
        self.status = 101
        with self.assertRaisesRegex(c.Invalid, 'archive extraction failed'):
            self.extract()
        self.assertFalse((self.root / self.a.EXTRACTION).exists())

    def test_stale_metadata_cannot_stand_in_for_an_extraction_that_wrote_none(self):
        for name in self.w.ARCHIVE_METADATA:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('left behind by something else')
        self.writes = False
        with self.assertRaisesRegex(c.Invalid, 'nextest metadata'):
            self.extract()
        self.assertFalse((self.root / self.a.EXTRACTION).exists())

    def test_a_second_extraction_in_the_same_job_is_refused(self):
        self.extract()
        with self.assertRaisesRegex(c.Invalid, 'already extracted'):
            self.extract()
        self.assertEqual(len(self.calls), 1)

    def test_an_owner_loads_only_the_extraction_its_step_output_names(self):
        digest = self.extract()
        origin = self.origin()
        for missing in (None, '', 'not-a-digest'):
            with self.subTest(digest=missing), self.assertRaisesRegex(c.Invalid, 'trusted archive extraction digest required'):
                self.a.load_extraction(self.root, missing, self.context, origin)
        with self.assertRaisesRegex(c.Invalid, 'trusted step output'):
            self.a.load_extraction(self.root, 'f' * 64, self.context, origin)
        receipt = self.root / self.a.EXTRACTION
        row = c.read_json(receipt)
        row['metadata'][self.w.ARCHIVE_METADATA[0]] = '0' * 64
        receipt.write_text(json.dumps(row))
        with self.assertRaisesRegex(c.Invalid, 'trusted step output'):
            self.a.load_extraction(self.root, digest, self.context, origin)

    def test_an_extraction_of_another_archive_run_attempt_or_command_is_refused(self):
        self.extract()
        origin = self.origin()
        row = c.read_json(self.root / self.a.EXTRACTION)
        self.a.check_extraction(row, self.context, origin)
        changes = {
            'archive': dict(row, archive_sha256='9' * 64),
            'origin receipt': dict(row, archive_origin_sha256='9' * 64),
            'run': dict(row, context=dict(row['context'], run_id='999')),
            'earlier attempt': dict(row, context=dict(row['context'], attempt='1')),
            'host': dict(row, context=dict(row['context'], host='darwin')),
            'command': dict(row, command=row['command'][:-2]),
            'relative tool': dict(row, command=['cargo-nextest'] + row['command'][1:]),
            'failed': dict(row, exit_code=1),
            'missing metadata': dict(row, metadata={self.w.ARCHIVE_METADATA[0]: '3' * 64}),
            'bad digest': dict(row, metadata={name: 'xyz' for name in self.w.ARCHIVE_METADATA}),
            'unknown field': dict(row, files={}),
        }
        for name, changed in changes.items():
            with self.subTest(change=name), self.assertRaises(c.Invalid):
                self.a.check_extraction(changed, self.context, origin)

    def test_the_extract_operation_publishes_the_receipt_digest_as_a_step_output(self):
        output = self.root / 'github-output'
        with patch.object(self.a.make_owner, 'capture_tools', return_value=self.tools), \
                patch.object(self.a, 'extract', return_value='e' * 64) as extract:
            (self.root / 'needs.json').write_text(json.dumps(self.needs))
            status = self.a.main(['extract', '--root', str(self.root), '--host', 'linux',
                                  '--needs', str(self.root / 'needs.json'), '--commit', self.context['commit'],
                                  '--run-id', '123', '--attempt', '2', '--github-output', str(output)])
        self.assertEqual(status, 0)
        self.assertEqual(extract.call_args.args[1], self.context)
        self.assertEqual(output.read_text(), 'archive_extraction_sha256=' + 'e' * 64 + '\n')


class SealedArchivedOwner(unittest.TestCase):
    """Aggregation rechecks that an archived owner ran its job's extracted build."""

    def setUp(self):
        self.a = importlib.import_module('workflow_adapter')
        self.w = self.a.w
        self.context = dict(commit='a' * 40, run_id='123', attempt='2', host='linux')
        fixture = test_owner_tools.OwnerTools()
        fixture.setUp()
        self.addCleanup(fixture.tearDown)
        self.tools = fixture.tools
        self.manifest = {'gates': {'cluster-tests': {'mode': 'ci', 'hosts': ['linux'], 'command': ['make', 'test-cluster']}},
                         'contracts': [{'cases': [{'binary': 'cluster_failover', 'test': 'failover',
                                                   'requires': ['cluster-tests'], 'sources': ['tests/cluster_failover.rs']}]}]}
        builder = dict(schema_version=1, commit='a' * 40, run_id='123', attempt='2', host='linux',
                       command=self.w.ARCHIVE_COMMAND, exit_code=0, archive_sha256='1' * 64)
        origin = dict(path='archive-origin.json', sha256='2' * 64, builder=builder)
        metadata = {self.w.ARCHIVE_METADATA[0]: '3' * 64, self.w.ARCHIVE_METADATA[1]: '4' * 64}
        extraction = dict(schema_version=1, context=dict(self.context), archive_sha256='1' * 64,
                          archive_origin_sha256='2' * 64,
                          command=self.w.extraction_command(self.tools['nextest']['path']),
                          exit_code=0, metadata=metadata)
        entry = dict(gate='cluster-tests', host='linux', owner_command=['make', 'test-cluster'],
                     selectors=self.w.ARCHIVED_SELECTORS + ['--run-ignored=only', '-E', 'binary(cluster_failover)'],
                     run_options=['--no-tests=fail', '--retries=0'], environment={},
                     inputs=dict({'.config/nextest.toml': '5' * 64}, **metadata),
                     source_files=['tests/cluster_failover.rs'], junit_source='target/nextest/ci/junit.xml',
                     archive_origin=origin, archive_extraction=extraction)
        self.expected = dict(context=dict(self.context), entry=entry, tools=self.tools)

    def validate(self, expected, archive='1' * 64):
        self.a.validate_expected(self.manifest, 'cluster-tests', self.context, expected, archive=archive)

    def test_an_owner_that_ran_the_builders_extracted_metadata_is_accepted(self):
        self.validate(self.expected)

    def test_an_owner_bound_to_another_archive_or_metadata_is_refused(self):
        def changed(change):
            expected = copy.deepcopy(self.expected)
            change(expected['entry'])
            return expected
        variants = {
            'no extraction': changed(lambda entry: entry.pop('archive_extraction')),
            'metadata input': changed(lambda entry: entry['inputs'].update({self.w.ARCHIVE_METADATA[0]: '0' * 64})),
            'reopened archive': changed(lambda entry: entry.update(
                selectors=['--archive-file', 'tests.tar.zst'] + entry['selectors'])),
            'other selectors': changed(lambda entry: entry.update(selectors=['--profile', 'ci'] + entry['selectors'][10:])),
            'extraction of another archive': changed(lambda entry: entry['archive_extraction'].update(archive_sha256='9' * 64)),
            'another nextest': changed(lambda entry: entry['archive_extraction'].update(
                command=self.w.extraction_command('/elsewhere/cargo-nextest'))),
        }
        for name, expected in variants.items():
            with self.subTest(change=name), self.assertRaises(c.Invalid):
                self.validate(expected)
        with self.assertRaises(c.Invalid):
            self.validate(self.expected, archive='9' * 64)
        with self.assertRaises(c.Invalid):
            self.validate(self.expected, archive=None)


if __name__ == '__main__':
    unittest.main()
