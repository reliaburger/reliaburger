"""The raw VM baseline must remain distinct from three accepted job tiers."""
import importlib.util
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]

class JobTiers(unittest.TestCase):
    def test_three_public_paths_and_an_extra_baseline_have_distinct_counts(self):
        spec = importlib.util.spec_from_file_location('tiers', ROOT / 'scripts/demo/record-job-tiers.py')
        tiers = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(tiers)
        baseline = dict(path='Bare',count=1000000,verified_successes=1000000,failures=0,elapsed_seconds=60)
        rows = [dict(runtime=mode, total=count, unique_accepted_successes=count,
                     accepted_elapsed_seconds=100, all_tasks_succeeded=True)
                for mode,count in [('runc',1000),('shared-runc',10000),('process',10000)]]
        report = tiers.build_report(baseline,rows,400)
        self.assertEqual(report['unique_accepted_successes'],21000)
        self.assertEqual(report['baseline']['verified_successes'],1000000)
        self.assertEqual(report['recording_elapsed_seconds'],400)
        self.assertFalse(report['qualified_100m_per_day'])
        self.assertTrue(report['all_tasks_succeeded'])
        with self.assertRaises(ValueError): tiers.build_report(baseline,list(reversed(rows)),400)
        rows[-1]['unique_accepted_successes']=9999
        self.assertFalse(tiers.build_report(baseline,rows,400)['all_tasks_succeeded'])

    def test_demo_uses_the_default_three_attempt_policy_and_reports_it(self):
        spec = importlib.util.spec_from_file_location('tiers_default', ROOT / 'scripts/demo/record-job-tiers.py')
        tiers = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(tiers)
        for runtime, count in tiers.TIERS:
            cohort = tiers.build_manifest(runtime, count, 'image@sha256:digest', '/bin/busybox')
            self.assertIn('max_attempts = 3\n', cohort)
            self.assertIn(f'count = {count}\n', cohort)
            self.assertIn(f'runtime = "{runtime}"\n', cohort)
            self.assertEqual('image = ' in cohort, runtime != 'process')

    def test_host_demo_volume_is_explicit_and_verified_against_the_recording(self):
        spec = importlib.util.spec_from_file_location('tiers_volume', ROOT / 'scripts/demo/record-job-tiers.py')
        tiers = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(tiers)
        baseline = dict(path='Bare', count=1000000, verified_successes=1000000, failures=0)
        rows = [dict(runtime=mode, total=count, unique_accepted_successes=count, all_tasks_succeeded=True)
                for mode, count in [('runc',1000), ('shared-runc',10000), ('process',500000)]]
        report = tiers.build_report(baseline, rows, 400, process_count=500000)
        self.assertTrue(report['all_tasks_succeeded'])
        self.assertEqual(report['unique_accepted_successes'], 511000)
        with self.assertRaises(ValueError): tiers.build_report(baseline, rows, 400)
        for invalid in [0, -1, True]:
            with self.assertRaises(ValueError):
                tiers.build_report(baseline, rows, 400, process_count=invalid)
