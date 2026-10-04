"""Python process/IPC acceptance fixtures; no Cargo or instrumented Rust proof."""
import copy
import importlib
import json
import os
import shutil
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
import contracts as c
import coverage_adapter as a
import coverage_contract as k

# Independent expected front door copied from the existing Makefile.
EXPECTED_RECIPE = """COVERAGE_MIN_LINES ?= 78.65
COVERAGE_REPORT = $(CARGO) llvm-cov report --failure-mode all
coverage: ## Run the portable suite once under line coverage and enforce the floor
	$(CARGO) llvm-cov clean --workspace
	$(CARGO) llvm-cov --no-report nextest --profile $(NEXTEST_PROFILE) --no-tests=fail
	mkdir -p target/coverage
	$(COVERAGE_REPORT) --lcov --output-path target/coverage/lcov.info
	$(COVERAGE_REPORT) --html --output-dir target/coverage/html
	$(COVERAGE_REPORT) --fail-under-lines $(COVERAGE_MIN_LINES)
"""

class Owner(unittest.TestCase):

    def setUp(self):
        self.o = importlib.import_module('coverage_owner')
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name).resolve() / 'repo'
        self.root.mkdir()
        self.directory = Path(self.tmp.name) / 'receipt'
        self.root.joinpath('Makefile').write_text(EXPECTED_RECIPE)
        config = self.root / '.config'
        config.mkdir()
        (config / 'nextest.toml').write_text('[store]\ndir = "target/nextest"\n[profile.ci]\nretries = 0\njunit = { path = "junit.xml" }\n')
        self.commands = {}
        self.calls = []
        self.mode = 'healthy'
        self.child_count = 0
        versions = {
            'cargo': 'cargo 1.98.0 (synthetic)\ncommit-hash: ' + 'a' * 40 + '\nhost: x86_64-unknown-linux-gnu\nrelease: 1.98.0',
            'rustc': 'rustc 1.98.0 (synthetic)\ncommit-hash: ' + 'a' * 40 + '\nhost: x86_64-unknown-linux-gnu\nrelease: 1.98.0\nLLVM version: 22',
            'nextest': 'cargo-nextest 0.9.145',
            'coverage': 'cargo-llvm-cov 0.9.1',
        }
        self.versions = versions
        for name in versions:
            p = self.root / name
            p.write_text('synthetic ' + name)
            self.commands[name] = [str(p)] + (['llvm-cov', '--version'] if name == 'coverage' else ['--version'] + (['--verbose'] if name == 'rustc' else []))
        self.sysroot = self.root / 'sysroot'
        binpath = self.sysroot / 'lib/rustlib/x86_64-unknown-linux-gnu/bin'
        binpath.mkdir(parents=True)
        for name in ['llvm-cov', 'llvm-profdata']:
            (binpath / name).write_text('synthetic ' + name)
        self.environment = {'PATH': '/usr/bin:/bin', 'CARGO_HOME': str(self.root / 'cargo-home')}
        self.context = dict(commit='a' * 40, run_id='123', attempt='2', host='linux')

    def tearDown(self):
        self.tmp.cleanup()

    def fake_run(self, cmd, **kw):
        self.calls.append(list(cmd))
        if cmd[0] == 'git':
            out = self.context['commit'] + '\n' if cmd[1] == 'rev-parse' else 'src/meat/cron.rs\n' if '--error-unmatch' in cmd else ''
            return SimpleNamespace(returncode=0, stdout=out)
        name = Path(cmd[0]).name
        if name == 'coverage' and '--version' in cmd and cmd[1:] != ['llvm-cov', '--version']:
            return SimpleNamespace(returncode=1, stdout='', stderr="expected subcommand 'llvm-cov'")
        if '--version' in cmd:
            return SimpleNamespace(
                returncode=0,
                stdout=self.versions.get(name, 'LLVM version 22.1.0') + '\n',
            )
        if '--print' in cmd:
            return SimpleNamespace(returncode=0, stdout=str(self.sysroot) + '\n')
        if 'metadata' in cmd:
            data = dict(
                version=2 if self.mode == 'metadata-version' else 1,
                workspace_root=str(self.root),
                target_directory=str(self.root / 'target'),
                workspace_members=['reliaburger-id'],
                packages=[dict(id='reliaburger-id', name='reliaburger', targets=[dict(name='reliaburger'), dict(name='suite')])],
            )
            return SimpleNamespace(returncode=0, stdout=json.dumps(data))
        if cmd[0] == 'make':
            configuration = c.read_json(kw['env']['RELIABURGER_COVERAGE_OWNER_CONFIG'])
            args = [['llvm-cov'] + x for x in self.o.recipe_stages('ci', '78.65')]
            for stage in args:
                try:
                    with patch.dict(os.environ, kw['env'], clear=True):
                        status = self.o.stage(stage, configuration, self.fake_run)
                except c.Invalid:
                    status = 1
                if status:
                    return SimpleNamespace(returncode=status)
            return SimpleNamespace(returncode=0)
        if name == 'coverage':
            if '--no-report' in cmd:
                if self.mode == 'disabled':
                    return SimpleNamespace(returncode=0)
                env = dict(kw['env'])
                target = str(self.root / 'target/llvm-cov-target')
                env.update({'RUSTC_WRAPPER': str(self.root / 'coverage'), 'CARGO_LLVM_COV': '1', '__CARGO_LLVM_COV_RUSTC_WRAPPER': '1', '__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS': '-C\x1finstrument-coverage\x1f--cfg=coverage', '__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES': 'reliaburger,reliaburger_tests,reliaburger,suite', 'LLVM_PROFILE_FILE': target + '/' + self.root.name + '-%p-%2m.profraw'})
                if self.mode == 'context':
                    env['RUSTFLAGS'] = '-C opt-level=3'
                configuration = c.read_json(env['RELIABURGER_COVERAGE_OWNER_CONFIG'])
                actual = [
                    'nextest',
                    'run',
                    '--manifest-path',
                    str(self.root / 'Cargo.toml'),
                    '--target-dir',
                    target,
                    '--no-tests=fail',
                    '--profile',
                    'ci',
                ]
                if self.mode == 'legacy-order':
                    actual[-3:] = ['--profile', 'ci', '--no-tests=fail']
                if self.mode == 'profile-change':
                    actual[-1] = 'default'
                if self.mode == 'missing-no-tests':
                    actual.remove('--no-tests=fail')
                if self.mode == 'selector':
                    actual += ['--features=ebpf']
                if self.mode == 'tool-change':
                    self.root.joinpath('nextest').write_text('changed after owner observation')
                if self.mode == 'nonce':
                    configuration['nonce'] = 'wrong live nonce'
                try:
                    result = a.invoke(
                        actual,
                        self.o.adapter_configuration(configuration),
                        env,
                        self.fake_run,
                    )
                    if self.mode == 'duplicate':
                        result = a.invoke(
                            actual,
                            self.o.adapter_configuration(configuration),
                            env,
                            self.fake_run,
                        )
                except c.Invalid as error:
                    self.refusal = str(error)
                    return SimpleNamespace(returncode=77)
                return SimpleNamespace(returncode=17 if self.mode == 'test-exit' else result)
            if '--lcov' in cmd and self.mode == 'lcov':
                return SimpleNamespace(returncode=18)
            if '--fail-under-lines' in cmd and self.mode == 'floor':
                return SimpleNamespace(returncode=9)
            return SimpleNamespace(returncode=0)
        if name == 'nextest':
            if cmd[2] == 'list':
                d = {
                    'rust-build-meta': {},
                    'test-count': 1,
                    'rust-suites': {'reliaburger': {'binary-id': 'reliaburger', 'status': 'listed', 'testcases': {'cron::boundary': {'filter-match': {'status': 'matches'}}}}},
                }
                kw['stdout'].write(json.dumps(d).encode())
            else:
                self.child_count += 1
                p = self.root / 'target/nextest/ci/junit.xml'
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text('<testsuites><testsuite name="reliaburger"><testcase classname="reliaburger" name="cron::boundary"/></testsuite></testsuites>')
            return SimpleNamespace(returncode=0)
        raise AssertionError(cmd)

    def execute(self):
        return self.o.execute(
            self.root,
            self.directory,
            self.context,
            ['src/meat/cron.rs'],
            self.fake_run,
            self.commands,
            self.environment,
        )

    def test_default_owner_observes_coverage_version_with_required_subcommand(self):
        binaries = {'cargo-nextest': str(self.root / 'nextest'),
                    'cargo-llvm-cov': str(self.root / 'coverage')}
        rust = {name: str(self.root / name) for name in ('cargo', 'rustc')}
        with patch.object(self.o.owner_tools, 'rust_tools', return_value=rust), \
             patch.object(self.o.shutil, 'which', side_effect=lambda name, **kwargs: binaries[name]):
            self.o.execute(self.root, self.directory, self.context,
                           ['src/meat/cron.rs'], self.fake_run, environment=self.environment)
        queries = [command for command in self.calls
                   if Path(command[0]).name == 'coverage' and '--version' in command]
        self.assertEqual(queries, [[str(self.root / 'coverage'), 'llvm-cov', '--version']])
        self.assertEqual(self.child_count, 1)

    def test_pre_execution_owner_handshake_then_one_run_and_all_existing_stages(self):
        expected = self.execute()
        self.assertEqual(self.child_count, 1)
        self.assertEqual(
            k.evidence(expected, self.directory, {('reliaburger', 'cron::boundary')}),
            {('reliaburger', 'cron::boundary')},
        )
        coverage = [x for x in self.calls if Path(x[0]).name == 'coverage' and '--version' not in x]
        self.assertEqual([x[2:] for x in coverage], self.o.recipe_stages('ci', '78.65'))
        self.assertEqual(len([x for x in self.calls if x[0] == 'make']), 1)
        self.assertTrue((self.directory / 'handshake.json').is_file())

    def test_disabled_overlay_cannot_supply_success_owner(self):
        self.mode = 'disabled'
        with self.assertRaisesRegex(c.Invalid, 'handshake|interception'):
            self.execute()
        self.assertEqual(self.child_count, 0)

    def test_duplicate_interception_fails_owner_without_second_run(self):
        self.mode = 'duplicate'
        with self.assertRaisesRegex(c.Invalid, 'failed|handshake|interception'):
            self.execute()
        self.assertEqual(self.child_count, 1)

    def test_mismatched_generated_environment_refused_before_discovery(self):
        self.mode = 'context'
        with self.assertRaisesRegex(c.Invalid, 'failed|context'):
            self.execute()
        self.assertEqual(self.child_count, 0)

    def test_failed_floor_preserves_completion_failure_and_does_not_repeat_tests(self):
        self.mode = 'floor'
        with self.assertRaisesRegex(c.Invalid, 'failed'):
            self.execute()
        owner = c.read_json(self.directory / 'owner.json')
        self.assertEqual(owner['stages'][-1]['exit_code'], 9)
        self.assertEqual(self.child_count, 1)

    def test_run_directory_and_nonce_are_exclusive(self):
        self.execute()
        with self.assertRaisesRegex(c.Invalid, 'exclusive'):
            self.execute()
        self.assertEqual(self.child_count, 1)

    def test_source_recipe_drift_refused_before_tool_or_make_execution(self):
        self.root.joinpath('Makefile').write_text(self.o.MAKE_RECIPE.replace('78.65', '70'))
        with self.assertRaisesRegex(c.Invalid, 'recipe'):
            self.execute()
        self.assertFalse(self.calls)

    def test_unaudited_host_tool_is_truthfully_refused(self):
        self.versions['coverage'] = 'cargo-llvm-cov 0.8.7'
        with self.assertRaisesRegex(c.Invalid, 'unaudited'):
            self.execute()
        self.assertEqual(self.child_count, 0)

    def test_changed_metadata_schema_refused_before_make(self):
        self.mode = 'metadata-version'
        with self.assertRaisesRegex(c.Invalid, 'metadata'):
            self.execute()
        self.assertEqual(self.child_count, 0)

    def test_changed_ci_retry_policy_refused_before_make(self):
        path = self.root / '.config/nextest.toml'
        path.write_text(path.read_text().replace('retries = 0', 'retries = 2'))
        with self.assertRaisesRegex(c.Invalid, 'nextest'):
            self.execute()
        self.assertEqual(self.child_count, 0)

    def test_unexpected_finite_selector_refused_before_discovery(self):
        self.mode = 'selector'
        with self.assertRaisesRegex(c.Invalid, 'failed'):
            self.execute()
        self.assertEqual(self.child_count, 0)

    def test_genuine_nextest_changed_after_resolution_refused(self):
        self.mode = 'tool-change'
        with self.assertRaisesRegex(c.Invalid, 'failed'):
            self.execute()
        self.assertEqual(self.child_count, 0)

    def test_wrong_nonce_refused_by_live_controller(self):
        self.mode = 'nonce'
        with self.assertRaisesRegex(c.Invalid, 'failed'):
            self.execute()
        self.assertEqual(self.child_count, 0)

    def test_test_stage_exit_cannot_be_hidden_by_healthy_junit(self):
        self.mode = 'test-exit'
        with self.assertRaisesRegex(c.Invalid, 'failed'):
            self.execute()
        owner = c.read_json(self.directory / 'owner.json')
        self.assertEqual(owner['stages'][-1]['exit_code'], 17)
        self.assertEqual(len(owner['stages']), 2)
        self.assertEqual(self.child_count, 1)

    def test_failed_lcov_stops_remaining_stages_without_repeating_tests(self):
        self.mode = 'lcov'
        with self.assertRaisesRegex(c.Invalid, 'failed'):
            self.execute()
        owner = c.read_json(self.directory / 'owner.json')
        self.assertEqual(owner['stages'][-1]['exit_code'], 18)
        self.assertEqual(len(owner['stages']), 3)
        self.assertEqual(self.child_count, 1)

    def test_runtime_tool_and_original_front_door_bytes_bind_completion(self):
        expected = self.execute()
        p = self.directory / 'owner-runtime.json'
        row = c.read_json(p)
        row['make_command'] = ['true']
        p.write_text(json.dumps(row))
        with self.assertRaisesRegex(c.Invalid, 'runtime changed'):
            k.evidence(expected, self.directory, {('reliaburger', 'cron::boundary')})

    def test_child_cannot_choose_checkout_nonce_or_command(self):
        expected = self.execute()
        configuration = c.read_json(self.directory / 'config.json')
        with self.assertRaises((c.Invalid, OSError)):
            self.o.request(
                configuration,
                dict(kind='handshake', nonce='late-self-authored', argv=expected['run_argv'], environment=expected['instrumentation']),
            )
        self.assertEqual(self.child_count, 1)

    def test_owner_accepts_exact_recorded_091_generated_child_order(self):
        self.execute()
        configuration = c.read_json(self.directory / 'config.json')
        self.assertEqual(configuration['expected']['run_argv'], [
            str(self.root / 'nextest'), 'nextest', 'run',
            '--manifest-path', str(self.root / 'Cargo.toml'),
            '--target-dir', str(self.root / 'target/llvm-cov-target'),
            '--no-tests=fail', '--profile', 'ci',
        ])
        self.assertEqual(self.child_count, 1)
        self.assertEqual(configuration['expected']['stages'][1][2:],
                         ['--no-report', 'nextest', '--profile', 'ci', '--no-tests=fail'])

    def test_owner_refuses_old_order_without_normalizing_equivalent_flags(self):
        self.mode = 'legacy-order'
        with self.assertRaisesRegex(c.Invalid, 'handshake|interception'):
            self.execute()
        self.assertEqual(self.child_count, 0)
        self.assertIn('actual finite child selectors differ', self.refusal)

    def test_owner_refuses_changed_or_missing_recorded_finite_selectors(self):
        for mode in ('profile-change', 'missing-no-tests'):
            with self.subTest(mode=mode):
                self.mode = mode
                with self.assertRaisesRegex(c.Invalid, 'handshake|interception'):
                    self.execute()
                self.assertEqual(self.child_count, 0)
                self.assertIn('actual finite child selectors differ', self.refusal)
                shutil.rmtree(self.directory)

if __name__ == '__main__':
    unittest.main()
