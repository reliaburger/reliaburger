"""Exercise strict contract parsing with synthetic evidence.

These fixtures test policy acceptance and refusal; they do not execute Rust or
qualify a runtime, coverage run or workflow job.
"""
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import contracts


class Contracts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / 'Makefile').write_text('test:\n\tcargo nextest run\n')
        self.manifest = {'schema_version': 1, 'gates': {'portable-linux': {'mode': 'ci', 'hosts': ['linux'], 'command': ['make', 'test']}}, 'contracts': [{'id': 'quota', 'issue': 540, 'promise': 'reserve before write', 'supported_paths': ['CAS'], 'refused_paths': ['over-cap write'], 'boundaries': ['restart'], 'cases': [{'id': 'bounded', 'binary': 'reliaburger', 'test': 'pickle::tests::bounded', 'requires': ['portable-linux']}]}]}
        self.discovery = {'rust-build-meta': {}, 'test-count': 1, 'rust-suites': {'reliaburger': {'binary-id': 'reliaburger', 'status': 'listed', 'testcases': {'pickle::tests::bounded': {'ignored': False, 'filter-match': {'status': 'matches'}}}}}}
        self.context = {'commit': 'a' * 40, 'run_id': '100', 'attempt': '1', 'host': 'linux'}
        self.report = '<testsuites><testsuite name="reliaburger"><testcase classname="reliaburger" name="pickle::tests::bounded"/></testsuite></testsuites>'
        self.receipt = dict(self.context, schema_version=1, gate='portable-linux', command=['make', 'test'], exit_code=0)
        self.write()

    def tearDown(self):
        self.tmp.cleanup()

    def write(self):
        (self.root / 'discovery.json').write_text(json.dumps(self.discovery))
        (self.root / 'junit.xml').write_text(self.report)
        self.receipt.update(discovery_sha256=contracts.digest(self.root / 'discovery.json'), junit_sha256=contracts.digest(self.root / 'junit.xml'))
        (self.root / 'receipt.json').write_text(json.dumps(self.receipt))

    def check(self):
        manifest = contracts.inventory(self.manifest, self.root)
        return contracts.gate_evidence(manifest, 'portable-linux', self.root, self.context)

    def rejects(self, expected):
        with self.assertRaisesRegex(contracts.Invalid, expected):
            self.check()

    def test_native_host_cases_have_exact_linux_gate_contracts(self):
        root = Path(__file__).resolve().parents[2]
        manifest = json.loads((root / 'tests/contracts/manifest.json').read_text())
        cases = {(case['binary'], case['test']): case['requires']
                 for contract in manifest['contracts'] for case in contract['cases']}
        for name in (
            'cgroup_host_jobs_reuse_owned_helpers_with_fresh_processes_and_enforced_profiles',
            'cgroup_host_executor_recovery_waits_for_original_retirement_after_a_dropped_caller',
        ):
            self.assertEqual(cases.get(('reliaburger::owned_task_arrays', name)),
                             ['linux-root-storage'], name)

    def test_current_successful_discovered_execution_passes(self):
        self.assertEqual(self.check(), {('reliaburger', 'pickle::tests::bounded')})

    def test_unknown_schema_fields_fail(self):
        self.manifest['spelling_error'] = []
        self.rejects('unknown fields')

    def test_schema_bool_is_not_version_number(self):
        self.manifest['schema_version'] = True
        self.rejects('schema_version')

    def test_empty_inventory_fails(self):
        self.manifest['contracts'] = []
        self.rejects('empty contracts')

    def test_contract_requires_every_declared_path_and_boundary(self):
        for key in ('supported_paths', 'refused_paths', 'boundaries'):
            with self.subTest(key=key):
                saved = self.manifest['contracts'][0][key]
                self.manifest['contracts'][0][key] = []
                self.rejects(key)
                self.manifest['contracts'][0][key] = saved

    def test_nonexistent_make_gate_fails(self):
        self.manifest['gates']['portable-linux']['command'] = ['make', 'absent']
        self.rejects('Makefile gate missing')

    def test_unassigned_declared_gate_fails(self):
        self.manifest['gates']['unused'] = copy.deepcopy(self.manifest['gates']['portable-linux'])
        self.rejects('no assigned')

    def test_duplicate_contract_case_or_assignment_fails(self):
        item = copy.deepcopy(self.manifest['contracts'][0])
        self.manifest['contracts'].append(item)
        self.rejects('duplicate contract')
        item['id'] = 'second'
        self.rejects('duplicate case')
        item['cases'][0]['id'] = 'second-case'
        self.rejects('duplicate binary/test/gate')

    def test_unknown_required_gate_fails(self):
        self.manifest['contracts'][0]['cases'][0]['requires'] = ['absent']
        self.rejects('unknown required')

    def test_manual_owner_required(self):
        self.manifest['gates']['portable-linux']['mode'] = 'manual'
        self.rejects('manual owner')

    def test_missing_report_discovery_or_receipt_fails(self):
        for name in ('junit.xml', 'discovery.json', 'receipt.json'):
            with self.subTest(name=name):
                (self.root / name).unlink()
                self.rejects('No such file')
                self.write()

    def test_stale_commit_run_attempt_and_wrong_platform_fail(self):
        for key, value in [('commit', 'b' * 40), ('run_id', '99'), ('attempt', '0'), ('host', 'darwin')]:
            with self.subTest(key=key):
                saved = self.receipt[key]
                self.receipt[key] = value
                self.write()
                self.rejects(key)
                self.receipt[key] = saved

    def test_wrong_gate_or_command_fails(self):
        for key, value in [('gate', 'test-cluster'), ('command', ['make', 'test-slow'])]:
            with self.subTest(key=key):
                saved = self.receipt[key]
                self.receipt[key] = value
                self.write()
                self.rejects('wrong gate')
                self.receipt[key] = saved

    def test_failed_gate_does_not_count_successful_selected_case(self):
        self.receipt['exit_code'] = 1
        self.write()
        self.rejects('command failed')

    def test_tampered_junit_or_discovery_fails(self):
        for name in ('junit.xml', 'discovery.json'):
            with self.subTest(name=name):
                (self.root / name).write_text('replaced')
                self.rejects('hash mismatch')
                self.write()

    def test_empty_or_malformed_discovery_fails(self):
        self.discovery['rust-suites'] = {}
        self.write()
        self.rejects('empty discovery')
        (self.root / 'discovery.json').write_text('{')
        self.receipt['discovery_sha256'] = contracts.digest(self.root / 'discovery.json')
        (self.root / 'receipt.json').write_text(json.dumps(self.receipt))
        self.rejects('Expecting')

    def test_filter_mismatch_is_not_discovery(self):
        self.discovery['rust-suites']['reliaburger']['testcases']['pickle::tests::bounded']['filter-match'] = {'status': 'mismatch', 'reason': 'expression'}
        self.write()
        self.rejects('empty selected')

    def test_renamed_case_fails_exact_match(self):
        self.manifest['contracts'][0]['cases'][0]['test'] = 'pickle::tests::renamed'
        self.rejects('absent/mismatched')

    def test_full_module_identity_required(self):
        self.manifest['contracts'][0]['cases'][0]['test'] = 'bounded'
        self.rejects('absent/mismatched')

    def test_other_binary_does_not_count(self):
        self.report = self.report.replace('reliaburger', 'different')
        self.write()
        self.rejects('absent from assigned discovery')

    def test_junit_classname_mismatch_fails(self):
        self.report = self.report.replace('classname="reliaburger"', 'classname="wrong"')
        self.write()
        self.rejects('classname mismatch')

    def test_skipped_failed_error_or_success_after_failure_fails(self):
        for tag in ('skipped', 'failure', 'error', 'rerunFailure', 'flakyFailure'):
            with self.subTest(tag=tag):
                self.report = f'<testsuites><testsuite name="reliaburger"><testcase classname="reliaburger" name="pickle::tests::bounded"><{tag}/></testcase></testsuite></testsuites>'
                self.write()
                self.rejects('did not pass exactly once')

    def test_duplicate_junit_testcase_fails(self):
        self.report = self.report.replace('</testsuite>', '<testcase classname="reliaburger" name="pickle::tests::bounded"/></testsuite>')
        self.write()
        self.rejects('duplicate JUnit')

    def test_malformed_or_empty_junit_fails(self):
        for report in ('', '<testsuites/>', '<wrong/>'):
            with self.subTest(report=report):
                self.report = report
                self.write()
                self.rejects('malformed|empty|invalid')

    def test_discovered_but_unexecuted_case_fails(self):
        self.discovery['rust-suites']['reliaburger']['testcases']['unrelated'] = {'filter-match': {'status': 'matches'}}
        self.report = self.report.replace('pickle::tests::bounded', 'unrelated')
        self.write()
        self.rejects('not successfully executed')

    def test_duplicate_json_key_fails(self):
        (self.root / 'receipt.json').write_text('{"schema_version":1,"schema_version":1}')
        self.rejects('duplicate JSON key')

    def test_manual_other_host_does_not_prove_ci_case(self):
        self.context['host'] = 'darwin'
        self.rejects('not applicable')

    def test_unrelated_skipped_binary_is_allowed_but_required_one_is_not(self):
        self.discovery['rust-suites']['unrelated'] = {'binary-id': 'unrelated', 'status': 'skipped', 'testcases': {}}
        self.write()
        self.assertTrue(self.check())
        self.discovery['rust-suites']['reliaburger']['status'] = 'skipped'
        self.write()
        self.rejects('empty selected')

    def test_missing_build_metadata_or_zero_test_count_fails(self):
        del self.discovery['rust-build-meta']
        self.write()
        self.rejects('build metadata')
        self.discovery['rust-build-meta'] = {}
        self.discovery['test-count'] = 0
        self.write()
        self.rejects('test count')

    def test_unknown_discovery_status_fails(self):
        self.discovery['rust-suites']['reliaburger']['testcases']['pickle::tests::bounded']['filter-match'] = {'status': 'eventually'}
        self.write()
        self.rejects('unknown filter')


if __name__ == '__main__':
    unittest.main()
