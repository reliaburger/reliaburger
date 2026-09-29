"""Release packaging tests use ephemeral keys, never the release secret."""
import base64
import hashlib
import json
from pathlib import Path
import subprocess
import shutil
import tempfile
import unittest

from package import GUEST_METADATA, guest_image_statement, package_release

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
        self.pins = json.loads((Path(__file__).with_name("guest-images.json")).read_text())
        for image in self.pins["images"].values():
            (self.assets / image["asset"]).write_bytes(b"test image: " + image["asset"].encode())

    def package(self):
        package_release(self.assets, "v0.1.0", "reliaburger/reliaburger", self.key, self.trusted, self.pins)

    def verify_signature(self, data, encoded):
        message, signature = self.root / "message", self.root / "signature"
        message.write_bytes(data)
        signature.write_bytes(base64.b64decode(encoded))
        return subprocess.run(["openssl", "pkeyutl", "-verify", "-rawin", "-keyform", "DER", "-inkey", str(self.key),
                               "-in", str(message), "-sigfile", str(signature)], capture_output=True).returncode == 0

    def test_statement_text_matches_the_cli(self):
        # artifacts.rs asserts the same text for the same inputs.
        self.assertEqual(guest_image_statement("v0.1.0", "aarch64", "guest.qcow2", "ab", "cd"),
                         b"reliaburger guest image v1\nversion v0.1.0\narch aarch64\nasset guest.qcow2\n"
                         b"sha256 ab\nsource-sha256 cd\n")

    def test_guest_images_are_signed_with_their_pinned_source(self):
        self.package()
        metadata = json.loads((self.assets / GUEST_METADATA).read_text())
        self.assertEqual(metadata["schema"], 1)
        self.assertEqual(metadata["version"], "v0.1.0")
        self.assertEqual(set(metadata["images"]), set(self.pins["images"]))
        sums = (self.assets / "SHA256SUMS").read_text()
        for arch, pin in self.pins["images"].items():
            image = metadata["images"][arch]
            path = self.assets / pin["asset"]
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            self.assertEqual(image["sha256"], digest)
            self.assertEqual(image["size"], path.stat().st_size)
            self.assertEqual(image["source"], {"url": pin["source"]["url"], "sha256": pin["source"]["sha256"]})
            self.assertIn(f"{digest}  {pin['asset']}\n", sums)
            statement = guest_image_statement("v0.1.0", arch, pin["asset"], digest, pin["source"]["sha256"])
            self.assertTrue(self.verify_signature(statement, image["signature"]))
            forged = guest_image_statement("v0.1.0", arch, pin["asset"], "0" * 64, pin["source"]["sha256"])
            self.assertFalse(self.verify_signature(forged, image["signature"]))

    def test_missing_guest_image_cannot_publish_metadata(self):
        (self.assets / self.pins["images"]["x86_64"]["asset"]).unlink()
        with self.assertRaises(ValueError):
            self.package()
        self.assertFalse((self.assets / "metadata.json").exists())
        self.assertFalse((self.assets / GUEST_METADATA).exists())

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

    def flaky_tools(self, name, chunk):
        """Fake `curl` that delivers `chunk` more bytes per call and fails
        with exit 18 (partial file) until the whole fixture has arrived. Each
        call must ask to continue, or it never gets past the first chunk.
        The installer script itself always arrives in three pieces.
        `sleep` is a no-op, so retries don't slow the test down."""
        tools = self.root / name
        tools.mkdir()
        (tools / "uname").write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n')
        (tools / "sleep").write_text("#!/bin/sh\n")
        (tools / "curl").write_text(f"""#!/bin/sh
resume=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    --continue-at) resume=true; shift ;;
    -o) output=$2; shift ;;
    https://*) url=$1 ;;
  esac
  shift
done
printf '%s\\n' "$url" >> "$URLS"
# The installer itself arrives in three pieces, the binary in `chunk`s.
case "$url" in
  */install.sh) source=$INSTALLER; piece=$(($(wc -c < "$INSTALLER") / 3 + 1)) ;;
  *) source=$FIXTURE; piece={chunk} ;;
esac
[ "$resume" = true ] && [ -f "$output" ] || : > "$output"
have=$(wc -c < "$output")
tail -c +$((have + 1)) "$source" | head -c "$piece" >> "$output"
[ "$(wc -c < "$output")" -eq "$(wc -c < "$source")" ] || exit 18
""")
        for tool in tools.iterdir():
            tool.chmod(0o755)
        return tools

    def test_installers_resume_a_dropped_download(self):
        import os
        self.package()
        genuine = (self.assets / "relish-macos-aarch64").read_bytes()
        tools = self.flaky_tools("flaky-tools", 12)
        urls = self.root / "urls"
        home = self.root / "flaky-home"
        home.mkdir()
        env = dict(os.environ, HOME=str(home), PATH=str(tools) + os.pathsep + os.environ["PATH"],
                   URLS=str(urls), FIXTURE=str(self.assets / "relish-macos-aarch64"),
                   INSTALLER=str(self.assets / "install.sh"), RELIABURGER_RELEASE_BASE_URL="https://example.com/r")
        for shell, script in [(shell, script) for shell in POSIX_SHELLS for script in [self.assets / "install.sh", BOOTSTRAP]]:
            with self.subTest(shell=shell, script=script):
                urls.unlink(missing_ok=True)
                result = subprocess.run([shell, str(script), "--install-only"], env=env, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((home / ".reliaburger/bin/relish").read_bytes(), genuine)
                self.assertIn(b"resuming, attempt 2 of 5", result.stderr)
                # The binary arrives in 12-byte pieces; every retry asks for the original URL.
                pieces = -(-len(genuine) // 12)
                self.assertEqual(urls.read_text().splitlines().count("https://example.com/r/relish-macos-aarch64"), pieces)

    def test_installer_gives_up_on_a_download_that_never_finishes(self):
        import os
        self.package()
        tools = self.flaky_tools("stuck-tools", 1)
        home = self.root / "stuck-home"
        home.mkdir()
        env = dict(os.environ, HOME=str(home), PATH=str(tools) + os.pathsep + os.environ["PATH"],
                   URLS=str(self.root / "urls"), FIXTURE=str(self.assets / "relish-macos-aarch64"),
                   INSTALLER=str(self.assets / "install.sh"))
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell):
                result = subprocess.run([shell, str(self.assets / "install.sh"), "--install-only"], env=env, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(b"attempt 5 of 5", result.stderr)
                self.assertIn(b"could not download", result.stderr)
                self.assertFalse((home / ".reliaburger/bin/relish").exists())

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
            package_release(self.assets, "../main", "reliaburger/reliaburger", self.key, self.trusted, self.pins)


if __name__ == "__main__":
    unittest.main()


class ReleaseKeyFormatTests(unittest.TestCase):
    """The secret may be set from DER, a PEM file or a bare seed; all must sign."""

    def setUp(self):
        from package import release_key_der

        self.release_key_der = release_key_der
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        der = self.root / "key.der"
        subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-outform", "DER", "-out", str(der)], check=True, capture_output=True)
        self.der = der.read_bytes()
        self.pem = subprocess.run(["openssl", "pkey", "-inform", "DER", "-in", str(der), "-outform", "PEM"], check=True, capture_output=True).stdout
        self.public = self.public_key(self.der)

    def public_key(self, der):
        path = self.root / "check.der"
        path.write_bytes(der)
        return subprocess.run(["openssl", "pkey", "-inform", "DER", "-in", str(path), "-pubout", "-outform", "DER"], check=True, capture_output=True).stdout

    def converted(self, raw):
        scratch = Path(tempfile.mkdtemp(dir=self.root))
        return self.release_key_der(base64.b64encode(raw).decode(), scratch)

    def test_der_pem_and_seed_all_yield_the_same_key(self):
        self.assertEqual(self.public_key(self.converted(self.der)), self.public)
        self.assertEqual(self.public_key(self.converted(self.pem)), self.public)
        self.assertEqual(self.public_key(self.converted(self.der[-32:])), self.public)

    def test_wrapped_base64_is_accepted(self):
        encoded = base64.encodebytes(self.pem).decode()
        self.assertIn("\n", encoded)
        scratch = Path(tempfile.mkdtemp(dir=self.root))
        self.assertEqual(self.public_key(self.release_key_der(encoded, scratch)), self.public)

    def test_unrecognised_key_fails_without_revealing_it(self):
        junk = b"not a key at all, just some text!"
        with self.assertRaises(ValueError) as caught:
            self.converted(junk)
        self.assertIn("33 bytes", str(caught.exception))
        self.assertNotIn("not a key", str(caught.exception))

    def test_invalid_base64_is_named(self):
        scratch = Path(tempfile.mkdtemp(dir=self.root))
        with self.assertRaises(ValueError) as caught:
            self.release_key_der("***", scratch)
        self.assertIn("not valid base64", str(caught.exception))
