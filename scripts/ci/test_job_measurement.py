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

    def test_manifest_indexes_use_each_cohort_identity_and_local_index_range(self):
        summary = self.summary(kind='manifest',cohorts=[dict(batch_id=8,total=10,runtime='shared-runc'),dict(batch_id=9,total=4,runtime='runc')])
        self.assertEqual(measurement.indexed_queries(summary),[(8,0),(8,5),(8,9),(9,0),(9,2),(9,3)])
        self.assertEqual(measurement.indexed_queries(self.summary(total=1)),[(7,0)])
        with self.assertRaises(ValueError): measurement.indexed_queries(self.summary(kind='manifest',cohorts=[]))
        with self.assertRaises(ValueError): measurement.indexed_queries(self.summary(kind='manifest',cohorts=[dict(batch_id=8,total=1)]*33))

    def test_homogeneous_manifest_displays_the_actual_container_mode(self):
        summary = self.summary(kind='manifest',cohorts=[dict(runtime='shared-runc')])
        self.assertIn('runtime shared-runc;',measurement.display(summary,5))
        summary['cohorts'].append(dict(runtime='runc'))
        self.assertIn('runtime mixed;',measurement.display(summary,5))

    def test_explicit_runtime_is_reported_for_host_and_shared_jobs(self):
        for mode in ['process', 'shared-runc', 'runc']:
            self.assertIn('runtime ' + mode + ';', measurement.display(self.summary(runtime=mode), 5))

    def test_selected_results_must_match_the_requested_identity_and_index(self):
        result=dict(batch_id=8,rows=[dict(index=5,succeeded=True,attempts=1)])
        measurement.check_indexed_result(result,8,5)
        for bad in [dict(result,batch_id=9),dict(result,rows=[]),dict(result,rows=[dict(index=4,succeeded=True)]),dict(result,rows=[dict(index=5,succeeded=False)]),dict(result,unreachable=['worker'])]:
            with self.assertRaises(ValueError): measurement.check_indexed_result(bad,8,5)

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

    def test_storage_churn_reports_an_incomplete_observation_without_aborting(self):
        from contextlib import nullcontext
        from unittest.mock import Mock, patch
        entry=Mock()
        entry.stat.side_effect=FileNotFoundError('atomic write renamed away')
        with patch.object(measurement.os,'scandir',return_value=nullcontext([entry])):
            row=measurement.bounded_storage('/task-owned-fixture')
        self.assertFalse(row['complete'])
        self.assertEqual(row['raced_entries'],1)
        with patch.object(measurement.os,'scandir',side_effect=FileNotFoundError('retired directory')):
            self.assertFalse(measurement.bounded_storage('/task-owned-fixture')['complete'])

    def test_display_does_not_replace_unknown_command_activity_with_callers(self):
        text = measurement.display(self.summary(nodes=[dict(counters=dict(running=256))]),5)
        self.assertIn('commands unknown',text)
        self.assertNotIn('commands 256',text)

if __name__ == '__main__': unittest.main()

class ProcessObservation(unittest.TestCase):
    def test_rss_samples_refuse_reused_or_missing_process_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            process = root / '42'
            process.mkdir()
            def stat(start):
                fields = ['S'] + ['0'] * 18 + [str(start)]
                (process / 'stat').write_text('42 (name with spaces) ' + ' '.join(fields))
            stat(100)
            (process / 'status').write_text('Name:\tfixture\nVmRSS:\t17 kB\nVmHWM:\t23 kB\n')
            self.assertEqual(measurement.process_observation(42, 100, root),
                             dict(pid=42, start_ticks=100, rss_bytes=17408, peak_rss_bytes=23552, complete=True))
            stat(101)
            self.assertFalse(measurement.process_observation(42, 100, root)['complete'])
            (process / 'status').unlink()
            self.assertFalse(measurement.process_observation(42, 101, root)['complete'])


class MatchedTierResources(unittest.TestCase):
    def test_all_public_tiers_use_the_same_concurrency_and_resource_profile(self):
        spec = importlib.util.spec_from_file_location('tier_recording', ROOT / 'scripts/demo/record-job-tiers.py')
        recording = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(recording)
        for runtime in ['runc', 'shared-runc', 'process']:
            source = recording.build_manifest(runtime, 1000, 'fixture@sha256:' + 'a'*64, '/bin/busybox')
            self.assertRegex(source, r'(?m)^per_node_concurrency = 27$')
            self.assertRegex(source, r'(?m)^cpu = "100m-1000m"$')
            self.assertRegex(source, r'(?m)^memory = "32Mi"$')
