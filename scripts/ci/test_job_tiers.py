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
                for mode,count in [('runc',1000),('shared-runc',10000),('process',1000000)]]
        report = tiers.build_report(baseline,rows,400)
        self.assertEqual(report['unique_accepted_successes'],1011000)
        self.assertEqual(report['baseline']['verified_successes'],1000000)
        self.assertEqual(report['recording_elapsed_seconds'],400)
        self.assertFalse(report['qualified_100m_per_day'])
        self.assertTrue(report['all_tasks_succeeded'])
        with self.assertRaises(ValueError): tiers.build_report(baseline,list(reversed(rows)),400)
        rows[-1]['unique_accepted_successes']=999999
        self.assertFalse(tiers.build_report(baseline,rows,400)['all_tasks_succeeded'])
