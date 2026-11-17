"""bun waits a little for the clock, and never for ever.

bun and its peers expect a node's wall clock to move forward across
restarts. A Wyse with a flat RTC battery, or a freshly imaged disk, boots
with its clock behind until systemd-timesyncd first syncs, so
`reliaburger-time-wait.service` gives the clock up to 30 s before bun starts.

systemd's own `systemd-time-wait-sync.service` has `TimeoutStartSec=infinity`
(systemd 259), so ordering bun after `time-sync.target` would leave a node on
a LAN with no NTP server without bun at all. These tests hold the units to
the bounded wait.

    python3 -m unittest discover -s image/tests -p 'test_time_wait.py'
"""

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
UNITS = ROOT / "image" / "mkosi.extra" / "usr" / "lib" / "systemd" / "system"
# boot-check gives a counted boot 120 s, and bun answers in 8 to 23 s.
COUNTED_BOOT_BUDGET = 120
SLOWEST_HEALTHY_BUN = 23


def unit(name):
    """A unit file as sections of option lists. Not configparser: systemd
    repeats options (After=, ExecStart=), which configparser refuses."""
    sections = {}
    current = None
    for line in (UNITS / name).read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("[") and line.endswith("]"):
            current = sections.setdefault(line[1:-1], {})
            continue
        key, _, value = line.partition("=")
        current.setdefault(key, []).append(value)
    return sections


def words(section, key):
    return [word for value in section.get(key, []) for word in value.split()]


class TimeWaitTests(unittest.TestCase):
    def test_bun_starts_after_the_bounded_wait(self):
        bun = unit("reliaburger.service")["Unit"]
        self.assertIn("reliaburger-time-wait.service", words(bun, "After"))
        self.assertIn("reliaburger-time-wait.service", words(bun, "Wants"))

    def test_bun_never_waits_on_the_unbounded_sync(self):
        bun = unit("reliaburger.service")["Unit"]
        for key in ("After", "Wants", "Requires", "BindsTo"):
            self.assertNotIn("time-sync.target", words(bun, key), key)
            self.assertNotIn("systemd-time-wait-sync.service", words(bun, key), key)
        # Nor may the wait itself stop bun when it gives up.
        self.assertNotIn("reliaburger-time-wait.service", words(bun, "Requires"))

    def test_the_wait_is_bounded_and_giving_up_is_not_a_failure(self):
        service = unit("reliaburger-time-wait.service")["Service"]
        (start,) = service["ExecStart"]
        self.assertTrue(start.startswith("-/usr/bin/timeout "), start)
        seconds = int(start.split()[1])
        self.assertTrue(start.endswith(" /usr/lib/systemd/systemd-time-wait-sync"), start)
        # A clock that never syncs must still leave a counted boot time to
        # see bun healthy, or every update on an offline LAN would fall back.
        self.assertLessEqual(seconds + SLOWEST_HEALTHY_BUN, COUNTED_BOOT_BUDGET // 2)
        self.assertEqual(service["Type"], ["oneshot"])
        # Once per boot: bun's own restarts don't wait again.
        self.assertEqual(service["RemainAfterExit"], ["yes"])

    def test_the_wait_starts_once_the_network_and_timesyncd_are_up(self):
        wait = unit("reliaburger-time-wait.service")["Unit"]
        self.assertIn("network-online.target", words(wait, "After"))
        self.assertIn("systemd-timesyncd.service", words(wait, "After"))
        self.assertIn("reliaburger.service", words(wait, "Before"))

    def test_timesyncd_is_enabled(self):
        preset = (ROOT / "image" / "mkosi.extra" / "usr" / "lib" / "systemd" / "system-preset"
                  / "80-reliaburger.preset").read_text().splitlines()
        self.assertIn("enable systemd-timesyncd.service", preset)


if __name__ == "__main__":
    unittest.main()
