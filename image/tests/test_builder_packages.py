"""Tests for image/tests/builder_packages.py: the appliance can build images
(#2144). It carries Buildah and what Buildah needs to build, and leaves out
the recommends a node doesn't use, which would cost room in both /usr slots.

    python3 -m unittest discover -s image/tests -p 'test_builder_packages.py'
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
IMAGE = HERE.parent
CHECK = HERE / "builder_packages.py"


def run(*args):
    return subprocess.run([sys.executable, str(CHECK), *map(str, args)],
                          capture_output=True, text=True)


# What a good build lists, Buildah's dependencies included.
BUILDER = ("buildah", "golang-github-containers-common", "golang-github-containers-image",
           "netavark", "gpg", "runc", "uidmap")


class Config(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.image = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, packages, remove=("/etc/ssh/ssh_host_*", "/usr/libexec/buildah")):
        lines = ["[Content]", "Packages="]
        lines += [f"        {p}" for p in packages]
        lines += ["", "RemoveFiles="]
        lines += [f"        {r}" for r in remove]
        (self.image / "mkosi.conf").write_text("\n".join(lines) + "\n")

    def test_the_repository_s_image_can_build(self):
        done = run("config", IMAGE)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_a_config_with_buildah_and_its_named_dependencies_passes(self):
        self.write(["systemd", "gpg", "netavark", "runc", "uidmap", "buildah"])
        done = run("config", self.image)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_each_missing_package_fails_naming_it(self):
        for missing in ("buildah", "netavark", "gpg", "runc", "uidmap"):
            with self.subTest(missing=missing):
                self.write([p for p in ("gpg", "netavark", "runc", "uidmap", "buildah")
                            if p != missing])
                done = run("config", self.image)
                self.assertEqual(done.returncode, 1)
                self.assertIn(missing, done.stdout)

    def test_keeping_buildah_s_test_helpers_fails(self):
        # 97 MB of test binaries Buildah never runs, twice over.
        self.write(["gpg", "netavark", "runc", "uidmap", "buildah"], remove=("/etc/ssh/ssh_host_*",))
        done = run("config", self.image)
        self.assertEqual(done.returncode, 1)
        self.assertIn("/usr/libexec/buildah", done.stdout)


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

    def test_a_manifest_with_buildah_and_its_dependencies_passes(self):
        done = run("manifest", self.manifest("systemd", *BUILDER))
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

    def test_a_manifest_missing_a_builder_package_fails_naming_it(self):
        for missing in ("buildah", "netavark", "golang-github-containers-common"):
            with self.subTest(missing=missing):
                done = run("manifest", self.manifest(*(p for p in BUILDER if p != missing)))
                self.assertEqual(done.returncode, 1)
                self.assertIn(missing, done.stdout)

    def test_recommends_the_node_never_uses_fail(self):
        # gnupg would bring GnuPG's agents and key servers in for libgpgme's
        # "gnupg | gpg"; the node builds with vfs and runc, without cpp.
        for extra in ("gnupg", "dirmngr", "gpg-agent", "fuse-overlayfs", "crun", "cpp"):
            with self.subTest(extra=extra):
                done = run("manifest", self.manifest(*BUILDER, extra))
                self.assertEqual(done.returncode, 1)
                self.assertIn(extra, done.stdout)

    def test_a_manifest_with_no_package_list_fails(self):
        path = self.dir / "empty.manifest"
        path.write_text("{}")
        done = run("manifest", path)
        self.assertEqual(done.returncode, 1)


if __name__ == "__main__":
    unittest.main()
