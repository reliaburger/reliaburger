"""Tests for image/lab/pi/, the Raspberry Pi that routes the Wyse lab.

The static checks run anywhere. The parse checks run the real tools
(`dnsmasq --test`, `nft -c`) and skip when a tool is missing, as on macOS.
The appliance workflow installs both and sets PI_ROUTER_TOOLS=required, so
in CI a missing tool fails instead of skipping.

    python3 -m unittest discover -s image/tests -p 'test_pi_router.py'
"""

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

PI = Path(__file__).resolve().parents[1] / "lab/pi"
DNSMASQ = PI / "dnsmasq.conf"
NFTABLES = PI / "nftables.conf"
CHRONY = PI / "chrony.conf"

# Options that make dnsmasq a boot server. Any of them would make
# `relish netboot` refuse to start (NetbootError::CompetingServer).
BOOT_KEYS = {
    "dhcp-boot", "pxe-service", "pxe-prompt", "enable-tftp", "tftp-root",
    "tftp-secure", "tftp-unique-root", "tftp-no-blocksize", "dhcp-match",
}
# DHCP options 66 (TFTP server), 67 (boot file), 43 (vendor, PXE menus),
# 60 (vendor class, "PXEClient") and 93-97 (PXE architecture, UUID).
BOOT_OPTIONS = re.compile(
    r"(^|,)\s*(option:)?(66|67|43|60|9[3-7]|tftp-server|bootfile-name|"
    r"vendor-class|client-arch|client-machine-id)\s*(,|$)")


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
    """The dnsmasq.conf template with every reservation filled in and
    uncommented, as the README tells the operator to do."""
    count = 0

    def fill(match):
        nonlocal count
        count += 1
        return f"dhcp-host=6c:4b:90:00:00:{count:02x}"

    return re.sub(r"^#dhcp-host=<[^>]+>", fill, text, flags=re.M), count


def required():
    return os.environ.get("PI_ROUTER_TOOLS") == "required"


def tool(name):
    path = shutil.which(name) or next(
        (p for p in (f"/usr/sbin/{name}", f"/sbin/{name}") if os.access(p, os.X_OK)), None)
    if path is None:
        if required():
            raise AssertionError(f"{name} is not installed and PI_ROUTER_TOOLS=required")
        raise unittest.SkipTest(f"{name} is not installed")
    return path


class DnsmasqConfig(unittest.TestCase):
    def test_no_boot_or_pxe_options(self):
        for key, value in settings(DNSMASQ):
            self.assertNotIn(key, BOOT_KEYS, f"{key}={value} makes the Pi a boot server")
            if key in ("dhcp-option", "dhcp-option-force"):
                self.assertIsNone(BOOT_OPTIONS.search(value),
                                  f"{key}={value} is a boot option")

    def test_serves_only_the_lab_switch(self):
        conf = settings(DNSMASQ)
        self.assertIn(("interface", "eth0"), conf)
        self.assertIn(("bind-dynamic", ""), conf)
        self.assertFalse([k for k, v in conf if "wlan0" in v or k == "except-interface"])
        self.assertEqual([v for k, v in conf if k == "interface"], ["eth0"])

    def test_hands_out_route_dns_and_ntp_pointing_at_the_pi(self):
        options = [v for k, v in settings(DNSMASQ) if k == "dhcp-option"]
        for option in ("router", "dns-server", "ntp-server"):
            self.assertIn(f"option:{option},10.77.0.1", options)

    def test_the_operator_mac_gets_no_default_route(self):
        options = [v for k, v in settings(DNSMASQ) if k == "dhcp-option"]
        self.assertIn("tag:operator,option:router", options)
        self.assertIn("#dhcp-host=<mac-adapter-mac>,10.77.0.2,mac,set:operator",
                      DNSMASQ.read_text())

    def test_ten_wyse_reservations_below_the_pool(self):
        lines = re.findall(r"^#dhcp-host=<wyse-(\d\d)-mac>,10\.77\.0\.(\d+),wyse-(\d\d)$",
                           DNSMASQ.read_text(), flags=re.M)
        self.assertEqual([n for n, _, _ in lines], [f"{i:02d}" for i in range(1, 11)])
        self.assertEqual([int(a) for _, a, _ in lines], list(range(11, 21)))
        self.assertTrue(all(n == name for n, _, name in lines))
        self.assertIn(("dhcp-range", "10.77.0.100,10.77.0.199,255.255.255.0,12h"),
                      settings(DNSMASQ))

    def test_parses(self):
        self.dnsmasq_test(DNSMASQ.read_text())

    def test_parses_with_the_reservations_filled_in(self):
        text, count = filled_reservations(DNSMASQ.read_text())
        self.assertEqual(count, 11)
        self.dnsmasq_test(text)

    def dnsmasq_test(self, text):
        dnsmasq = tool("dnsmasq")
        with tempfile.NamedTemporaryFile("w", suffix=".conf") as conf:
            conf.write(text)
            conf.flush()
            done = subprocess.run([dnsmasq, "--test", f"--conf-file={conf.name}"],
                                  capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("syntax check OK", done.stdout + done.stderr)


class Nftables(unittest.TestCase):
    def test_masquerades_out_of_the_uplink_only(self):
        text = NFTABLES.read_text()
        self.assertIn('define LAN = "eth0"', text)
        self.assertIn('define WAN = "wlan0"', text)
        self.assertIn("oifname $WAN masquerade", text)
        self.assertIn("iifname $LAN oifname $WAN accept", text)

    def test_drops_by_default(self):
        text = NFTABLES.read_text()
        for chain in ("input", "forward"):
            self.assertRegex(text, rf"chain {chain} {{\s*type filter hook {chain} "
                                   r"priority filter; policy drop;")

    def test_nothing_open_on_the_uplink(self):
        accepts = [line.strip() for line in NFTABLES.read_text().splitlines()
                   if "dport" in line]
        self.assertTrue(accepts)
        self.assertTrue(all(line.startswith("iifname $LAN") for line in accepts), accepts)

    def test_parses(self):
        nft = tool("nft")
        # A check still talks netlink, which needs CAP_NET_ADMIN; CI runners
        # have password-less sudo.
        check = [nft, "-c", "-f", str(NFTABLES)]
        done = subprocess.run(check, capture_output=True, text=True)
        if done.returncode != 0 and "Operation not permitted" in done.stderr:
            if shutil.which("sudo") is None:
                done = None
            else:
                done = subprocess.run(["sudo", "-n", *check], capture_output=True, text=True)
                if "password is required" in done.stderr:
                    done = None
            if done is None:
                if required():
                    self.fail("nft -c needs CAP_NET_ADMIN, and sudo -n is unavailable")
                raise unittest.SkipTest("nft -c needs CAP_NET_ADMIN, and sudo -n is unavailable")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)


class Chrony(unittest.TestCase):
    def test_serves_the_lab_only(self):
        lines = [line.strip() for line in CHRONY.read_text().splitlines()
                 if line.strip() and not line.startswith("#")]
        self.assertEqual([line for line in lines if line.startswith("allow")],
                         ["allow 10.77.0.0/24"])
        self.assertFalse(any(line.startswith("local") for line in lines))


if __name__ == "__main__":
    unittest.main()
