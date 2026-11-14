"""Tests for image/tests/no_ssh_server.py: a published image has no SSH
server (#401). bun is the only way into an appliance; the `lab` profile
(mkosi.conf.d/30-lab.conf) adds openssh-server for debugging, and nothing
else may.

    python3 -m unittest discover -s image/tests -p 'test_no_ssh_server.py'
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
IMAGE = HERE.parent
CHECK = HERE / "no_ssh_server.py"


def run(*args):
    return subprocess.run([sys.executable, str(CHECK), *map(str, args)],
                          capture_output=True, text=True)


BASE = """[Distribution]
Distribution=ubuntu

[Output]
ManifestFormat=json

[Content]
Packages=
        systemd
        udev
"""

LAB = """[Match]
Profiles=lab

[Content]
Packages=openssh-server
"""


class Config(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.image = Path(self.tmp.name)
        (self.image / "mkosi.conf.d").mkdir()
        (self.image / "mkosi.conf").write_text(BASE)
        (self.image / "mkosi.conf.d" / "30-lab.conf").write_text(LAB)

    def tearDown(self):
        self.tmp.cleanup()

    def test_the_repository_s_configs_keep_ssh_in_the_lab_profile(self):
        done = run("config", IMAGE)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_an_ssh_server_only_in_the_lab_profile_passes(self):
        done = run("config", self.image)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_an_ssh_server_in_the_base_config_fails(self):
        (self.image / "mkosi.conf").write_text(BASE + "        openssh-server\n")
        done = run("config", self.image)
        self.assertEqual(done.returncode, 1)
        self.assertIn("mkosi.conf", done.stdout)
        self.assertIn("openssh-server", done.stdout)

    def test_an_ssh_server_on_one_line_or_in_an_arch_drop_in_fails(self):
        (self.image / "mkosi.conf.d" / "10-x86-64.conf").write_text(
            "[Match]\nArchitecture=x86-64\n\n[Content]\nPackages=dropbear-bin,curl\n")
        done = run("config", self.image)
        self.assertEqual(done.returncode, 1)
        self.assertIn("10-x86-64.conf", done.stdout)
        self.assertIn("dropbear-bin", done.stdout)

    def test_an_ssh_server_in_the_installer_subimage_fails(self):
        installer = self.image / "mkosi.images" / "installer"
        installer.mkdir(parents=True)
        (installer / "mkosi.conf").write_text("[Content]\nPackages=\n        curl\n        tinysshd\n")
        done = run("config", self.image)
        self.assertEqual(done.returncode, 1)
        self.assertIn("tinysshd", done.stdout)

    def test_an_image_or_subimage_without_a_manifest_fails(self):
        # #1926: the manifest check is what catches an SSH server pulled in
        # as a dependency, so every image has to write one.
        installer = self.image / "mkosi.images" / "installer"
        installer.mkdir(parents=True)
        (installer / "mkosi.conf").write_text("[Content]\nPackages=curl\n")
        done = run("config", self.image)
        self.assertEqual(done.returncode, 1)
        self.assertIn("installer", done.stdout)
        self.assertIn("ManifestFormat=json", done.stdout)
        # A drop-in may set it.
        (installer / "mkosi.conf.d").mkdir()
        (installer / "mkosi.conf.d" / "20-manifest.conf").write_text("[Output]\nManifestFormat=json\n")
        done = run("config", self.image)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        # The main image needs one too.
        (self.image / "mkosi.conf").write_text(BASE.replace("ManifestFormat=json", "Format=disk"))
        done = run("config", self.image)
        self.assertEqual(done.returncode, 1)
        self.assertIn(f"{self.image}: no ManifestFormat=json", done.stdout)

    def test_a_profile_besides_lab_does_not_excuse_it(self):
        (self.image / "mkosi.conf.d" / "40-debug.conf").write_text(
            "[Match]\nProfiles=debug\n\n[Content]\nPackages=openssh-server\n")
        done = run("config", self.image)
        self.assertEqual(done.returncode, 1)
        self.assertIn("40-debug.conf", done.stdout)


class Manifest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def manifest(self, *names):
        path = self.dir / "reliaburger-os_1.manifest"
        path.write_text(json.dumps({
            "packages": [{"type": "deb", "name": n, "version": "1"} for n in names]}))
        return path

    def test_a_published_manifest_without_an_ssh_server_passes(self):
        done = run("manifest", self.manifest("systemd", "openssh-client", "runc"))
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_a_published_manifest_with_one_fails_naming_it(self):
        for package in ("openssh-server", "dropbear", "tinysshd"):
            with self.subTest(package=package):
                done = run("manifest", self.manifest("systemd", package))
                self.assertEqual(done.returncode, 1)
                self.assertIn(package, done.stdout)

    def test_a_lab_manifest_must_have_openssh_server_so_the_check_can_see_one(self):
        done = run("manifest", "--lab", self.manifest("systemd", "openssh-server"))
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        done = run("manifest", "--lab", self.manifest("systemd"))
        self.assertEqual(done.returncode, 1)
        self.assertIn("lab", done.stdout)

    def test_a_manifest_with_no_package_list_fails(self):
        path = self.dir / "empty.manifest"
        path.write_text("{}")
        done = run("manifest", path)
        self.assertEqual(done.returncode, 1)


if __name__ == "__main__":
    unittest.main()
