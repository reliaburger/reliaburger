"""Boot logs over the network (#2149): `reliaburger-netlog.service`.

A Wyse 3040 has no serial port, so the only way to watch it boot was its
monitor. `/usr/lib/reliaburger/netlog` points the kernel's netconsole at the
address in `reliaburger/netlog.conf` on the ESP (or `reliaburger.netlog=` on
the installer's command line), sends the kernel log so far once, and gives
the boot units StandardOutput=kmsg, so `relish netboot
--stream-logs` prints what the machine is doing.

The tests run the script with stub `ip` (one route, one neighbour),
`modprobe`, `mountpoint`, `mount`, `dmesg` (a short kernel log), `systemctl`
and `sleep`, a temporary directory for configfs and /run, and a UDP socket
on 127.0.0.1 as the operator's machine. `NETLOG_CMDLINE`, `NETLOG_CONF`,
`NETLOG_CONFIGFS` and `NETLOG_RUN` point the script at the stand-ins.

    python3 -m unittest discover -s image/tests -p 'test_netlog.py'
"""

import os
import socket
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
IMAGE = ROOT / "image"
NETLOG = IMAGE / "mkosi.extra" / "usr" / "lib" / "reliaburger" / "netlog"
INSTALLER_NETLOG = IMAGE / "mkosi.images" / "installer" / "mkosi.extra" / "usr" / "lib" / "reliaburger" / "netlog"
UNITS = IMAGE / "mkosi.extra" / "usr" / "lib" / "systemd" / "system"
INSTALLER = IMAGE / "mkosi.images" / "installer" / "mkosi.extra" / "usr" / "lib"
FORWARDED_UNITS = [
    "reliaburger-boot-check", "reliaburger", "reliaburger-seed", "reliaburger-time-wait",
    "reliaburger-install",
]

# What the drop-ins give the boot units while streaming.
OUTPUT_TO_KERNEL_LOG = "StandardOutput=kmsg"
# The fixed values systemd.exec(5) accepts for StandardOutput=.
SYSTEMD_OUTPUTS = {"inherit", "null", "tty", "journal", "kmsg", "journal+console", "kmsg+console"}

KERNEL_LOG = [
    "[    0.000000] Linux version 7.0.0-43-generic",
    "[    2.104000] r8169 0000:01:00.0 eth0: RTL8168h/8111h",
]

STUBS = {
    "ip": 'echo "ip $*" >> "$STUB_DIR/calls"\n'
          'case "$*" in\n'
          '  *"route get"*) [ -n "${NO_ROUTE:-}" ] && exit 2\n'
          '                 echo "$ROUTE" ;;\n'
          '  *"neigh show"*) [ -n "${NO_NEIGHBOUR:-}" ] && exit 0\n'
          '                 echo "127.0.0.1 dev lo lladdr 3c:22:fb:00:00:01 REACHABLE" ;;\n'
          "esac\n",
    "modprobe": 'echo "modprobe $*" >> "$STUB_DIR/calls"\n'
                '[ "$1" = netconsole ] && [ -n "${NO_NETCONSOLE:-}" ] && exit 1\n'
                "exit 0\n",
    "mountpoint": "exit 0\n",
    "mount": 'echo "mount $*" >> "$STUB_DIR/calls"\n',
    "dmesg": 'if [ "$#" -gt 0 ]; then echo "dmesg $*" >> "$STUB_DIR/calls"; exit 0; fi\n'
             + "".join(f"echo '{line}'\n" for line in KERNEL_LOG),
    "systemctl": 'echo "systemctl $*" >> "$STUB_DIR/calls"\n',
    "sleep": "exit 0\n",
}


class Run:
    """One run of the script: what it printed, called, wrote and sent."""

    def __init__(self, result, calls, configfs, run_dir, received):
        self.result = result
        self.calls = calls
        self.configfs = configfs
        self.run_dir = run_dir
        self.received = received

    def target(self, name):
        path = self.configfs / "netconsole" / "reliaburger" / name
        return path.read_text().strip() if path.exists() else None


