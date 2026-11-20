"""Tests for image/tests/lab-release.sh, which lays out a CI lab build's next
version as a GitHub release beside a lab os-channel.json, for
`relish os upgrade --channel` in the lab and in os-update.sh, and optionally
its broken version beside it, for the fallback test.

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
BROKEN = "2026.41.9"
BROKEN_USR = f"reliaburger-os_{BROKEN}.usr.0b5e7a43-36a4-4c29-9a8e-2f0c3d1e5b77.raw.zst"
BROKEN_VERITY = f"reliaburger-os_{BROKEN}.usr-verity.6d1c9e20-8a3f-4b52-b7e4-91a0f2c8d3e6.raw.zst"


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
        # A broken build's output: its own SHA256SUMS signed by the same key,
        # but no channel.
        self.broken = root / "broken"
        self.broken.mkdir()
        for name in [f"reliaburger-os_{BROKEN}.efi", BROKEN_USR, BROKEN_VERITY,
                     f"reliaburger-os_{BROKEN}.SHA256SUMS", f"reliaburger-os_{BROKEN}.SHA256SUMS.sig",
                     f"reliaburger-os_{BROKEN}.raw.zst"]:
            (self.broken / name).write_text(name)

    def tearDown(self):
        self.tmp.cleanup()

    def lay_out(self, *extra):
        return subprocess.run(["bash", str(SCRIPT), str(self.next), NEXT, str(self.tree), *extra],
                              capture_output=True, text=True)

    def files(self):
        return sorted(str(p.relative_to(self.tree)) for p in self.tree.rglob("*") if p.is_file())

    def test_the_next_version_sits_beside_the_channel_as_on_github(self):
        done = self.lay_out()
        self.assertEqual(done.returncode, 0, done.stderr)
        files = self.files()
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

    def test_the_broken_version_sits_beside_the_next_one_and_the_channel_still_names_next(self):
        (self.next / "os-channel.json").write_text(f'{{"version":"{NEXT}"}}')
        done = self.lay_out(str(self.broken), BROKEN)
        self.assertEqual(done.returncode, 0, done.stderr)
        broken = f"releases/download/os-{BROKEN}-x86_64"
        for name in [f"reliaburger-os_{BROKEN}.efi", BROKEN_USR, BROKEN_VERITY,
                     f"reliaburger-os_{BROKEN}.SHA256SUMS", f"reliaburger-os_{BROKEN}.SHA256SUMS.sig"]:
            self.assertIn(f"{broken}/{name}", self.files())
        self.assertNotIn(f"{broken}/reliaburger-os_{BROKEN}.raw.zst", self.files())
        self.assertIn(f"releases/download/os-{NEXT}-x86_64/{USR}", self.files())
        channel = self.tree / "releases/download/os-channel/os-channel.json"
        self.assertEqual(channel.read_text(), f'{{"version":"{NEXT}"}}')

    def test_a_broken_version_without_its_signature_fails(self):
        (self.broken / f"reliaburger-os_{BROKEN}.SHA256SUMS.sig").unlink()
        done = self.lay_out(str(self.broken), BROKEN)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn(f"reliaburger-os_{BROKEN}.SHA256SUMS.sig is missing", done.stderr)

    def test_a_broken_directory_without_its_version_fails(self):
        done = self.lay_out(str(self.broken))
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("usage", done.stderr)

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
