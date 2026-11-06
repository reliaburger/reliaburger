"""Tests for image/tests/disk.sh, the disk the QEMU tests give an appliance:
7.25 GiB like a Wyse 3040's eMMC, and a clear failure when the image doesn't
fit, never a disk image cut short.

    python3 -m unittest discover -s image/tests
"""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
DISK = HERE / "disk.sh"
MIB = 1024 * 1024


def run(script, disk_mib=None):
    env = dict(os.environ)
    env.pop("DISK_MIB", None)
    if disk_mib is not None:
        env["DISK_MIB"] = str(disk_mib)
    return subprocess.run(["bash", "-c", f'set -euo pipefail; . "{DISK}"; {script}'],
                          env=env, capture_output=True, text=True)


class Disk(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_the_test_disk_is_7_25_gib_not_8(self):
        done = run('echo "$DISK_MIB"')
        self.assertEqual(done.stdout.strip(), "7424")
        self.assertEqual(7424 * MIB, int(7.25 * 1024 ** 3))

    def test_no_qemu_test_uses_an_8_gib_disk(self):
        for script in [*HERE.glob("*.sh"), HERE.parents[1] / ".github/workflows/appliance.yml"]:
            with self.subTest(script=script.name):
                self.assertFalse("truncate -s 8G" in script.read_text(), script.name)

    def test_an_image_that_fits_is_grown_to_the_disk(self):
        raw = self.dir / "disk.raw"
        raw.write_bytes(b"x" * (2 * MIB))
        # 2 MiB of image and 1164 MiB of slot B fit 1166 MiB exactly.
        done = run(f'disk_from_image "{raw}"', disk_mib=1166)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(raw.stat().st_size, 1166 * MIB)
        self.assertEqual(raw.read_bytes()[:2 * MIB], b"x" * (2 * MIB))

    def test_an_image_without_room_for_slot_b_fails_and_is_left_whole(self):
        raw = self.dir / "disk.raw"
        raw.write_bytes(b"x" * (2 * MIB + 1))
        done = run(f'disk_from_image "{raw}"', disk_mib=1166)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("doesn't fit a 1166 MiB disk", done.stderr)
        self.assertIn("slot B", done.stderr)
        self.assertEqual(raw.stat().st_size, 2 * MIB + 1)

    def test_an_image_bigger_than_the_disk_is_never_truncated(self):
        raw = self.dir / "disk.raw"
        raw.write_bytes(b"x" * (3 * MIB))
        done = run(f'disk_from_image "{raw}"', disk_mib=2)
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual(raw.stat().st_size, 3 * MIB)

    @unittest.skipUnless(shutil.which("zstd"), "needs zstd")
    def test_a_blank_disk_checks_the_image_the_installer_will_write(self):
        image = self.dir / "reliaburger-os_2026.41.0.raw.zst"
        source = self.dir / "image.raw"
        source.write_bytes(b"\0" * (3 * MIB))
        subprocess.run(["zstd", "-q", str(source), "-o", str(image)], check=True)
        blank = self.dir / "blank.raw"
        done = run(f'blank_disk "{blank}" "{image}"', disk_mib=1167)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(blank.stat().st_size, 1167 * MIB)
        self.assertEqual(blank.read_bytes(), b"\0" * (1167 * MIB))
        blank.unlink()
        done = run(f'blank_disk "{blank}" "{image}"', disk_mib=1166)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("reliaburger-os_2026.41.0.raw.zst doesn't fit", done.stderr)
        self.assertFalse(blank.exists())


if __name__ == "__main__":
    unittest.main()
