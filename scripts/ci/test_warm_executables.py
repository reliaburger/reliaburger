"""warm_executables.py runs each freshly built binary once before the suite (#722)."""
from pathlib import Path
import io
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout

sys.path.insert(0, str(Path(__file__).resolve().parent))
import warm_executables as warm  # noqa: E402

MANIFEST = """\
[package]
name = "reliaburger"

[[bin]]
name = "bun"
path = "src/bin/bun.rs"

[[bin]]
name = "relish"
path = "src/bin/relish.rs"
"""


class WarmExecutablesTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "Cargo.toml").write_text(MANIFEST)
        self.binaries = self.root / "target" / "debug"
        self.binaries.mkdir(parents=True)
        self.calls = self.root / "calls"

    def tearDown(self):
        self.tmp.cleanup()

    def binary(self, name, body='printf "%s %s\\n" "$0" "$*" >> "$CALLS"'):
        path = self.binaries / name
        path.write_text(f"#!/bin/sh\n{body}\n")
        path.chmod(0o700)
        return path

    def run_warm(self, platform="darwin", timeout=30.0):
        output = io.StringIO()
        environment = dict(os.environ, CALLS=str(self.calls))
        with redirect_stdout(output):
            status = warm.main(["--manifest", str(self.root / "Cargo.toml"),
                                "--target-dir", str(self.root / "target")],
                               platform=platform, timeout=timeout, environment=environment)
        return status, output.getvalue()

    def calls_made(self):
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def test_the_binary_names_come_from_the_manifests_bin_tables(self):
        self.assertEqual(warm.binary_names(self.root / "Cargo.toml"), ["bun", "relish"])

    def test_each_built_binary_runs_once_with_help_on_macos(self):
        bun = self.binary("bun")
        relish = self.binary("relish")
        status, output = self.run_warm()
        self.assertEqual(status, 0)
        self.assertEqual(self.calls_made(), [f"{bun} --help", f"{relish} --help"])
        self.assertIn("warmed bun in", output)

    def test_a_binary_that_was_not_built_is_skipped(self):
        bun = self.binary("bun")
        status, output = self.run_warm()
        self.assertEqual(status, 0)
        self.assertEqual(self.calls_made(), [f"{bun} --help"])
        self.assertIn("relish not built", output)

    def test_nothing_runs_on_linux_which_has_no_first_exec_assessment(self):
        self.binary("bun")
        status, output = self.run_warm(platform="linux")
        self.assertEqual(status, 0)
        self.assertEqual(self.calls_made(), [])
        self.assertIn("nothing to warm", output)

    def test_a_binary_that_hangs_is_stopped_and_the_next_still_runs(self):
        self.binary("bun", body="exec sleep 30")
        relish = self.binary("relish")
        status, output = self.run_warm(timeout=0.5)
        self.assertEqual(status, 0)
        self.assertIn("bun did not exit", output)
        self.assertEqual(self.calls_made(), [f"{relish} --help"])

    def test_a_binary_that_fails_is_still_warm(self):
        self.binary("bun", body='printf "%s %s\\n" "$0" "$*" >> "$CALLS"; exit 2')
        status, output = self.run_warm()
        self.assertEqual(status, 0)
        self.assertIn("warmed bun in", output)


if __name__ == "__main__":
    unittest.main()
