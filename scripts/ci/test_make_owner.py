"""Synthetic Make-owner process/handshake tests; no Cargo or mirror execution."""
import copy
import importlib
import json
import os
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

import contracts as c
import test_contracts as base
import workflow_assembly as w


MAKEFILE = '''test:
\t$(NEXTEST)
test-linux:
\t$(CARGO) build --features ebpf --bin bun
\tRELIABURGER_RUNC_TESTS=1 RELIABURGER_BUN_BINARY="$(CURDIR)/target/debug/bun" $(WITH_TEST_IMAGES) $(NEXTEST) --features ebpf --run-ignored=only -E 'binary(test_storage) $(LINUX_EXCLUDE)'
'''


class MakeOwner(unittest.TestCase):
    def setUp(self):
        base.Contracts.setUp(self)
        self.m = importlib.import_module('make_owner')
        self.root = self.root.resolve()
        self.root.joinpath('Makefile').write_text(MAKEFILE)
        self.root.joinpath('src').mkdir()
        self.root.joinpath('src/contract.rs').write_text('synthetic case')
        self.root.joinpath('.config').mkdir()
        self.root.joinpath('.config/nextest.toml').write_text('synthetic expected config')
        self.directory = self.root / 'target/evidence'
        self.context.update(host='darwin')
        self.context.pop('gate', None)
        self.tools = {}
        for name in ['cargo', 'rustc', 'nextest']:
            path = self.root / name
            path.write_text('synthetic actual ' + name)
            self.tools[name] = dict(path=str(path), sha256=c.digest(path))
        self.entry = dict(gate='portable-darwin', host='darwin', owner_command=['make', 'test'],
                          selectors=['--profile', 'ci'], run_options=['--no-tests=fail', '--retries=0'],
                          source_files=['src/contract.rs'], environment={},
                          inputs={'.config/nextest.toml': c.digest(self.root / '.config/nextest.toml')},
                          junit_source='target/nextest/ci/junit.xml')
        self.calls = []
        self.mode = 'healthy'
        self.child_status = 0
        self.discovery_status = 0
        self.owner_status = 0

    def write(self):
        base.Contracts.write(self)

    def tearDown(self):
        base.Contracts.tearDown(self)

    def fake_run(self, command, **kwargs):
        self.calls.append((list(command), kwargs))
        if command[0] == 'git':
            out = self.context['commit'] + '\n' if command[1] == 'rev-parse' else 'src/contract.rs\n' if '--error-unmatch' in command else ''
            return SimpleNamespace(returncode=0, stdout=out)
        if command[0] == 'make':
            if self.mode == 'no-interception' or self.owner_status:
                return SimpleNamespace(returncode=self.owner_status)
            config = c.read_json(self.directory / 'config.json')
            env = dict(kwargs['env'])
            env.update(self.entry['environment'])
            arguments = self.entry['selectors'][:]
            if self.mode == 'selector-change':
                arguments += ['--features=ebpf']
            if self.mode == 'compiler-change':
                env['RUSTFLAGS'] = '-C opt-level=3'
            if self.mode == 'fixture-change':
                env['RELIABURGER_RUNC_TESTS'] = '1'
            try:
                status = self.m.child(config, arguments, env, self.fake_run)
                if self.mode == 'duplicate':
                    status = self.m.child(config, arguments, env, self.fake_run)
            except c.Invalid:
                status = 1
            return SimpleNamespace(returncode=status)
        if 'list' in command:
            kwargs['stdout'].write(json.dumps(self.discovery).encode())
            return SimpleNamespace(returncode=self.discovery_status)
        if self.mode != 'no-junit':
            report = self.root / self.entry['junit_source']
            report.parent.mkdir(parents=True, exist_ok=True)
            report.write_text(self.report)
        return SimpleNamespace(returncode=self.child_status)

    def execute(self):
        return self.m.execute(self.root, self.directory, self.context, self.entry,
                              tools=self.tools, environment={'PATH': '/usr/bin:/bin'}, run=self.fake_run)

    def evidence(self):
        return self.m.evidence(c.read_json(self.directory / 'expected.json'), self.directory,
                               {('reliaburger', 'pickle::tests::bounded')})

    def test_actual_owner_runs_once_and_child_discovery_run_share_selectors(self):
        self.assertEqual(self.execute(), 0)
        self.assertEqual(self.evidence(), {('reliaburger', 'pickle::tests::bounded')})
        actual = [cmd for cmd, _ in self.calls if cmd[0] == self.tools['nextest']['path']]
        self.assertEqual(len(actual), 2)
        self.assertEqual(actual[0][3:5], actual[1][3:5])
        self.assertEqual(sum(cmd[0] == 'make' for cmd, _ in self.calls), 1)

    def test_linux_retains_make_build_mirror_and_original_exclusions(self):
        self.context['host'] = 'linux'
        self.entry.update(gate='linux-root-storage', host='linux', owner_command=['make', 'test-linux'])
        selectors, env = w.make_selection(self.root, 'test-linux')
        self.entry.update(selectors=selectors, environment=env)
        self.assertEqual(self.execute(), 0)
        command = next(cmd for cmd, _ in self.calls if cmd[0] == 'make')
        self.assertEqual(command[:2], ['make', 'test-linux'])
        self.assertIn('LINUX_EXCLUDE=' + w.LINUX_EXCLUDE, command)
        self.assertEqual(self.evidence(), {('reliaburger', 'pickle::tests::bounded')})

    def test_missing_or_duplicate_interception_refuses_despite_healthy_reports(self):
        for mode in ['no-interception', 'duplicate']:
            with self.subTest(mode=mode):
                self.mode = mode
                self.assertNotEqual(self.execute(), 0)
                with self.assertRaises(c.Invalid):
                    self.evidence()
                import shutil
                shutil.rmtree(self.directory)

    def test_compiler_fixture_or_selector_change_refuses_before_actual_tests(self):
        for mode in ['compiler-change', 'fixture-change', 'selector-change']:
            with self.subTest(mode=mode):
                self.mode = mode
                self.assertNotEqual(self.execute(), 0)
                self.assertFalse(any(cmd[0] == self.tools['nextest']['path'] for cmd, _ in self.calls))
                import shutil
                shutil.rmtree(self.directory)
                self.calls.clear()

    def test_prerequisite_failure_records_real_make_exit_without_child(self):
        self.owner_status = 17
        self.assertEqual(self.execute(), 17)
        owner = c.read_json(self.directory / 'owner.json')
        self.assertEqual(owner['exit_code'], 17)
        self.assertEqual(owner['interception_count'], 0)
        with self.assertRaises(c.Invalid):
            self.evidence()

    def test_failed_nextest_and_failed_discovery_remain_failed(self):
        self.child_status = 18
        self.assertEqual(self.execute(), 18)
        self.assertEqual(c.read_json(self.directory / 'child.json')['exit_code'], 18)
        with self.assertRaises(c.Invalid):
            self.evidence()

    def test_discovery_failure_does_not_execute_tests(self):
        self.discovery_status = 19
        self.assertNotEqual(self.execute(), 0)
        child = c.read_json(self.directory / 'child.json')
        self.assertEqual(child['discovery_exit'], 19)
        self.assertFalse(any('run' in cmd for cmd, _ in self.calls if cmd[0] == self.tools['nextest']['path']))

    def test_no_new_report_cannot_reuse_prior_success(self):
        path = self.root / self.entry['junit_source']
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(self.report)
        self.mode = 'no-junit'
        self.assertNotEqual(self.execute(), 0)
        self.assertFalse(self.directory.joinpath('junit.xml').exists())

    def test_failed_skipped_or_errored_cases_cannot_complete_owner(self):
        self.report = self.report.replace('</testcase>', '<skipped/></testcase>')
        # Fixture uses self-closing testcase: make the refusal explicit.
        self.report = self.report.replace('/>', '><skipped/></testcase>', 1) if '<skipped' not in self.report else self.report
        self.assertNotEqual(self.execute(), 0)

    def test_owner_directory_and_tool_bytes_are_fenced(self):
        self.assertEqual(self.execute(), 0)
        with self.assertRaises(c.Invalid):
            self.execute()

    def test_changed_current_plan_cannot_authorize_previous_owner(self):
        self.execute()
        expected = c.read_json(self.directory / 'expected.json')
        expected['context']['attempt'] = '99'
        with self.assertRaises(c.Invalid):
            self.m.evidence(expected, self.directory, {('reliaburger', 'pickle::tests::bounded')})

    def test_payload_hash_change_refuses(self):
        self.execute()
        self.directory.joinpath('junit.xml').write_text('<testsuites/>')
        with self.assertRaises(c.Invalid):
            self.evidence()
