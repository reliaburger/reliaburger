"""Tests for image/lab/fleet-nodes.py, which tells fleet-measure.sh which
nodes to sample: from a claim directory's fleet.json, or `relish nodes
--output json`.

    python3 -m unittest discover -s image/tests -p 'test_fleet_nodes.py'
"""

import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "lab/fleet-nodes.py"
MEASURE = Path(__file__).resolve().parents[1] / "lab/fleet-measure.sh"
spec = importlib.util.spec_from_file_location("fleet_nodes", SCRIPT)
fleet_nodes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fleet_nodes)

# What `relish machines claim --create` writes (bare_metal::Fleet).
CLAIMED = {
    "schema": 1,
    "cluster": "wyse",
    "ca_fingerprint": "sha256:00",
    "operators": ["10.77.0.2"],
    "network": "10.77.0.0/24",
    "faults": False,
    "nodes": [
        {"name": "wyse-1", "mac": "6c:4b:90:00:00:01", "address": "10.77.0.11"},
        {"name": "wyse-2", "mac": "6c:4b:90:00:00:02", "address": "10.77.0.12"},
    ],
}

# What `relish nodes --output json` prints (bun's NodeStatus): `address` is
# the gossip endpoint, with its port.
RELISH_NODES = [
    {"node_id": "wyse-1", "address": "10.77.0.11:9443", "state": "alive",
     "incarnation": 0, "is_council": True, "is_leader": True, "labels": {}},
    {"node_id": "wyse-2", "address": "10.77.0.12:9443",
     "api_address": "10.77.0.12:9117", "state": "dead", "incarnation": 3,
     "is_council": False, "is_leader": False, "labels": {"zone": "a"}},
    {"node_id": "v6", "address": "[fd00::7]:9443", "state": "alive",
     "incarnation": 0, "is_council": False, "is_leader": False, "labels": {}},
]


class FleetNodes(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_a_claim_directory_names_its_nodes(self):
        (self.dir / "fleet.json").write_text(json.dumps(CLAIMED))
        self.assertEqual(fleet_nodes.from_directory(self.dir),
                         [("wyse-1", "10.77.0.11"), ("wyse-2", "10.77.0.12")])

    def test_a_retired_seed_fleet_directory_is_not_a_fleet(self):
        # seed-fleet.sh's `fleet` file went with the script: relish writes
        # fleet.json for every cluster it creates or claims.
        (self.dir / "fleet").write_text("1 6c:4b:90:00:00:01 10.77.0.11\n")
        with self.assertRaisesRegex(ValueError, "no fleet.json"):
            fleet_nodes.from_directory(self.dir)

    def test_a_directory_without_fleet_json_says_so(self):
        with self.assertRaisesRegex(ValueError, "no fleet.json"):
            fleet_nodes.from_directory(self.dir)

    def test_relish_nodes_drops_the_gossip_port(self):
        # Dead nodes stay in: they get an empty row, so the gap shows.
        self.assertEqual(fleet_nodes.from_relish_nodes(json.dumps(RELISH_NODES)),
                         [("wyse-1", "10.77.0.11"), ("wyse-2", "10.77.0.12"),
                          ("v6", "fd00::7")])

    def test_relish_nodes_without_a_cluster_is_an_error(self):
        for text in ("no cluster nodes (single-node mode)\n", "[]"):
            with self.assertRaisesRegex(ValueError, "no nodes"):
                fleet_nodes.from_relish_nodes(text)

    def test_the_command_prints_one_node_per_line(self):
        (self.dir / "fleet.json").write_text(json.dumps(CLAIMED))
        done = subprocess.run([sys.executable, str(SCRIPT), str(self.dir)],
                              capture_output=True, text=True, check=True)
        self.assertEqual(done.stdout, "wyse-1 10.77.0.11\nwyse-2 10.77.0.12\n")

    def test_the_command_reads_relish_json_on_stdin(self):
        done = subprocess.run([sys.executable, str(SCRIPT), "--relish-json", "-"],
                              input=json.dumps(RELISH_NODES[:1]),
                              capture_output=True, text=True, check=True)
        self.assertEqual(done.stdout, "wyse-1 10.77.0.11\n")

    def test_the_command_fails_cleanly(self):
        done = subprocess.run([sys.executable, str(SCRIPT), str(self.dir)],
                              capture_output=True, text=True)
        self.assertEqual(done.returncode, 1)
        self.assertIn("no fleet.json", done.stderr)


@unittest.skipUnless(shutil.which("ssh"), "needs ssh")
class FleetMeasure(unittest.TestCase):
    """fleet-measure.sh end to end, against nodes that refuse SSH: each gets
    a CSV with the header and a timestamp-only row, the visible gap."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def measure(self, *args, env=None):
        # Port 1 on loopback: refused at once, so no timeout to wait for.
        env = dict(os.environ, SSH_OPTS="-p 1", **(env or {}))
        return subprocess.run(["bash", str(MEASURE), *args, str(self.dir), "0", "1"],
                              env=env, capture_output=True, text=True)

    def assert_gap_rows(self, names):
        for name in names:
            rows = (self.dir / "measure" / f"{name}.csv").read_text().splitlines()
            self.assertEqual(len(rows), 2, rows)
            self.assertTrue(rows[0].startswith("time,mem_total_kb,"))
            self.assertRegex(rows[1], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        self.assertEqual(sorted(p.stem for p in (self.dir / "measure").iterdir()),
                         sorted(names))

    def test_a_claim_directory(self):
        fleet = dict(CLAIMED, nodes=[dict(n, address="127.0.0.1") for n in CLAIMED["nodes"]])
        (self.dir / "fleet.json").write_text(json.dumps(fleet))
        done = self.measure()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("wyse-2 (127.0.0.1): no answer", done.stderr)
        self.assert_gap_rows(["wyse-1", "wyse-2"])

    def test_relish_nodes(self):
        relish = self.dir / "relish"
        nodes = [dict(RELISH_NODES[0], address="127.0.0.1:9443")]
        relish.write_text(f"#!/bin/sh\necho '{json.dumps(nodes)}'\n")
        relish.chmod(0o755)
        done = self.measure("--relish", env={"RELISH": str(relish)})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assert_gap_rows(["wyse-1"])

    def test_a_directory_without_a_fleet_fails_before_sampling(self):
        done = self.measure()
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("no fleet.json", done.stderr)
        self.assertFalse((self.dir / "measure").exists())


if __name__ == "__main__":
    unittest.main()