class NetlogScript(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.stubs = self.dir / "stubs"
        self.stubs.mkdir()
        for name, body in STUBS.items():
            path = self.stubs / name
            path.write_text("#!/bin/sh\n" + body)
            path.chmod(path.stat().st_mode | stat.S_IXUSR)
        self.configfs = self.dir / "config"
        self.configfs.mkdir()
        self.run_dir = self.dir / "run"
        self.run_dir.mkdir()
        self.cmdline = self.dir / "cmdline"
        self.cmdline.write_text("console=tty0 console=ttyS0,115200\n")
        self.conf = self.dir / "esp" / "reliaburger" / "netlog.conf"
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.settimeout(0.2)
        self.port = self.listener.getsockname()[1]

    def tearDown(self):
        self.listener.close()
        self.tmp.cleanup()

    def write_conf(self, text):
        self.conf.parent.mkdir(parents=True, exist_ok=True)
        self.conf.write_text(text)

    def run_netlog(self, **extra_env):
        calls = self.stubs / "calls"
        calls.unlink(missing_ok=True)
        env = dict(
            os.environ,
            PATH=f"{self.stubs}:{os.environ['PATH']}",
            STUB_DIR=str(self.stubs),
            ROUTE="127.0.0.1 dev lo src 127.0.0.1 uid 0 \\    cache",
            NETLOG_CMDLINE=str(self.cmdline),
            NETLOG_CONF=str(self.conf),
            NETLOG_CONFIGFS=str(self.configfs),
            NETLOG_RUN=str(self.run_dir),
            **extra_env,
        )
        result = subprocess.run(["bash", str(NETLOG)], env=env, capture_output=True,
                                text=True, timeout=30)
        received = []
        while True:
            try:
                data, _ = self.listener.recvfrom(4096)
            except socket.timeout:
                break
            received.extend(data.decode().splitlines())
        return Run(result, calls.read_text().splitlines() if calls.exists() else [],
                   self.configfs, self.run_dir, received)

    def assert_not_streaming(self, run, why):
        self.assertEqual(run.result.returncode, 0, run.result.stdout + run.result.stderr)
        self.assertIn(why, run.result.stderr)
        self.assertNotIn("modprobe netconsole", run.calls)
        self.assertIsNone(run.target("enabled"))
        self.assertFalse((run.run_dir / "systemd").exists())
        self.assertEqual(run.received, [])

    def test_without_a_target_nothing_streams_and_the_boot_goes_on(self):
        run = self.run_netlog()
        self.assert_not_streaming(run, "no reliaburger.netlog= and no netlog.conf")

    def test_a_retarget_with_no_neighbour_answer_falls_back_to_broadcast(self):
        # `relish os boot-logs --to` restarts this unit with a new target
        # (#2255), and the configfs target outlives the restart. If the new
        # next hop doesn't answer ARP in time, the old hop's MAC must not
        # stay: netconsole would send every line to the wrong machine.
        self.write_conf(f"target=127.0.0.1:{self.port}\n")
        first = self.run_netlog()
        self.assertEqual(first.target("remote_mac"), "3c:22:fb:00:00:01")
        again = self.run_netlog(NO_NEIGHBOUR="1")
        self.assertEqual(again.result.returncode, 0, again.result.stdout + again.result.stderr)
        self.assertEqual(again.target("remote_mac"), "ff:ff:ff:ff:ff:ff")
        self.assertEqual(again.target("enabled"), "1")

    def test_a_target_in_netlog_conf_starts_netconsole_and_sends_the_kernel_log_once(self):
        self.write_conf(f"# relish netboot --stream-logs\ntarget=127.0.0.1:{self.port}\n")
        run = self.run_netlog()
        self.assertEqual(run.result.returncode, 0, run.result.stdout + run.result.stderr)
        self.assertIn(f"streaming kernel and boot logs from lo (127.0.0.1) to 127.0.0.1:{self.port}",
                      run.result.stderr)
        self.assertIn("modprobe netconsole", run.calls)
        self.assertEqual(run.target("dev_name"), "lo")
        self.assertEqual(run.target("local_ip"), "127.0.0.1")
        self.assertEqual(run.target("remote_ip"), "127.0.0.1")
        self.assertEqual(run.target("remote_port"), str(self.port))
        self.assertEqual(run.target("remote_mac"), "3c:22:fb:00:00:01")
        self.assertEqual(run.target("enabled"), "1")
        # Info lines reach netconsole, which is a console.
        self.assertIn("dmesg -n 7", run.calls)
        # The hello: relish names the machine by the MAC in it
        # (src/relish/netboot/netlog.rs, hello_mac), so its shape is fixed.
        # The stub interface may have no MAC to name.
        self.assertRegex(
            run.received[0],
            rf"^reliaburger-netlog: streaming from 127\.0\.0\.1 \(lo ([0-9a-f]{{2}}(:[0-9a-f]{{2}}){{5}})?\) "
            rf"to 127\.0\.0\.1:{self.port}$",
        )
        # The kernel log so far, between a header and a footer.
        self.assertEqual(run.received[1:], [
            "reliaburger-netlog: the kernel log so far, 2 lines",
            *KERNEL_LOG,
            "reliaburger-netlog: end of the kernel log so far",
        ])

        again = self.run_netlog()
        self.assertEqual(again.target("enabled"), "1")
        self.assertNotIn("reliaburger-netlog: the kernel log so far, 2 lines", again.received,
                         "the backlog goes once per boot")

    def test_the_boot_units_send_their_lines_to_the_kernel_log_only_while_streaming(self):
        self.write_conf(f"target=127.0.0.1:{self.port}\n")
        run = self.run_netlog()
        for unit in FORWARDED_UNITS:
            dropin = run.run_dir / "systemd" / "system" / f"{unit}.service.d" / "50-netlog.conf"
            # `kmsg` is the journal and the kernel log. systemd has no
            # `journal+kmsg`: it logs "Failed to parse output specifier",
            # ignores the line, and the unit's lines never leave the
            # machine. `kmsg+console` would print each line on the console
            # twice, once from journald and once from the kernel.
            self.assertIn(OUTPUT_TO_KERNEL_LOG, dropin.read_text().splitlines(), unit)
            self.assertIn(OUTPUT_TO_KERNEL_LOG.partition("=")[2], SYSTEMD_OUTPUTS)
        self.assertIn("systemctl daemon-reload", run.calls)
        again = self.run_netlog()
        self.assertNotIn("systemctl daemon-reload", again.calls, "nothing changed, nothing to reload")

    def test_the_port_is_6666_unless_netlog_conf_says_otherwise(self):
        self.write_conf("target=127.0.0.1\n")
        run = self.run_netlog()
        self.assertEqual(run.target("remote_port"), "6666")

    def test_comments_blank_lines_crlf_and_unknown_keys_are_fine(self):
        self.write_conf(f"# a comment\r\n\r\nlater=key\r\ntarget=127.0.0.1:{self.port}\r\n")
        run = self.run_netlog()
        self.assertEqual(run.target("remote_port"), str(self.port))
        self.assertEqual(run.target("enabled"), "1")

    def test_a_malformed_netlog_conf_is_ignored_and_says_why(self):
        cases = {
            "target=300.1.1.1:6666\n": "isn't <IPv4 address>[:<port>]",
            "target=10.0.0.1:0\n": "isn't <IPv4 address>[:<port>]",
            "target=10.0.0.1:70000\n": "isn't <IPv4 address>[:<port>]",
            "target=10.0.0.1:66a\n": "isn't <IPv4 address>[:<port>]",
            "target=10.0.0\n": "isn't <IPv4 address>[:<port>]",
            "target=lab.example:6666\n": "isn't <IPv4 address>[:<port>]",
            "target=\n": "it has no target=",
            "# only a comment\n": "it has no target=",
            "10.0.0.1:6666\n": "isn't key=value",
        }
        for text, why in cases.items():
            with self.subTest(conf=text):
                self.write_conf(text)
                run = self.run_netlog()
                self.assert_not_streaming(run, why)

    def test_the_command_line_wins_over_netlog_conf(self):
        # The installer gets its target from relish netboot this way.
        self.write_conf("target=10.9.9.9:1\n")
        self.cmdline.write_text(f"console=tty0 reliaburger.netlog=127.0.0.1:{self.port} quiet\n")
        run = self.run_netlog()
        self.assertEqual(run.target("remote_ip"), "127.0.0.1")
        self.assertEqual(run.target("remote_port"), str(self.port))

    def test_a_malformed_command_line_target_is_ignored(self):
        self.cmdline.write_text("reliaburger.netlog=nowhere\n")
        run = self.run_netlog()
        self.assert_not_streaming(run, "ignoring reliaburger.netlog=nowhere")

    def test_no_route_or_no_netconsole_means_no_streaming_and_no_failure(self):
        self.write_conf(f"target=127.0.0.1:{self.port}\n")
        self.assert_not_streaming(self.run_netlog(NO_ROUTE="1"), "no route to 127.0.0.1")
        run = self.run_netlog(NO_NETCONSOLE="1")
        self.assertEqual(run.result.returncode, 0)
        self.assertIn("can't load netconsole; not streaming", run.result.stderr)
        self.assertIsNone(run.target("enabled"))
        self.assertFalse((run.run_dir / "systemd").exists())


def unit(path):
    """A unit file as sections of option lists (systemd repeats options,
    which configparser refuses)."""
    sections = {}
    current = None
    for line in path.read_text().splitlines():
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


class NetlogUnits(unittest.TestCase):
    def test_the_installer_runs_the_same_script(self):
        self.assertEqual(NETLOG.read_bytes(), INSTALLER_NETLOG.read_bytes())
        for path in (NETLOG, INSTALLER_NETLOG):
            self.assertTrue(os.access(path, os.X_OK), path)

    def test_the_unit_starts_once_the_network_is_up_and_before_the_units_it_forwards(self):
        service = unit(UNITS / "reliaburger-netlog.service")
        self.assertIn("network-online.target", words(service["Unit"], "After"))
        for forwarded in ("reliaburger-seed.service", "reliaburger-time-wait.service",
                          "reliaburger.service"):
            self.assertIn(forwarded, words(service["Unit"], "Before"))
        # Not before the boot check: that would make the check wait for the
        # network after all (#2245). The check's lines still reach the
        # console and the journal, and its result goes out over netconsole
        # once streaming starts.
        self.assertNotIn("reliaburger-boot-check.service", words(service["Unit"], "Before"))
        self.assertEqual(service["Service"]["ExecStart"], ["-/usr/lib/reliaburger/netlog"])
        self.assertEqual(
            sorted(words(service["Service"], "UnsetEnvironment")),
            ["NETLOG_CMDLINE", "NETLOG_CONF", "NETLOG_CONFIGFS", "NETLOG_RUN"],
        )
        # Enabled from /usr, so a node installed from an older image gets it
        # with its next OS update.
        wants = UNITS / "multi-user.target.wants" / "reliaburger-netlog.service"
        self.assertTrue(wants.is_symlink())
        self.assertEqual(os.readlink(wants), "../reliaburger-netlog.service")

    def test_the_installer_streams_before_it_installs(self):
        service = unit(INSTALLER / "systemd" / "system" / "reliaburger-netlog.service")
        self.assertIn("network-online.target", words(service["Unit"], "After"))
        self.assertIn("reliaburger-install.service", words(service["Unit"], "Before"))
        preset = (INSTALLER / "systemd" / "system-preset" / "80-reliaburger-installer.preset").read_text()
        self.assertIn("enable reliaburger-netlog.service", preset.splitlines())
        # A preset enables a unit through its [Install] section: without
        # one, `enable` does nothing and the installer never streams.
        self.assertEqual(words(service.get("Install", {}), "WantedBy"), ["multi-user.target"])

    def test_the_boot_units_keep_the_console_when_not_streaming(self):
        # The CI boot tests read boot-check's lines on the serial console;
        # kmsg is only a runtime drop-in while streaming.
        service = unit(UNITS / "reliaburger-boot-check.service")
        self.assertEqual(service["Service"]["StandardOutput"], ["journal+console"])

    def test_journald_forwards_info_lines_to_the_kernel_log(self):
        for path in (IMAGE / "mkosi.extra" / "usr" / "lib" / "systemd" / "journald.conf.d" / "50-reliaburger.conf",
                     INSTALLER / "systemd" / "journald.conf.d" / "50-reliaburger.conf"):
            self.assertEqual(unit(path)["Journal"]["MaxLevelKMsg"], ["info"], path)

    def test_the_installer_writes_netlog_conf_from_its_command_line(self):
        install = (INSTALLER / "reliaburger" / "install").read_text()
        self.assertIn("netlog=$(arg reliaburger.netlog)", install)
        self.assertIn('"target=$target" >"$mnt/reliaburger/netlog.conf"', install)


if __name__ == "__main__":
    unittest.main()
