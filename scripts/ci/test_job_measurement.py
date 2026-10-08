"""Measurement tooling contracts; synthetic fixtures are not throughput evidence."""
import importlib.util
import pathlib
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('job_measurement', ROOT / 'scripts/demo/measure-jobs.py')
measurement = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measurement)

class JobMeasurement(unittest.TestCase):
    def summary(self, **changes):
        row = dict(batch_id=7,total=1000,succeeded=1000,failed=0,not_run=0,retried=0,done=True)
        row.update(changes)
        return row

    def test_accepted_identity_counts_and_counter_reset_are_checked(self):
        counts = measurement.AcceptedCounts(7)
        self.assertEqual(counts.observe(self.summary(succeeded=400,done=False)),(400,0,0))
        self.assertEqual(counts.observe(self.summary()),(1000,0,0))
        for bad in [self.summary(batch_id=8),self.summary(succeeded=999),self.summary(succeeded=1001),self.summary(total=1001),self.summary(succeeded=True)]:
            with self.assertRaises(ValueError): counts.observe(bad)

    def test_elapsed_includes_submission_and_cannot_qualify_a_short_burst(self):
        report = measurement.finish_report(self.summary(),started=10,accepted_at=29)
        self.assertEqual(report['accepted_elapsed_seconds'],19)
        self.assertAlmostEqual(report['accepted_successes_per_second'],1000/19)
        self.assertFalse(report['qualified_100m_per_day'])
        self.assertFalse(measurement.finish_report(self.summary(failed=1,succeeded=999),10,29)['all_tasks_succeeded'])

    def test_cast_keeps_actual_observation_time_without_compressing_pauses(self):
        with tempfile.TemporaryDirectory() as root:
            path = pathlib.Path(root)/'jobs.cast'
            cast = measurement.Recording(path,started=10)
            cast.emit('first',now=10.5)
            cast.emit('next',now=44.75)
            with self.assertRaises(ValueError): cast.emit('stale',now=44)
            cast.close()
            import json
            rows = list(map(json.loads,path.read_text().splitlines()))
            self.assertEqual(rows[0]['version'],2)
            self.assertEqual([r[0] for r in rows[1:]],[0.5,34.75])

    def test_storage_traversal_stops_at_its_entry_budget(self):
        with tempfile.TemporaryDirectory() as root:
            for index in range(10): (pathlib.Path(root)/str(index)).write_text('data')
            self.assertFalse(measurement.bounded_storage(root,max_files=4)['complete'])
            self.assertEqual(measurement.bounded_storage(root,max_files=4)['files_observed'],4)

    def test_display_does_not_replace_unknown_command_activity_with_callers(self):
        text = measurement.display(self.summary(nodes=[dict(counters=dict(running=256))]),5)
        self.assertIn('commands unknown',text)
        self.assertNotIn('commands 256',text)

if __name__ == '__main__': unittest.main()
