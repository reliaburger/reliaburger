"""Real Python/shell entry controls; no Rust, coverage or namespaces are run."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import contracts as c
import oci_driver


class FrontdoorEnvironment(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name) / 'checkout'
        self.root.mkdir()
        scriptdir = self.root / 'scripts/ci'
        scriptdir.mkdir(parents=True)
        source = Path(__file__).parent
        for name in ('contracts', 'gates', 'completion', 'coverage_contract',
                     'coverage_adapter', 'coverage_owner', 'owner_tools', 'make_owner',
                     'oci_driver', 'workflow_assembly', 'workflow_adapter'):
            shutil.copyfile(source / (name + '.py'), scriptdir / (name + '.py'))
        release = self.root / 'scripts/release'
        release.mkdir()
        shutil.copyfile(source.parent / 'release/qualify-oci-interruptions.sh',
                        release / 'qualify-oci-interruptions.sh')
        (self.root / 'src').mkdir()
        (self.root / 'src/case.rs').write_text('// source identity fixture\n')
        (self.root / 'Makefile').write_text('test:\n\ttrue\n')
        (self.root / '.gitignore').write_text('__pycache__/\ntarget/\n')
        manifest = dict(schema_version=1, gates={'oci-interruptions': dict(
            mode='ci', hosts=['linux'], command=['scripts/release/qualify-oci-interruptions.sh'])},
            contracts=[dict(id='entry', issue=555, promise='entry completion',
                            supported_paths=['success'], refused_paths=['failure'], boundaries=['source'],
                            cases=[dict(id='entry-case', binary='reliaburger::owned_runc', test='case',
                                        requires=['oci-interruptions'], sources=['src/case.rs'])])])
        (self.root / 'manifest.json').write_text(json.dumps(manifest))
        for command in (['git', 'init', '-q'], ['git', 'add', '.'],
                        ['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                         'commit', '-qm', 'source fixture']):
            subprocess.run(command, cwd=self.root, check=True, capture_output=True)
        self.commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=self.root, text=True).strip()
        self.environment = dict(os.environ)
        for name in ('PYTHONDONTWRITEBYTECODE', 'PYTHONPYCACHEPREFIX', 'GITHUB_ACTIONS',
                     'EXPECTED_CHECKOUT_COMMIT', 'GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT', 'GITHUB_OUTPUT'):
            self.environment.pop(name, None)
        # Direct Python calls use the same controlled environment as child entries.
        # CI-refusal controls add their explicit GitHub state inside each test.
        self.environment_patch = patch.dict(os.environ, self.environment, clear=True)
        self.environment_patch.start()
        self.addCleanup(self.environment_patch.stop)

    def tearDown(self):
        self.temporary.cleanup()

    def test_normal_python_entry_preserves_clean_sources_and_child_cache_policy(self):
        # Normal CPython writes beside imports. Apple CLT's global cache prefix
        # is disabled only in this subprocess, without altering packaged source.
        launcher = '''import os,runpy,sys
sys.pycache_prefix=None
sys.path.insert(0, 'scripts/ci')
entry=runpy.run_path('scripts/ci/workflow_adapter.py', run_name='controlled_entry')
def stop_before_tools():
    assert os.environ.get('PYTHONDONTWRITEBYTECODE') == '1'
    raise RuntimeError('REACHED_TRUSTED_TOOL_ENTRY')
entry['oci_driver'].resolve_tools=stop_before_tools
entry['main'](sys.argv[1:])
'''
        command = [sys.executable, '-c', launcher, 'produce', '--root', '.',
                   '--manifest', 'manifest.json', '--gate', 'oci-interruptions', '--host', 'linux',
                   '--directory', 'target/contracts/linux/oci-interruptions', '--commit', self.commit,
                   '--run-id', '123', '--attempt', '1', '--github-output', 'target/output.txt']
        result = subprocess.run(command, cwd=self.root, env=self.environment, capture_output=True, text=True)
        self.assertIn('REACHED_TRUSTED_TOOL_ENTRY', result.stderr)
        self.assertEqual(list((self.root / 'scripts').rglob('*.pyc')), [])

    def test_default_disposable_host_script_selects_manual_without_ci_inputs(self):
        result, argv = self.shell_entry([])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(argv[:2], ['scripts/ci/oci_driver.py', 'manual'])
        self.assertFalse((self.root / 'github-output').exists())

    def test_manual_script_preserves_explicit_session_and_owner(self):
        result, argv = self.shell_entry(['--manual-session', 'manual:review', '--owner', 'operator'])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(argv[-4:], ['--manual-session', 'manual:review', '--owner', 'operator'])

    def test_github_script_requires_current_ci_inputs_before_any_dispatch(self):
        result, argv = self.shell_entry([], {'GITHUB_ACTIONS': 'true'})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('current workflow checkout', result.stderr)
        self.assertIsNone(argv)

    def shell_entry(self, arguments, extra=None):
        tools = Path(self.temporary.name) / 'tools'
        tools.mkdir(exist_ok=True)
        record = Path(self.temporary.name) / 'argv.json'
        stub = tools / 'python3'
        stub.write_text('#!' + sys.executable + '\nimport json,sys\n'
                        + 'open(' + repr(str(record)) + ', "w").write(json.dumps(sys.argv[1:]))\n')
        stub.chmod(0o755)
        environment = dict(self.environment, PATH=str(tools) + os.pathsep + self.environment['PATH'])
        environment.update(extra or {})
        result = subprocess.run(['bash', 'scripts/release/qualify-oci-interruptions.sh'] + arguments,
                                cwd=self.root, env=environment, capture_output=True, text=True)
        return result, json.loads(record.read_text()) if record.exists() else None

    def test_manual_cli_observes_checkout_and_operator_without_ci_authority(self):
        seen = []
        with patch.object(oci_driver, 'resolve_tools', return_value={}), \
                patch.object(oci_driver, 'execute', side_effect=lambda *args: seen.append(args) or 0), \
                patch.object(oci_driver.getpass, 'getuser', return_value='observed-operator'):
            self.assertEqual(oci_driver.main(['manual', '--root', str(self.root)]), 0)
        context = seen[0][2]
        self.assertEqual(context['commit'], self.commit)
        self.assertEqual(context['authority'], 'manual')
        self.assertEqual(context['owner'], 'observed-operator')
        self.assertTrue(context['run_id'].startswith('manual:'))
        self.assertEqual(context['attempt'], '1')
        self.assertTrue(Path(seen[0][1]).is_relative_to(self.root.resolve() / 'target'))

    def test_manual_cli_requires_an_owner_if_observation_is_unavailable(self):
        with patch.object(oci_driver.getpass, 'getuser', side_effect=OSError('no operator')):
            with self.assertRaisesRegex(c.Invalid, '--owner'):
                oci_driver.main(['manual', '--root', str(self.root)])

    def test_ci_script_dispatches_only_trusted_current_workflow_inputs(self):
        result, argv = self.shell_entry([], dict(GITHUB_ACTIONS='true',
            EXPECTED_CHECKOUT_COMMIT=self.commit, GITHUB_RUN_ID='456',
            GITHUB_RUN_ATTEMPT='2', GITHUB_OUTPUT=str(self.root / 'github-output')))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(argv[:2], ['scripts/ci/workflow_adapter.py', 'produce'])
        self.assertIn(self.commit, argv)
        self.assertIn('456', argv)
        self.assertNotIn('manual', argv)

    def test_ci_script_rejects_manual_options_before_dispatch(self):
        result, argv = self.shell_entry(['--owner', 'operator'], {'GITHUB_ACTIONS': 'true'})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('manual options are not allowed', result.stderr)
        self.assertIsNone(argv)

    def test_manual_cli_preserves_explicit_identity_and_rejects_github_mode(self):
        seen = []
        with patch.object(oci_driver, 'resolve_tools', return_value={}), \
                patch.object(oci_driver, 'execute', side_effect=lambda *args: seen.append(args) or 0):
            self.assertEqual(oci_driver.main(['manual', '--root', str(self.root),
                '--manual-session', 'manual:explicit', '--owner', 'supplied-operator']), 0)
        self.assertEqual(seen[0][2]['run_id'], 'manual:explicit')
        self.assertEqual(seen[0][2]['owner'], 'supplied-operator')
        with patch.dict(os.environ, {'GITHUB_ACTIONS': 'true'}):
            with self.assertRaisesRegex(c.Invalid, 'current CI owner entry'):
                oci_driver.main(['manual', '--root', str(self.root)])

    def test_manual_cli_refuses_empty_operator_or_nonmanual_session(self):
        with patch.object(oci_driver.getpass, 'getuser', return_value=''):
            with self.assertRaisesRegex(c.Invalid, '--owner'):
                oci_driver.main(['manual', '--root', str(self.root)])
        with self.assertRaisesRegex(c.Invalid, 'explicit session identity'):
            oci_driver.main(['manual', '--root', str(self.root),
                             '--manual-session', '123', '--owner', 'operator'])

    def test_disabled_preimport_policy_is_refused_by_unchanged_source_guard(self):
        entry = self.root / 'scripts/ci/workflow_adapter.py'
        original = entry.read_text()
        entry.write_text(original.replace("sys.dont_write_bytecode = True\n", '')
                         .replace("os.environ['PYTHONDONTWRITEBYTECODE'] = '1'\n", ''))
        for command in (['git', 'add', '.'], ['git', '-c', 'user.name=Fixture',
                        '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'negative control']):
            subprocess.run(command, cwd=self.root, check=True, capture_output=True)
        commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=self.root, text=True).strip()
        launcher = "import runpy,sys; sys.pycache_prefix=None; sys.path.insert(0,'scripts/ci'); sys.argv=sys.argv[1:]; runpy.run_path(sys.argv[0],run_name='__main__')"
        command = [sys.executable, '-c', launcher, 'scripts/ci/workflow_adapter.py', 'produce',
                   '--root', '.', '--manifest', 'manifest.json', '--gate', 'oci-interruptions',
                   '--host', 'linux', '--directory', 'target/contracts/linux/oci-interruptions',
                   '--commit', commit, '--run-id', '123', '--attempt', '1',
                   '--github-output', 'target/output.txt']
        result = subprocess.run(command, cwd=self.root, env=self.environment,
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('untracked source/helper cannot qualify candidate: scripts/ci/__pycache__/', result.stderr)
        self.assertTrue(list((self.root / 'scripts').rglob('*.pyc')))
