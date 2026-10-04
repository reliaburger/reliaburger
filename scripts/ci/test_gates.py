"""Synthetic producer/aggregate tests; no Rust qualification is implied."""
import copy
import json
from pathlib import Path
from types import SimpleNamespace
import subprocess
import sys
import unittest
import test_contracts as base
import contracts as c
import gates


class Gates(unittest.TestCase):
    def setUp(self):
        base.Contracts.setUp(self)
        (self.root/'src').mkdir(); (self.root/'src/contract.rs').write_text('// synthetic parser fixture\n')
        self.manifest['contracts'][0]['cases'][0]['sources'] = ['src/contract.rs']
        self.plan = dict(schema_version=1, commit=self.context['commit'], run_id='100', attempt='1', mode='portable', entries=[dict(gate='portable-linux', host='linux', owner_command=['make', 'test'], selectors=['--profile=ci', '--run-ignored=default', '--ignore-default-filter', '-E', 'test(pickle::tests::bounded)'], run_options=['--no-tests=fail', '--no-fail-fast', '--test-threads=2', '--retries=0'], environment={'CARGO_NET_OFFLINE': 'true'}, inputs={}, junit_source='target/nextest/ci/junit.xml')])
        self.expected = dict(commit=self.context['commit'], run_id='100', attempt='1', mode='portable')
        self.plan['entries'][0]['source_files'] = ['src/contract.rs']
        self.output = self.root / 'artifacts/linux/portable-linux'
        self.calls = []
        self.exit_code = 0
        self.discover_exit = 0
        self.make_report = True
        self.dirty = False
        self.untracked = []
        self.ignored = []

    def write(self):
        base.Contracts.write(self)

    def tearDown(self):
        base.Contracts.tearDown(self)

    def fake_run(self, command, **kwargs):
        self.calls.append((command, kwargs))
        if command[0] == 'git':
            if command[1] == 'ls-files':
                if '--error-unmatch' in command:
                    return SimpleNamespace(returncode=0, stdout='src/contract.rs\n')
                return SimpleNamespace(returncode=0, stdout='\n'.join(self.ignored if '--ignored' in command else self.untracked))
            return SimpleNamespace(returncode=int(self.dirty) if command[1] == 'diff' else 0, stdout=self.context['commit'] + '\n')
        if command[2] == 'list':
            kwargs['stdout'].write(json.dumps(self.discovery).encode())
            return SimpleNamespace(returncode=self.discover_exit)
        if self.make_report:
            path = self.root / self.plan['entries'][0]['junit_source']
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(self.report)
        return SimpleNamespace(returncode=self.exit_code)

    def produce(self):
        gates.validate_plan(self.plan, c.inventory(self.manifest, self.root), self.expected)
        return gates.produce(self.plan, self.plan['entries'][0], self.root, self.output, self.fake_run)

    def aggregate(self):
        return gates.aggregate(self.plan, self.manifest, self.root / 'artifacts')

    def test_real_argv_separates_owner_from_run_and_list(self):
        self.assertEqual(self.produce(), 0)
        receipt = c.read_json(self.output / 'receipt.json')
        self.assertEqual(receipt['owner_command'], ['make', 'test'])
        actual = [command for command, _ in self.calls if command[0] == 'cargo']
        self.assertEqual(receipt['command'], actual[1])
        self.assertEqual(receipt['discovery_command'], actual[0])
        self.assertEqual(receipt['selectors'], self.plan['entries'][0]['selectors'])
        self.assertEqual(self.aggregate(), {('portable-linux', 'linux'): {('reliaburger', 'pickle::tests::bounded')}})

    def test_features_archive_remap_ignored_and_exclusions_shared(self):
        extra = ['--features=ebpf', '--archive-file=tests.tar.zst', '--workspace-remap=.', '--target-dir-remap=target', '-E', 'not binary(oci_crash)']
        path = self.root / 'tests.tar.zst'; path.write_text('current archive')
        self.plan['entries'][0]['inputs']['tests.tar.zst'] = c.digest(path)
        builder = dict(schema_version=1, commit=self.plan['commit'], run_id='100', attempt='1', host='linux', command=['cargo', 'nextest', 'archive', '--locked', '--archive-file=tests.tar.zst'], exit_code=0, archive_sha256=c.digest(path))
        origin = self.root/'build-origin.json'; origin.write_text(json.dumps(builder))
        self.plan['entries'][0]['archive_origin'] = dict(path='build-origin.json', sha256=c.digest(origin), builder=builder)
        self.plan['entries'][0]['selectors'] += extra
        self.produce()
        discovery, run = [command for command, _ in self.calls if command[0] == 'cargo']
        common = self.plan['entries'][0]['selectors']
        self.assertEqual(run[3:3+len(common)], discovery[3:3+len(common)])
        self.assertTrue(self.aggregate())

    def test_execution_selector_tampering_is_refused(self):
        self.produce()
        self.plan['entries'][0]['selectors'].append('--features=ebpf')
        with self.assertRaisesRegex(c.Invalid, 'wrong execution'):
            self.aggregate()

    def test_discovery_argv_tampering_is_refused(self):
        self.produce()
        path = self.output / 'receipt.json'
        receipt = c.read_json(path)
        receipt['discovery_command'].append('-E=all()')
        path.write_text(json.dumps(receipt))
        with self.assertRaisesRegex(c.Invalid, 'discovery_command'):
            self.aggregate()

    def test_command_failure_retained_and_refused(self):
        self.exit_code = 100
        self.assertEqual(self.produce(), 100)
        self.assertEqual(c.read_json(self.output / 'receipt.json')['exit_code'], 100)
        with self.assertRaisesRegex(c.Invalid, 'command failed'):
            self.aggregate()

    def test_discovery_failure_cannot_execute_or_reuse_receipt(self):
        self.produce()
        self.discover_exit = 7
        self.calls.clear()
        with self.assertRaisesRegex(c.Invalid, 'discovery command failed'):
            self.produce()
        self.assertEqual(len(self.calls), 6)
        self.assertFalse((self.output / 'receipt.json').exists())
        self.assertFalse((self.output / 'junit.xml').exists())

    def test_missing_new_report_cannot_reuse_previous_success(self):
        self.produce()
        self.make_report = False
        self.assertEqual(self.produce(), 1)
        self.assertFalse((self.output / 'junit.xml').exists())
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_changed_input_refused_before_launch(self):
        path = self.root / 'archive.tar.zst'; path.write_text('current')
        self.plan['entries'][0]['inputs']['archive.tar.zst'] = c.digest(path)
        self.produce(); self.calls.clear(); path.write_text('stale')
        with self.assertRaisesRegex(c.Invalid, 'input changed'):
            self.produce()
        self.assertFalse(self.calls)

    def test_wrong_checkout_cannot_receive_current_head_receipt(self):
        def stale(command, **kwargs):
            return SimpleNamespace(returncode=0, stdout='b' * 40)
        with self.assertRaisesRegex(c.Invalid, 'expected candidate'):
            gates.produce(self.plan, self.plan['entries'][0], self.root, self.output, stale)

    def test_hidden_nextest_environment_does_not_change_scope(self):
        from unittest.mock import patch
        with patch.dict('os.environ', {'NEXTEST_PROFILE': 'other', 'NEXTEST_FILTERSET': 'none()', 'RELIABURGER_FOO': '1'}):
            self.produce()
        env = [kwargs['env'] for command, kwargs in self.calls if command[0] == 'cargo'][1]
        self.assertNotIn('NEXTEST_PROFILE', env)
        self.assertNotIn('RELIABURGER_FOO', env)
        self.assertEqual(env['CARGO_NET_OFFLINE'], 'true')

    def test_unknown_env_or_retry_or_selector_control_refused(self):
        for change in (lambda e: e['environment'].update(NEXTEST_PROFILE='other'), lambda e: e['run_options'].append('--retries=1'), lambda e: e['selectors'].append('--message-format=json')):
            saved = copy.deepcopy(self.plan)
            change(self.plan['entries'][0])
            with self.assertRaises(c.Invalid):
                self.produce()
            self.plan = saved

    def test_archive_or_config_without_expected_hash_refused(self):
        for option in ('--archive-file=other.tar.zst', '--config-file=other.toml'):
            saved = copy.deepcopy(self.plan)
            self.plan['entries'][0]['selectors'].append(option)
            with self.assertRaisesRegex(c.Invalid, 'input hash'):
                self.produce()
            self.plan = saved

    def test_dirty_tracked_source_does_not_qualify_current_head(self):
        self.dirty = True
        with self.assertRaisesRegex(c.Invalid, 'tracked source'):
            self.produce()
        self.assertFalse((self.output / 'receipt.json').exists())

    def test_untracked_or_ignored_source_and_helper_is_refused(self):
        for source, ignored in (('src/new.rs', False), ('tests/fixture.bin', False), ('scripts/helper', False), ('scripts/ignored-helper', True), ('build.rs', True), ('src/generated.rs', True), ('docs/manual/new.md', True), ('examples/new.toml', True), ('brioche/dist/new.js', False), ('ebpf/new.h', True), ('.cargo/config.toml', True)):
            self.untracked = [] if ignored else [source]
            self.ignored = [source] if ignored else []
            with self.assertRaisesRegex(c.Invalid, 'untracked source/helper'):
                self.produce()
        self.untracked = ['plan.json', 'evidence/junit.xml']; self.ignored = ['target/generated.rs', 'tests/contracts/cron/target/generated.rs']
        self.assertEqual(self.produce(), 0)

    def test_arbitrary_archive_hash_without_origin_is_refused(self):
        path = self.root/'tests.tar.zst'; path.write_text('archive')
        entry = self.plan['entries'][0]
        entry['selectors'].append('--archive-file=tests.tar.zst')
        entry['inputs']['tests.tar.zst'] = c.digest(path)
        with self.assertRaisesRegex(c.Invalid, 'build-origin receipt'):
            self.produce()

    def test_finite_case_source_inventory_must_be_present(self):
        del self.manifest['contracts'][0]['cases'][0]['sources']
        with self.assertRaisesRegex(c.Invalid, 'concrete CI case sources'):
            self.produce()
        self.manifest['contracts'][0]['cases'][0]['sources'] = ['src/missing.rs']
        with self.assertRaisesRegex(c.Invalid, 'case source'):
            self.produce()

    def test_finite_source_symlink_and_untracked_index_are_refused(self):
        source = self.root/'src/contract.rs'; source.unlink()
        source.symlink_to(self.root/'Makefile')
        with self.assertRaisesRegex(c.Invalid, 'regular file'):
            self.produce()
        source.unlink(); source.write_text('// restored synthetic case\n')
        def unknown(command, **kwargs):
            if '--error-unmatch' in command:
                return SimpleNamespace(returncode=1, stdout='')
            return self.fake_run(command, **kwargs)
        with self.assertRaisesRegex(c.Invalid, 'not tracked'):
            gates.produce(self.plan, self.plan['entries'][0], self.root, self.output, unknown)

    def test_archive_origin_stale_builder_failed_or_tampered_refused(self):
        self.test_features_archive_remap_ignored_and_exclusions_shared()
        saved = copy.deepcopy(self.plan)
        for key, value in (('commit', 'b'*40), ('run_id', '99'), ('attempt', '2'), ('exit_code', 1), ('archive_sha256', '0'*64)):
            self.plan = copy.deepcopy(saved)
            self.plan['entries'][0]['archive_origin']['builder'][key] = value
            with self.assertRaises(c.Invalid):
                self.produce()
        self.plan = saved
        (self.root/'build-origin.json').write_text('{}')
        with self.assertRaisesRegex(c.Invalid, 'receipt changed'):
            self.produce()

    def test_archive_builder_keeps_actual_completion_and_refuses_missing_archive(self):
        command = ['cargo', 'nextest', 'archive', '--archive-file=tests.tar.zst']
        context = dict(commit=self.plan['commit'], run_id='100', attempt='1', host='linux')
        def builder_run(argv, **kwargs):
            if argv[0] == 'git':
                return self.fake_run(argv, **kwargs)
            (self.root/'tests.tar.zst').write_text('built')
            return SimpleNamespace(returncode=self.exit_code)
        for status in (0, 17):
            self.exit_code = status
            self.assertEqual(gates.archive_build(command, self.root, 'tests.tar.zst', 'origin.json', context, builder_run), status)
            receipt = c.read_json(self.root/'origin.json')
            self.assertEqual(receipt['command'], command)
            self.assertEqual(receipt['exit_code'], status)
            self.assertEqual(receipt['archive_sha256'], c.digest(self.root/'tests.tar.zst'))
        def missing(argv, **kwargs):
            return self.fake_run(argv, **kwargs) if argv[0] == 'git' else SimpleNamespace(returncode=0)
        self.assertEqual(gates.archive_build(command, self.root, 'tests.tar.zst', 'origin.json', context, missing), 1)
        self.assertIsNone(c.read_json(self.root/'origin.json')['archive_sha256'])

    def test_empty_new_report_does_not_pass_producer(self):
        self.report = '<testsuites/>'
        self.assertEqual(self.produce(), 1)
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_implicit_scope_is_refused(self):
        for flag in ('--profile=ci', '--run-ignored=default', '--ignore-default-filter'):
            saved = copy.deepcopy(self.plan)
            self.plan['entries'][0]['selectors'].remove(flag)
            with self.assertRaises(c.Invalid):
                self.produce()
            self.plan = saved

    def test_missing_portable_mac_artifact_fails_global_aggregate(self):
        self.manifest['gates']['portable-linux']['hosts'].append('darwin')
        entry = copy.deepcopy(self.plan['entries'][0]); entry['host'] = 'darwin'
        self.plan['entries'].append(entry)
        self.produce()
        with self.assertRaises(c.Invalid):
            self.aggregate()

    def test_selected_plan_cannot_omit_or_add_gates(self):
        self.manifest['gates']['portable-linux']['hosts'].append('darwin')
        with self.assertRaisesRegex(c.Invalid, 'missing='):
            self.produce()
        self.manifest['gates']['portable-linux']['hosts'] = ['linux']
        self.plan['entries'].append(copy.deepcopy(self.plan['entries'][0]))
        with self.assertRaisesRegex(c.Invalid, 'duplicate gate'):
            self.produce()

    def test_explicit_portable_plan_excludes_heavy_but_full_requires_it(self):
        gate = copy.deepcopy(self.manifest['gates']['portable-linux'])
        gate['required_in'] = ['full']; self.manifest['gates']['linux-heavy'] = gate
        case = copy.deepcopy(self.manifest['contracts'][0]['cases'][0]); case['id'] = 'heavy-case'; case['requires'] = ['linux-heavy']
        self.manifest['contracts'][0]['cases'].append(case)
        self.produce()
        self.plan['mode'] = self.expected['mode'] = 'full'
        with self.assertRaisesRegex(c.Invalid, 'missing='):
            self.produce()

    def test_stale_expected_current_mode_head_run_attempt_refused(self):
        for key, value in (('commit', 'b'*40), ('run_id', '101'), ('attempt', '2'), ('mode', 'full')):
            saved = copy.deepcopy(self.plan)
            self.plan[key] = value
            with self.assertRaisesRegex(c.Invalid, 'stale/wrong plan'):
                self.produce()
            self.plan = saved

    def test_manual_gate_never_satisfies_ci_selection(self):
        self.manifest['gates']['portable-linux'].update(mode='manual', owner='maintainer')
        with self.assertRaisesRegex(c.Invalid, 'manual gate'):
            self.produce()

    def test_cli_aggregate_executes_parser_with_external_current_context(self):
        self.produce()
        (self.root / 'manifest.json').write_text(json.dumps(self.manifest))
        (self.root / 'plan.json').write_text(json.dumps(self.plan))
        args = [sys.executable, str(Path(gates.__file__)), 'aggregate', '--manifest', str(self.root/'manifest.json'), '--plan', str(self.root/'plan.json'), '--root', str(self.root), '--artifacts', str(self.root/'artifacts'), '--commit', self.context['commit'], '--run-id', '100', '--attempt', '1', '--mode', 'portable']
        self.assertEqual(subprocess.run(args, capture_output=True).returncode, 0)
        args[-1] = 'full'
        self.assertNotEqual(subprocess.run(args, capture_output=True).returncode, 0)

    def test_cli_producer_runs_real_child_process_and_keeps_actual_exit(self):
        # The child is an ordinary synthetic nextest fixture, not Rust evidence.
        subprocess.run(['git', 'init', '-q', str(self.root)], check=True)
        subprocess.run(['git', 'add', 'Makefile', 'src/contract.rs'], cwd=self.root, check=True)
        subprocess.run(['git', '-c', 'user.name=Contract Fixture', '-c', 'user.email=fixture@example.invalid', '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture'], cwd=self.root, check=True)
        self.plan['commit'] = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=self.root, text=True).strip()
        binary = self.root / 'bin'; binary.mkdir()
        cargo = binary / 'cargo'
        cargo.write_text('#!' + sys.executable + '\n' + '''import json, os, pathlib, sys
path = pathlib.Path(os.environ['RELIABURGER_FIXTURE_TRACE'])
with path.open('a') as trace: trace.write(json.dumps(sys.argv[1:]) + '\\n')
if sys.argv[2] == 'list':
    print(os.environ['RELIABURGER_FIXTURE_DISCOVERY'])
else:
    report = pathlib.Path('target/nextest/ci/junit.xml')
    report.parent.mkdir(parents=True, exist_ok=True)
    report.write_text(os.environ['RELIABURGER_FIXTURE_JUNIT'])
    sys.exit(int(os.environ['RELIABURGER_FIXTURE_EXIT']))
''')
        cargo.chmod(0o700)
        entry = self.plan['entries'][0]
        entry['environment'].update(RELIABURGER_FIXTURE_TRACE=str(self.root/'trace.jsonl'), RELIABURGER_FIXTURE_DISCOVERY=json.dumps(self.discovery), RELIABURGER_FIXTURE_JUNIT=self.report)
        (self.root/'manifest.json').write_text(json.dumps(self.manifest))
        args = [sys.executable, str(Path(gates.__file__)), 'produce', '--manifest', str(self.root/'manifest.json'), '--plan', str(self.root/'plan.json'), '--root', str(self.root), '--artifacts', str(self.root/'artifacts'), '--commit', self.plan['commit'], '--run-id', '100', '--attempt', '1', '--mode', 'portable', '--gate', 'portable-linux', '--host', 'linux']
        import os
        env = os.environ.copy(); env['PATH'] = str(binary) + os.pathsep + env['PATH']
        for status in (0, 19):
            entry['environment']['RELIABURGER_FIXTURE_EXIT'] = str(status)
            (self.root/'plan.json').write_text(json.dumps(self.plan))
            result = subprocess.run(args, env=env, capture_output=True)
            self.assertEqual(result.returncode, status, result.stderr.decode())
            receipt = c.read_json(self.output/'receipt.json')
            self.assertEqual(receipt['exit_code'], status)
            self.assertEqual(receipt['commit'], self.plan['commit'])
            if status == 0:
                self.assertTrue(gates.aggregate(self.plan, self.manifest, self.root/'artifacts'))
            else:
                with self.assertRaisesRegex(c.Invalid, 'command failed'):
                    gates.aggregate(self.plan, self.manifest, self.root/'artifacts')
        calls = [json.loads(line) for line in (self.root/'trace.jsonl').read_text().splitlines()]
        self.assertEqual([call[1] for call in calls], ['list', 'run', 'list', 'run'])


    def test_actual_separated_profile_and_implicit_default_policy_are_preserved(self):
        entry=self.plan['entries'][0]
        entry['selectors']=['--profile','ci','-E','test(pickle::tests::bounded)']
        entry['selector_context']=dict(profile='ci',ignored='default',default_filter='honor')
        self.assertEqual(self.produce(),0);self.assertTrue(self.aggregate())
        actual=[command for command,_ in self.calls if command[0]=='cargo']
        self.assertIn('ci',actual[0]);self.assertNotIn('--ignore-default-filter',actual[1])
    def test_declared_default_filter_policy_cannot_lie(self):
        entry=self.plan['entries'][0];entry['selector_context']=dict(profile='ci',ignored='default',default_filter='honor')
        with self.assertRaisesRegex(c.Invalid,'default-filter policy'):self.produce()
    def test_separated_hashed_config_path_is_preserved(self):
        entry=self.plan['entries'][0];path=self.root/'actual-nextest.toml';path.write_text('[profile.ci]\nretries=0\n')
        entry['inputs']['actual-nextest.toml']=c.digest(path);entry['selectors']+=['--config-file','actual-nextest.toml']
        self.assertEqual(self.produce(),0);self.assertTrue(self.aggregate())
        actual=[command for command,_ in self.calls if command[0]=='cargo'];self.assertIn('--config-file',actual[0]);self.assertIn('--config-file',actual[1])

if __name__ == '__main__':
    unittest.main()
