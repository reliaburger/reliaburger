"""Archive preparation controls use synthetic payloads, never Rust or a VM."""
import copy
import importlib
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest

import contracts as c


class ArchiveReuse(unittest.TestCase):
    def setUp(self):
        self.a = importlib.import_module('archive_reuse')
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.context = dict(commit='a' * 40, run_id='123', attempt='2', host='linux')
        (self.root / 'tests.tar.zst').write_bytes(b'synthetic archive')
        builder = dict(schema_version=1, **self.context,
                       command=['cargo', 'nextest', 'archive', '--locked', '--archive-file', 'tests.tar.zst'],
                       exit_code=0, archive_sha256=c.digest(self.root / 'tests.tar.zst'))
        builder['attempt'] = '1'  # A passed builder survives a failed-job rerun.
        (self.root / 'archive-origin.json').write_text(json.dumps(builder))
        self.origin = dict(path='archive-origin.json', sha256=c.digest(self.root / 'archive-origin.json'),
                           builder=builder)
        self.nextest = dict(path='/synthetic/cargo-nextest', sha256='b' * 64)
        self.calls = []
        self.status = 0
        self.omit = None

    def fake_run(self, command, **kwargs):
        self.calls.append(command)
        for name in ('target/nextest/cargo-metadata.json', 'target/nextest/binaries-metadata.json',
                     'target/debug/deps/test-binary', 'target/debug/bun',
                     'target/debug/build/example/out/fixture', 'target/debug/deps/libexample.so'):
            if name != self.omit:
                path = self.root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('synthetic ' + name)
        return SimpleNamespace(returncode=self.status)

    def prepare(self):
        return self.a.prepare(self.root, self.context, self.origin, self.nextest, run=self.fake_run)

    def load(self, digest=None, context=None):
        return self.a.load(self.root, digest or c.digest(self.root / self.a.RECEIPT),
                           context or self.context, self.origin, self.nextest)

    def test_extracts_once_without_running_tests_and_binds_all_payloads(self):
        row = self.prepare()
        self.assertEqual(len(self.calls), 1)
        self.assertEqual(self.calls[0][:3], ['/synthetic/cargo-nextest', 'nextest', 'list'])
        self.assertIn('binaries-only', self.calls[0])
        self.assertIn('--extract-overwrite', self.calls[0])
        self.assertEqual(len(row['files']), 6)
        self.assertIn('target/debug/build/example/out/fixture', row['files'])
        self.assertEqual(row['archive_origin']['builder']['attempt'], '1')
        self.assertEqual(row['context']['attempt'], '2')
        self.assertEqual(self.load(), row)
        self.assertEqual(self.load(), row)
        self.assertEqual(len(self.calls), 1)

    def test_rejects_changed_archive_before_extraction(self):
        (self.root / 'tests.tar.zst').write_bytes(b'changed')
        with self.assertRaises(c.Invalid):
            self.prepare()
        self.assertEqual(self.calls, [])

    def test_refuses_preexisting_payload_instead_of_reusing_stale_binaries(self):
        path = self.root / 'target/debug/bun'
        path.parent.mkdir(parents=True)
        path.write_text('stale')
        with self.assertRaises(c.Invalid):
            self.prepare()
        self.assertEqual(self.calls, [])

    def test_failed_extraction_does_not_publish_a_receipt(self):
        self.status = 17
        with self.assertRaises(c.Invalid):
            self.prepare()
        self.assertFalse((self.root / self.a.RECEIPT).exists())

    def test_missing_metadata_does_not_publish_a_receipt(self):
        self.omit = 'target/nextest/binaries-metadata.json'
        with self.assertRaises(c.Invalid):
            self.prepare()
        self.assertFalse((self.root / self.a.RECEIPT).exists())

    def test_changed_or_missing_payload_is_refused(self):
        row = self.prepare()
        for name in row['files']:
            with self.subTest(name=name):
                path = self.root / name
                original = path.read_bytes()
                path.write_bytes(b'changed')
                with self.assertRaises(c.Invalid):
                    self.load()
                path.unlink()
                with self.assertRaises((c.Invalid, OSError)):
                    self.load()
                path.write_bytes(original)

    def test_receipt_must_match_trusted_step_output_and_consumer_context(self):
        self.prepare()
        with self.assertRaises(c.Invalid):
            self.load('0' * 64)
        for field, value in [('commit', 'c' * 40), ('run_id', '124'), ('attempt', '3'), ('host', 'darwin')]:
            with self.subTest(field=field), self.assertRaises(c.Invalid):
                self.load(context=dict(self.context, **{field: value}))

    def test_extracted_payload_symlink_escape_is_refused(self):
        self.prepare()
        path = self.root / 'target/debug/bun'
        outside = self.root / 'outside'
        outside.write_bytes(path.read_bytes())
        path.unlink()
        path.symlink_to(outside)
        with self.assertRaises(c.Invalid):
            self.load()

    def test_generated_reports_are_not_immutable_archive_inputs(self):
        row = self.prepare()
        generated = self.root / 'target/nextest/ci/junit.xml'
        generated.parent.mkdir(parents=True)
        generated.write_text('new result')
        self.assertEqual(self.load(), row)

    def test_aggregate_can_validate_the_receipt_without_runner_local_binaries(self):
        row = self.prepare()
        self.a.validate(row, self.context, self.origin, self.nextest)
        changed = copy.deepcopy(row)
        changed['files']['target/nextest/ci/junit.xml'] = 'c' * 64
        with self.assertRaises(c.Invalid):
            self.a.validate(changed, self.context, self.origin, self.nextest)

    def test_consumers_use_metadata_and_never_reextract(self):
        selectors = self.a.SELECTORS
        self.assertNotIn('--archive-file', selectors)
        self.assertNotIn('--extract-overwrite', selectors)
        for name in ('--cargo-metadata', '--binaries-metadata', '--target-dir-remap', '--workspace-remap'):
            self.assertIn(name, selectors)
