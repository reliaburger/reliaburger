"""Tests for os_release.py: signing, the channel document and a CI lab
build's channel, versions, the quiet-week check and pruning."""
import contextlib
import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

import os_release


def make_key(directory):
    key = Path(directory) / "key.der"
    subprocess.run(["openssl", "genpkey", "-algorithm", "ed25519", "-outform", "DER", "-out", key], check=True)
    pub = Path(directory) / "pub.pem"
    subprocess.run(["openssl", "pkey", "-inform", "DER", "-in", key, "-pubout", "-out", pub], check=True)
    return key, pub


def verifies(pub, path):
    return subprocess.run(["openssl", "pkeyutl", "-verify", "-pubin", "-inkey", pub, "-rawin",
                           "-in", path, "-sigfile", str(path) + ".sig"], capture_output=True).returncode == 0


class Signing(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.key, self.pub = make_key(self.dir)

    def tearDown(self):
        self.tmp.cleanup()

    def test_every_sums_file_is_signed_the_way_the_installer_checks(self):
        for name in ["reliaburger-os_2026.40.0.SHA256SUMS", "reliaburger-os-installer_2026.40.0.SHA256SUMS"]:
            (self.dir / name).write_text("ab  file\n")
        signed = os_release.sign_sums([self.dir], self.key)
        self.assertEqual(len(signed), 2)
        for path in signed:
            self.assertEqual(len(Path(str(path) + ".sig").read_bytes()), 64)
            self.assertTrue(verifies(self.pub, path))

    def test_a_tampered_sums_file_no_longer_verifies(self):
        sums = self.dir / "reliaburger-os_2026.40.0.SHA256SUMS"
        sums.write_text("ab  file\n")
        os_release.sign_sums([self.dir], self.key)
        sums.write_text("cd  file\n")
        self.assertFalse(verifies(self.pub, sums))

    def test_nothing_to_sign_is_an_error(self):
        with self.assertRaises(ValueError):
            os_release.sign_sums([self.dir], self.key)

    def test_the_channel_is_canonical_signed_json(self):
        sums = self.dir / "reliaburger-os_2026.40.3.SHA256SUMS"
        sums.write_text("ab  reliaburger-os_2026.40.3.raw.zst\n")
        path = os_release.write_channel("2026.40.3", {"aarch64": sums}, self.dir / "out", self.key)
        raw = path.read_bytes()
        document = json.loads(raw)
        self.assertEqual(document["version"], "2026.40.3")
        self.assertEqual(document["architectures"]["aarch64"]["tag"], "os-2026.40.3-aarch64")
        self.assertEqual(document["architectures"]["aarch64"]["sums"], sums.name)
        self.assertEqual(document["architectures"]["aarch64"]["sums_sha256"], os_release.sha256_file(sums))
        self.assertEqual(raw, json.dumps(document, sort_keys=True, separators=(",", ":")).encode() + b"\n")
        self.assertTrue(verifies(self.pub, path))

    def test_the_channel_refuses_a_sums_file_of_another_version_or_arch(self):
        sums = self.dir / "reliaburger-os_2026.40.2.SHA256SUMS"
        sums.write_text("ab  x\n")
        with self.assertRaises(ValueError):
            os_release.channel_document("2026.40.3", {"aarch64": sums})
        good = self.dir / "reliaburger-os_2026.40.3.SHA256SUMS"
        good.write_text("ab  x\n")
        with self.assertRaises(ValueError):
            os_release.channel_document("2026.40.3", {"riscv64": good})
        with self.assertRaises(ValueError):
            os_release.channel_document("v0.1.0", {"aarch64": good})


class LabChannel(unittest.TestCase):
    """A CI lab build's channel: the next version, signed with the run's
    throwaway key instead of the release key."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.key, self.pub = make_key(self.dir)
        self.installed = self.dir / "reliaburger-os_2026.41.7.SHA256SUMS"
        self.installed.write_text("ab  reliaburger-os_2026.41.7.efi\n")
        self.next = self.dir / "reliaburger-os_2026.41.8.SHA256SUMS"
        self.next.write_text("cd  reliaburger-os_2026.41.8.efi\n")

    def tearDown(self):
        self.tmp.cleanup()

    def lab_channel(self, version, sums):
        with contextlib.redirect_stdout(io.StringIO()):
            return os_release.main(["lab-channel", "--key", str(self.key), "--version", version,
                                    "--out", str(self.dir / "out"), f"x86_64={sums}"])

    def test_a_lab_channel_names_the_next_version_signed_with_the_given_key(self):
        self.assertEqual(self.lab_channel("2026.41.8", self.next), 0)
        path = self.dir / "out" / "os-channel.json"
        document = json.loads(path.read_bytes())
        self.assertEqual(document["version"], "2026.41.8")
        self.assertEqual(document["architectures"], {"x86_64": {
            "tag": "os-2026.41.8-x86_64", "sums": self.next.name,
            "sums_sha256": os_release.sha256_file(self.next)}})
        self.assertEqual(len((self.dir / "out" / "os-channel.json.sig").read_bytes()), 64)
        self.assertTrue(verifies(self.pub, path))
        # The same canonical bytes the release channel has, so bun and relish
        # parse a lab channel exactly as they parse a published one.
        self.assertEqual(path.read_bytes(), os_release.channel_document("2026.41.8", {"x86_64": self.next}))

    def test_a_lab_channel_refuses_the_installed_versions_sums(self):
        # The nodes run the first build; the channel names the one they update to.
        with self.assertRaises(ValueError):
            self.lab_channel("2026.41.8", self.installed)

    def test_a_lab_channel_never_reads_the_release_key(self):
        os.environ["RELIABURGER_RELEASE_KEY"] = "not a key"
        try:
            self.assertEqual(self.lab_channel("2026.41.8", self.next), 0)
            self.assertEqual(os.environ["RELIABURGER_RELEASE_KEY"], "not a key")
        finally:
            del os.environ["RELIABURGER_RELEASE_KEY"]
        self.assertTrue(verifies(self.pub, self.dir / "out" / "os-channel.json"))

    def test_a_lab_channel_needs_a_key_file(self):
        with self.assertRaises(FileNotFoundError):
            os_release.main(["lab-channel", "--key", str(self.dir / "missing.der"), "--version", "2026.41.8",
                             "--out", str(self.dir / "out"), f"x86_64={self.next}"])


class ShippedKey(unittest.TestCase):
    def test_the_image_and_installer_trust_the_release_key(self):
        import base64, re
        root = Path(__file__).resolve().parents[2]
        trusted = re.findall(r'"ed25519:([A-Za-z0-9+/=]+)"', (root / "src/upgrade/keys.rs").read_text())
        for pem in ["image/mkosi.extra/usr/lib/reliaburger/os-signing-key.pub.pem",
                    "image/mkosi.images/installer/mkosi.extra/usr/lib/reliaburger/os-signing-key.pub.pem"]:
            der = subprocess.run(["openssl", "pkey", "-pubin", "-in", root / pem, "-outform", "DER"],
                                 capture_output=True, check=True).stdout
            self.assertIn(base64.b64encode(der[12:]).decode(), trusted, pem)


class Versions(unittest.TestCase):
    def test_the_first_release_of_a_week_is_zero(self):
        self.assertEqual(os_release.next_version("2026.41", ["os-2026.40.4-x86_64", "v0.1.2"]), "2026.41.0")

    def test_later_releases_in_the_week_count_up(self):
        tags = ["os-2026.41.0-x86_64", "os-2026.41.2-aarch64", "os-2026.40.9-x86_64", "os-channel"]
        self.assertEqual(os_release.next_version("2026.41", tags), "2026.41.3")

    def test_a_bad_week_is_refused(self):
        with self.assertRaises(ValueError):
            os_release.next_version("2026-41", [])


class QuietWeeks(unittest.TestCase):
    def manifest(self, **packages):
        return {"packages": [{"name": n, "version": v, "type": "deb"} for n, v in packages.items()]}

    def test_the_same_packages_are_unchanged(self):
        self.assertTrue(os_release.unchanged(self.manifest(a="1", b="2"), self.manifest(b="2", a="1")))

    def test_a_new_bun_or_image_recipe_is_a_change(self):
        m = self.manifest(a="1")
        self.assertTrue(os_release.unchanged(m, m, "bun v0.1.2\nimage abc", "bun v0.1.2\nimage abc"))
        self.assertFalse(os_release.unchanged(m, m, "bun v0.1.2\nimage abc", "bun v0.1.3\nimage abc"))
        self.assertFalse(os_release.unchanged(m, m, "bun v0.1.2\nimage abc", "bun v0.1.2\nimage def"))
        # A release from before build records existed counts as a change.
        self.assertFalse(os_release.unchanged(m, m, None, "bun v0.1.2\nimage abc"))

    def test_a_new_version_or_package_is_a_change(self):
        self.assertFalse(os_release.unchanged(self.manifest(a="1"), self.manifest(a="2")))
        self.assertFalse(os_release.unchanged(self.manifest(a="1"), self.manifest(a="1", b="1")))


class Pruning(unittest.TestCase):
    def test_keeps_the_newest_by_version_not_by_name(self):
        tags = [f"os-{v}-{a}" for v in ["2026.40.10", "2026.40.9", "2026.41.0", "2026.39.1"]
                for a in ["x86_64", "aarch64"]] + ["os-channel", "v0.1.2"]
        self.assertEqual(os_release.prune(tags, 2), ["os-2026.40.9-x86_64", "os-2026.40.9-aarch64",
                                                     "os-2026.39.1-x86_64", "os-2026.39.1-aarch64"])

    def test_nothing_to_prune(self):
        self.assertEqual(os_release.prune(["os-2026.40.0-x86_64", "os-2026.40.0-aarch64"], 8), [])


if __name__ == "__main__":
    unittest.main()
