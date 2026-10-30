"""Tests for image/lab/mac/, the Mac that routes an isolated Wyse lab.

The static checks run anywhere. The parse check runs the real dnsmasq
(`dnsmasq --test`) and skips when it's missing, as on a Mac without
Homebrew's. The appliance workflow installs it and sets
MAC_ROUTER_TOOLS=required, so in CI a missing dnsmasq fails instead of
skipping. pf has no checker on Linux, so pf-lab.conf gets static checks only.

    python3 -m unittest discover -s image/tests -p 'test_mac_router.py'
"""

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

MAC = Path(__file__).resolve().parents[1] / "lab/mac"
DNSMASQ = MAC / "dnsmasq.conf"
PF = MAC / "pf-lab.conf"

# Options that would make dnsmasq a boot server itself, racing relish.
# Option 60 is the one boot-related option it must send (below).
BOOT_KEYS = {
    "dhcp-boot", "pxe-service", "pxe-prompt", "enable-tftp", "tftp-root",
    "tftp-secure", "tftp-unique-root", "tftp-no-blocksize",
}
# DHCP options 66 (TFTP server), 67 (boot file), 43 (vendor, PXE menus).
BOOT_OPTIONS = re.compile(
    r"(^|,)\s*(option:)?(66|67|43|tftp-server|bootfile-name)\s*(,|$)")


def settings(path):
    """The active `key=value` (or bare `key`) lines of a dnsmasq-style file."""
    out = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, _, value = line.partition("=")
        out.append((key.strip(), value.strip()))
    return out


def filled_reservations(text):
    """dnsmasq.conf with every reservation filled in and uncommented, as the
    README tells the operator to do."""
    count = 0

    def fill(match):
        nonlocal count
        count += 1
        return f"dhcp-host=6c:4b:90:00:00:{count:02x}"

    return re.sub(r"^#dhcp-host=<[^>]+>", fill, text, flags=re.M), count


def dnsmasq():
    path = shutil.which("dnsmasq") or next(
        (p for p in ("/usr/sbin/dnsmasq", "/sbin/dnsmasq") if os.access(p, os.X_OK)), None)
    if path is None:
        if os.environ.get("MAC_ROUTER_TOOLS") == "required":
            raise AssertionError("dnsmasq is not installed and MAC_ROUTER_TOOLS=required")
        raise unittest.SkipTest("dnsmasq is not installed")
    return path


class DnsmasqConfig(unittest.TestCase):
    def test_sends_pxe_clients_to_port_4011_with_option_60(self):
        conf = settings(DNSMASQ)
        self.assertIn(("dhcp-vendorclass", "set:pxe,PXEClient"), conf)
        self.assertIn(("dhcp-option-force", "tag:pxe,60,PXEClient"), conf)

    def test_is_no_boot_server_itself(self):
        for key, value in settings(DNSMASQ):
            self.assertNotIn(key, BOOT_KEYS, f"{key}={value} makes dnsmasq a boot server")
            if key in ("dhcp-option", "dhcp-option-force"):
                self.assertIsNone(BOOT_OPTIONS.search(value), f"{key}={value} is a boot option")

    def test_serves_only_the_lab_adapter(self):
        conf = settings(DNSMASQ)
        self.assertEqual([v for k, v in conf if k == "interface"], ["en7"])
        self.assertIn(("bind-interfaces", ""), conf)
        self.assertFalse([k for k, v in conf if "en0" in v or k == "except-interface"])

    def test_runs_no_dns_server_and_routes_through_the_mac(self):
        conf = settings(DNSMASQ)
        self.assertIn(("port", "0"), conf)
        options = [v for k, v in conf if k == "dhcp-option"]
        self.assertIn("option:router,10.77.0.1", options)
        self.assertTrue(any(v.startswith("option:dns-server,") for v in options), options)

    def test_ten_wyse_reservations_below_the_pool(self):
        # Named as `relish machines claim --create --name wyse` names the
        # nodes (bare_metal::node_name): wyse-1, not wyse-01.
        lines = re.findall(
            r"^#dhcp-host=<wyse-(\d+)-mac>,10\.77\.0\.(\d+),wyse-(\d+),infinite$",
            DNSMASQ.read_text(), flags=re.M)
        self.assertEqual([n for n, _, _ in lines], [str(i) for i in range(1, 11)])
        self.assertEqual([int(a) for _, a, _ in lines], list(range(11, 21)))
        self.assertTrue(all(n == name for n, _, name in lines))
        self.assertIn(("dhcp-range", "10.77.0.100,10.77.0.199,255.255.255.0,12h"),
                      settings(DNSMASQ))

    def test_parses(self):
        self.dnsmasq_test(DNSMASQ.read_text())

    def test_parses_with_the_reservations_filled_in(self):
        text, count = filled_reservations(DNSMASQ.read_text())
        self.assertEqual(count, 10)
        self.dnsmasq_test(text)

    def dnsmasq_test(self, text):
        binary = dnsmasq()
        with tempfile.NamedTemporaryFile("w", suffix=".conf") as conf:
            conf.write(text)
            conf.flush()
            done = subprocess.run([binary, "--test", f"--conf-file={conf.name}"],
                                  capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("syntax check OK", done.stdout + done.stderr)


class PfConfig(unittest.TestCase):
    def test_nats_the_lab_out_through_the_wifi_only(self):
        text = PF.read_text()
        self.assertIn('wifi = "en0"', text)
        self.assertIn('lab = "10.77.0.0/24"', text)
        rules = [line.strip() for line in text.splitlines()
                 if line.strip() and not line.startswith("#") and "=" not in line]
        self.assertEqual(rules, ["nat on $wifi inet from $lab to any -> ($wifi)"])

    def test_loads_into_an_anchor_under_com_apple(self):
        # macOS's /etc/pf.conf has `nat-anchor "com.apple/*"`; an anchor
        # beneath it leaves the system's own rules alone.
        self.assertIn("pfctl -a com.apple/reliaburger-lab -f image/lab/mac/pf-lab.conf",
                      PF.read_text())


if __name__ == "__main__":
    unittest.main()
