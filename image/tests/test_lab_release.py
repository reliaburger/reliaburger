"""Tests for image/tests/lab-release.sh, which lays out a CI lab build's next
version as a GitHub release beside a lab os-channel.json, for
`relish os upgrade --channel` in the lab and in os-update.sh.

    python3 -m unittest discover -s image/tests
"""

import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "lab-release.sh"
NEXT = "2026.41.8"
USR = f"reliaburger-os_{NEXT}.usr.4f68bce3-e8cd-4db1-96e7-fbcaf984b709.raw.zst"
VERITY = f"reliaburger-os_{NEXT}.usr-verity.8f3b2f2c-1b1a-4f4e-9c55-0d3c2b7a6e10.raw.zst"


class LabRelease(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.next = root / "next"
        self.tree = root / "tree"
        self.next.mkdir()
        for name in [f"reliaburger-os_{NEXT}.efi", USR, VERITY, f"reliaburger-os_{NEXT}.SHA256SUMS",
                     f"reliaburger-os_{NEXT}.SHA256SUMS.sig", "os-channel.json", "os-channel.json.sig",
                     "spike-signing-key.pub.pem",
                     # The full disk image and the manifest aren't part of an update.
                     f"reliaburger-os_{NEXT}.raw.zst", f"reliaburger-os_{NEXT}.manifest"]:
            (self.next / name).write_text(name)

    def tearDown(self):
        self.tmp.cleanup()

    def lay_out(self):
        return subprocess.run(["bash", str(SCRIPT), str(self.next), NEXT, str(self.tree)],
                              capture_output=True, text=True)

    def test_the_next_version_sits_beside_the_channel_as_on_github(self):
        done = self.lay_out()
        self.assertEqual(done.returncode, 0, done.stderr)
        files = sorted(str(p.relative_to(self.tree)) for p in self.tree.rglob("*") if p.is_file())
        release = f"releases/download/os-{NEXT}-x86_64"
        self.assertEqual(files, sorted([
            "lab-signing-key.pub.pem",
            "releases/download/os-channel/os-channel.json",
            "releases/download/os-channel/os-channel.json.sig",
            f"{release}/reliaburger-os_{NEXT}.efi",
            f"{release}/{USR}",
            f"{release}/{VERITY}",
            f"{release}/reliaburger-os_{NEXT}.SHA256SUMS",
            f"{release}/reliaburger-os_{NEXT}.SHA256SUMS.sig",
        ]))
        self.assertEqual((self.tree / release / USR).read_text(), USR)
        self.assertEqual((self.tree / "lab-signing-key.pub.pem").read_text(), "spike-signing-key.pub.pem")

    def test_a_missing_channel_fails(self):
        (self.next / "os-channel.json.sig").unlink()
        done = self.lay_out()
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("os-channel.json.sig is missing", done.stderr)

    def test_a_missing_usr_image_fails(self):
        (self.next / VERITY).unlink()
        done = self.lay_out()
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("found 1", done.stderr)


if __name__ == "__main__":
    unittest.main()
