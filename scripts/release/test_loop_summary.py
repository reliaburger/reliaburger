"""Loop summaries count every stress iteration and never turn silence into a pass."""
import json
from pathlib import Path
import tempfile
import unittest

from loop_summary import bound, main

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


class LoopSummaryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)

    def summarise(self, junit_text, log_text=""):
        junit = self.directory / "junit.xml"
        if junit_text is not None:
            junit.write_text(junit_text)
        log = self.directory / "loop.log"
        log.write_text(log_text)
        output = self.directory / "lane" / "summary.md"
        main(["--lane", "lane (os)", "--junit", str(junit), "--log", str(log), "--output", str(output)])
        return output, json.loads(output.with_suffix(".json").read_text())

    def test_every_iteration_counts_as_a_run_and_a_failure_fails_the_lane(self):
        log = " Summary [   0.143s] 3/3 stress run iterations: 2 passed, 1 failed\n"
        output, rows = self.summarise(JUNIT, log)
        self.assertEqual(rows[0]["runs"], 3)
        self.assertEqual(rows[0]["failures"], 1)
        self.assertEqual(rows[0]["iterations_reported"], 3)
        self.assertIn("Verdict: **FAIL**", output.read_text())

    def test_missing_junit_is_a_failed_lane_not_an_empty_pass(self):
        output, rows = self.summarise(None)
        self.assertEqual(rows[0]["runs"], 0)
        self.assertIn("Verdict: **FAIL**", output.read_text())

    def test_rule_of_three_bound(self):
        self.assertEqual(bound(200, 0), "< 1.50%")
        self.assertEqual(bound(0, 0), "no runs")
        self.assertEqual(bound(10, 2), "FAILED (2/10)")

    def test_combined_table_reads_every_lane(self):
        passing = JUNIT.replace('<failure message="panicked"/>', "")
        self.summarise(passing)
        combined = self.directory / "combined.md"
        main(["--combine", str(self.directory), "--output", str(combined)])
        text = combined.read_text()
        self.assertIn("| lane (os) | `reliaburger::suite::log_export::lock` | 3 | 0 | < 100.00% |", text)
        self.assertIn("Verdict: **PASS**", text)


if __name__ == "__main__":
    unittest.main()
