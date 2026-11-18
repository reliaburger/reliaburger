"""The boot check says why bun isn't healthy, not just that it isn't.

`image/mkosi.extra/usr/lib/reliaburger/boot-check` waits for bun's health
endpoint. When bun never answers, the serial console is all a CI boot test
keeps, and bun's own stderr goes to the journal. So the failure path prints
the tail of `reliaburger.service`'s journal to the console, where
`seeded-pair.sh` and the other QEMU tests record it.

A counted boot, a new OS version on trial, gets 120 s and then reboots, so
three failed tries fall back in minutes. Any other boot gets 300 s and
doesn't reboot. `BOOT_CHECK_EFIVARS` points the check at a stand-in for
`/sys/firmware/efi/efivars`, where systemd-boot says a boot is counted.

The budget is counted in seconds since boot, not by the wall clock: a
machine with a flat RTC battery boots behind, and its first NTP sync jumps
the clock forward, which would otherwise use the budget up at once.
`BOOT_CHECK_UPTIME` points the check at a stand-in for `/proc/uptime`.

The test runs the script with stub `curl` (bun never answers, or answers
after `HEALTHY_AFTER` calls), `sleep` (each call moves the stand-in uptime
10 s on), `date` (each call jumps a day, like a clock syncing),
`systemctl` (which logs its arguments) and `journalctl`.

    python3 -m unittest discover -s image/tests -p 'test_boot_check.py'
"""

import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
# systemd-boot sets this EFI variable only on a boot whose entry has tries left.
COUNTED_VARIABLE = "LoaderBootCountPath-4a67b082-0a4c-41cf-b6c7-440b29bb8c4f"
BOOT_CHECK = ROOT / "image" / "mkosi.extra" / "usr" / "lib" / "reliaburger" / "boot-check"

STUBS = {
    # Refused, until the HEALTHY_AFTER-th call if that's set.
    "curl": 'n=$(( $(cat "$STUB_DIR/curls" 2>/dev/null || echo 0) + 1 ))\n'
            'echo "$n" > "$STUB_DIR/curls"\n'
            '[ -n "${HEALTHY_AFTER:-}" ] && [ "$n" -ge "$HEALTHY_AFTER" ] && exit 0\n'
            "exit 7\n",
    # A wall clock that jumps a day on every call: the check mustn't use it.
    "date": 'n=$(cat "$STUB_DIR/clock" 2>/dev/null || echo 1790000000)\n'
            'echo $((n + 86400)) > "$STUB_DIR/clock"\necho "$n"\n',
    # Each wait moves the uptime on 10 s.
    "sleep": 'read -r up _ < "$STUB_DIR/uptime"\n'
             'echo "$((${up%.*} + 10)).25 0.00" > "$STUB_DIR/uptime"\n',
    "systemctl": 'echo "$*" >> "$STUB_DIR/systemctl.log"\n',
    "journalctl": 'case "$*" in\n'
                  '  *reliaburger.service*) echo "bun: timed out waiting for replicated API credentials" ;;\n'
                  "esac\n",
}


@unittest.skipUnless(Path("/usr/lib/os-release").exists(), "boot-check reads /usr/lib/os-release")
class BootCheckTests(unittest.TestCase):
    def run_boot_check(self, counted=False, healthy_after=None):
        """Run the check; return its result and what it asked systemctl."""
        with tempfile.TemporaryDirectory() as stub_dir:
            for name, body in STUBS.items():
                path = Path(stub_dir) / name
                path.write_text("#!/bin/sh\n" + body)
                path.chmod(path.stat().st_mode | stat.S_IXUSR)
            efivars = Path(stub_dir) / "efivars"
            efivars.mkdir()
            if counted:
                (efivars / COUNTED_VARIABLE).write_bytes(b"")
            uptime = Path(stub_dir) / "uptime"
            uptime.write_text("42.17 80.03\n")
            env = dict(os.environ, PATH=f"{stub_dir}:{os.environ['PATH']}", STUB_DIR=stub_dir,
                       BOOT_CHECK_EFIVARS=str(efivars), BOOT_CHECK_UPTIME=str(uptime))
            if healthy_after is not None:
                env["HEALTHY_AFTER"] = str(healthy_after)
            result = subprocess.run(["sh", str(BOOT_CHECK)], env=env, capture_output=True,
                                    text=True, timeout=30)
            log = Path(stub_dir) / "systemctl.log"
            calls = log.read_text().splitlines() if log.exists() else []
            return result, calls

    def test_an_unhealthy_bun_fails_the_check_and_shows_buns_journal(self):
        result, _ = self.run_boot_check()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("reliaburger: bun not healthy after 300 s", result.stdout)
        self.assertIn(
            "reliaburger: bun: bun: timed out waiting for replicated API credentials",
            result.stdout,
        )

    def test_an_uncounted_boot_waits_five_minutes_and_does_not_reboot(self):
        result, calls = self.run_boot_check()
        self.assertIn("reliaburger: bun not healthy after 300 s", result.stdout)
        self.assertNotIn("reboot", calls)
        self.assertNotIn("counted boot failed", result.stdout)

    def test_a_counted_boot_waits_two_minutes_then_reboots(self):
        result, calls = self.run_boot_check(counted=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("reliaburger: bun not healthy after 120 s", result.stdout)
        self.assertIn("reliaburger: counted boot failed, rebooting", result.stdout)
        self.assertIn("reboot", calls)

    def test_a_clock_jumping_forward_leaves_a_counted_boot_its_budget(self):
        # bun answers on the third try, 20 s in, while the wall clock leaps
        # a day per reading. Timed by `date`, the 120 s would be gone at the
        # first leap and a good update would reboot and fall back.
        result, calls = self.run_boot_check(counted=True, healthy_after=5)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("reliaburger: bun healthy", result.stdout)
        self.assertNotIn("reboot", calls)


class BootCheckUnitTests(unittest.TestCase):
    """The unit, which runs everywhere (no os-release needed)."""

    def test_the_unit_drops_the_test_override_from_the_manager_s_environment(self):
        # BOOT_CHECK_EFIVARS is the tests' way to fake a counted boot. A
        # value inherited from the service manager (systemd.setenv=, or a
        # DefaultEnvironment= drop-in in /etc) would let a node decide its
        # own boots were never counted, so a bad update would never fall
        # back. A frozen BOOT_CHECK_UPTIME would keep the check waiting for
        # good. The unit unsets both before the script runs.
        unit = (ROOT / "image" / "mkosi.extra" / "usr" / "lib" / "systemd" / "system"
                / "reliaburger-boot-check.service").read_text()
        service = unit.split("[Service]", 1)[1].split("\n[", 1)[0]
        self.assertIn("\nUnsetEnvironment=BOOT_CHECK_EFIVARS BOOT_CHECK_UPTIME\n", service)


if __name__ == "__main__":
    unittest.main()
