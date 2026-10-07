"""Incomplete or unsuccessful tests cannot qualify a CI contract."""
from pathlib import Path
import tempfile
import unittest

import ignored_owners


class EvidenceCompletionContract(unittest.TestCase):
    def check_case(self, child):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            (root / "test-linux.xml").write_text(
                "<testsuites><testsuite name='reliaburger::owned_runc'>"
                "<testcase name='runc_starts' classname='reliaburger::owned_runc'>"
                + child + "</testcase></testsuite></testsuites>")
            seen, _ = ignored_owners.executed(root)
            self.assertNotIn(
                ("reliaburger::owned_runc", "runc_starts"), seen,
                "a non-successful testcase was accepted as execution evidence")

    def test_skipped_junit_does_not_prove_success(self):
        self.check_case("<skipped/>")

    def test_failed_junit_does_not_prove_success(self):
        self.check_case("<failure message='regression'/>")

    def test_errored_junit_does_not_prove_success(self):
        self.check_case("<error message='killed'/>")

    def test_started_but_never_completed_libtest_does_not_prove_success(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            (root / "test-linux.xml").write_text(
                "<testsuites><testsuite name='reliaburger'>"
                "<testcase name='healthy_control'/></testsuite></testsuites>")
            (root / "oci_crash-0123abcd.log").write_text(
                "running 1 test\ntest bun_sigkill ... \n"
                "fixture running then killed\n")
            seen, _ = ignored_owners.executed(root)
            self.assertNotIn(
                ("reliaburger::oci_crash", "bun_sigkill"), seen,
                "a started test without completion was accepted as execution evidence")


if __name__ == "__main__":
    unittest.main(verbosity=2)
