"""Four equal windows count completions, not submitted volumes."""
import importlib.util
import pathlib
import unittest
ROOT = pathlib.Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('tiers', ROOT / 'scripts/demo/record-job-tiers.py')
tiers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tiers)
class JobTiers(unittest.TestCase):
    def test_equal_windows_validate_completion_health_and_daily_projection(self):
        baseline = dict(path='Bare', verified_successes=900000, failures=0,
                        requested_seconds=60, measurement_complete=True)
        rows = [dict(runtime=mode, unique_accepted_successes=count, terminal_failures=0,
                     accepted_retries=0, application_failures=0, cleanup_failed_submissions=[],
                     requested_seconds=60, measurement_complete=True)
                for mode, count in [('runc',200),('shared-runc',200000),('process',500000)]]
        report = tiers.build_report(baseline, rows, 250)
        self.assertTrue(report['all_windows_healthy'])
        self.assertEqual(report['unique_accepted_successes'],700200)
        self.assertEqual(report['tiers'][0]['extrapolated_runs_per_day'],288000)
        self.assertEqual(report['baseline']['extrapolated_runs_per_day'],1296000000)
        self.assertFalse(report['qualified_100m_per_day'])
        with self.assertRaises(ValueError): tiers.build_report(baseline, list(reversed(rows)),250)
        rows[-1]['requested_seconds']=59
        with self.assertRaises(ValueError): tiers.build_report(baseline,rows,250)
        rows[-1]['requested_seconds']=60
        rows[-1]['cleanup_failed_submissions']=[9]
        self.assertFalse(tiers.build_report(baseline,rows,250)['all_windows_healthy'])
        rows[-1]['cleanup_failed_submissions']=[]
        rows[0]['unique_accepted_successes']=0
        self.assertFalse(tiers.build_report(baseline,rows,250)['all_windows_healthy'])
    def test_human_counts_use_readable_units(self):
        for number, expected in [(900,'900'),(10000,'10k'),(200000,'200k'),(900000,'900k'),
                                 (1296000000,'1.3B'),(999999,'1M'),(0,'0')]:
            self.assertEqual(tiers.human_count(number),expected)
    def test_public_paths_share_the_declared_resource_profile_and_concurrency(self):
        for runtime in ['runc','shared-runc','process']:
            cohort=tiers.build_manifest(runtime,1000000,'image@sha256:digest','/bin/busybox')
            self.assertIn('max_attempts = 3\n',cohort)
            self.assertIn('per_node_concurrency = 27\n',cohort)
            self.assertIn('cpu = "100m-1000m"',cohort)
            self.assertIn('memory = "32Mi"',cohort)
            self.assertEqual('image = ' in cohort,runtime != 'process')
            self.assertIn('chunk_size = '+('1' if runtime=='runc' else '1000')+'\n',cohort)

    def test_selected_concurrency_and_request_apply_to_every_public_path(self):
        for runtime in tiers.RUNTIMES:
            manifest=tiers.build_manifest(runtime,16000000,'image@sha256:digest','/bin/busybox',concurrency=64,cpu_request=25)
            self.assertIn('per_node_concurrency = 64\n',manifest)
            self.assertIn('cpu = "25m-1000m"',manifest)
            self.assertIn('memory = "32Mi"',manifest)
        for concurrency, request in [(0,25),(257,25),(16,0),(16,1001)]:
            with self.assertRaises(ValueError):
                tiers.build_manifest('process',1000,'image@sha256:digest','/bin/busybox',concurrency=concurrency,cpu_request=request)

    def test_next_scenario_waits_for_idle_job_contexts_to_retire(self):
        import tempfile, json
        with tempfile.TemporaryDirectory() as folder:
            data=pathlib.Path(folder)
            records=data/'instances/runc/bundles/.intents/records'
            records.mkdir(parents=True)
            job=records/'job';job.mkdir()
            record=job/'intent.json'
            record.write_text(json.dumps(dict(instance_id='default__executor-job-0',phase=dict(state='running'))))
            self.assertFalse(tiers.owned_jobs_retired(data))
            record.write_text(json.dumps(dict(instance_id='default__executor-job-0',phase=dict(state='retired'))))
            self.assertTrue(tiers.owned_jobs_retired(data))
            service=records/'app';service.mkdir()
            (service/'intent.json').write_text(json.dumps(dict(instance_id='default__live-service-0',phase=dict(state='running'))))
            self.assertTrue(tiers.owned_jobs_retired(data))
            owners=data/'instances/process-owners/host';owners.mkdir(parents=True)
            owner=owners/'owner.json'
            owner.write_text(json.dumps(dict(command=['/data/host-executors/id/helper'],phase=dict(state='running'))))
            self.assertFalse(tiers.owned_jobs_retired(data))
            owner.write_text(json.dumps(dict(command=['/data/host-executors/id/helper'],phase=dict(state='retired'))))
            self.assertTrue(tiers.owned_jobs_retired(data))
