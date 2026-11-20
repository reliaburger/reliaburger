"""Tests for image/tests/fallback.sh, the Wyse exit test's limit on a broken
OS update's fallback (S5): back on the old version within 10 minutes of the
reboot into the broken one. `os-update.sh --fallback` fails past it, so CI
can't pass on a fallback the hardware run would fail.

    python3 -m unittest discover -s image/tests -p 'test_fallback_limit.py'
"""

import subprocess
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
FALLBACK = HERE / "fallback.sh"


def run(script):
    return subprocess.run(["bash", "-c", f'set -euo pipefail; . "{FALLBACK}"; {script}'],
                          capture_output=True, text=True)


class FallbackLimit(unittest.TestCase):
    def test_the_limit_is_the_exit_tests_10_minutes(self):
        self.assertEqual(run('echo "$FALLBACK_LIMIT_S"').stdout.strip(), "600")

    def test_a_fallback_within_the_limit_passes(self):
        for seconds in (0, 420, 600):
            with self.subTest(seconds=seconds):
                done = run(f"fallback_within_limit {seconds}")
                self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
                self.assertEqual(done.stdout, "")

    def test_a_fallback_past_the_limit_fails_and_says_why(self):
        done = run("fallback_within_limit 601")
        self.assertEqual(done.returncode, 1)
        self.assertIn("::error::the fallback took 601 s", done.stdout)
        self.assertIn("10 minutes", done.stdout)

    def test_os_update_fails_on_a_slow_fallback_rather_than_warning(self):
        script = (HERE / "os-update.sh").read_text()
        self.assertIn('. "$(dirname "$0")/fallback.sh"', script)
        self.assertIn("fallback_within_limit", script)
        self.assertNotIn("::warning::the fallback took", script)


if __name__ == "__main__":
    unittest.main()
