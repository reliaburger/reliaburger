"""The boot check says why bun isn't healthy, not just that it isn't.

`image/mkosi.extra/usr/lib/reliaburger/boot-check` waits for bun's health
endpoint. When bun never answers, the serial console is all a CI boot test
keeps, and bun's own stderr goes to the journal. So the failure path prints
the tail of `reliaburger.service`'s journal to the console, where
`seeded-pair.sh` and the other QEMU tests record it.

The test runs the script with stub `curl` (bun never answers), `date` (the
deadline passes at once), `sleep`, `systemctl` and `journalctl`.

    python3 -m unittest discover -s image/tests -p 'test_boot_check.py'
"""

import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BOOT_CHECK = ROOT / "image" / "mkosi.extra" / "usr" / "lib" / "reliaburger" / "boot-check"

STUBS = {
    "curl": "exit 7\n",
    # Each call is a minute later, so the five-minute limit passes in a few.
    "date": 'n=$(cat "$STUB_DIR/clock" 2>/dev/null || echo 0)\n'
            'echo $((n + 60)) > "$STUB_DIR/clock"\necho "$n"\n',
    "sleep": "exit 0\n",
    "systemctl": "exit 0\n",
    "journalctl": 'case "$*" in\n'
                  '  *reliaburger.service*) echo "bun: timed out waiting for replicated API credentials" ;;\n'
                  "esac\n",
}


@unittest.skipUnless(Path("/usr/lib/os-release").exists(), "boot-check reads /usr/lib/os-release")
class BootCheckTests(unittest.TestCase):
    def run_boot_check(self):
        with tempfile.TemporaryDirectory() as stub_dir:
            for name, body in STUBS.items():
                path = Path(stub_dir) / name
                path.write_text("#!/bin/sh\n" + body)
                path.chmod(path.stat().st_mode | stat.S_IXUSR)
            env = dict(os.environ, PATH=f"{stub_dir}:{os.environ['PATH']}", STUB_DIR=stub_dir)
            return subprocess.run(["sh", str(BOOT_CHECK)], env=env, capture_output=True,
                                  text=True, timeout=30)

    def test_an_unhealthy_bun_fails_the_check_and_shows_buns_journal(self):
        result = self.run_boot_check()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("reliaburger: bun not healthy after 300 s", result.stdout)
        self.assertIn(
            "reliaburger: bun: bun: timed out waiting for replicated API credentials",
            result.stdout,
        )


if __name__ == "__main__":
    unittest.main()
