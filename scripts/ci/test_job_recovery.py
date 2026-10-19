"""A crash probe must refuse an unrelated PID, executable or configuration."""
import importlib.util
import os
import pathlib
import re
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('job_recovery', ROOT / 'scripts/demo/verify-job-recovery.py')
recovery = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recovery)


def system_only_routes():
    """Routes the API reserves for the node-to-node system principal."""
    authz = (ROOT / 'src/bun/authz.rs').read_text()
    return re.findall(r'route\((?:Get|Post|Put|Delete), "([^"]+)", System\)', authz)


class JobRecovery(unittest.TestCase):
    def test_probe_calls_no_system_only_route(self):
        routes = system_only_routes()
        self.assertIn('/v1/batch/array/sync', routes)
        source = (ROOT / 'scripts/demo/verify-job-recovery.py').read_text()
        for route in routes:
            # Compare the fixed part before any `{id}` segment.
            prefix = route.split('{')[0]
            self.assertNotIn(prefix, source, f'the probe calls the system-only route {route}')
        self.assertNotIn('RELIABURGER_TOKEN', source,
                         'the probe must not handle the token itself; relish reads it')

    def test_signal_authority_requires_the_original_executable_and_config_arguments(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            root = root.resolve()
            bun, config = root / 'bun', root / 'node.toml'
            bun.touch()
            config.touch()
            owner = root / 'proc' / '42'
            owner.mkdir(parents=True)
            (owner / 'exe').symlink_to(bun)
            def command(args):
                (owner / 'cmdline').write_bytes(b'\0'.join(os.fsencode(arg) for arg in args) + b'\0')
            original = [str(bun), '--config', str(config), '--cluster', '--runtime', 'runc']
            command(original)
            self.assertEqual(recovery.verify_owner(42, bun, config, root / 'proc'), original)
            for args in ([bun, '--config', config], [bun, '--config', root / 'other', '--cluster'],
                         [bun, config, '--cluster'], [bun, '--config', config, '--config', config, '--cluster']):
                command(args)
                with self.assertRaises(ValueError):
                    recovery.verify_owner(42, bun, config, root / 'proc')
            command([bun, '--config', config, '--cluster'])
            with self.assertRaises(ValueError):
                recovery.verify_owner(42, root / 'another-bun', config, root / 'proc')
            with self.assertRaises(ValueError):
                recovery.verify_owner(1, bun, config, root / 'proc')
