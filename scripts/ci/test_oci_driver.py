"""Synthetic OCI whole-driver controls; these do not execute Rust or namespaces."""
import importlib
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

import contracts as c


class OciDriver(unittest.TestCase):
    def setUp(self):
        # Synthetic driver inputs must not inherit developer build overrides.
        # Refusal controls add their explicit overrides after this setup.
        self.environment_patch = patch.dict('os.environ', {'PATH': '/usr/bin:/bin'}, clear=True)
        self.environment_patch.start()
        self.addCleanup(self.environment_patch.stop)
        self.d = importlib.import_module('oci_driver')
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name).resolve() / 'repo'
        self.root.mkdir()
        self.directory = self.root / 'target' / 'oci-evidence'
        self.context = dict(commit='a' * 40, run_id='123', attempt='2', host='linux', authority='ci', owner=None)
        self.calls = []
        self.statuses = {}
        self.tools = {}
        for name in ['cargo', 'rustc', 'timeout', 'sudo', 'unshare', 'bash']:
            path = self.root / name
            path.write_text('synthetic genuine ' + name)
            self.tools[name] = {'path': str(path), 'sha256': c.digest(path)}
        self.artifacts = {}
        for name in ['owned_runc', 'owned_network', 'oci_crash']:
            path = self.root / 'target' / name
            path.parent.mkdir(exist_ok=True)
            path.write_text('synthetic executable ' + name)
            self.artifacts['reliaburger::' + name] = dict(executable=str(path), sha256=c.digest(path))
        self.origin = dict(schema_version=1, context=self.context, command=self.d.w.OCI_BUILD_COMMAND, exit_code=0, artifacts=self.artifacts)

    def tearDown(self):
        # LegacyOciOwner also creates and tears down this fixture directly.
        self.environment_patch.stop()
        self.tmp.cleanup()

    def fake_build(self, expected, command, root, directory, binaries, run):
        self.calls.append(('build', command))
        directory.mkdir(parents=True, exist_ok=True)
        origin = dict(self.origin, exit_code=self.statuses.get('build', 0))
        (directory / 'build-origin.json').write_text(json.dumps(origin))
        return origin['exit_code']

    def fake_produce(self, plan, root, directory, run):
        self.calls.append(('produce', plan))
        directory.mkdir(parents=True, exist_ok=True)
        status = self.statuses.get(plan['binary'], 0)
        (directory / 'receipt.json').write_text(json.dumps({'exit_code': status, 'binary': plan['binary']}))
        return status

    def execute(self):
        with patch.object(self.d.completion, 'produce_build', self.fake_build), patch.object(self.d.completion, 'produce', self.fake_produce), patch.object(self.d.gates, 'candidate_sources'):
            return self.d.execute(self.root, self.directory, self.context, self.tools, run=lambda *a, **k: SimpleNamespace(returncode=0))

    def test_one_builder_and_three_original_binary_runs(self):
        self.assertEqual(self.execute(), 0)
        self.assertEqual([kind for kind, _ in self.calls], ['build', 'produce', 'produce', 'produce'])
        row = c.read_json(self.directory / 'driver.json')
        self.assertEqual(row['exit_code'], 0)
        self.assertEqual(row['context'], self.context)
        self.d.driver_evidence(self.directory, self.context, self.tools, 'target/oci-evidence/build/build-origin.json')

    def test_original_oci_selectors_and_namespace_timeout_are_preserved(self):
        self.execute()
        plan = self.calls[1][1]
        self.assertEqual(plan['selectors'], ['--ignored', '--skip', 'normal_rootless_bun', '--skip', 'actual_host_reboot', '--skip', 'actual_bun_kernel_discovery_host_reboot'])
        self.assertEqual(plan['run_options'], ['--nocapture', '--test-threads=1', '--format=pretty', '--color=never'])
        wrapper = plan['wrapper']
        self.assertEqual(wrapper[:3], [self.tools['timeout']['path'], '420s', self.tools['sudo']['path']])
        self.assertIn('--mount', wrapper)
        self.assertIn('--net', wrapper)
        self.assertIn('mount -t tmpfs tmpfs /run/netns', wrapper[-2])

    def test_failed_builder_cannot_be_completed_by_old_or_partial_binary_reports(self):
        self.statuses['build'] = 17
        self.assertEqual(self.execute(), 17)
        self.assertEqual(len(self.calls), 1)
        with self.assertRaises(c.Invalid):
            self.d.driver_evidence(self.directory, self.context, self.tools, 'target/oci-evidence/build/build-origin.json')

    def test_binary_failure_is_whole_driver_failure_and_preserves_actual_status(self):
        self.statuses['reliaburger::owned_network'] = 18
        self.assertEqual(self.execute(), 18)
        self.assertEqual(len(self.calls), 3)
        row = c.read_json(self.directory / 'driver.json')
        self.assertEqual(row['exit_code'], 18)
        with self.assertRaises(c.Invalid):
            self.d.driver_evidence(self.directory, self.context, self.tools, 'target/oci-evidence/build/build-origin.json')

    def test_current_context_and_all_three_complete_children_are_required(self):
        self.execute()
        stale = dict(self.context, attempt='1')
        with self.assertRaises(c.Invalid):
            self.d.driver_evidence(self.directory, stale, self.tools, 'target/oci-evidence/build/build-origin.json')
        row = c.read_json(self.directory / 'driver.json')
        row['children'].pop()
        (self.directory / 'driver.json').write_text(json.dumps(row))
        with self.assertRaises(c.Invalid):
            self.d.driver_evidence(self.directory, self.context, self.tools, 'target/oci-evidence/build/build-origin.json')

    def test_receipt_and_builder_payload_changes_refuse(self):
        self.execute()
        self.directory.joinpath('build/build-origin.json').write_text('{}')
        with self.assertRaises(c.Invalid):
            self.d.driver_evidence(self.directory, self.context, self.tools, 'target/oci-evidence/build/build-origin.json')

    def test_disposable_directory_is_exclusive(self):
        self.execute()
        with self.assertRaises(c.Invalid):
            self.execute()

    def test_driver_refuses_a_non_linux_context(self):
        self.context.update(host='darwin')
        with self.assertRaises(c.Invalid):
            self.execute()

    def test_changed_tools_or_unexpected_binary_set_refuses(self):
        Path(self.tools['cargo']['path']).write_text('changed')
        with self.assertRaises(c.Invalid):
            self.execute()

    def test_child_receipt_is_not_itself_proof_of_successful_driver(self):
        self.execute()
        row = c.read_json(self.directory / 'driver.json')
        row['exit_code'] = 19
        (self.directory / 'driver.json').write_text(json.dumps(row))
        with self.assertRaises(c.Invalid):
            self.d.driver_evidence(self.directory, self.context, self.tools, 'target/oci-evidence/build/build-origin.json')

    def test_late_child_selector_or_context_cannot_authorize_itself(self):
        self.execute()
        row = c.read_json(self.directory / 'driver.json')
        row['children'][0]['plan']['selectors'] = ['--ignored']
        self.directory.joinpath('driver.json').write_text(json.dumps(row))
        with self.assertRaises(c.Invalid):
            self.d.driver_evidence(self.directory, self.context, self.tools, 'target/oci-evidence/build/build-origin.json')

    def test_external_directory_refuses_before_any_actual_builder(self):
        self.directory = Path(self.tmp.name) / 'external-evidence'
        with self.assertRaises(c.Invalid):
            self.execute()
        self.assertFalse(self.calls)

    def test_direct_workflow_entry_refuses_build_overrides_before_dispatch(self):
        import os
        overrides = {
            'RUSTFLAGS': '-Copt-level=3',
            'CARGO_ENCODED_RUSTFLAGS': '-C\x1finstrument-coverage',
            'RUSTC_WRAPPER': '/unreviewed/wrapper',
            'RUSTC_WORKSPACE_WRAPPER': '/unreviewed/workspace-wrapper',
            'CARGO_TARGET_DIR': '/unreviewed/artifacts',
            'CARGO_BUILD_TARGET': 'unreviewed-target',
        }
        for name, value in overrides.items():
            with self.subTest(override=name):
                self.directory = self.root / 'target' / name
                self.calls.clear()
                with patch.dict(os.environ, {name: value}), self.assertRaisesRegex(
                    c.Invalid, 'OCI build override requires separate audit'
                ):
                    self.execute()
                self.assertEqual(self.calls, [], 'builder dispatched before context refusal')
                self.assertFalse(self.directory.exists(), 'owner artifacts created before refusal')

    def test_direct_workflow_entry_refuses_a_different_build_identity(self):
        import os
        with patch.dict(os.environ, {'RELIABURGER_GIT_SHA': 'b' * 40}):
            with self.assertRaisesRegex(c.Invalid, 'OCI build identity differs'):
                self.execute()
        self.assertEqual(self.calls, [], 'builder dispatched with a different checkout identity')
        self.assertFalse(self.directory.exists())

    def test_manual_driver_runs_original_groups_but_cannot_qualify_ci(self):
        self.context.update(authority='manual', run_id='manual:review', owner='tester')
        self.assertEqual(self.execute(), 0)
        self.assertEqual([kind for kind, _ in self.calls], ['build', 'produce', 'produce', 'produce'])
        with self.assertRaisesRegex(c.Invalid, 'manual OCI evidence cannot qualify CI'):
            self.d.driver_evidence(self.directory, self.context, self.tools,
                                  'target/oci-evidence/build/build-origin.json')
        self.d.driver_evidence(self.directory, self.context, self.tools,
                               'target/oci-evidence/build/build-origin.json', allow_manual=True)

    def test_unreviewed_cargo_or_rustc_environment_refuses_before_builder(self):
        import os
        for name in ('RUSTC', 'CARGO'):
            with self.subTest(name=name), patch.dict(os.environ, {name: '/unreviewed/tool'}):
                with self.assertRaisesRegex(c.Invalid, 'OCI build override requires separate audit'):
                    self.execute()
        self.assertFalse(self.calls)
        self.assertFalse(self.directory.exists())
