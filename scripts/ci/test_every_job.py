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
        # lint, MSRV, CI policy and the benchmarks own no
        # evidence, but the merge queue requires this one check.
        problems = every_job.problems(needs(task_array_benchmarks='failure'), 'merge_group')
        self.assertEqual(problems, ["task-array-benchmarks finished as 'failure'"])

    def test_a_merge_group_passes_with_portable_macos_skipped(self):
        # select-jobs.sh leaves portable macOS out of every merge group (24
        # November 2026); a skipped job counts only because selection skipped it.
        found = needs(macos='skipped')
        found['changes']['outputs'].update(heavy='true', macos='false')
        self.assertEqual(every_job.problems(found, 'merge_group'), [])
        self.assertEqual(every_job.problems(needs(macos='failure'), 'merge_group'),
                         ["macos finished as 'failure'"])

    def test_selection_must_succeed_before_any_skip_counts(self):
        found = needs()
        found['changes']['result'] = 'failure'
        self.assertIn("changes finished as 'failure'", every_job.problems(found, 'pull_request'))
        del found['changes']
        self.assertEqual(every_job.problems(found, 'pull_request'), ['select jobs did not run'])

    def test_only_a_pull_request_or_a_merge_group_may_skip_the_code_jobs(self):
        # Either one changing only documentation (select-jobs.sh).
        found = needs()
        found['changes']['outputs']['code'] = 'false'
        self.assertEqual(every_job.problems(found, 'pull_request'), [])
        self.assertEqual(every_job.problems(found, 'merge_group'), [])
        for event in ['push', 'schedule', 'workflow_dispatch']:
            with self.subTest(event=event):
                self.assertEqual(every_job.problems(found, event),
                                 [f'a {event} run must select the code jobs'])

    def test_a_push_the_merge_queue_already_tested_may_skip_the_code_jobs(self):
        found = needs()
        found['changes']['outputs'].update(code='false', tested_by='44820647332')
        self.assertEqual(every_job.problems(found, 'push'), [])
        for event in ['schedule', 'workflow_dispatch']:
            with self.subTest(event=event):
                self.assertEqual(every_job.problems(found, event), [f'a {event} run must select the code jobs'])

    def test_a_push_naming_no_queue_run_must_still_select_the_code_jobs(self):
        for tested_by in ['', 'yes', '12a']:
            with self.subTest(tested_by=tested_by):
                found = needs()
                found['changes']['outputs'].update(code='false', tested_by=tested_by)
                self.assertEqual(every_job.problems(found, 'push'), ['a push run must select the code jobs'])

    def test_a_skipped_push_says_which_queue_run_it_rests_on(self):
        found = needs()
        found['changes']['outputs'].update(code='false', tested_by='44820647332')
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / 'needs.json'
            path.write_text(json.dumps(found))
            done = subprocess.run([sys.executable, str(SCRIPT), '--needs', str(path), '--event', 'push'],
                                  capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('::notice::', done.stdout)
        self.assertIn('44820647332', done.stdout)

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
