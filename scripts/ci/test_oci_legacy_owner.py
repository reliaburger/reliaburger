"""Synthetic driver/legacy front-door controls; no Rust/runtime qualification."""
import copy
import json
from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
import contracts as c
import completion
import oci_legacy_owner as bridge
import test_oci_driver


class LegacyOciOwner(unittest.TestCase):
    def setUp(self):
        self.fixture = test_oci_driver.OciDriver()
        self.fixture.setUp()
        f = self.fixture
        self.root = f.root
        f.directory = self.root / 'target/contracts/linux/oci-interruptions'
        self.directory = f.directory
        self.context = {key: f.context[key] for key in ('commit', 'run_id', 'attempt', 'host')}
        (self.root / 'tests').mkdir()
        (self.root / '.github/workflows').mkdir(parents=True)
        (self.root / 'Makefile').write_text('test-linux:\n\tcargo nextest run --run-ignored=only\n')
        (self.root / '.github/workflows/ci.yml').write_text('run: make test-linux\nrun: scripts/release/qualify-oci-interruptions.sh\n')
        self.rows = []
        self.bindings = []
        self.names = {}
        for group in ('owned_runc', 'owned_network', 'oci_crash'):
            source = 'tests/' + group + '.rs'
            name = group + '_case'
            self.names['reliaburger::' + group] = [name]
            reason = 'run with make test-linux' if group != 'oci_crash' else 'run with scripts/release/qualify-oci-interruptions.sh'
            (self.root / source).write_text('#[test]\n#[ignore = "' + reason + '"]\nfn ' + name + '() {}\n')
            if group == 'oci_crash':
                continue
            binding = dict(source=source, function=name, binary='reliaburger::' + group, test=name)
            self.bindings.append(binding)
            self.rows.append(dict(**binding, declared_owner=['make', 'test-linux'], gate='oci-interruptions',
                                  group=group, runtime='runc' if group == 'owned_runc' else 'linux-network-namespaces',
                                  source_sha256=c.digest(self.root / source), case_id='legacy-' + group))
        self.manifest = dict(contracts=[dict(cases=[dict(id=row['case_id'], binary=row['binary'], test=row['test'],
                                                       requires=['oci-interruptions'], sources=[row['source']])
                                                  for row in self.rows])])
        self.manifest_path = self.root / 'manifest.json'
        self.manifest_path.write_text(json.dumps(self.manifest))
        self.verified = {(row['binary'], row['test']) for row in self.rows}
        plans = {}
        original_produce = completion.produce
        def produce(plan, root, directory, run):
            plans[plan['binary']] = plan
            names = self.names[plan['binary']]
            def invoke(command, **kwargs):
                if command[0] == 'git':
                    output = plan['context']['commit'] + '\n' if command[1] == 'rev-parse' else (
                        '\n'.join(plan['source_files']) + '\n' if '--error-unmatch' in command else '')
                    return SimpleNamespace(returncode=0, stdout=output)
                if '--list' in command:
                    output = ''.join(name + ': test\n' for name in names) + f'\n{len(names)} tests, 0 benchmarks\n'
                else:
                    output = f'running {len(names)} tests\n' + ''.join('test ' + name + ' ... ok\n' for name in names)
                    output += f'\ntest result: ok. {len(names)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n'
                kwargs['stdout'].write(output.encode())
                return SimpleNamespace(returncode=0)
            return original_produce(plan, root, directory, invoke)
        with patch.object(f.d.completion, 'produce_build', f.fake_build), patch.object(f.d.completion, 'produce', produce), patch.object(f.d.gates, 'candidate_sources'):
            self.assertEqual(f.d.execute(self.root, self.directory, f.context, f.tools), 0)
        self.approved = dict(context=self.context, tools=f.tools, plans=plans,
                             origin_path='target/contracts/linux/oci-interruptions/build/build-origin.json')
        (self.directory / 'approved.json').write_text(json.dumps(self.approved))
        self.job = dict(result='success', outputs={})
        self.reseal()

    def tearDown(self):
        self.fixture.tearDown()

    def reseal(self):
        seal = dict(schema_version=1, gate='oci-interruptions', context=self.context,
                    manifest_sha256=c.digest(self.manifest_path), files={})
        for path in self.directory.rglob('*'):
            if path.is_file() and path.name != 'gate-seal.json':
                seal['files'][path.relative_to(self.directory).as_posix()] = c.digest(path)
        (self.directory / 'gate-seal.json').write_text(json.dumps(seal))
        self.job['outputs']['oci_interruptions_sha256'] = c.digest(self.directory / 'gate-seal.json')

    def qualify(self):
        return bridge.qualify(self.root, self.rows, self.bindings, self.manifest, self.manifest_path,
                              self.context, self.job, self.directory, self.verified)

    def test_success_adds_only_reviewed_case_identities_to_the_original_owner(self):
        self.assertEqual(self.qualify(), self.verified)
        policy_before = [case.reason for case in bridge.ignored_owners.find_ignored(self.root)]
        # Limit this independent compatibility assertion to the two mapped source
        # rows. The OCI crash family keeps its original script owner separately.
        owned = {bridge.DECLARED_OWNER: self.qualify(), ('script', 'scripts/release/qualify-oci-interruptions.sh'):
                 {('reliaburger::oci_crash', 'oci_crash_case')}}
        bindings = self.bindings + [dict(source='tests/oci_crash.rs', function='oci_crash_case',
                                         binary='reliaburger::oci_crash', test='oci_crash_case')]
        self.assertEqual(bridge.ignored_owners.evidence_problems(self.root, self.directory, bindings, owned), [])
        self.assertEqual(policy_before, [case.reason for case in bridge.ignored_owners.find_ignored(self.root)])

    def test_missing_mapping_row_is_not_a_generic_binary_substitution(self):
        self.rows.pop()
        with self.assertRaisesRegex(c.Invalid, 'incomplete finite'):
            self.qualify()

    def test_duplicate_mapping_row_is_refused(self):
        self.rows.append(copy.deepcopy(self.rows[0]))
        with self.assertRaises(c.Invalid):
            self.qualify()

    def test_unknown_binary_or_manual_fixture_cannot_be_mapped(self):
        self.rows[0]['binary'] = 'reliaburger::oci_crash'
        with self.assertRaises(c.Invalid):
            self.qualify()

    def test_runtime_group_declared_owner_or_substitute_gate_cannot_change(self):
        for field, value in [('runtime', 'docker'), ('group', 'owned_network'),
                             ('declared_owner', ['make', 'test-cluster']), ('gate', 'portable-linux')]:
            original = self.rows[0][field]
            self.rows[0][field] = value
            with self.subTest(field=field), self.assertRaises(c.Invalid):
                self.qualify()
            self.rows[0][field] = original

    def test_changed_source_or_full_name_requires_new_review(self):
        (self.root / self.rows[0]['source']).write_text('changed source')
        with self.assertRaises(c.Invalid):
            self.qualify()

    def test_changed_exact_binding_cannot_reuse_a_leaf(self):
        self.bindings[0]['test'] = 'another_module::' + self.bindings[0]['test']
        with self.assertRaises(c.Invalid):
            self.qualify()

    def test_missing_or_wrong_case_id_cannot_qualify(self):
        self.rows[0]['case_id'] = 'unknown-case'
        with self.assertRaises(c.Invalid):
            self.qualify()

    def test_expected_current_context_cannot_be_derived_from_old_artifact(self):
        for field, value in [('commit', 'b' * 40), ('run_id', '999'), ('attempt', '1'), ('host', 'darwin')]:
            original = self.context[field]
            self.context[field] = value
            with self.subTest(field=field), self.assertRaises(c.Invalid):
                self.qualify()
            self.context[field] = original

    def test_failed_skipped_or_unfinished_whole_job_is_refused(self):
        for status in ['failure', 'skipped', 'cancelled', 'in_progress']:
            self.job['result'] = status
            with self.subTest(status=status), self.assertRaises(c.Invalid):
                self.qualify()

    def test_no_trusted_step_output_cannot_use_its_own_artifact_hash(self):
        self.job['outputs'].clear()
        with self.assertRaises(c.Invalid):
            self.qualify()

    def test_unverified_alias_case_cannot_expand_legacy_owner_set(self):
        self.verified.remove(next(iter(self.verified)))
        with self.assertRaises(c.Invalid):
            self.qualify()

    def test_failed_whole_driver_cannot_qualify_even_with_child_success(self):
        path = self.directory / 'driver.json'
        row = c.read_json(path)
        row['exit_code'] = 1
        path.write_text(json.dumps(row))
        self.reseal()
        with self.assertRaises(c.Invalid):
            self.qualify()

    def test_missing_child_prevents_partial_case_requalification(self):
        path = self.directory / 'driver.json'
        row = c.read_json(path)
        row['children'].pop()
        path.write_text(json.dumps(row))
        self.reseal()
        with self.assertRaises(c.Invalid):
            self.qualify()

    def test_failed_or_skipped_or_start_only_child_cannot_qualify(self):
        path = self.directory / 'owned_runc/execution.log'
        original = path.read_text()
        for text in [original.replace('... ok', '... FAILED'), original.replace('... ok', '... ignored'),
                     'running 1 test\ntest owned_runc_case ...\n']:
            path.write_text(text)
            # Retain truthful hashes around the failing output so refusal must
            # inspect actual completion rather than stop at a stale-file hash.
            receipt_path = self.directory / 'owned_runc/receipt.json'
            receipt = c.read_json(receipt_path)
            receipt['execution_sha256'] = c.digest(path)
            receipt_path.write_text(json.dumps(receipt))
            driver_path = self.directory / 'driver.json'
            driver = c.read_json(driver_path)
            driver['children'][0]['receipt_sha256'] = c.digest(receipt_path)
            driver_path.write_text(json.dumps(driver))
            self.reseal()
            with self.subTest(text=text), self.assertRaises(c.Invalid):
                self.qualify()
        path.write_text(original)



if __name__ == '__main__':
    unittest.main()


class CurrentRepositoryBindings(unittest.TestCase):
    def test_current_ignored_bindings_and_reviewed_oci_sources_are_complete(self):
        import ignored_owners
        root = Path(__file__).resolve().parents[2]
        targets, scripts = ignored_owners.ci_owners(root)
        required = {(case.path, case.name) for case in ignored_owners.find_ignored(root)
                    if any((kind == 'make' and owner in targets) or
                           (kind == 'script' and owner in scripts)
                           for kind, owner in ignored_owners.owners(case))}
        bindings = c.read_json(root / 'tests/contracts/ignored-bindings.json')
        self.assertEqual(required, {(row['source'], row['function']) for row in bindings})
        manifest = c.inventory(c.read_json(root / 'tests/contracts/manifest.json'), root)
        bridge.reviewed_rows(root, c.read_json(root / 'scripts/ci/legacy-oci-aliases.json'),
                             bindings, manifest)
