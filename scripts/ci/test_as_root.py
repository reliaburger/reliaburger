"""scripts/ci/as-root hands root the whole environment without `sudo -E` (#1559)."""
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

AS_ROOT = Path(__file__).resolve().parent / "as-root"


class AsRootTests(unittest.TestCase):
    def run_with_fake_sudo(self, *command, **extra):
        with tempfile.TemporaryDirectory() as directory:
            record = Path(directory) / "sudo-args"
            sudo = Path(directory) / "sudo"
            # Stands in for sudo-rs: records its arguments and starts the
            # command with an empty environment, as -E would leave it.
            sudo.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$SUDO_RECORD"\nexec env -i "$@"\n')
            sudo.chmod(0o755)
            environment = dict(os.environ, PATH=f"{directory}:{os.environ['PATH']}",
                               SUDO_RECORD=str(record), **extra)
            result = subprocess.run([str(AS_ROOT), *command], env=environment,
                                    capture_output=True, text=True, check=True)
            return result.stdout, record.read_text().splitlines()

    def test_every_variable_reaches_the_command_even_with_newlines(self):
        output, _ = self.run_with_fake_sudo(
            "sh", "-c", 'printf "%s|%s" "$ODD_VALUE" "$CARGO_HOME"',
            ODD_VALUE="a b\nc=d", CARGO_HOME="/home/runner/.cargo")
        self.assertEqual(output, "a b\nc=d|/home/runner/.cargo")

    def test_it_never_asks_sudo_to_preserve_the_environment(self):
        _, arguments = self.run_with_fake_sudo("true")
        self.assertEqual(arguments[0:2], ["env", "--"])
        self.assertNotIn("-E", arguments)
        self.assertEqual(arguments[-1], "true")

    def test_it_refuses_to_run_nothing(self):
        result = subprocess.run([str(AS_ROOT)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage", result.stderr)


if __name__ == "__main__":
    unittest.main()
