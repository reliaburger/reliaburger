"""An unclaimed machine's screen shows its claim key (#2161).

Claiming over the LAN asks the operator to compare each machine's claim key
with its screen. The screen is `reliaburger-console.service` on tty1. It used
to be ordered after `reliaburger-seed.service`, which on an unclaimed machine
stays activating until the claim (`TimeoutStartSec=infinity`), so the console
started only once there was no key left to show. On the Wyse lab the screens
sat on the boot's last kernel messages instead.

These tests keep the console off anything that waits for a claim, and keep it
on tty1 in place of a login. `image/tests/claimed-pair.sh` checks the screen
itself, in CI: it reads tty1 of each unclaimed VM and looks for the key.

    python3 -m unittest discover -s image/tests -p 'test_console_unit.py'
"""

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
UNITS = ROOT / "image" / "mkosi.extra" / "usr" / "lib" / "systemd" / "system"
PRESET = ROOT / "image" / "mkosi.extra" / "usr" / "lib" / "systemd" / "system-preset" / "80-reliaburger.preset"
CONSOLE = "reliaburger-console.service"
# Ordering and requirement keys: a unit listed under any of them holds the
# console back until it has started.
WAITS_ON = ("After", "Requires", "Requisite", "BindsTo")


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


def waits_on(name):
    """The units `name` starts after, or can't start without."""
    section = unit(name).get("Unit", {})
    return {word for key in WAITS_ON for word in words(section, key)}


def waits_for_a_claim(name):
    """Whether one of our units can stay activating until the machine is
    claimed: a oneshot that never times out."""
    if not (UNITS / name).exists():
        return False
    service = unit(name).get("Service", {})
    return service.get("Type") == ["oneshot"] and service.get("TimeoutStartSec") == ["infinity"]


class ConsoleUnitTests(unittest.TestCase):
    def test_the_seed_unit_waits_for_a_claim(self):
        # The premise of the next test: if this changes, so may the console's
        # ordering.
        self.assertTrue(waits_for_a_claim("reliaburger-seed.service"))

    def test_the_console_waits_on_nothing_that_waits_for_a_claim(self):
        for name in sorted(waits_on(CONSOLE)):
            self.assertFalse(waits_for_a_claim(name), f"{CONSOLE} waits on {name}")
            # Nor through one more step: bun itself starts after the seed.
            if (UNITS / name).exists():
                for further in sorted(waits_on(name)):
                    self.assertFalse(waits_for_a_claim(further),
                                     f"{CONSOLE} waits on {name}, which waits on {further}")

    def test_the_console_is_not_ordered_after_the_seed_or_bun(self):
        self.assertNotIn("reliaburger-seed.service", waits_on(CONSOLE))
        self.assertNotIn("reliaburger.service", waits_on(CONSOLE))

    def test_the_console_replaces_the_login_on_tty1(self):
        console = unit(CONSOLE)["Unit"]
        self.assertIn("getty@tty1.service", words(console, "Conflicts"))

    def test_the_console_starts_on_every_boot_and_comes_back(self):
        self.assertIn(f"enable {CONSOLE}", PRESET.read_text().splitlines())
        self.assertIn("multi-user.target", words(unit(CONSOLE)["Install"], "WantedBy"))
        self.assertEqual(unit(CONSOLE)["Service"]["Restart"], ["always"])


if __name__ == "__main__":
    unittest.main()
