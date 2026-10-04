"""Selected-toolchain and version/schema controls using synthetic observations."""
import copy
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

import contracts as c
import make_owner
import owner_tools


class OwnerTools(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.sysroot = self.root / 'toolchain'
        self.sysroot.joinpath('bin').mkdir(parents=True)
        for name in ['cargo', 'rustc']:
            self.sysroot.joinpath('bin', name).write_text('actual synthetic selected ' + name)
        self.tools = {}
        for name in ['cargo', 'rustc', 'nextest']:
            path = str(self.root / name)
            prefix = 'cargo-nextest 0.9.145' if name == 'nextest' else name + ' 1.98.1 (synthetic)'
            verbose = '\nrelease: 1.98.1\nhost: x86_64-unknown-linux-gnu\ncommit-hash: ' + 'a' * 40
            version = prefix + (verbose if name != 'nextest' else '')
            if name == 'rustc':
                version += '\nLLVM version: 22.1.0'
            self.tools[name] = dict(path=path, realpath=path, sha256='b' * 64, version=version,
                                   version_command=[path, '--version'] + (['--verbose'] if name != 'nextest' else []), exit_code=0)

    def tearDown(self):
        self.tmp.cleanup()

    def test_resolves_selected_sysroot_bins_instead_of_hashing_proxy_only(self):
        calls = []
        def run(command, **kwargs):
            calls.append(command)
            return SimpleNamespace(returncode=0, stdout=str(self.sysroot))
        with patch.object(owner_tools.shutil, 'which', return_value='/synthetic/rustup-proxy'):
            result = owner_tools.rust_tools({'PATH': '/synthetic'}, run)
        self.assertEqual(result['cargo'], str(self.sysroot.joinpath('bin/cargo').resolve()))
        self.assertEqual(calls, [['/synthetic/rustup-proxy', '--print', 'sysroot']])

    def test_failed_or_relative_sysroot_refuses(self):
        for code, output in [(17, str(self.sysroot)), (0, 'relative')]:
            with patch.object(owner_tools.shutil, 'which', return_value='/synthetic/proxy'):
                with self.assertRaises(c.Invalid):
                    owner_tools.rust_tools({}, lambda *a, **k: SimpleNamespace(returncode=code, stdout=output))

    def test_missing_real_toolchain_binary_refuses(self):
        self.sysroot.joinpath('bin/cargo').unlink()
        with patch.object(owner_tools.shutil, 'which', return_value='/synthetic/proxy'):
            with self.assertRaises(c.Invalid):
                owner_tools.rust_tools({}, lambda *a, **k: SimpleNamespace(returncode=0, stdout=str(self.sysroot)))

    def test_qualified_patch_context_has_verbose_commit_host_llvm_and_exact_queries(self):
        make_owner.validate_tools(self.tools)

    def test_unknown_version_incomplete_verbose_and_schema_drift_refuse(self):
        for change in [lambda t: t['rustc'].update(version=t['rustc']['version'].replace('1.98.1', '1.99.0')),
                       lambda t: t['cargo'].update(version=t['cargo']['version'].replace('commit-hash:', 'omitted:')),
                       lambda t: t['nextest'].update(version='cargo-nextest 0.9.146'),
                       lambda t: t['rustc'].update(unknown=True),
                       lambda t: t['rustc'].update(exit_code=17)]:
            changed = copy.deepcopy(self.tools)
            change(changed)
            with self.assertRaises(c.Invalid):
                make_owner.validate_tools(changed)

    def test_header_release_mismatch_and_wrong_invoked_tool_refuse(self):
        changed = copy.deepcopy(self.tools)
        changed['cargo']['version'] = changed['cargo']['version'].replace('release: 1.98.1', 'release: 1.98.0')
        with self.assertRaises(c.Invalid):
            make_owner.validate_tools(changed)
        changed = copy.deepcopy(self.tools)
        changed['nextest']['version_command'][0] = '/different/executable'
        with self.assertRaises(c.Invalid):
            make_owner.validate_tools(changed)
