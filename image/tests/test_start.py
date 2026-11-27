"""bun's launcher keeps bun from moving backwards, except after an OS fallback.

`image/mkosi.extra/usr/lib/reliaburger/start` runs bun from
/var/lib/reliaburger/bin and installs the image's own bun there only when
it's newer than the active one, so an OS update can't undo a bun upgrade.
But an OS update that brings a bun which starts and never gets healthy
fails its boot check on both tries, systemd-boot falls back to the previous
OS, and that OS's older bun was never put back: the node kept the bad bun,
on a boot that isn't counted, so nothing rebooted it again.

So the launcher records which OS image installed the active bun. A boot of
an older OS than that, with that bun still active, is a fallback, and puts
this image's bun back. A bun the operator upgraded to since doesn't match
the record, and is left alone.

`RELIABURGER_BIN_DIR`, `RELIABURGER_IMAGE_BUN` and `RELIABURGER_OS_RELEASE`
point the launcher at a temporary directory. GNU `mv -T` makes the symlink
swap atomic, so this runs on Linux only, as CI's script job does.

    python3 -m unittest discover -s image/tests -p 'test_start.py'
"""

import fcntl
import os
import platform
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
START = ROOT / "image" / "mkosi.extra" / "usr" / "lib" / "reliaburger" / "start"


def fake_bun(path, version):
    """A bun that reports `version` and, for `appliance prepare`, says which
    binary ran, so a test can see which bun the launcher chose."""
    path.write_text(
        "#!/bin/sh\n"
        f'[ "$1" = --version ] && {{ echo "bun {version}"; exit 0; }}\n'
        f'echo "ran {version}: $*"\n')
    path.chmod(0o755)


@unittest.skipUnless(platform.system() == "Linux", "the launcher uses GNU mv -T")
class Launcher(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.bin = self.dir / "bin"
        self.image = self.dir / "image-bun"
        self.os_release = self.dir / "os-release"

    def tearDown(self):
        self.tmp.cleanup()

    def boot(self, os_version, image_bun):
        """Boot an OS image `os_version` carrying bun `image_bun`; return the
        launcher's output and which bun ran."""
        self.os_release.write_text(f'ID=ubuntu\nIMAGE_VERSION={os_version}\n')
        fake_bun(self.image, image_bun)
        env = dict(os.environ, RELIABURGER_BIN_DIR=str(self.bin),
                   RELIABURGER_IMAGE_BUN=str(self.image),
                   RELIABURGER_OS_RELEASE=str(self.os_release))
        done = subprocess.run(["sh", str(START), "prepare"], env=env,
                              capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        return done.stdout

    def active(self):
        return os.readlink(self.bin / "bun")

    def upgrade_bun(self, version):
        """What `relish upgrade` does: a new binary beside it, then the link."""
        fake_bun(self.bin / f"bun-v{version}", version)
        (self.bin / "bun").unlink()
        (self.bin / "bun").symlink_to(f"bun-v{version}")

    def test_first_boot_installs_the_images_bun(self):
        out = self.boot("2026.46.1", "0.3.0")
        self.assertEqual(self.active(), "bun-v0.3.0")
        self.assertIn("ran 0.3.0: appliance prepare", out)

    def test_an_os_update_with_a_newer_bun_installs_it(self):
        self.boot("2026.46.1", "0.3.0")
        self.boot("2026.46.2", "0.3.1")
        self.assertEqual(self.active(), "bun-v0.3.1")

    def test_an_os_update_with_an_older_bun_keeps_the_upgraded_one(self):
        self.boot("2026.46.1", "0.3.0")
        self.upgrade_bun("0.3.2")
        out = self.boot("2026.46.2", "0.3.1")
        self.assertEqual(self.active(), "bun-v0.3.2")
        self.assertIn("ran 0.3.2", out)

    def test_a_fallback_puts_the_older_images_bun_back(self):
        self.boot("2026.46.1", "0.3.0")
        self.boot("2026.46.2", "0.3.1")
        self.assertEqual(self.active(), "bun-v0.3.1")
        # 2026.46.2's bun never got healthy; systemd-boot fell back.
        out = self.boot("2026.46.1", "0.3.0")
        self.assertEqual(self.active(), "bun-v0.3.0")
        self.assertIn("ran 0.3.0", out)
        self.assertIn("fallback", out)

    def test_a_fallback_keeps_a_bun_the_operator_upgraded_to_since(self):
        self.boot("2026.46.1", "0.3.0")
        self.boot("2026.46.2", "0.3.1")
        self.upgrade_bun("0.3.2")
        self.boot("2026.46.1", "0.3.0")
        self.assertEqual(self.active(), "bun-v0.3.2")

    def test_a_fallback_from_an_update_that_kept_bun_changes_nothing(self):
        self.boot("2026.46.1", "0.3.0")
        self.boot("2026.46.2", "0.3.0")
        out = self.boot("2026.46.1", "0.3.0")
        self.assertEqual(self.active(), "bun-v0.3.0")
        self.assertNotIn("fallback", out)

    def test_after_a_fallback_the_next_good_update_installs_again(self):
        self.boot("2026.46.1", "0.3.0")
        self.boot("2026.46.2", "0.3.1")
        self.boot("2026.46.1", "0.3.0")
        self.boot("2026.46.3", "0.3.2")
        self.assertEqual(self.active(), "bun-v0.3.2")

    def test_one_launcher_installs_at_a_time(self):
        # The console starts beside the seed's prepare, and on a claimed
        # node beside bun, all through this launcher (#2161). Two installs
        # at once would write the same temporary files, so each takes
        # $bin/.install.lock first; flock(1) and fcntl.flock share the lock.
        self.bin.mkdir()
        self.os_release.write_text('ID=ubuntu\nIMAGE_VERSION=2026.46.1\n')
        fake_bun(self.image, "0.3.0")
        env = dict(os.environ, RELIABURGER_BIN_DIR=str(self.bin),
                   RELIABURGER_IMAGE_BUN=str(self.image),
                   RELIABURGER_OS_RELEASE=str(self.os_release))
        with open(self.bin / ".install.lock", "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            launcher = subprocess.Popen(["sh", str(START), "console"], env=env,
                                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                        text=True)
            self.addCleanup(launcher.stdout.close)
            self.addCleanup(launcher.kill)
            time.sleep(1)
            self.assertIsNone(launcher.poll(), "the launcher didn't wait for the lock")
            self.assertFalse((self.bin / "bun").exists())
            fcntl.flock(lock, fcntl.LOCK_UN)
        out, _ = launcher.communicate(timeout=30)
        self.assertEqual(launcher.returncode, 0, out)
        self.assertEqual(self.active(), "bun-v0.3.0")
        self.assertIn("ran 0.3.0: appliance console", out)

    def test_bun_does_not_hold_the_install_lock(self):
        # The launcher execs bun, which inherits its open files. A bun that
        # kept the lock would hold every later launcher (the console's
        # restarts, prepare) back for as long as it ran.
        self.boot("2026.46.1", "0.3.0")
        (self.bin / "bun-v0.3.0").write_text(
            "#!/bin/sh\n"
            '[ "$1" = --version ] && { echo "bun 0.3.0"; exit 0; }\n'
            f'exec 8>"{self.bin}/.install.lock"\n'
            'if flock -n 8; then echo "lock free"; else echo "lock held"; fi\n')
        out = self.boot("2026.46.1", "0.3.0")
        self.assertIn("lock free", out)


if __name__ == "__main__":
    unittest.main()
