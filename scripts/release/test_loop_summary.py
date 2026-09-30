"""Loop summaries count every stress iteration and never turn silence into a pass.

The subprocess tests run the script the way the workflow does and assert its
exit status, because the status (not the report text) is what fails a job.
"""
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

from loop_summary import bound

SCRIPT = Path(__file__).resolve().with_name("loop_summary.py")
LANES = Path(__file__).resolve().with_name("v02-loop-lanes.json")
WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "v02-loops.yml"
CANDIDATE = "0123456789abcdef0123456789abcdef01234567"
TEST = "reliaburger::suite::log_export::lock"

# The shape nextest 0.9.145 writes for `--stress-count 3`: one testsuite per iteration.
JUNIT = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="3">
  <testsuite name="reliaburger::suite@stress-0" tests="1">
    <testcase name="log_export::lock" classname="reliaburger::suite"/>
  </testsuite>
  <testsuite name="reliaburger::suite@stress-1" tests="1">
    <testcase name="log_export::lock" classname="reliaburger::suite">
      <failure message="panicked"/>
    </testcase>
  </testsuite>
  <testsuite name="reliaburger::suite@stress-2" tests="1">
    <testcase name="log_export::lock" classname="reliaburger::suite"/>
  </testsuite>
</testsuites>
"""
PASSING = JUNIT.replace('<failure message="panicked"/>', "")
SKIPPED = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="3">
  <testsuite name="reliaburger::suite@stress-0" tests="1">
    <testcase name="log_export::lock" classname="reliaburger::suite"><skipped/></testcase>
  </testsuite>
  <testsuite name="reliaburger::suite@stress-1" tests="1">
    <testcase name="log_export::lock" classname="reliaburger::suite"><skipped/></testcase>
  </testsuite>
  <testsuite name="reliaburger::suite@stress-2" tests="1">
    <testcase name="log_export::lock" classname="reliaburger::suite"><skipped/></testcase>
  </testsuite>
</testsuites>
"""
THREE_ITERATIONS = " Summary [   0.143s] 3/3 stress run iterations: 3 passed\n"


class LoopSummaryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)

    def run_script(self, *arguments):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *arguments],
            capture_output=True,
            text=True,
            check=False,
        )

    def summarise(
        self,
        junit_text,
        log_text=THREE_ITERATIONS,
        lane="lane (os)",
        candidate=CANDIDATE,
        extra=(),
    ):
        """Summarise one lane into loops/<lane>/, returning (process, rows, report)."""
        lane_directory = self.directory / "loops" / re.sub(r"\W+", "-", lane)
        lane_directory.mkdir(parents=True, exist_ok=True)
        junit = lane_directory / "junit.xml"
        if junit_text is not None:
            junit.write_text(junit_text)
        log = lane_directory / "loop.log"
        if log_text is not None:
            log.write_text(log_text)
        output = lane_directory / "summary.md"
        process = self.run_script(
            "--lane", lane, "--junit", str(junit), "--log", str(log),
            "--candidate", candidate, "--output", str(output), *extra,
        )
        rows = json.loads(output.with_suffix(".json").read_text())
        return process, rows, output.read_text()

    def manifest(self, lanes):
        path = self.directory / "lanes.json"
        path.write_text(json.dumps({"lanes": lanes}))
        return path

    def combine(self, manifest, candidate=CANDIDATE):
        output = self.directory / "combined.md"
        process = self.run_script(
            "--combine", str(self.directory / "loops"), "--manifest", str(manifest),
            "--candidate", candidate, "--output", str(output),
        )
        return process, output.read_text()

    # --- one lane ---

    def test_a_passing_lane_exits_zero(self):
        process, rows, report = self.summarise(PASSING)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual(rows[0]["status"], "pass")
        self.assertEqual(rows[0]["runs"], 3)
        self.assertEqual(rows[0]["candidate"], CANDIDATE)
        self.assertIn("Verdict: **PASS**", report)

    def test_a_failing_lane_exits_non_zero(self):
        log = " Summary [   0.143s] 3/3 stress run iterations: 2 passed, 1 failed\n"
        process, rows, report = self.summarise(JUNIT, log)
        self.assertNotEqual(process.returncode, 0)
        self.assertEqual(rows[0]["runs"], 3)
        self.assertEqual(rows[0]["failures"], 1)
        self.assertEqual(rows[0]["iterations_reported"], 3)
        self.assertEqual(rows[0]["status"], "fail")
        self.assertIn("Verdict: **FAIL**", report)

    def test_missing_junit_is_incomplete_and_exits_non_zero(self):
        process, rows, report = self.summarise(None)
        self.assertNotEqual(process.returncode, 0)
        self.assertEqual(rows[0]["runs"], 0)
        self.assertEqual(rows[0]["status"], "incomplete")
        self.assertIn("Verdict: **FAIL**", report)

    def test_skipped_only_junit_is_not_a_run_and_exits_non_zero(self):
        process, rows, report = self.summarise(SKIPPED)
        self.assertNotEqual(process.returncode, 0)
        self.assertEqual(rows[0]["runs"], 0)
        self.assertEqual(rows[0]["skipped"], 3)
        self.assertEqual(rows[0]["status"], "skip")
        self.assertIn("Verdict: **FAIL**", report)

    def test_a_partly_skipped_test_is_incomplete(self):
        # Two iterations ran; the third was skipped.
        partly = JUNIT.replace('<failure message="panicked"/>', "<skipped/>")
        process, rows, _ = self.summarise(partly)
        self.assertNotEqual(process.returncode, 0)
        self.assertEqual(rows[0]["status"], "incomplete")

    def test_fewer_runs_than_expected_iterations_is_incomplete(self):
        process, rows, _ = self.summarise(PASSING, extra=("--expected-iterations", "200"))
        self.assertNotEqual(process.returncode, 0)
        self.assertEqual(rows[0]["status"], "incomplete")
        self.assertIn("200", rows[0]["note"])

    def test_a_missing_iteration_count_is_incomplete(self):
        process, rows, _ = self.summarise(PASSING, log_text=None)
        self.assertNotEqual(process.returncode, 0)
        self.assertEqual(rows[0]["status"], "incomplete")

    def test_rule_of_three_bound(self):
        self.assertEqual(bound(200, 0), "< 1.50%")
        self.assertEqual(bound(0, 0), "no runs")
        self.assertEqual(bound(10, 2), "FAILED (2/10)")

    # --- combined record ---

    def test_a_complete_passing_manifest_exits_zero(self):
        self.summarise(PASSING, lane="first (os)")
        self.summarise(PASSING, lane="second (os)")
        manifest = self.manifest([{"lane": "first (os)", "tests": [TEST]}, {"lane": "second (os)"}])
        process, text = self.combine(manifest)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn(f"| first (os) | `{TEST}` | pass | 3 | 0 | < 100.00% |", text)
        self.assertIn("Verdict: **PASS**", text)

    def test_a_failing_lane_fails_the_combined_record(self):
        self.summarise(PASSING, lane="first (os)")
        self.summarise(JUNIT, lane="second (os)")
        manifest = self.manifest([{"lane": "first (os)"}, {"lane": "second (os)"}])
        process, text = self.combine(manifest)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn("Verdict: **FAIL**", text)

    def test_a_missing_lane_fails_the_combined_record(self):
        self.summarise(PASSING, lane="first (os)")
        manifest = self.manifest([{"lane": "first (os)"}, {"lane": "second (os)"}])
        process, text = self.combine(manifest)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn("| second (os) | (lane missing) | incomplete |", text)
        self.assertIn("Verdict: **FAIL**", text)

    def test_a_missing_expected_test_fails_the_combined_record(self):
        self.summarise(PASSING, lane="first (os)")
        manifest = self.manifest([{"lane": "first (os)", "tests": [TEST, "reliaburger::suite::other"]}])
        process, text = self.combine(manifest)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn("| first (os) | `reliaburger::suite::other` | incomplete |", text)

    def test_an_unexpected_lane_fails_the_combined_record(self):
        self.summarise(PASSING, lane="first (os)")
        self.summarise(PASSING, lane="stray (os)")
        manifest = self.manifest([{"lane": "first (os)"}])
        process, text = self.combine(manifest)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn("not in the lane manifest", text)

    def test_mixed_candidate_identities_fail_the_combined_record(self):
        self.summarise(PASSING, lane="first (os)")
        self.summarise(PASSING, lane="second (os)", candidate="f" * 40)
        manifest = self.manifest([{"lane": "first (os)"}, {"lane": "second (os)"}])
        process, text = self.combine(manifest)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn("candidate", text)
        self.assertIn("Verdict: **FAIL**", text)

    # --- the real manifest ---

    def test_the_manifest_names_every_workflow_lane(self):
        """Each matrix value in v02-loops.yml appears in some manifest lane."""
        workflow = WORKFLOW.read_text()
        lanes = [entry["lane"] for entry in json.loads(LANES.read_text())["lanes"]]
        self.assertEqual(len(lanes), len(set(lanes)), "duplicate lanes in the manifest")
        values = set(re.findall(r"^\s+-? ?case: ([\w-]+)$", workflow, re.MULTILINE))
        for listed in re.findall(r"^\s+(?:target|binary): \[([^\]]+)\]$", workflow, re.MULTILINE):
            values.update(value.strip() for value in listed.split(","))
        self.assertTrue(values)
        for value in values:
            self.assertTrue(
                any(lane.startswith(f"{value} (") for lane in lanes),
                f"workflow lane {value} is missing from {LANES.name}",
            )
        for lane in lanes:
            self.assertIn(lane.split(" (")[0], values, f"{lane} is not a workflow lane")


if __name__ == "__main__":
    unittest.main()
