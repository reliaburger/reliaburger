"""Exercise real nextest archive reuse with a script test binary, without compiling.

The synthetic libtest protocol checks nextest's metadata/remapping and JUnit
compatibility. It does not qualify Rust tests or Reliaburger runtime behaviour.
"""
from pathlib import Path
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
import xml.etree.ElementTree as ET

import archive_reuse as reuse
import contracts as c


TEST_BINARY = '''#!{python}
import os
import sys

if "--list" in sys.argv:
    if "--ignored" not in sys.argv:
        print("passes: test")
        print("fails_on_request: test")
    sys.exit(0)
if "fails_on_request" in sys.argv and os.environ.get("ARCHIVE_SMOKE_FAIL") == "1":
    print("intentional synthetic failure")
    sys.exit(42)
print("synthetic test passed")
'''


class ArchiveReuseNextest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.nextest = shutil.which('cargo-nextest')
        cls.cargo = shutil.which('cargo')
        cls.zstd = shutil.which('zstd')
        if not all((cls.nextest, cls.cargo, cls.zstd)):
            raise unittest.SkipTest('needs cargo metadata, cargo-nextest and zstd; no compilation')
        version = subprocess.run([cls.nextest, '--version'], capture_output=True, text=True)
        if version.returncode or not re.match(r'cargo-nextest 0\.9\.145(?:\s|$)', version.stdout):
            raise unittest.SkipTest('needs the pinned cargo-nextest 0.9.145')
        cls.host = re.search(r'^host: (.+)$', version.stdout, re.M).group(1)

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.build = self.root / 'build'
        self.direct = self.root / 'direct'
        self.reused = self.root / 'reused'
        self.environment = {key: value for key, value in os.environ.items()
                            if not key.startswith(('NEXTEST_', 'CARGO_', 'RUSTUP_', 'RUSTC_', 'RUSTFLAGS'))}
        self.environment['CARGO_NET_OFFLINE'] = 'true'
        for root in (self.build, self.direct, self.reused):
            (root / 'src').mkdir(parents=True)
            (root / 'src/lib.rs').write_text('// Synthetic metadata only; never compiled.\n')
            (root / 'Cargo.toml').write_text(
                '[package]\nname = "archive_smoke"\nversion = "0.1.0"\nedition = "2021"\n'
                '\n[workspace]\n')
            (root / '.config').mkdir()
            (root / '.config/nextest.toml').write_text(
                '[profile.ci]\nretries = 0\n[profile.ci.junit]\npath = "junit.xml"\n')
        # Cargo's metadata command resolves this dependency-free fixture; it
        # never invokes cargo build/test or compiles the dummy source file.
        metadata = self.checked([self.cargo, 'metadata', '--offline', '--format-version=1'], self.build)
        cargo_metadata = json.loads(metadata.stdout)
        package_id = cargo_metadata['packages'][0]['id']
        binary = self.build / 'target/debug/deps/archive_smoke'
        binary.parent.mkdir(parents=True)
        binary.write_text(TEST_BINARY.format(python=sys.executable))
        binary.chmod(0o755)
        platform = {'triple': self.host, 'target-features': []}
        binary_metadata = {
            'rust-build-meta': {
                'target-directory': str(self.build / 'target'),
                'base-output-directories': ['debug'],
                'non-test-binaries': {}, 'build-script-out-dirs': {}, 'linked-paths': [],
                'platforms': {'host': {'platform': platform,
                                      'libdir': {'status': 'available', 'path': str(self.build / 'target')}},
                              'targets': []},
                'target-platforms': [platform], 'target-platform': None,
            },
            'rust-binaries': {'archive_smoke': {
                'binary-id': 'archive_smoke', 'binary-name': 'archive_smoke',
                'package-id': package_id, 'kind': 'lib',
                'binary-path': str(binary), 'build-platform': 'target',
            }},
        }
        metadata_root = self.build / 'target/nextest'
        metadata_root.mkdir()
        (metadata_root / 'cargo-metadata.json').write_text(json.dumps(cargo_metadata))
        (metadata_root / 'binaries-metadata.json').write_text(json.dumps(binary_metadata))
        archive = self.root / 'tests.tar'
        with tarfile.open(archive, 'w') as output:
            for path in (binary, *(metadata_root / name for name in
                                 ('cargo-metadata.json', 'binaries-metadata.json'))):
                output.add(path, arcname=path.relative_to(self.build).as_posix())
        self.checked([self.zstd, '-q', '-f', str(archive), '-o', str(archive) + '.zst'], self.root)
        for root in (self.direct, self.reused):
            shutil.copyfile(str(archive) + '.zst', root / 'tests.tar.zst')
        # The original paths must no longer work: a missing remap cannot pass
        # by accidentally executing the script left in the builder workspace.
        shutil.rmtree(self.build)
        self.context = dict(commit='a' * 40, run_id='123', attempt='2', host='linux')
        builder = dict(schema_version=1, commit=self.context['commit'], run_id='123',
                       attempt='1', host='linux', command=['cargo', 'nextest', 'archive',
                                                         '--locked', '--archive-file', 'tests.tar.zst'],
                       exit_code=0, archive_sha256=c.digest(self.reused / 'tests.tar.zst'))
        (self.reused / 'archive-origin.json').write_text(json.dumps(builder))
        self.origin = dict(path='archive-origin.json', sha256=c.digest(self.reused / 'archive-origin.json'),
                           builder=builder)
        self.tool = dict(path=self.nextest, sha256=c.digest(self.nextest))
        self.direct_selectors = ['--archive-file', 'tests.tar.zst', '--extract-to', '.',
                                 '--extract-overwrite', '--workspace-remap', '.', '--profile', 'ci']

    def checked(self, command, root, *, environment=None):
        result = subprocess.run(command, cwd=root,
                                env=self.environment if environment is None else environment,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def listed(self, root, selectors):
        result = self.checked([self.nextest, 'nextest', 'list', *selectors,
                               '--message-format=json'], root)
        return c.selected(json.loads(result.stdout))

    def run_fixture(self, root, selectors, *, fail=False):
        report = root / 'target/nextest/ci/junit.xml'
        report.unlink(missing_ok=True)
        environment = dict(self.environment, ARCHIVE_SMOKE_FAIL='1' if fail else '0')
        result = subprocess.run([self.nextest, 'nextest', 'run', *selectors,
                                 '--no-tests=fail', '--retries=0'], cwd=root, env=environment,
                                capture_output=True, text=True)
        self.assertTrue(report.is_file(), result.stderr)
        return result, report

    def test_extract_once_matches_archive_discovery_execution_and_failure(self):
        expected = {('archive_smoke', 'passes'), ('archive_smoke', 'fails_on_request')}
        self.assertEqual(self.listed(self.direct, self.direct_selectors), expected)
        direct, direct_report = self.run_fixture(self.direct, self.direct_selectors)
        self.assertEqual(direct.returncode, 0, direct.stderr)
        self.assertEqual(c.passed_junit(direct_report), expected)

        # The workflow writes trusted builder outputs before preparation.
        (self.reused / 'target').mkdir()
        (self.reused / 'target/workflow-needs.json').write_text('{}')
        prepared = reuse.prepare(self.reused, self.context, self.origin, self.tool,
                                 environment=self.environment)
        self.assertIn('target/debug/deps/archive_smoke', prepared['files'])
        self.assertNotIn('--archive-file', reuse.SELECTORS)
        self.assertEqual(self.listed(self.reused, reuse.SELECTORS), expected)
        reuse.verify_files(self.reused, prepared)
        reused, reused_report = self.run_fixture(self.reused, reuse.SELECTORS)
        self.assertEqual(reused.returncode, 0, reused.stderr)
        self.assertEqual(c.passed_junit(reused_report), expected)
        reuse.verify_files(self.reused, prepared)

        failed_direct, direct_failed_report = self.run_fixture(self.direct, self.direct_selectors, fail=True)
        failed_reused, failed_report = self.run_fixture(self.reused, reuse.SELECTORS, fail=True)
        self.assertNotEqual(failed_direct.returncode, 0, failed_direct.stderr)
        self.assertEqual(failed_reused.returncode, failed_direct.returncode, failed_reused.stderr)
        for report in (direct_failed_report, failed_report):
            failures = {case.attrib['name'] for case in ET.parse(report).iter('testcase')
                        if case.find('failure') is not None or case.find('error') is not None}
            self.assertEqual(failures, {'fails_on_request'})
        with self.assertRaises(c.Invalid):
            c.passed_junit(failed_report)
        reuse.verify_files(self.reused, prepared)


if __name__ == '__main__':
    unittest.main()
