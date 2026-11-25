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

# A Wyse 3040's broken-update fallback, measured in the 0.3.2 lab (#2245,
# 23 Nov 2026, image 2026.47.270 to the broken 2026.47.272): 698 s, with
# three tries of a 90 s check that waited for the network first.
WYSE_MEASURED_FALLBACK = 698
WYSE_MEASURED_TRIES = 3
# The reboot into the broken version, to that try's first journal line.
WYSE_REBOOT = 49
# Between one try's last journal line and the next try's first: the reboot
# and the firmware.
WYSE_FIRMWARE = 46
# Kernel start to the check's start, when the check waited for the network
# (wait-online alone was 23 s of it).
WYSE_KERNEL_TO_NETWORK_CHECK = 66
WYSE_MEASURED_CHECK = 90
# The fallback boot, first journal line to `bun healthy`.
WYSE_LAST_BOOT = 58


def counted_boot_budget():
    """Seconds after the kernel starts by which a counted boot must be
    healthy."""
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


def wyse_fallback(budget_since_boot, tries_left):
    """Seconds from the reboot into a broken version to bun healthy on the
    old one, on a Wyse 3040 whose broken version never gets healthy. Each
    try runs from its kernel start to the budget, then the firmware."""
    return (WYSE_REBOOT + tries_left * budget_since_boot
            + (tries_left - 1) * WYSE_FIRMWARE + WYSE_FIRMWARE + WYSE_LAST_BOOT)


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
        old_budget = WYSE_KERNEL_TO_NETWORK_CHECK + WYSE_MEASURED_CHECK
        self.assertAlmostEqual(wyse_fallback(old_budget, WYSE_MEASURED_TRIES),
                               WYSE_MEASURED_FALLBACK, delta=15)

    def test_two_tries_so_one_transient_failure_does_not_roll_back(self):
        # Three tries couldn't fit in 10 minutes on a Wyse and still leave a
        # slow healthy boot its time; the maintainer chose two (#2245,
        # 24 Nov 2026). One bad boot still doesn't roll a good update back.
        self.assertEqual(tries(), 2)

    def test_a_wyse_falls_back_within_the_limit(self):
        limit = int(run('echo "$FALLBACK_LIMIT_S"').stdout)
        seconds = wyse_fallback(counted_boot_budget(), tries())
        self.assertLessEqual(seconds, limit, f"a Wyse would take {seconds} s to fall back")


if __name__ == "__main__":
    unittest.main()
