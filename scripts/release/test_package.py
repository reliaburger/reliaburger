"""Release packaging tests use ephemeral keys, never the release secret."""
import base64
import hashlib
import json
from pathlib import Path
import subprocess
import shutil
import tempfile
import unittest

from package import package_release

# The installers promise `curl ... | sh`. CI's `sh` is dash; macOS's is bash in
# POSIX mode. Run every installer test under each POSIX shell present.
POSIX_SHELLS = [shell for shell in ("sh", "dash") if shutil.which(shell)]
BOOTSTRAP = Path(__file__).resolve().parents[2] / "docs/website/install.sh"


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
        genuine = (self.assets / "relish-macos-aarch64").read_bytes()
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell):
                (self.assets / "relish-macos-aarch64").write_bytes(genuine)
                result = subprocess.run([shell, str(installer), "--install-only"], env=env, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                installed = home / ".reliaburger/bin/relish"
                original = installed.read_bytes()
                self.assertEqual(original, genuine)
                (self.assets / "relish-macos-aarch64").write_bytes(b"tampered")
                result = subprocess.run([shell, str(installer), "--install-only"], env=env, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(b"checksum mismatch", result.stderr)
                self.assertEqual(installed.read_bytes(), original)
                self.assertEqual([p.name for p in (home / ".reliaburger/bin").iterdir()], ["relish"],
                                 "a failed install left its staging directory behind")
                result = subprocess.run([shell, str(installer), "--install-only", "--nodes", "1"], env=env, capture_output=True)
                self.assertNotEqual(result.returncode, 0)

    def test_candidate_mirror_keeps_installer_bytes_and_forwards_signed_source(self):
        import os
        binary = self.assets / "relish-macos-aarch64"
        binary.write_text('#!/usr/bin/env bash\nprintf "%s\\n" "$@" > "$ARGUMENTS"\n')
        self.package()
        installer_bytes = (self.assets / "install.sh").read_bytes()
        tools = self.root / "mirror-tools"
        tools.mkdir()
        (tools / "uname").write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n')
        (tools / "curl").write_text("""#!/bin/sh
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) output=$2; shift ;;
    https://*) url=$1 ;;
  esac
  shift
done
printf '%s\\n' "$url" >> "$URLS"
case "$url" in
  */install.sh) cp "$INSTALLER" "$output" ;;
  *) cp "$FIXTURE" "$output" ;;
esac
""")
        for tool in tools.iterdir():
            tool.chmod(0o755)
        urls = self.root / "urls"
        arguments = self.root / "arguments"
        home = self.root / "mirror-home"
        home.mkdir()
        mirror = "https://example.com/staging/candidate-123"
        env = dict(os.environ, HOME=str(home), PATH=str(tools) + os.pathsep + os.environ["PATH"],
                   RELIABURGER_RELEASE_BASE_URL=mirror, FIXTURE=str(binary), URLS=str(urls),
                   ARGUMENTS=str(arguments), INSTALLER=str(self.assets / "install.sh"))
        bootstrap = BOOTSTRAP
        for shell, script in [(shell, script) for shell in POSIX_SHELLS for script in [self.assets / "install.sh", bootstrap]]:
            with self.subTest(shell=shell, script=script):
                urls.unlink(missing_ok=True)
                arguments.unlink(missing_ok=True)
                result = subprocess.run([shell, str(script), "--nodes", "1"], env=env, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                expected = [mirror + "/relish-macos-aarch64"]
                if script == bootstrap:
                    expected.insert(0, mirror + "/install.sh")
                self.assertEqual(urls.read_text().splitlines(), expected)
                self.assertEqual(arguments.read_text().splitlines(),
                                 ["setup", "--quickstart", "--release-mirror", mirror, "--nodes", "1"])
                self.assertEqual((self.assets / "install.sh").read_bytes(), installer_bytes)
                for invalid in ["http://example.com", "https://user:pass@example.com", "https://example.com/?key=x", "https://example.com/#fragment", "https://example.com/a b"]:
                    urls.unlink(missing_ok=True)
                    result = subprocess.run([shell, str(script), "--install-only"],
                                            env=dict(env, RELIABURGER_RELEASE_BASE_URL=invalid), capture_output=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertFalse(urls.exists(), "invalid mirror reached curl")

    def test_bootstrap_runs_when_piped_to_sh(self):
        import os
        tools = self.root / "pipe-tools"
        tools.mkdir()
        (tools / "curl").write_text("""#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = -o ]; then printf '%s\\n' 'printf "installer:%s\\n" "$@"' > "$2"; exit 0; fi
  shift
done
exit 1
""")
        (tools / "curl").chmod(0o755)
        env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"])
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell):
                result = subprocess.run([shell, "-s", "--", "--nodes", "1"], input=BOOTSTRAP.read_bytes(), env=env, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.decode().splitlines(), ["installer:--nodes", "installer:1"])
                for invalid in ["v0.1", "0.1.0", "v0.1.0\nv0.1.0", "v0.1.0;id", "v0.1.0 "]:
                    result = subprocess.run([shell, str(BOOTSTRAP)], env=dict(env, RELIABURGER_VERSION=invalid), capture_output=True)
                    self.assertNotEqual(result.returncode, 0, invalid)
                    self.assertIn(b"invalid RELIABURGER_VERSION", result.stderr)

    def test_installers_are_posix_sh(self):
        self.package()
        scripts = [self.assets / "install.sh", BOOTSTRAP]
        for script in scripts:
            text = script.read_text()
            self.assertTrue(text.startswith("#!/bin/sh\n"), script)
            code = "\n".join(line for line in text.splitlines() if not line.lstrip().startswith("#"))
            for bashism in ("[[", "=~", "pipefail", "local ", "function ", "<<<", "declare "):
                self.assertNotIn(bashism, code, f"{script} uses {bashism!r}")
            for shell in POSIX_SHELLS:
                result = subprocess.run([shell, "-n", str(script)], capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
        if shutil.which("shellcheck"):
            result = subprocess.run(["shellcheck", "-s", "sh", *map(str, scripts)], capture_output=True)
            self.assertEqual(result.returncode, 0, result.stdout)

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
