"""The hollow-node summary turns each fleet run's JSON into one table row,
and never prints a number a run didn't measure.
"""
import json
from pathlib import Path
import tempfile
import unittest

from hollow_summary import HollowSummaryError, load_reports, render_table, row

REPORT = {
    "plan": {"hollow_nodes": 100, "council_members": 3},
    "host": "Apple M1 Max, 10 logical CPUs",
    "start_all_ms": 2100,
    "gossip_converged_ms": 3500,
    "council_sees_all_ms": 4000,
    "council_voters": 3,
    "hollow_voters": 0,
    "replicas": 100,
    "apply_ms": 41,
    "first_replica_ms": 900,
    "all_replicas_ms": 6200,
    "small_applies": {"count": 20, "errors": 0, "p50": 30.6, "p90": 41.0, "p99": 55.25, "max": 55.25},
    "raft_commit": [
        {"phase": "council-alone", "latency": {"count": 9, "errors": 0, "p50": 25.0, "p99": 28.0}},
        {"phase": "steady", "latency": {"count": 60, "errors": 1, "p50": 31.0, "p99": 80.0}},
    ],
    "resources": [
        {"phase": "council-alone", "council_cpu_percent": 3.0, "council_rss_mb_max": 63.0,
         "fleet_cpu_percent": 0.3, "fleet_rss_mb_max": 18.0, "load_average_max": 7.0},
        {"phase": "steady", "council_cpu_percent": 42.25, "council_rss_mb_max": 140.6,
         "fleet_cpu_percent": 310.0, "fleet_rss_mb_max": 900.0, "load_average_max": 12.0},
    ],
    "churn": {"stopped_nodes": 10, "replicas_lost": 30, "detected_ms": 5200,
              "rescheduled_ms": 9100, "rejoined_ms": 3000},
    "stopped": None,
}


class RowTests(unittest.TestCase):
    def test_a_complete_run_fills_every_column(self):
        cells = row(REPORT)
        self.assertEqual(cells["nodes"], "100")
        self.assertEqual(cells["gossip converged"], "3.5 s")
        self.assertEqual(cells["all replicas"], "6.2 s")
        self.assertEqual(cells["council CPU (steady)"], "42%")
        self.assertEqual(cells["council RSS (steady)"], "141 MB")
        self.assertEqual(cells["Raft commit p50/p99 (steady)"], "31/80 ms")
        self.assertEqual(cells["apply p50/p99"], "31/55 ms")
        self.assertEqual(cells["dead detected"], "5.2 s")
        self.assertEqual(cells["rescheduled"], "9.1 s")
        self.assertEqual(cells["outcome"], "complete")

    def test_a_run_that_stopped_says_why_and_leaves_the_rest_unmeasured(self):
        stopped = dict(REPORT, all_replicas_ms=None, churn=None, resources=[], raft_commit=[],
                       stopped="only 3 of 100 replicas ran")
        cells = row(stopped)
        self.assertEqual(cells["all replicas"], "not reached")
        self.assertEqual(cells["council CPU (steady)"], "not measured")
        self.assertEqual(cells["dead detected"], "not measured")
        self.assertEqual(cells["outcome"], "stopped: only 3 of 100 replicas ran")

    def test_a_hollow_voter_is_called_out(self):
        cells = row(dict(REPORT, hollow_voters=2))
        self.assertIn("2 hollow voters", cells["outcome"])


class TableTests(unittest.TestCase):
    def test_rows_are_ordered_by_fleet_size(self):
        small = dict(REPORT, plan={"hollow_nodes": 20, "council_members": 3})
        table = render_table([REPORT, small])
        lines = table.splitlines()
        self.assertTrue(lines[0].startswith("| nodes |"))
        self.assertTrue(lines[2].startswith("| 20 |"))
        self.assertTrue(lines[3].startswith("| 100 |"))

    def test_reports_load_from_a_directory_and_refuse_garbage(self):
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, "hollow-100.json").write_text(json.dumps(REPORT))
            self.assertEqual(len(load_reports(directory)), 1)
            Path(directory, "hollow-500.json").write_text("{not json")
            with self.assertRaises(HollowSummaryError):
                load_reports(directory)

    def test_an_empty_directory_is_an_error(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(HollowSummaryError):
                load_reports(directory)


if __name__ == "__main__":
    unittest.main()
