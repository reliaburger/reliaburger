"""The landing-page recording must execute the packaged Relish scenarios."""
import importlib.util
import pathlib
import unittest

ROOT=pathlib.Path(__file__).resolve().parents[2]

class JobBenchRecording(unittest.TestCase):
    def setUp(self):
        spec=importlib.util.spec_from_file_location('job_bench_recording',ROOT/'scripts/demo/record-job-bench.py')
        self.recording=importlib.util.module_from_spec(spec);spec.loader.exec_module(self.recording)

    def test_default_recording_executes_the_user_commands_without_fixture_flags(self):
        for scenario in self.recording.SCENARIOS:
            self.assertEqual(self.recording.command(scenario),['relish','bench','--scenario',scenario])
        self.assertEqual(self.recording.command('jobs-shared-containers',seconds=3600,concurrency=8),
                         ['relish','bench','--scenario','jobs-shared-containers','--seconds','3600','--concurrency','8'])

    def report(self,scenario):
        return dict(schema_version=1,scenario=scenario,requested_seconds=60,concurrency=27,
                    measurement_complete=True,interrupted=False,cleanup_verified=True,error=None,
                    unique_accepted_successes=1000,verified_successes=0,terminal_failures=0,
                    accepted_retries=0,post_cutoff_drained=0,executable_sha256=None,
                    elapsed_including_drain_seconds=61,active_submissions=[7],batch_ids=[7],
                    drain_proofs=[dict(batch_id=7,done=True,held=0,active_commands=0)],
                    receipt_chunk_size=1000,cpu_request_millicores=25)

    def test_published_counts_and_drain_proofs_come_from_the_cli_report(self):
        source=self.report('jobs-shared-containers')
        row=self.recording.published_row(source,dict(application_probes=61,application_failures=0))
        self.assertEqual(row['runtime'],'shared-runc')
        self.assertEqual(row['unique_accepted_successes'],source['unique_accepted_successes'])
        self.assertEqual(row['post_cutoff_drain_proofs'],source['drain_proofs'])
        self.assertEqual(row['successes_per_second'],1000/60)
        self.assertEqual(row['application_probes'],61)
        for key,value in [('cleanup_verified',False),('measurement_complete',False),('interrupted',True),('error','API lost')]:
            bad=dict(source);bad[key]=value
            with self.assertRaises(ValueError):self.recording.published_row(bad,{})

    def test_raw_exits_remain_distinct_from_cluster_accepted_successes(self):
        source=self.report('jobs-vm-baseline');source.update(verified_successes=900000,
            unique_accepted_successes=0,executable_sha256='f'*64,active_submissions=[],batch_ids=[],drain_proofs=[])
        row=self.recording.published_row(source,{})
        self.assertEqual(row['path'],'Bare')
        self.assertEqual(row['verified_successes'],900000)
        self.assertEqual(row['failures'],0)
        self.assertEqual(row['elapsed_seconds'],60)
        self.assertEqual(row['executable_sha256'],'f'*64)
