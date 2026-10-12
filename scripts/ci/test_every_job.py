"""The final CI gate fails when any job it waited for failed, on any event."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import every_job

SCRIPT = Path(__file__).resolve().parent / 'every_job.py'


def needs(**results):
    found = {'changes': dict(result='success', outputs=dict(code='true', heavy='false', macos='false'))}
    for name in ('lint', 'portable', 'macos', 'acceptance'):
        found[name] = dict(result='success', outputs={})
    found['macos']['result'] = 'skipped'
    for name, result in results.items():
        found[name.replace('_', '-')] = dict(result=result, outputs={})
    return found


class EveryJob(unittest.TestCase):
    def test_finished_and_selectively_skipped_jobs_pass(self):
        self.assertEqual(every_job.problems(needs(), 'pull_request'), [])

    def test_a_failed_or_cancelled_job_fails_the_gate(self):
        for result in ['failure', 'cancelled', '', 'in_progress']:
            with self.subTest(result=result):
                self.assertEqual(every_job.problems(needs(lint=result), 'pull_request'),
                                 [f'lint finished as {result!r}'])

    def test_a_failed_job_outside_the_evidence_owners_still_fails(self):
        # lint, MSRV, the demo app, the audit and the benchmarks own no
        # evidence, but the merge queue requires this one check.
        problems = every_job.problems(needs(task_array_benchmarks='failure'), 'merge_group')
        self.assertEqual(problems, ["task-array-benchmarks finished as 'failure'"])

    def test_selection_must_succeed_before_any_skip_counts(self):
        found = needs()
        found['changes']['result'] = 'failure'
        self.assertIn("changes finished as 'failure'", every_job.problems(found, 'pull_request'))
        del found['changes']
        self.assertEqual(every_job.problems(found, 'pull_request'), ['select jobs did not run'])

    def test_only_a_pull_request_may_skip_the_code_jobs(self):
        found = needs()
        found['changes']['outputs']['code'] = 'false'
        self.assertEqual(every_job.problems(found, 'pull_request'), [])
        for event in ['push', 'merge_group', 'schedule', 'workflow_dispatch']:
            with self.subTest(event=event):
                self.assertEqual(every_job.problems(found, event),
                                 [f'a {event} run must select the code jobs'])

    def test_the_command_reads_the_workflow_needs_and_exits_nonzero_on_a_problem(self):
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / 'needs.json'
            path.write_text(json.dumps(needs()))
            ok = subprocess.run([sys.executable, str(SCRIPT), '--needs', str(path), '--event', 'merge_group'],
                                capture_output=True, text=True)
            self.assertEqual(ok.returncode, 0, ok.stderr)
            path.write_text(json.dumps(needs(portable='cancelled')))
            bad = subprocess.run([sys.executable, str(SCRIPT), '--needs', str(path), '--event', 'merge_group'],
                                 capture_output=True, text=True)
            self.assertEqual(bad.returncode, 1)
            self.assertIn("portable finished as 'cancelled'", bad.stderr)


if __name__ == '__main__':
    unittest.main()
