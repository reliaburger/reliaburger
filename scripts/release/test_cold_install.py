"""The V04 cold-install measurement (#288): qualify-cold-install.sh and the
workflow that runs it on fresh GitHub-hosted Linux x86_64 runners."""
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "qualify-cold-install.sh"
WORKFLOW = HERE.parent.parent / ".github" / "workflows" / "cold-install.yml"


class ColdInstallScriptTests(unittest.TestCase):
    def setUp(self):
        self.script = SCRIPT.read_text()

    def test_it_pipes_the_bootstrap_with_a_mode(self):
        # The issue's `bash -s -- --timings` predates #670: with no mode the
        # bootstrap only explains the choices and installs nothing.
        self.assertIn('curl -fsSL "$bootstrap" | bash -s -- --laptop --timings', self.script)
        self.assertIn("bootstrap=https://reliaburger.com/install.sh\n", self.script)

    def test_it_tears_down_and_compares_the_host_with_before(self):
        destroy = self.script.index('"$relish" local destroy --name laptop --yes')
        uninstall = self.script.index('"$relish" uninstall --yes')
        after = self.script.index('host_state > "$evidence/after.txt"')
        self.assertLess(destroy, uninstall)
        self.assertLess(uninstall, after)
        self.assertIn('diff -u "$evidence/before.txt" "$evidence/after.txt"', self.script)

    def test_it_refuses_a_host_that_is_not_linux(self):
        with tempfile.TemporaryDirectory() as tools:
            uname = Path(tools) / "uname"
            uname.write_text("#!/bin/sh\necho Darwin\n")
            uname.chmod(0o755)
            environment = dict(os.environ, PATH=f"{tools}:{os.environ['PATH']}")
            result = subprocess.run(["bash", str(SCRIPT)], env=environment,
                                    capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 1)
        self.assertIn("this measures a Linux host", result.stderr)

    def test_an_unknown_argument_is_refused(self):
        result = subprocess.run(["bash", str(SCRIPT), "--nope"],
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 1)
        self.assertIn("unknown argument: --nope", result.stderr)


class ColdInstallWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.workflow = WORKFLOW.read_text()

    def test_three_runs_each_on_its_own_fresh_runner(self):
        self.assertIn("        run: [1, 2, 3]\n", self.workflow)
        self.assertIn("      fail-fast: false\n", self.workflow)
        self.assertRegex(self.workflow, r"runs-on: ubuntu-\d")

    def test_the_evidence_is_kept_even_when_a_run_fails(self):
        upload = re.search(r"- name: Keep the evidence\n(?:        .*\n)+", self.workflow)
        self.assertIsNotNone(upload)
        self.assertIn("if: ${{ !cancelled() }}", upload.group(0))
        self.assertIn("if-no-files-found: error", upload.group(0))

    def test_it_runs_the_script_with_its_prerequisites(self):
        self.assertIn("scripts/release/qualify-cold-install.sh --prepare", self.workflow)
        self.assertIn("  workflow_dispatch:\n", self.workflow)


if __name__ == "__main__":
    unittest.main()
