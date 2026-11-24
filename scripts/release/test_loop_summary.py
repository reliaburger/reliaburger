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
LOCAL_LANES = Path(__file__).resolve().with_name("v02-loop-lanes-local.json")
LOCAL_SCRIPT = Path(__file__).resolve().with_name("qualify-v02-loops-local.sh")
WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "v02-loops.yml"
CANDIDATE = "0123456789abcdef0123456789abcdef01234567"
TEST = "reliaburger::suite::log_export::lock"

# The shape nextest 0.9.151 writes for `--stress-count 3`: one testsuite per iteration.
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

    def test_a_coloured_log_still_reports_its_iteration_count(self):
        # The workflow sets CARGO_TERM_COLOR=always, so nextest wraps the
        # words and numbers in escape codes, as run 43183887759 logged them.
        coloured = (
            "\x1b[32;1m     Summary\x1b[0m [   0.143s] \x1b[1m3\x1b[0m/\x1b[1m3\x1b[0m"
            " stress run iterations: \x1b[1m3\x1b[0m \x1b[32;1mpassed\x1b[0m\n"
        )
        process, rows, report = self.summarise(PASSING, log_text=coloured)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertEqual(rows[0]["iterations_reported"], 3)
        self.assertEqual(rows[0]["status"], "pass")
        self.assertIn("Verdict: **PASS**", report)

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

    def test_a_lane_the_manifest_skips_is_reported_as_skipped_not_missing(self):
        self.summarise(PASSING, lane="first (os)")
        manifest = self.manifest([
            {"lane": "first (os)", "tests": [TEST]},
            {"lane": "first (x86-64)", "tests": [TEST], "skip": "no x86-64 host"},
        ])
        process, text = self.combine(manifest)
        self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
        self.assertIn("| first (x86-64) | (lane skipped) | skip | 0 | 0 | no runs | "
                      "skipped by the manifest: no x86-64 host |", text)
        self.assertNotIn("lane missing", text)
        self.assertIn("Verdict: **PASS**. 1 lane skipped by the manifest.", text)

    def test_a_record_of_only_skipped_lanes_fails(self):
        (self.directory / "loops").mkdir()
        manifest = self.manifest([{"lane": "first (x86-64)", "skip": "no x86-64 host"}])
        process, text = self.combine(manifest)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn("Verdict: **FAIL**", text)

    def test_a_skip_in_the_manifest_does_not_excuse_a_failing_lane_that_ran(self):
        self.summarise(PASSING, lane="first (os)")
        self.summarise(JUNIT, lane="first (x86-64)")
        manifest = self.manifest([
            {"lane": "first (os)"},
            {"lane": "first (x86-64)", "skip": "no x86-64 host"},
        ])
        process, text = self.combine(manifest)
        self.assertNotEqual(process.returncode, 0)
        self.assertIn("| first (x86-64) | `reliaburger::suite::log_export::lock` | fail |", text)
        self.assertIn("Verdict: **FAIL**", text)

    def test_a_skipped_test_inside_a_run_lane_still_fails_the_record(self):
        # Only a whole lane the manifest names as skipped is excused; a test
        # nextest skipped is no run.
        self.summarise(SKIPPED, lane="first (os)")
        manifest = self.manifest([{"lane": "first (os)"}, {"lane": "first (x86-64)", "skip": "no host"}])
        process, text = self.combine(manifest)
        self.assertNotEqual(process.returncode, 0)
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


class LocalLaneTests(unittest.TestCase):
    """qualify-v02-loops-local.sh runs the workflow's lanes on a Mac Studio.

    Its manifest names every workflow lane once per platform, under the
    Studio's labels, and skips the x86-64 ones it has no host for; its
    archive, lanes and filters match the workflow's word for word.
    """

    PLATFORMS = {
        "macos-15": "macos-studio",
        "ubuntu-24.04-arm": "linux-arm64-lima",
        "ubuntu-latest": "linux-x86-64",
    }

    def setUp(self):
        self.hosted = json.loads(LANES.read_text())["lanes"]
        self.local = json.loads(LOCAL_LANES.read_text())["lanes"]
        self.script = LOCAL_SCRIPT.read_text()
        self.workflow = WORKFLOW.read_text()

    @staticmethod
    def split(lane):
        case, platform = lane.split(" (", 1)
        return case, platform.rstrip(")")

    def script_lanes(self, variable):
        found = re.search(rf'^{variable}="([^"]+)"$', self.script, re.MULTILINE)
        self.assertIsNotNone(found, f"{variable} is missing from {LOCAL_SCRIPT.name}")
        return found.group(1).split()

    def test_the_local_manifest_has_no_duplicates(self):
        lanes = [entry["lane"] for entry in self.local]
        self.assertEqual(len(lanes), len(set(lanes)))

    def test_every_hosted_lane_has_a_local_lane_with_the_same_tests(self):
        local = {entry["lane"]: entry for entry in self.local}
        for entry in self.hosted:
            case, runner = self.split(entry["lane"])
            name = f"{case} ({self.PLATFORMS[runner]})"
            self.assertIn(name, local, f"{entry['lane']} has no local lane")
            self.assertEqual(local[name].get("tests", []), entry.get("tests", []), name)

    def test_the_local_lanes_that_run_are_the_ones_the_script_runs(self):
        expected = {f"{case} (macos-studio)" for case in self.script_lanes("macos_lanes")}
        expected |= {f"{case} (linux-arm64-lima)" for case in self.script_lanes("linux_lanes")}
        running = {entry["lane"] for entry in self.local if not entry.get("skip")}
        self.assertEqual(running, expected)

    def test_only_the_x86_64_lanes_are_skipped_and_each_says_why(self):
        for entry in self.local:
            _, platform = self.split(entry["lane"])
            if platform == "linux-x86-64":
                self.assertTrue(entry.get("skip"), f"{entry['lane']} needs a skip reason")
            else:
                self.assertNotIn("skip", entry, entry["lane"])

    def test_every_local_case_is_a_workflow_case(self):
        hosted = {self.split(entry["lane"])[0] for entry in self.hosted}
        for entry in self.local:
            self.assertIn(self.split(entry["lane"])[0], hosted, entry["lane"])

    def test_the_script_archives_the_workflow_test_binaries(self):
        archive = re.search(r"cargo nextest archive(.*?)\n\s+- uses", self.workflow, re.DOTALL)
        self.assertIsNotNone(archive)
        script = re.search(r"^archive_tests=\((.*)\)$", self.script, re.MULTILINE)
        self.assertIsNotNone(script)
        self.assertEqual(
            re.findall(r"--test (\w+)", script.group(1)),
            re.findall(r"--test (\w+)", archive.group(1)),
        )

    def test_the_script_uses_the_workflow_filters_for_counted_lanes(self):
        hosted = dict(re.findall(r"case: ([\w-]+)\n\s+filter: (.+)$", self.workflow, re.MULTILINE))
        self.assertTrue(hosted)
        block = self.script[self.script.index("lane_filter() {"):]
        block = block[:block.index("\n}\n")]
        local = dict(re.findall(r"^\s+([\w-]+)\) echo '([^']+)' ;;$", block, re.MULTILINE))
        self.assertEqual(local, hosted)


if __name__ == "__main__":
    unittest.main()
