"""Release packaging tests use ephemeral keys, never the release secret."""
import base64
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

from package import package_release


class ReleasePackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.key = self.root / "key.der"
        subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-outform", "DER", "-out", str(self.key)], check=True, capture_output=True)
        public = subprocess.run(["openssl", "pkey", "-inform", "DER", "-in", str(self.key), "-pubout", "-outform", "DER"], check=True, capture_output=True).stdout
        self.trusted = ["ed25519:" + base64.b64encode(public[-32:]).decode()]
        self.assets = self.root / "assets"
        self.assets.mkdir()
        for name in ("bun-linux-aarch64", "bun-linux-x86_64", "relish-linux-aarch64", "relish-linux-x86_64", "relish-macos-aarch64", "relish-macos-x86_64"):
            (self.assets / name).write_bytes(b"test binary: " + name.encode())

    def package(self):
        package_release(self.assets, "v0.1.0", "reliaburger/reliaburger", self.key, self.trusted)

    def test_metadata_keeps_bun_and_cli_selection_separate(self):
        self.package()
        bun = json.loads((self.assets / "metadata.json").read_text())
        cli = json.loads((self.assets / "cli-metadata.json").read_text())
        self.assertEqual(bun["schema"], 1)
        self.assertEqual(bun["latest"], "v0.1.0")
        self.assertEqual(set(bun["releases"][0]["platforms"]), {"linux-aarch64", "linux-x86_64"})
        artifact = cli["releases"][0]["platforms"]["macos-aarch64"]
        self.assertEqual(artifact["url"], "https://github.com/reliaburger/reliaburger/releases/download/v0.1.0/relish-macos-aarch64")
        path = self.assets / "relish-macos-aarch64"
        self.assertEqual(artifact["sha256"], hashlib.sha256(path.read_bytes()).hexdigest())
        envelope = json.loads(path.with_name(path.name + ".sig").read_text())
        self.assertEqual(artifact["embedded_signature"], envelope["embedded"])
        signature = self.root / "signature"
        signature.write_bytes(base64.b64decode(envelope["embedded"]))
        result = subprocess.run(["openssl", "pkeyutl", "-verify", "-rawin", "-keyform", "DER", "-inkey", str(self.key), "-in", str(path), "-sigfile", str(signature)], capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        path.write_bytes(b"tampered")
        result = subprocess.run(["openssl", "pkeyutl", "-verify", "-rawin", "-keyform", "DER", "-inkey", str(self.key), "-in", str(path), "-sigfile", str(signature)], capture_output=True)
        self.assertNotEqual(result.returncode, 0)

    def test_installer_pins_cli_hash_and_refuses_modified_download(self):
        self.package()
        installer = self.assets / "install.sh"
        self.assertTrue(installer.is_file())
        home = self.root / "home"
        home.mkdir()
        tools = self.root / "tools"
        tools.mkdir()
        (tools / "uname").write_text("#!/bin/sh\ncase \"$1\" in -s) echo Darwin;; -m) echo arm64;; esac\n")
        (tools / "curl").write_text("#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do if [ \"$1\" = -o ]; then cp \"$FIXTURE\" \"$2\"; exit; fi; shift; done\nexit 1\n")
        for tool in tools.iterdir():
            tool.chmod(0o755)
        import os
        env = dict(os.environ, HOME=str(home), PATH=str(tools) + os.pathsep + os.environ["PATH"], FIXTURE=str(self.assets / "relish-macos-aarch64"))
        result = subprocess.run(["bash", str(installer), "--install-only"], env=env, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        installed = home / ".reliaburger/bin/relish"
        original = installed.read_bytes()
        (self.assets / "relish-macos-aarch64").write_bytes(b"tampered")
        result = subprocess.run(["bash", str(installer), "--install-only"], env=env, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(installed.read_bytes(), original)

    def test_untrusted_key_cannot_publish_metadata(self):
        self.trusted = ["ed25519:" + base64.b64encode(bytes(32)).decode()]
        with self.assertRaises(ValueError):
            self.package()
        self.assertFalse((self.assets / "metadata.json").exists())

    def test_incomplete_matrix_cannot_publish_metadata(self):
        (self.assets / "bun-linux-aarch64").unlink()
        with self.assertRaises(ValueError):
            self.package()
        self.assertFalse((self.assets / "metadata.json").exists())

    def test_invalid_release_tag_is_rejected(self):
        with self.assertRaises(ValueError):
            package_release(self.assets, "../main", "reliaburger/reliaburger", self.key, self.trusted)


if __name__ == "__main__":
    unittest.main()
