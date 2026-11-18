"""The appliance runs bun at the same OOM score as every unit we ship.

Each systemd unit we ship for bun (the quickstart's in
src/relish/quickstart/provision.rs, the ones in docs/linux-servers.md) sets
`OOMScoreAdjust=-900`, so under memory pressure the kernel kills workloads
(which bun starts at +500) before bun. The appliance's unit has to say so
too: without it bun runs at 0, closer to its workloads than any other node's.

    python3 -m unittest discover -s image/tests -p 'test_bun_unit.py'
"""

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
UNIT = ROOT / "image" / "mkosi.extra" / "usr" / "lib" / "systemd" / "system" / "reliaburger.service"


def service_section(text):
    """The `[Service]` section's lines, stripped."""
    lines, inside = [], False
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("["):
            inside = line == "[Service]"
            continue
        if inside and line and not line.startswith("#"):
            lines.append(line)
    return lines


class BunUnitTest(unittest.TestCase):
    def test_bun_runs_at_the_shipped_oom_score(self):
        self.assertIn("OOMScoreAdjust=-900", service_section(UNIT.read_text()))

    def test_bun_keeps_the_shipped_restart_policy(self):
        service = service_section(UNIT.read_text())
        for line in ("Restart=always", "RestartSec=2", "KillMode=process"):
            self.assertIn(line, service)


if __name__ == "__main__":
    unittest.main()
