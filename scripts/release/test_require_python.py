"""A host whose python3 can't run the release scripts is told so up front (#1483)."""
from pathlib import Path
import os
import stat
import subprocess
import sys
import tempfile
import unittest

RELEASE = Path(__file__).resolve().parent
REPOSITORY = RELEASE.parents[1]


def without_tomllib(directory):
    """A python3 for PATH whose standard library has no tomllib, like macOS's 3.9."""
    shim = Path(directory) / "shim"
    shim.mkdir()
    (shim / "tomllib.py").write_text("raise ModuleNotFoundError(\"No module named 'tomllib'\", name='tomllib')\n")
    python = Path(directory) / "python3"
    python.write_text(f'#!/bin/sh\nPYTHONPATH="{shim}" exec "{sys.executable}" "$@"\n')
    python.chmod(python.stat().st_mode | stat.S_IEXEC)
    return f"{directory}{os.pathsep}{os.environ['PATH']}"


class RequirePythonTests(unittest.TestCase):
    def test_a_python_with_tomllib_passes_silently(self):
        result = subprocess.run([sys.executable, str(RELEASE / "require_python.py")], capture_output=True, text=True)
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, "", ""))

    def test_a_python_without_tomllib_says_what_to_install(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run(["python3", str(RELEASE / "require_python.py")], capture_output=True, text=True,
                                    env={**os.environ, "PATH": without_tomllib(directory)})
        self.assertEqual(result.returncode, 1)
        self.assertIn("Python 3.11 or later", result.stderr)
        self.assertIn("uv python install", result.stderr)

    def test_a_soak_stops_before_it_starts(self):
        for script in ("qualify-sustained.sh", "qualify-profile.sh"):
            with self.subTest(script), tempfile.TemporaryDirectory() as directory:
                result = subprocess.run([str(RELEASE / script), "--plan"], capture_output=True, text=True,
                                        env={**os.environ, "PATH": without_tomllib(directory)})
                self.assertEqual(result.returncode, 1)
                self.assertIn("Python 3.11 or later", result.stderr)

    def test_the_script_tests_check_first(self):
        makefile = (REPOSITORY / "Makefile").read_text()
        recipe = makefile[makefile.index("test-ci-scripts:"):]
        self.assertLess(recipe.index("require_python.py"), recipe.index("unittest discover"))


if __name__ == "__main__":
    unittest.main()
