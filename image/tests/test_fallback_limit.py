"""Tests for image/tests/fallback.sh, the Wyse exit test's limit on a broken
OS update's fallback (S5): back on the old version within 10 minutes of the
reboot into the broken one. `os-update.sh --fallback` fails past it, so CI
can't pass on a fallback the hardware run would fail.

    python3 -m unittest discover -s image/tests -p 'test_fallback_limit.py'
"""

import re
import subprocess
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
FALLBACK = HERE / "fallback.sh"
LIB = HERE.parent / "mkosi.extra" / "usr" / "lib" / "reliaburger"

# A Wyse 3040's broken-update fallback, measured on the lab's first hardware
# run (#2143, 20 Nov 2026, image 2026.47.226), which took 628 s with
# boot-check's old 120 s budget.
WYSE_MEASURED_FALLBACK = 628
WYSE_MEASURED_BUDGET = 120
# The reboot into the broken version, to that try's first journal line.
WYSE_REBOOT = 41
# Each try's boot before the check, its reboot gap and the firmware.
WYSE_BOOT_PER_TRY = 65
# The fallback boot, first journal line to `bun healthy`.
WYSE_LAST_BOOT = 38


def counted_boot_budget():
    match = re.search(r"^if counted; then limit=(\d+); else limit=\d+; fi$",
                      (LIB / "boot-check").read_text(), re.MULTILINE)
    if match is None:
        raise AssertionError("boot-check no longer sets its limits on one line")
    return int(match.group(1))


def tries():
    match = re.search(r"^TriesLeft=(\d+)$",
                      (LIB / "sysupdate.d" / "10-uki.transfer").read_text(), re.MULTILINE)
    if match is None:
        raise AssertionError("10-uki.transfer no longer sets TriesLeft=")
    return int(match.group(1))


def wyse_fallback(budget):
    """Seconds from the reboot into a broken version to bun healthy on the
    old one, on a Wyse 3040 whose broken bun never answers."""
    return WYSE_REBOOT + tries() * (WYSE_BOOT_PER_TRY + budget) + WYSE_LAST_BOOT


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



class WyseFallback(unittest.TestCase):
    """The S5 limit on the hardware it's for. CI's VMs boot in 9 s, so only
    this arithmetic, from the Wyse's own timings, stands in for the lab
    between hardware runs."""

    def test_the_model_matches_the_wyse_run_it_came_from(self):
        self.assertAlmostEqual(wyse_fallback(WYSE_MEASURED_BUDGET), WYSE_MEASURED_FALLBACK,
                               delta=10)

    def test_three_tries_so_one_transient_failure_does_not_roll_back(self):
        self.assertEqual(tries(), 3)

    def test_a_wyse_falls_back_within_the_limit(self):
        limit = int(run('echo "$FALLBACK_LIMIT_S"').stdout)
        seconds = wyse_fallback(counted_boot_budget())
        self.assertLessEqual(seconds, limit, f"a Wyse would take {seconds} s to fall back")


if __name__ == "__main__":
    unittest.main()
