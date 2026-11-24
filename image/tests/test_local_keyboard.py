"""Tests for image/tests/local_keyboard.py: the appliance's keyboard reaches
only its status screen on tty1 (#2279). No gettys on tty2-tty6, Ctrl-Alt-Del
does nothing, and magic SysRq is limited to sync, remount read-only and
reboot. CI runs the same script on each built image's /usr partition
(.github/workflows/appliance.yml).

    python3 -m unittest discover -s image/tests -p 'test_local_keyboard.py'
"""

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
IMAGE = HERE.parent
CHECK = HERE / "local_keyboard.py"


def run(*args):
    return subprocess.run([sys.executable, str(CHECK), *map(str, args)],
                          capture_output=True, text=True)


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def link(path, target):
    path.parent.mkdir(parents=True, exist_ok=True)
    os.symlink(target, path)


class Config(unittest.TestCase):
    def test_the_repository_s_image_passes(self):
        done = run("config", IMAGE)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)


class BuiltUsr(unittest.TestCase):
    """A /usr partition as fsck.erofs extracts it, laid out as the real
    image is: the drop-ins under lib/, /etc's factory copy under
    share/factory/etc."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.usr = Path(self.tmp.name)
        self.etc = self.usr / "share" / "factory" / "etc"
        units = self.usr / "lib" / "systemd" / "system"
        write(units / "reliaburger-console.service", "[Service]\n")
        link(self.etc / "systemd" / "system" / "multi-user.target.wants" / "reliaburger-console.service",
             "/usr/lib/systemd/system/reliaburger-console.service")
        link(self.etc / "systemd" / "system" / "getty.target.wants" / "getty@tty1.service",
             "/usr/lib/systemd/system/getty@.service")
        write(self.usr / "lib" / "systemd" / "logind.conf.d" / "50-reliaburger-keyboard.conf",
              "[Login]\nNAutoVTs=0\nReserveVT=0\n")
        write(self.usr / "lib" / "systemd" / "system.conf.d" / "50-reliaburger-keyboard.conf",
              "[Manager]\nCtrlAltDelBurstAction=none\n")
        # What Ubuntu ships: systemd's default and procps' file, both earlier.
        write(self.usr / "lib" / "sysctl.d" / "50-default.conf", "kernel.sysrq = 16\n")
        write(self.etc / "sysctl.d" / "10-magic-sysrq.conf", "kernel.sysrq = 176\n")
        write(self.usr / "lib" / "sysctl.d" / "60-reliaburger-sysrq.conf", "kernel.sysrq = 176\n")
        for unit in ("ctrl-alt-del.target", "getty-static.service"):
            link(self.etc / "systemd" / "system" / unit, "/dev/null")

    def tearDown(self):
        self.tmp.cleanup()

    def check(self):
        return run("usr", self.usr)

    def assert_fails_with(self, *words):
        done = self.check()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        for word in words:
            self.assertIn(word, done.stdout)

    def test_a_hardened_image_passes(self):
        done = self.check()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("kernel.sysrq = 176", done.stdout)

    def test_autovts_left_on_fail(self):
        (self.usr / "lib" / "systemd" / "logind.conf.d" / "50-reliaburger-keyboard.conf").unlink()
        self.assert_fails_with("NAutoVTs", "ReserveVT")

    def test_a_later_logind_drop_in_that_turns_autovts_back_on_fails(self):
        write(self.etc / "systemd" / "logind.conf.d" / "90-vendor.conf", "[Login]\nNAutoVTs=6\n")
        self.assert_fails_with("NAutoVTs is 6")

    def test_an_etc_drop_in_with_the_same_name_overrides_ours(self):
        write(self.etc / "systemd" / "logind.conf.d" / "50-reliaburger-keyboard.conf", "[Login]\n")
        self.assert_fails_with("NAutoVTs", "ReserveVT")

    def test_ctrl_alt_del_unmasked_fails(self):
        (self.etc / "systemd" / "system" / "ctrl-alt-del.target").unlink()
        self.assert_fails_with("ctrl-alt-del.target")

    def test_a_mask_pointing_anywhere_but_dev_null_fails(self):
        (self.etc / "systemd" / "system" / "getty-static.service").unlink()
        link(self.etc / "systemd" / "system" / "getty-static.service",
             "/usr/lib/systemd/system/getty-static.service")
        self.assert_fails_with("getty-static.service")

    def test_the_ctrl_alt_del_burst_left_at_its_default_fails(self):
        (self.usr / "lib" / "systemd" / "system.conf.d" / "50-reliaburger-keyboard.conf").unlink()
        self.assert_fails_with("CtrlAltDelBurstAction")

    def test_a_later_sysctl_file_that_reopens_sysrq_fails(self):
        write(self.etc / "sysctl.d" / "99-sysctl.conf", "kernel/sysrq=1\n")
        self.assert_fails_with("kernel.sysrq is 1", "99-sysctl.conf")

    def test_a_sysctl_symlink_into_etc_is_followed(self):
        # procps' 99-sysctl.conf is a symlink to /etc/sysctl.conf.
        write(self.etc / "sysctl.conf", "kernel.sysrq = 438\n")
        link(self.etc / "sysctl.d" / "99-sysctl.conf", "/etc/sysctl.conf")
        self.assert_fails_with("kernel.sysrq is 438")

    def test_no_sysrq_setting_at_all_fails(self):
        for name in ("50-default.conf", "60-reliaburger-sysrq.conf"):
            (self.usr / "lib" / "sysctl.d" / name).unlink()
        (self.etc / "sysctl.d" / "10-magic-sysrq.conf").unlink()
        self.assert_fails_with("the kernel default")

    def test_a_getty_enabled_on_tty2_fails(self):
        link(self.etc / "systemd" / "system" / "getty.target.wants" / "getty@tty2.service",
             "/usr/lib/systemd/system/getty@.service")
        self.assert_fails_with("getty@tty2.service")

    def test_an_autovt_enabled_on_tty6_fails(self):
        link(self.etc / "systemd" / "system" / "multi-user.target.wants" / "autovt@tty6.service",
             "/usr/lib/systemd/system/autovt@.service")
        self.assert_fails_with("autovt@tty6.service")

    def test_the_console_not_enabled_fails(self):
        (self.etc / "systemd" / "system" / "multi-user.target.wants" / "reliaburger-console.service").unlink()
        self.assert_fails_with("reliaburger-console.service isn't enabled")

    def test_usage_without_a_mode(self):
        self.assertEqual(run().returncode, 2)


if __name__ == "__main__":
    unittest.main()
