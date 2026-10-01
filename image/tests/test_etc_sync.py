"""Tests for image/mkosi.extra/usr/lib/reliaburger/etc-sync, the boot-time
/etc update. They run the real script against temporary directories, so they
need Linux (GNU coreutils and bash 4): the appliance workflow runs them.

    python3 -m unittest discover -s image/tests
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "mkosi.extra/usr/lib/reliaburger/etc-sync"


class EtcSync(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.factory = root / "factory"
        self.etc = root / "etc"
        self.state = root / "state" / "etc-factory"
        self.factory.mkdir()
        self.etc.mkdir()

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, base, rel, text, mode=0o644):
        path = base / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        path.chmod(mode)

    def run_sync(self, version):
        env = dict(os.environ, FACTORY=str(self.factory), ETC=str(self.etc),
                   STATE=str(self.state), VERSION=version)
        done = subprocess.run(["bash", str(SCRIPT)], env=env, capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        return done.stdout

    def install_image(self, files, version):
        """A new image: its factory /etc becomes exactly `files`."""
        for path in sorted(self.factory.rglob("*"), reverse=True):
            path.unlink() if not path.is_dir() or path.is_symlink() else path.rmdir()
        for rel, text in files.items():
            self.write(self.factory, rel, text)
        return self.run_sync(version)

    def test_first_sync_adds_missing_files_and_keeps_differing_ones(self):
        self.write(self.etc, "ld.so.conf", "old linker paths\n")
        out = self.install_image({"ld.so.conf": "new linker paths\n", "ssl/certs/ca.crt": "CA v1\n"}, "1")
        self.assertEqual((self.etc / "ssl/certs/ca.crt").read_text(), "CA v1\n")
        # Without a record, a file that differs might be the node's own change.
        self.assertEqual((self.etc / "ld.so.conf").read_text(), "old linker paths\n")
        self.assertIn("1 added", out)
        self.assertIn("kept /etc/ld.so.conf", out)

    def test_an_unchanged_file_follows_the_image(self):
        self.install_image({"ssl/certs/ca.crt": "CA v1\n"}, "1")
        out = self.install_image({"ssl/certs/ca.crt": "CA v2\n"}, "2")
        self.assertEqual((self.etc / "ssl/certs/ca.crt").read_text(), "CA v2\n")
        self.assertIn("1 updated", out)

    def test_a_file_changed_on_the_node_is_kept(self):
        self.install_image({"systemd/timesyncd.conf": "image v1\n"}, "1")
        self.write(self.etc, "systemd/timesyncd.conf", "my NTP server\n")
        out = self.install_image({"systemd/timesyncd.conf": "image v2\n"}, "2")
        self.assertEqual((self.etc / "systemd/timesyncd.conf").read_text(), "my NTP server\n")
        self.assertIn("kept /etc/systemd/timesyncd.conf", out)
        # Still the node's on the next image too.
        self.install_image({"systemd/timesyncd.conf": "image v3\n"}, "3")
        self.assertEqual((self.etc / "systemd/timesyncd.conf").read_text(), "my NTP server\n")

    def test_a_file_deleted_on_the_node_stays_deleted(self):
        self.install_image({"motd": "hello v1\n"}, "1")
        (self.etc / "motd").unlink()
        self.install_image({"motd": "hello v2\n"}, "2")
        self.assertFalse((self.etc / "motd").exists())

    def test_a_file_the_image_dropped_is_removed_unless_changed(self):
        self.install_image({"old.conf": "v1\n", "edited.conf": "v1\n", "keep.conf": "v1\n"}, "1")
        self.write(self.etc, "edited.conf", "mine\n")
        out = self.install_image({"keep.conf": "v1\n"}, "2")
        self.assertFalse((self.etc / "old.conf").exists())
        self.assertEqual((self.etc / "edited.conf").read_text(), "mine\n")
        self.assertIn("1 removed", out)

    def test_symlinks_follow_the_image(self):
        (self.factory / "systemd/system/multi-user.target.wants").mkdir(parents=True)
        (self.factory / "systemd/system/multi-user.target.wants/a.service").symlink_to("/usr/lib/systemd/system/a.service")
        self.run_sync("1")
        link = self.etc / "systemd/system/multi-user.target.wants/a.service"
        self.assertEqual(os.readlink(link), "/usr/lib/systemd/system/a.service")
        # The next image enables b instead of a.
        link_dir = self.factory / "systemd/system/multi-user.target.wants"
        (link_dir / "a.service").unlink()
        (link_dir / "b.service").symlink_to("/usr/lib/systemd/system/b.service")
        self.run_sync("2")
        self.assertFalse(os.path.lexists(link))
        self.assertEqual(os.readlink(self.etc / "systemd/system/multi-user.target.wants/b.service"),
                         "/usr/lib/systemd/system/b.service")

    def test_a_mode_change_counts_as_a_change(self):
        self.install_image({"sudoers": "v1\n"}, "1")
        (self.etc / "sudoers").chmod(0o600)
        self.install_image({"sudoers": "v2\n"}, "2")
        self.assertEqual((self.etc / "sudoers").read_text(), "v1\n")

    def test_the_node_s_own_files_are_never_touched(self):
        for rel in ["machine-id", "passwd", "shadow", "ssh/ssh_host_ed25519_key", "reliaburger/node.toml"]:
            self.write(self.etc, rel, "node's own\n")
        self.install_image({rel: "image\n" for rel in
                            ["machine-id", "passwd", "shadow", "ssh/ssh_host_ed25519_key", "reliaburger/node.toml"]}, "1")
        self.install_image({"hosts": "v2\n"}, "2")
        for rel in ["machine-id", "passwd", "shadow", "ssh/ssh_host_ed25519_key", "reliaburger/node.toml"]:
            self.assertEqual((self.etc / rel).read_text(), "node's own\n", rel)

    def test_it_runs_once_per_image_version(self):
        self.install_image({"ssl/certs/ca.crt": "CA v1\n"}, "1")
        (self.etc / "ssl/certs/ca.crt").unlink()
        # Same version again (a plain reboot): nothing happens.
        self.assertEqual(self.run_sync("1"), "")
        self.assertFalse((self.etc / "ssl/certs/ca.crt").exists())


if __name__ == "__main__":
    unittest.main()
