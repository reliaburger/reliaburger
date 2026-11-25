"""The boot check says why bun isn't healthy, not just that it isn't.

`image/mkosi.extra/usr/lib/reliaburger/boot-check` waits for bun's health
endpoint. When bun never answers, the serial console is all a CI boot test
keeps, and bun's own stderr goes to the journal. So the failure path prints
the tail of `reliaburger.service`'s journal to the console, where
`seeded-pair.sh` and the other QEMU tests record it.

A counted boot, a new OS version on trial, must have bun healthy and an IPv4
address within 150 s of the kernel starting, or it reboots, so two failed
tries fall back in minutes even on a Wyse 3040 (#2143, #2245). The 150 s run
from boot, not from when the check starts: the check no longer waits for the
network, so a version whose network never comes up still uses up its tries.
Any other boot gets 300 s from the check's start, needs no address, and
doesn't reboot. `BOOT_CHECK_EFIVARS` points the check at a stand-in for
`/sys/firmware/efi/efivars`, where systemd-boot says a boot is counted.

The budget is counted in seconds since boot, not by the wall clock: a
machine with a flat RTC battery boots behind, and its first NTP sync jumps
the clock forward, which would otherwise use the budget up at once.
`BOOT_CHECK_UPTIME` points the check at a stand-in for `/proc/uptime`.

The test runs the script with stub `curl` (bun never answers, or answers
after `HEALTHY_AFTER` calls), `ip` (one global address, or none with
`NO_ADDRESS`), `sleep` (each call moves the stand-in uptime 10 s on), `date`
(each call jumps a day, like a clock syncing),
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
    # One global IPv4 address, unless NO_ADDRESS is set.
    "ip": '[ -n "${NO_ADDRESS:-}" ] && exit 0\n'
          'echo "2: enp1s0    inet 10.77.0.11/24 brd 10.77.0.255 scope global dynamic enp1s0"\n',
    "systemctl": 'echo "$*" >> "$STUB_DIR/systemctl.log"\n',
    "journalctl": 'case "$*" in\n'
                  '  *reliaburger.service*) echo "bun: timed out waiting for replicated API credentials" ;;\n'
                  "esac\n",
}


@unittest.skipUnless(Path("/usr/lib/os-release").exists(), "boot-check reads /usr/lib/os-release")
class BootCheckTests(unittest.TestCase):
    def run_boot_check(self, counted=False, healthy_after=None, uptime_at_start=42,
                       no_address=False, streaming=False, kmsg_out=None):
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
            uptime.write_text(f"{uptime_at_start}.17 80.03\n")
            run_dir = Path(stub_dir) / "run"
            if streaming:
                dropin = run_dir / "systemd" / "system" / "reliaburger-boot-check.service.d"
                dropin.mkdir(parents=True)
                (dropin / "50-netlog.conf").write_text("[Service]\nStandardOutput=kmsg\n")
            kmsg = Path(stub_dir) / "kmsg"
            kmsg.write_text("")
            env = dict(os.environ, PATH=f"{stub_dir}:{os.environ['PATH']}", STUB_DIR=stub_dir,
                       BOOT_CHECK_EFIVARS=str(efivars), BOOT_CHECK_UPTIME=str(uptime),
                       BOOT_CHECK_RUN=str(run_dir), BOOT_CHECK_KMSG=str(kmsg))
            if healthy_after is not None:
                env["HEALTHY_AFTER"] = str(healthy_after)
            if no_address:
                env["NO_ADDRESS"] = "1"
            result = subprocess.run(["sh", str(BOOT_CHECK)], env=env, capture_output=True,
                                    text=True, timeout=30)
            log = Path(stub_dir) / "systemctl.log"
            calls = log.read_text().splitlines() if log.exists() else []
            if kmsg_out is not None:
                kmsg_out.extend(kmsg.read_text().splitlines())
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

    def test_a_counted_boot_gives_up_150_seconds_after_boot_then_reboots(self):
        result, calls = self.run_boot_check(counted=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("reliaburger: bun not healthy after 150 s", result.stdout)
        self.assertIn("reliaburger: counted boot failed, rebooting", result.stdout)
        self.assertIn("reboot", calls)

    def test_a_clock_jumping_forward_leaves_a_counted_boot_its_budget(self):
        # bun answers on the third try, 20 s in, while the wall clock leaps
        # a day per reading. Timed by `date`, the budget would be gone at the
        # first leap and a good update would reboot and fall back.
        result, calls = self.run_boot_check(counted=True, healthy_after=5)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("reliaburger: bun healthy", result.stdout)
        self.assertNotIn("reboot", calls)

    def test_a_slow_but_healthy_wyse_keeps_its_update(self):
        # The slowest healthy start we know, counted from the kernel: a Wyse
        # 3040 reaches the network about 66 s in (#2245's lab), then a LAN
        # with no NTP server spends the whole 30 s clock wait and bun takes
        # up to 38 s, so about 134 s. The check starts at 42 s of uptime;
        # each failed poll is two curls and a 10 s sleep, so the 21st curl
        # answers at 142 s.
        result, calls = self.run_boot_check(counted=True, healthy_after=21)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("reliaburger: bun healthy", result.stdout)
        self.assertNotIn("reboot", calls)

    def test_a_bun_still_silent_150_seconds_after_boot_fails_the_try(self):
        # Answering at 152 s of uptime is too late for a counted boot.
        result, calls = self.run_boot_check(counted=True, healthy_after=23)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("reliaburger: bun not healthy after 150 s", result.stdout)
        self.assertIn("reboot", calls)

    def test_a_counted_boot_with_no_network_address_fails_the_try(self):
        # bun answers on the node itself, but a version that breaks the
        # network would leave the node stranded off it: on trial, that's a
        # failed boot, so it falls back (#2245).
        result, calls = self.run_boot_check(counted=True, healthy_after=1, no_address=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("reliaburger: no IPv4 address after 150 s", result.stdout)
        self.assertNotIn("reliaburger: bun healthy", result.stdout)
        self.assertIn("reboot", calls)

    def test_an_uncounted_boot_with_no_address_still_reports_bun_healthy(self):
        # Nothing falls back from an ordinary boot, and an unplugged cable
        # isn't the OS's fault.
        result, calls = self.run_boot_check(healthy_after=1, no_address=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("reliaburger: bun healthy", result.stdout)
        self.assertNotIn("reboot", calls)

    def test_a_streaming_boot_sends_the_check_s_lines_through_the_kernel_log(self):
        # The check starts before reliaburger-netlog, so the drop-in netlog
        # writes can't reach it; it writes to /dev/kmsg itself once the
        # drop-in is there, and not to the console too, or the screen shows
        # each line twice.
        kmsg = []
        result, calls = self.run_boot_check(counted=True, streaming=True, kmsg_out=kmsg)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertNotIn("bun not healthy", result.stdout)
        self.assertIn("<6>reliaburger-boot-check: reliaburger: bun not healthy after 150 s on OS",
                      "\n".join(kmsg))
        self.assertIn("<6>reliaburger-boot-check: reliaburger: counted boot failed, rebooting",
                      "\n".join(kmsg))
        self.assertIn("reboot", calls)

    def test_a_boot_that_isn_t_streaming_keeps_the_check_on_the_console(self):
        kmsg = []
        result, _ = self.run_boot_check(counted=True, healthy_after=1, kmsg_out=kmsg)
        self.assertIn("reliaburger: bun healthy", result.stdout)
        self.assertEqual(kmsg, [])

    def test_a_check_that_starts_past_the_budget_still_looks_once(self):
        # A boot slow to reach the check (a long fsck, say) gets one look at
        # bun before the budget, already spent, fails it.
        result, calls = self.run_boot_check(counted=True, healthy_after=1, uptime_at_start=160)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("reliaburger: bun healthy", result.stdout)
        result, calls = self.run_boot_check(counted=True, uptime_at_start=160)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("reboot", calls)


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
        self.assertIn("\nUnsetEnvironment=BOOT_CHECK_EFIVARS BOOT_CHECK_UPTIME BOOT_CHECK_RUN "
                      "BOOT_CHECK_KMSG\n", service)

    def test_the_check_never_waits_for_the_network(self):
        # Ordered after the network, a version whose network never came up
        # never started the check, so it never failed its tries or fell
        # back; and every try waited for the network first, which made the
        # Wyse's fallback 11 min 38 s (#2245, #2143). bun still waits for
        # the network itself; the check only has to be running.
        unit = (ROOT / "image" / "mkosi.extra" / "usr" / "lib" / "systemd" / "system"
                / "reliaburger-boot-check.service").read_text()
        head = unit.split("[Service]", 1)[0]
        ordering = [line.split("=", 1)[1].split() for line in head.splitlines()
                    if line.startswith(("After=", "Requires=", "BindsTo="))]
        ordered_after = [name for names in ordering for name in names]
        for network in ("network-online.target", "network.target",
                        "systemd-networkd-wait-online.service", "reliaburger-netlog.service",
                        "reliaburger-time-wait.service"):
            self.assertNotIn(network, ordered_after)


if __name__ == "__main__":
    unittest.main()
