"""Tests for image/tests/seed_wiped.py, which seeded-pair.sh runs on node 1's
RBSEED stick image once the node is up: the create seed carried the
cluster's master key, so no run of its bytes may be left anywhere on the
stick, deleted or not.

    python3 -m unittest discover -s image/tests -p 'test_seed_wiped.py'
"""

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SEED_WIPED = HERE / "seed_wiped.py"


class SeedWiped(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.seed = self.dir / "node.seed"
        self.seed.write_bytes(os.urandom(4096))

    def tearDown(self):
        self.tmp.cleanup()

    def check(self, stick):
        image = self.dir / "stick.img"
        image.write_bytes(stick)
        return subprocess.run([sys.executable, str(SEED_WIPED), str(image), str(self.seed)],
                              capture_output=True, text=True)

    def test_a_stick_whose_seed_was_zeroed_passes(self):
        done = self.check(b"\0" * 65536)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_a_stick_still_holding_the_seed_fails(self):
        seed = self.seed.read_bytes()
        done = self.check(b"\0" * 1000 + seed + b"\0" * 1000)
        self.assertEqual(done.returncode, 1)
        self.assertIn("still holds", done.stdout)

    def test_a_seed_unlinked_but_not_zeroed_fails(self):
        # FAT's delete marks the directory entry and frees the clusters; the
        # bytes stay. Even part of one cluster left behind is a failure.
        seed = self.seed.read_bytes()
        done = self.check(b"\0" * 1000 + seed[1024:2048] + b"\0" * 1000)
        self.assertEqual(done.returncode, 1)

    def test_a_stick_with_other_data_passes(self):
        done = self.check(os.urandom(65536))
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)


if __name__ == "__main__":
    unittest.main()
