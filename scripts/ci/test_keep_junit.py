"""keep-junit.sh gives every suite its own JUnit report, or fails the step."""
from pathlib import Path
import os
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parent / "keep-junit.sh"
REPORT = '<?xml version="1.0"?><testsuites name="nextest-run"></testsuites>\n'


class KeepJunitTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(["git", "-c", "user.name=f", "-c", "user.email=f@example.com",
                        "commit", "-q", "--allow-empty", "-m", "base"],
                       cwd=self.root, check=True)
        self.commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=self.root, check=True,
                                     capture_output=True, text=True).stdout.strip()
        self.nextest = self.root / "target" / "nextest" / "ci"
        self.nextest.mkdir(parents=True)
        self.kept = self.root / "target" / "junit"

    def tearDown(self):
        self.tmp.cleanup()

    def keep(self, suite, *command):
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("GITHUB_", "RUNNER_"))}
        return subprocess.run(["bash", str(SCRIPT), suite, *command], cwd=self.root, env=env,
                              capture_output=True, text=True)

    def test_a_report_is_moved_aside_under_its_suite_name_with_its_provenance(self):
        (self.nextest / "junit.xml").write_text(REPORT)
        result = self.keep("test-cluster", "make", "test-cluster", "NEXTEST=archived")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.kept / "test-cluster.xml").read_text(), REPORT)
        self.assertFalse((self.nextest / "junit.xml").exists())
        meta = (self.kept / "test-cluster.meta.txt").read_text()
        self.assertIn("suite: test-cluster\n", meta)
        self.assertIn("command: make test-cluster NEXTEST=archived\n", meta)
        self.assertIn(f"commit: {self.commit}\n", meta)
        self.assertIn("host: ", meta)

    def test_a_suite_that_wrote_no_report_fails(self):
        result = self.keep("test-slow", "make", "test-slow")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("test-slow wrote no JUnit report", result.stderr)

    def test_an_empty_report_fails(self):
        (self.nextest / "junit.xml").write_text("")
        result = self.keep("test-slow", "make", "test-slow")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("test-slow wrote no JUnit report", result.stderr)

    def test_a_later_suite_cannot_pass_off_an_earlier_suites_report(self):
        # The acceptance job runs three suites against one nextest path.
        (self.nextest / "junit.xml").write_text(REPORT)
        self.assertEqual(self.keep("test-slow", "make", "test-slow").returncode, 0)
        result = self.keep("test-upgrade-node", "make", "test-upgrade-node")
        self.assertNotEqual(result.returncode, 0)

    def test_two_suites_with_one_name_fail(self):
        (self.nextest / "junit.xml").write_text(REPORT)
        self.assertEqual(self.keep("test-slow", "make", "test-slow").returncode, 0)
        (self.nextest / "junit.xml").write_text(REPORT)
        result = self.keep("test-slow", "make", "test-slow")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already kept", result.stderr)


if __name__ == "__main__":
    unittest.main()
