"""A completed measurement is distinct from daily-rate qualification."""
import importlib.util
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('job_qualification', ROOT / 'scripts/demo/qualify-jobs.py')
qualification = importlib.util.module_from_spec(spec)
spec.loader.exec_module(qualification)

class JobQualification(unittest.TestCase):
    def test_one_hour_records_a_missed_target_without_claiming_daily_qualification(self):
        report = qualification.finish_report(elapsed=3601, requested_seconds=3600,
            successes=10000, failures=2, retries=0, probes=100, unavailable=0,
            completed=3, active=[4], headroom=1.2)
        self.assertTrue(report['measurement_complete'])
        self.assertFalse(report['throughput_pass'])
        self.assertEqual(report['terminal_failures'], 2)
        self.assertEqual(report['active_submissions'], [4])
        self.assertAlmostEqual(report['successes_per_second'], 10000/3601)
        self.assertFalse(report['qualification_pass'])

    def test_daily_rate_requires_the_full_duration_and_headroom(self):
        report = qualification.finish_report(elapsed=86400, requested_seconds=86400,
            successes=120000000, failures=0, retries=0, probes=100, unavailable=0,
            completed=100, active=[], headroom=1.2)
        self.assertTrue(report['throughput_pass'])
        self.assertFalse(report['qualification_pass'])
        report = qualification.finish_report(elapsed=3590, requested_seconds=3600,
            successes=1000000, failures=0, retries=0, probes=100, unavailable=0,
            completed=3, active=[], headroom=1.2)
        self.assertFalse(report['measurement_complete'])

    def test_existing_submission_measurement_excludes_earlier_successes(self):
        report = qualification.existing_window_report(
            elapsed=3600, requested_seconds=3600,
            initial=(1200, 1, 4), final=(10000, 3, 9),
            probes=3600, unavailable=0, batch_id=7, done=False, headroom=1.2)
        self.assertEqual(report['unique_accepted_successes'],8800)
        self.assertEqual(report['terminal_failures'],2)
        self.assertEqual(report['accepted_retries'],5)
        self.assertEqual(report['active_submissions'],[7])
        self.assertEqual(report['measurement_ownership'],'read-only; submission remains owned by its caller')
        self.assertFalse(report['throughput_pass'])
