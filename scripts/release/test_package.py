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
        for name in ("bun-linux-aarch64", "bun-linux-x86_64", "relish-linux-aarch64", "relish-linux-x86_64", "relish-macos-aarch64"):
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

    def test_metadata_json_carries_the_compatibility_pair(self):
        # `relish upgrade check` compares this pair with the cluster's: a
        # release with another pair needs a fresh cluster, not a rolling upgrade.
        self.package()
        source = (Path(__file__).resolve().parents[2] / "src/compatibility.rs").read_text()
        block = source[source.index("pub const CURRENT"):]
        expected = {
            "protocol": int(block.split("protocol:")[1].split(",")[0]),
            "state": int(block.split("state:")[1].split(",")[0]),
        }
        bun = json.loads((self.assets / "metadata.json").read_text())
        self.assertEqual(bun["releases"][0]["compatibility"], expected)

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
                result = subprocess.run([shell, str(script), "--laptop", "--nodes", "1"], env=env, capture_output=True)
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

    # --- Choosing what to install (#670) ------------------------------------

    def arguments_tools(self, name):
        """`recording_tools` plus a relish that writes its arguments to
        $ARGUMENTS, so a test can see what the installer asked it to do."""
        tools = self.recording_tools(name)
        (self.assets / "relish-macos-aarch64").write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$ARGUMENTS"\n')
        self.package()
        return tools

    def test_installer_without_a_mode_explains_the_choices_and_changes_nothing(self):
        tools = self.arguments_tools("choices-tools")
        for shell, script in [(shell, script) for shell in POSIX_SHELLS for script in [self.assets / "install.sh", BOOTSTRAP]]:
            with self.subTest(shell=shell, script=script.parent.name):
                home = self.root / f"choices-home-{shell}-{script.parent.name}"
                home.mkdir()
                calls, env = self.download_env(tools, home)
                result = subprocess.run([shell, str(script)], env=env, capture_output=True)
                # Non-zero: an old `curl | sh && relish ...` script must not
                # carry on as if it had a cluster.
                self.assertEqual(result.returncode, 2, result.stderr)
                choices = result.stdout.decode()
                for promise in ("--laptop", "Lima", "docs/linux-servers.md", "relish init", "relish join",
                                "--install-only", "Netboot", "not for production",
                                "curl -fsSL https://reliaburger.com/install.sh | sh -s -- --laptop"):
                    self.assertIn(promise, choices)
                self.assertIn("Nothing was installed", choices)
                self.assertFalse(calls.exists(), "the installer downloaded something without a mode")
                self.assertFalse((home / ".reliaburger").exists(), "the installer made a store without a mode")

    def test_bootstrap_and_installer_explain_the_choices_in_the_same_words(self):
        def explanation(text):
            start = text.index("explain_choices() {")
            return text[start:text.index("\n}\n", start)]
        self.assertEqual(explanation(BOOTSTRAP.read_text()),
                         explanation(Path(__file__).with_name("install.sh.in").read_text()))

    def test_installer_help_shows_the_choices_and_succeeds(self):
        tools = self.arguments_tools("help-tools")
        home = self.root / "help-home"
        home.mkdir()
        for shell in POSIX_SHELLS:
            for flag in ("--help", "-h"):
                with self.subTest(shell=shell, flag=flag):
                    calls, env = self.download_env(tools, home)
                    result = subprocess.run([shell, str(self.assets / "install.sh"), flag], env=env, capture_output=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("--laptop", result.stdout.decode())
                    self.assertFalse(calls.exists())
                    self.assertFalse((home / ".reliaburger").exists())

    def test_laptop_installs_relish_and_builds_the_laptop_cluster(self):
        tools = self.arguments_tools("laptop-tools")
        arguments = self.root / "laptop-arguments"
        mirror = "https://example.com/r"
        cases = {
            ("--laptop",): [],
            ("--laptop", "--nodes", "1"): ["--nodes", "1"],
            ("--no-modify-path", "--laptop", "--nodes", "1"): ["--nodes", "1"],
            ("--laptop", "--no-modify-path", "--nodes", "1"): ["--nodes", "1"],
        }
        for shell in POSIX_SHELLS:
            for number, (given, forwarded) in enumerate(cases.items()):
                with self.subTest(shell=shell, arguments=given):
                    home = self.root / f"laptop-home-{shell}-{number}"
                    home.mkdir()
                    arguments.unlink(missing_ok=True)
                    _, env = self.download_env(tools, home)
                    env = dict(env, ARGUMENTS=str(arguments))
                    result = subprocess.run([shell, str(self.assets / "install.sh"), *given], env=env, capture_output=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(arguments.read_text().splitlines(),
                                     ["setup", "--quickstart", "--release-mirror", mirror, *forwarded])

    def test_installer_refuses_an_unknown_option_with_usage(self):
        tools = self.arguments_tools("unknown-tools")
        cases = [
            ["--bogus"],
            # Setup options go after --laptop, so alone they're unknown.
            ["--nodes", "1"],
            ["--install-only", "--nodes", "1"],
            ["--laptop", "--install-only"],
            ["--install-only", "--laptop"],
        ]
        for shell in POSIX_SHELLS:
            for number, given in enumerate(cases):
                with self.subTest(shell=shell, arguments=given):
                    home = self.root / f"unknown-home-{shell}-{number}"
                    home.mkdir()
                    calls, env = self.download_env(tools, home)
                    result = subprocess.run([shell, str(self.assets / "install.sh"), *given], env=env, capture_output=True)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    stderr = result.stderr.decode()
                    self.assertIn("usage:", stderr)
                    self.assertIn("--laptop", stderr)
                    self.assertFalse(calls.exists(), "a refused option reached a download")
                    self.assertFalse((home / ".reliaburger").exists())

    def test_bootstrap_drops_laptop_for_releases_from_before_it(self):
        """0.2.1 and older build the laptop cluster without a flag and would
        hand --laptop to `relish setup`, which refuses it. The Intel Mac
        advice installs 0.2.1 this way."""
        import os
        tools = self.root / "old-release-tools"
        tools.mkdir()
        (tools / "curl").write_text("""#!/bin/sh
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) printf '%s\\n' 'printf "installer:%s\\n" "$@"' > "$2"; shift ;;
    https://*) printf '%s\\n' "$1" >> "$URLS" ;;
  esac
  shift
done
""")
        (tools / "curl").chmod(0o755)
        urls = self.root / "old-release-urls"
        env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"], URLS=str(urls))
        env.pop("RELIABURGER_RELEASE_BASE_URL", None)
        cases = {
            "v0.1.6": ["--nodes", "1"],
            "v0.2.1": ["--nodes", "1"],
            "v0.2.2": ["--laptop", "--nodes", "1"],
            "v0.2.2-rc.1": ["--laptop", "--nodes", "1"],
            "v0.3.0": ["--laptop", "--nodes", "1"],
            "v1.0.0": ["--laptop", "--nodes", "1"],
        }
        for shell in POSIX_SHELLS:
            for version, forwarded in cases.items():
                with self.subTest(shell=shell, version=version):
                    urls.unlink(missing_ok=True)
                    result = subprocess.run([shell, "-s", "--", "--laptop", "--nodes", "1"], input=BOOTSTRAP.read_bytes(),
                                            env=dict(env, RELIABURGER_VERSION=version), capture_output=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.decode().splitlines(), [f"installer:{a}" for a in forwarded])
                    self.assertEqual(urls.read_text().splitlines(),
                                     [f"https://github.com/reliaburger/reliaburger/releases/download/{version}/install.sh"])
            with self.subTest(shell=shell, mirror=True):
                # A mirror serves a candidate of this release, which knows the flag.
                result = subprocess.run([shell, str(BOOTSTRAP), "--laptop"], capture_output=True,
                                        env=dict(env, RELIABURGER_VERSION="v0.2.1",
                                                 RELIABURGER_RELEASE_BASE_URL="https://example.com/r"))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.decode().splitlines(), ["installer:--laptop"])

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

    def recording_tools(self, name):
        """Fake `curl` that delivers each file whole and appends its
        arguments to $CALLS. With --progress-bar it draws a bar on stderr,
        as the real one does, so a test can see where the bar went."""
        tools = self.root / name
        tools.mkdir()
        (tools / "uname").write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n')
        (tools / "curl").write_text("""#!/bin/sh
printf '%s\\n' "$*" >> "$CALLS"
bar=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    --progress-bar) bar=true ;;
    -o) output=$2; shift ;;
    https://*) url=$1 ;;
  esac
  shift
done
case "$url" in
  */install.sh) cp "$INSTALLER" "$output" ;;
  *) cp "$FIXTURE" "$output" ;;
esac
[ "$bar" = false ] || printf '######################################## 100.0%%\\r' >&2
""")
        for tool in tools.iterdir():
            tool.chmod(0o755)
        return tools

    def download_env(self, tools, home):
        import os
        calls = self.root / (tools.name + "-calls")
        calls.unlink(missing_ok=True)
        return calls, dict(os.environ, HOME=str(home), PATH=str(tools) + os.pathsep + os.environ["PATH"],
                           CALLS=str(calls), FIXTURE=str(self.assets / "relish-macos-aarch64"),
                           INSTALLER=str(self.assets / "install.sh"), RELIABURGER_NO_MODIFY_PATH="1",
                           RELIABURGER_RELEASE_BASE_URL="https://example.com/r")

    def run_with_terminal_stderr(self, command, env):
        """Run `command` with stderr on a pseudo-terminal, as `curl | sh`
        typed at a prompt has it. Returns (exit code, terminal output)."""
        import os
        import pty
        controller, terminal = pty.openpty()
        try:
            process = subprocess.Popen(command, env=env, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.DEVNULL, stderr=terminal)
            os.close(terminal)
            terminal = None
            seen = b""
            while True:
                try:
                    chunk = os.read(controller, 4096)
                except OSError:
                    break  # Linux reports EIO once the last writer has gone
                if not chunk:
                    break
                seen += chunk
            return process.wait(), seen.replace(b"\r\n", b"\n")
        finally:
            os.close(controller)
            if terminal is not None:
                os.close(terminal)

    def test_installers_announce_each_download_in_plain_lines_without_a_terminal(self):
        import re
        self.package()
        size = (self.assets / "relish-macos-aarch64").stat().st_size
        tools = self.recording_tools("plain-tools")
        home = self.root / "plain-home"
        home.mkdir()
        for shell, script in [(shell, script) for shell in POSIX_SHELLS for script in [self.assets / "install.sh", BOOTSTRAP]]:
            with self.subTest(shell=shell, script=script):
                calls, env = self.download_env(tools, home)
                result = subprocess.run([shell, str(script), "--install-only"], env=env, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                stderr = result.stderr.decode()
                self.assertIn(f"Downloading relish v0.1.0 for macos-aarch64 ({size} B)...\n", stderr)
                self.assertRegex(stderr, rf"Downloaded {size} bytes in \d+ s\n")
                if script == BOOTSTRAP:
                    version = re.search(r"RELIABURGER_VERSION:-(v[^}]+)}", BOOTSTRAP.read_text()).group(1)
                    self.assertIn(f"Downloading the Reliaburger {version} installer from https://example.com/r...\n", stderr)
                    self.assertEqual(len(re.findall(r"Downloaded \d+ bytes", stderr)), 2)
                # CI logs get lines, not a bar redrawn with carriage returns.
                self.assertNotIn("\r", stderr)
                for call in calls.read_text().splitlines():
                    self.assertIn("--silent", call.split())
                    self.assertNotIn("--progress-bar", call.split())

    def test_installers_show_curl_progress_bar_on_a_terminal(self):
        self.package()
        size = (self.assets / "relish-macos-aarch64").stat().st_size
        tools = self.recording_tools("terminal-tools")
        home = self.root / "terminal-home"
        home.mkdir()
        for shell, script in [(shell, script) for shell in POSIX_SHELLS for script in [self.assets / "install.sh", BOOTSTRAP]]:
            with self.subTest(shell=shell, script=script):
                calls, env = self.download_env(tools, home)
                status, seen = self.run_with_terminal_stderr([shell, str(script), "--install-only"], env)
                self.assertEqual(status, 0, seen)
                seen = seen.decode()
                self.assertIn(f"Downloading relish v0.1.0 for macos-aarch64 ({size} B)...", seen)
                self.assertIn("100.0%", seen)
                self.assertRegex(seen, rf"Downloaded {size} bytes in \d+ s")
                for call in calls.read_text().splitlines():
                    self.assertIn("--progress-bar", call.split())
                    self.assertNotIn("--silent", call.split())

    def test_installers_name_each_resume_and_report_the_whole_file(self):
        import os
        import re
        self.package()
        size = (self.assets / "relish-macos-aarch64").stat().st_size
        tools = self.flaky_tools("resume-tools", 12)
        home = self.root / "resume-home"
        home.mkdir()
        env = dict(os.environ, HOME=str(home), PATH=str(tools) + os.pathsep + os.environ["PATH"],
                   URLS=str(self.root / "resume-urls"), FIXTURE=str(self.assets / "relish-macos-aarch64"),
                   INSTALLER=str(self.assets / "install.sh"), RELIABURGER_NO_MODIFY_PATH="1",
                   RELIABURGER_RELEASE_BASE_URL="https://example.com/r")
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell):
                result = subprocess.run([shell, str(self.assets / "install.sh"), "--install-only"], env=env, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                stderr = result.stderr.decode()
                self.assertIn("reliaburger: download interrupted (curl exit 18) after 12 B; resuming, attempt 2 of 5\n", stderr)
                self.assertIn("reliaburger: download interrupted (curl exit 18) after 24 B; resuming, attempt 3 of 5\n", stderr)
                # The summary counts the whole file, not just the last piece.
                self.assertEqual(re.findall(r"Downloaded (\d+) bytes in \d+ s", stderr), [str(size)])

    def test_qualification_still_parses_installer_output(self):
        """qualify-staged-install.sh tees `curl | sh` 2>&1 into install.log,
        then greps it for the mirror notice and quotes setup's timing summary.
        The download lines must not break either."""
        qualify = (Path(__file__).with_name("qualify-staged-install.sh")).read_text()
        mirror_check = "grep -q 'using an explicit release mirror' \"$evidence/install.log\""
        summary_quote = "sed -n '/^where the time went/,$p' \"$evidence/install.log\""
        self.assertIn(mirror_check, qualify)
        self.assertIn(summary_quote, qualify)
        # Without a mode the installer only explains the choices (#670).
        self.assertIn("sh -s -- --laptop --timings", qualify)
        relish_output = ("using an explicit release mirror with checksum and signature verification\n"
                         "... check host\n"
                         "where the time went\n"
                         "  download 3.0 s\n")
        binary = self.assets / "relish-macos-aarch64"
        binary.write_text("#!/bin/sh\nprintf '%s' '" + relish_output + "'\n")
        self.package()
        tools = self.recording_tools("qualify-tools")
        home = self.root / "qualify-home"
        home.mkdir()
        evidence = self.root / "evidence"
        evidence.mkdir()
        log = evidence / "install.log"
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell):
                _, env = self.download_env(tools, home)
                with log.open("wb") as output:
                    result = subprocess.run([shell, str(BOOTSTRAP), "--laptop", "--timings"], env=env,
                                            stdout=output, stderr=subprocess.STDOUT)
                self.assertEqual(result.returncode, 0, log.read_text())
                env = dict(env, evidence=str(evidence))
                self.assertEqual(subprocess.run(["sh", "-c", mirror_check], env=env).returncode, 0)
                quoted = subprocess.run(["sh", "-c", summary_quote], env=env, capture_output=True, check=True)
                self.assertEqual(quoted.stdout.decode(), "where the time went\n  download 3.0 s\n")
                text = log.read_text()
                self.assertIn("Downloading relish v0.1.0 for macos-aarch64", text)
                self.assertNotIn("\r", text)

    def test_both_installers_download_with_the_same_helpers(self):
        def helpers(text):
            code = text[text.index("human_size() {"):text.index("\n}\n", text.index("download() {"))]
            return [line for line in code.splitlines() if not line.lstrip().startswith("#")]
        self.assertEqual(helpers(BOOTSTRAP.read_text()),
                         helpers(Path(__file__).with_name("install.sh.in").read_text()))

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

    def test_cli_is_built_for_apple_silicon_only_on_macos(self):
        # Maintainer decision, 17 October 2026 (#1018): 0.2.1 is the last
        # release with an Intel macOS build.
        self.package()
        cli = json.loads((self.assets / "cli-metadata.json").read_text())
        self.assertEqual(set(cli["releases"][0]["platforms"]), {"linux-aarch64", "linux-x86_64", "macos-aarch64"})
        self.assertFalse((self.assets / "relish-macos-x86_64.sig").exists())
        self.assertNotIn("macos-x86_64", (self.assets / "SHA256SUMS").read_text())
        installer = (self.assets / "install.sh").read_text()
        self.assertNotIn("@MACOS_X86_64", installer)
        self.assertNotIn("platform=macos-x86_64", installer)

    def intel_mac_tools(self, name, sysctl):
        """A Mac whose uname says x86_64, with `sysctl` given by the caller."""
        tools = self.root / name
        tools.mkdir()
        (tools / "uname").write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo x86_64;; esac\n')
        (tools / "sysctl").write_text("#!/bin/sh\n" + sysctl)
        (tools / "curl").write_text('#!/bin/sh\nfor argument; do printf "%s\\n" "$argument" >>"$CALLS"; done\n'
                                    'while [ "$#" -gt 0 ]; do if [ "$1" = -o ]; then cp "$FIXTURE" "$2"; exit; fi; shift; done\nexit 1\n')
        for tool in tools.iterdir():
            tool.chmod(0o755)
        return tools

    def test_installer_refuses_an_intel_mac_and_says_what_to_use(self):
        import os
        self.package()
        installer = self.assets / "install.sh"
        intel = {
            # An Intel Mac has no hw.optional.arm64 at all.
            "missing": 'echo "sysctl: unknown oid \'hw.optional.arm64\'" >&2\nexit 1\n',
            "zero": 'echo 0\n',
        }
        for shell in POSIX_SHELLS:
            for case, sysctl in intel.items():
                with self.subTest(shell=shell, sysctl=case):
                    tools = self.intel_mac_tools(f"intel-{shell}-{case}", sysctl)
                    home = self.root / f"intel-home-{shell}-{case}"
                    home.mkdir()
                    calls = self.root / f"intel-calls-{shell}-{case}"
                    env = dict(os.environ, HOME=str(home), PATH=str(tools) + os.pathsep + os.environ["PATH"],
                               CALLS=str(calls), FIXTURE=str(self.assets / "relish-macos-aarch64"))
                    for arguments in (["--install-only"], ["--laptop"], [], ["--bogus"]):
                        result = subprocess.run([shell, str(installer), *arguments], env=env, capture_output=True)
                        self.assertEqual(result.returncode, 1, arguments)
                        stderr = result.stderr.decode()
                        self.assertIn("Intel Mac", stderr)
                        self.assertIn("Apple silicon", stderr)
                        self.assertIn("Linux", stderr)
                        self.assertIn("RELIABURGER_VERSION=v0.2.1 sh -s -- --laptop", stderr)
                        self.assertEqual(result.stdout, b"", "an Intel Mac was shown the choices")
                        self.assertFalse(calls.exists(), "an Intel Mac reached a download")
                        self.assertFalse((home / ".reliaburger").exists(), "an Intel Mac got a store")

    def test_installer_gives_apple_silicon_under_rosetta_the_native_build(self):
        # A shell running under Rosetta sees `uname -m` say x86_64 on an
        # Apple silicon Mac; the hardware still says arm64.
        import os
        self.package()
        installer = self.assets / "install.sh"
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell):
                tools = self.intel_mac_tools(f"rosetta-{shell}", 'echo 1\n')
                home = self.root / f"rosetta-home-{shell}"
                home.mkdir()
                calls = self.root / f"rosetta-calls-{shell}"
                env = dict(os.environ, HOME=str(home), PATH=str(tools) + os.pathsep + os.environ["PATH"],
                           CALLS=str(calls), FIXTURE=str(self.assets / "relish-macos-aarch64"))
                result = subprocess.run([shell, str(installer), "--install-only"], env=env, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("https://github.com/reliaburger/reliaburger/releases/download/v0.1.0/relish-macos-aarch64",
                              calls.read_text().splitlines())
                self.assertEqual((home / ".reliaburger/bin/relish").read_bytes(),
                                 (self.assets / "relish-macos-aarch64").read_bytes())

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
