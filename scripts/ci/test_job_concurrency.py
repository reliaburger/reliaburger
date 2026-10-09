"""Concurrency curves compare accepted counter deltas, preserving cold/warm windows."""
import importlib.util
import pathlib
import unittest
ROOT=pathlib.Path(__file__).resolve().parents[2]
spec=importlib.util.spec_from_file_location('curve',ROOT/'scripts/demo/sweep-job-concurrency.py')
curve=importlib.util.module_from_spec(spec)
spec.loader.exec_module(curve)
class JobConcurrency(unittest.TestCase):
    def test_curve_summary_preserves_caps_health_and_warm_deltas(self):
        rows=[dict(summary=dict(active_commands=3,other_in_flight_attempts=4,held=1000)),dict(observation=dict(cpu=dict(cpu_ticks=[1,2]))),dict(summary=dict(active_commands=5,other_in_flight_attempts=2,held=2000))]
        report=dict(unique_accepted_successes=12000,requested_seconds=60,window_pass=True,terminal_failures=0,application_failures=0,accepted_retries=0,initial_accepted_counts=[5000,0,0])
        result=curve.window_summary(report,rows)
        self.assertEqual(result['successes_per_second'],200)
        self.assertEqual(result['maximum_sampled_active_commands'],5)
        self.assertEqual(result['maximum_sampled_other_in_flight_attempts'],4)
        self.assertEqual(result['maximum_sampled_held_tasks'],2000)
        self.assertEqual(result['initial_accepted_counts'],[5000,0,0])
        self.assertTrue(result['healthy'])
        report['window_pass']=False
        self.assertFalse(curve.window_summary(report,rows)['healthy'])
    def test_fixed_rig_admission_bound_discloses_pool_limit(self):
        self.assertEqual(curve.admission_bound('process',64,25,3000),32)
        self.assertEqual(curve.admission_bound('shared-runc',64,100,3000),27)
        self.assertEqual(curve.admission_bound('runc',64,25,3000),64)
    def test_cancellation_uses_cli_success_status_then_positive_drain(self):
        from unittest.mock import patch
        with patch.object(curve.subprocess,'run') as run, patch.object(curve.qualification,'wait_cancelled',return_value=dict(done=True,held=0,active_commands=0)) as wait:
            result=curve.cancel_submission(['relish'],7)
        run.assert_called_once_with(['relish','batch','cancel','7'],check=True,timeout=30,stdout=curve.subprocess.DEVNULL)
        wait.assert_called_once_with(['relish'],7)
        self.assertTrue(result['done'])
    def test_unavailable_activity_is_not_zero_or_an_observed_concurrency(self):
        report=dict(unique_accepted_successes=1000,requested_seconds=60,window_pass=True)
        rows=[dict(summary=dict(active_commands=None,other_in_flight_attempts=None,held=1000)),dict(summary=dict(active_commands=3,other_in_flight_attempts=2,held=2000))]
        self.assertEqual(curve.window_summary(report,rows)['maximum_sampled_active_commands'],3)
        result=curve.window_summary(report,rows[:1])
        self.assertIsNone(result['maximum_sampled_active_commands'])
        self.assertIsNone(result['maximum_sampled_other_in_flight_attempts'])
