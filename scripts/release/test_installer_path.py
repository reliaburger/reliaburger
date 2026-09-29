"""How the generated installer puts `relish` on PATH (decision D6).

The installer runs against a fake HOME with fake `curl` and `uname`, under
every POSIX shell present. Prompts go to /dev/tty, so the interactive cases
give the installer a pseudo-terminal as its controlling terminal.
"""
import base64
import fcntl
import os
from pathlib import Path
import pty
import select
import shutil
import subprocess
import tempfile
import termios
import unittest

from package import package_release

POSIX_SHELLS = [shell for shell in ("sh", "dash") if shutil.which(shell)]
STORE_LINE = 'export PATH="$HOME/.reliaburger/bin:$PATH"'


class InstallerPathTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        key = self.root / "key.der"
        subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519", "-outform", "DER", "-out", str(key)], check=True, capture_output=True)
        public = subprocess.run(["openssl", "pkey", "-inform", "DER", "-in", str(key), "-pubout", "-outform", "DER"], check=True, capture_output=True).stdout
        assets = self.root / "assets"
        assets.mkdir()
        for name in ("bun-linux-aarch64", "bun-linux-x86_64", "relish-linux-aarch64", "relish-linux-x86_64", "relish-macos-aarch64", "relish-macos-x86_64"):
            (assets / name).write_bytes(b"test binary: " + name.encode())
        pins = {"packages": ["runc"], "images": {"fixture": {"asset": "guest.qcow2", "source": {
            "file": "ubuntu.img", "url": "https://images.example/ubuntu.img", "sha256": "5" * 64}}}}
        (assets / "guest.qcow2").write_bytes(b"test image")
        package_release(assets, "v0.1.0", "reliaburger/reliaburger", key,
                        ["ed25519:" + base64.b64encode(public[-32:]).decode()], pins)
        self.installer = assets / "install.sh"
        self.tools = self.root / "tools"
        self.tools.mkdir()
        (self.tools / "uname").write_text('#!/bin/sh\ncase "$1" in -s) echo "$FAKE_SYSTEM";; -m) echo arm64;; esac\n')
        (self.tools / "curl").write_text('#!/bin/sh\nwhile [ "$#" -gt 0 ]; do if [ "$1" = -o ]; then cp "$FIXTURE" "$2"; exit; fi; shift; done\nexit 1\n')
        for tool in self.tools.iterdir():
            tool.chmod(0o755)
        self.fixture = assets / "relish-macos-aarch64"
        self.homes = 0

    def fresh_home(self):
        self.homes += 1
        home = self.root / f"home-{self.homes}"
        home.mkdir()
        return home

    def env(self, home, shell_path="/bin/zsh", extra_path=(), **overrides):
        path = os.pathsep.join([*map(str, extra_path), str(self.tools), "/usr/bin", "/bin"])
        env = {"HOME": str(home), "PATH": path, "SHELL": shell_path, "FAKE_SYSTEM": "Darwin", "FIXTURE": str(self.fixture if overrides.get("FAKE_SYSTEM", "Darwin") == "Darwin" else self.fixture.with_name("relish-linux-aarch64"))}
        env.update(overrides)
        return env

    def run_installer(self, shell, env, *args):
        return subprocess.run([shell, str(self.installer), "--install-only", *args], env=env, capture_output=True, stdin=subprocess.DEVNULL)

    def run_with_terminal(self, shell, env, answer):
        """Run with a controlling terminal whose keyboard types `answer`."""
        controller, terminal = pty.openpty()

        def attach_terminal():
            os.setsid()
            fcntl.ioctl(terminal, termios.TIOCSCTTY, 0)

        # stdin is a pipe, exactly as with `curl ... | sh`.
        process = subprocess.Popen([shell, str(self.installer), "--install-only"], env=env,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   preexec_fn=attach_terminal, pass_fds=(terminal,))
        os.close(terminal)
        seen = b""
        while b"[y/N]" not in seen:
            ready, _, _ = select.select([controller], [], [], 10)
            if not ready:
                process.kill()
                self.fail(f"no prompt on the terminal; saw {seen!r}")
            seen += os.read(controller, 1024)
        os.write(controller, answer.encode() + b"\n")
        process.stdin.close()
        # Keep draining the terminal: closing a terminal waits for its unread
        # output (here the echoed answer), so an undrained one hangs the exit.
        deadline = 100
        while process.poll() is None and deadline:
            deadline -= 1
            ready, _, _ = select.select([controller], [], [], 0.1)
            if ready:
                try:
                    seen += os.read(controller, 1024)
                except OSError:
                    pass
        process.wait(timeout=10)
        stdout, stderr = process.stdout.read(), process.stderr.read()
        process.stdout.close()
        process.stderr.close()
        os.close(controller)
        return process.returncode, stdout.decode(), stderr.decode(), seen.decode()

    def test_links_into_local_bin_when_it_is_on_path(self):
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell):
                home = self.fresh_home()
                env = self.env(home, extra_path=[home / ".local/bin"])
                result = self.run_installer(shell, env)
                self.assertEqual(result.returncode, 0, result.stderr)
                link = home / ".local/bin/relish"
                self.assertTrue(link.is_symlink())
                self.assertEqual(os.readlink(link), str(home / ".reliaburger/bin/relish"))
                self.assertIn(b"Linked", result.stdout)
                self.assertNotIn(b"not on your PATH", result.stdout)
                # Re-running keeps our link and edits nothing else.
                result = self.run_installer(shell, env)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(os.readlink(link), str(home / ".reliaburger/bin/relish"))
                self.assertFalse((home / ".zshrc").exists())

    def test_leaves_someone_elses_relish_in_local_bin_alone(self):
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell):
                home = self.fresh_home()
                (home / ".local/bin").mkdir(parents=True)
                (home / ".local/bin/relish").write_text("mine")
                result = self.run_installer(shell, self.env(home, extra_path=[home / ".local/bin"]))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((home / ".local/bin/relish").read_text(), "mine")
                self.assertIn(b"left", result.stderr)
                self.assertIn(STORE_LINE.encode(), result.stdout)

    def test_prints_the_line_for_each_shell_without_editing(self):
        cases = [
            ("/bin/zsh", "Darwin", ".zshrc", STORE_LINE),
            ("/bin/bash", "Darwin", ".bash_profile", STORE_LINE),
            ("/usr/bin/bash", "Linux", ".bashrc", STORE_LINE),
            ("/usr/bin/fish", "Darwin", ".config/fish/config.fish", 'fish_add_path "$HOME/.reliaburger/bin"'),
            ("/bin/ksh", "Darwin", ".profile", STORE_LINE),
        ]
        for shell in POSIX_SHELLS:
            for login_shell, system, rc, line in cases:
                with self.subTest(shell=shell, login_shell=login_shell):
                    home = self.fresh_home()
                    result = self.run_installer(shell, self.env(home, shell_path=login_shell, FAKE_SYSTEM=system))
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(f"Add this line to {home / rc}:\n\n  {line}\n".encode(), result.stdout)
                    # Without a terminal there is nobody to ask, so nothing changes.
                    self.assertFalse((home / rc).exists())
                    self.assertFalse((home / ".local").exists())

    def test_no_modify_path_keeps_everything_inside_the_store(self):
        for shell in POSIX_SHELLS:
            for flag, extra in [("--no-modify-path", {}), (None, {"RELIABURGER_NO_MODIFY_PATH": "1"})]:
                with self.subTest(shell=shell, flag=flag):
                    home = self.fresh_home()
                    env = self.env(home, extra_path=[home / ".local/bin"], **extra)
                    result = self.run_installer(shell, env, *([flag] if flag else []))
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertFalse((home / ".local").exists())
                    self.assertIn(STORE_LINE.encode(), result.stdout)

    def test_store_already_on_path_needs_nothing(self):
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell):
                home = self.fresh_home()
                result = self.run_installer(shell, self.env(home, extra_path=[home / ".reliaburger/bin"]))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn(b"not on your PATH", result.stdout)

    def test_isolated_home_uses_its_own_bin_and_never_links(self):
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell):
                home = self.fresh_home()
                isolated = self.root / f"isolated-{shell}"
                env = self.env(home, extra_path=[home / ".local/bin"], RELIABURGER_HOME=str(isolated))
                result = self.run_installer(shell, env)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue((isolated / "bin/relish").is_file())
                self.assertFalse((home / ".local").exists())
                self.assertIn(f'export PATH="{isolated}/bin:$PATH"'.encode(), result.stdout)
                for invalid in ["relative/path", "/tmp/quote\"d", "/tmp/dollar$HOME"]:
                    result = self.run_installer(shell, dict(env, RELIABURGER_HOME=invalid))
                    self.assertNotEqual(result.returncode, 0, invalid)

    def test_asks_on_the_terminal_before_editing_the_rc_file(self):
        for shell in POSIX_SHELLS:
            with self.subTest(shell=shell, answer="y"):
                home = self.fresh_home()
                (home / ".zshrc").write_text("# existing\n")
                env = self.env(home)
                code, stdout, stderr, terminal = self.run_with_terminal(shell, env, "y")
                self.assertEqual(code, 0, stderr)
                self.assertIn("Add it to", terminal)
                rc = (home / ".zshrc").read_text()
                self.assertTrue(rc.startswith("# existing\n"))
                self.assertEqual(rc.count(STORE_LINE), 1)
                # A second run finds the line and does not ask again.
                result = self.run_installer(shell, env)
                self.assertIn(b"already has it", result.stdout)
                self.assertEqual((home / ".zshrc").read_text().count(STORE_LINE), 1)
            with self.subTest(shell=shell, answer="default"):
                home = self.fresh_home()
                code, stdout, stderr, _ = self.run_with_terminal(shell, self.env(home), "")
                self.assertEqual(code, 0, stderr)
                self.assertIn("unchanged", stdout)
                self.assertFalse((home / ".zshrc").exists())


if __name__ == "__main__":
    unittest.main()
