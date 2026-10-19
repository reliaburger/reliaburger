"""The idle-memory measurement reads RSS the same way on Linux and macOS and
never publishes a number from a node that died or was never sampled.
"""
from pathlib import Path
import unittest

from idle_memory import (
    IdleMemoryError,
    Sample,
    descendants,
    isolate_config,
    parse_proc_status,
    parse_ps_table,
    render_record,
    summarise,
)

PROC_STATUS = """Name:\tbun
Umask:\t0022
State:\tS (sleeping)
VmPeak:\t 2100000 kB
VmHWM:\t  151234 kB
VmRSS:\t  142080 kB
RssAnon:\t  120000 kB
Threads:\t37
"""

PS_TABLE = """  PID  PPID    RSS COMM
  100     1 143360 /usr/local/bin/bun
  101   100   2048 /usr/local/bin/bun
  102   101   1024 runc
  200     1   9999 unrelated
"""


class ParseTests(unittest.TestCase):
    def test_proc_status_gives_rss_and_peak_in_kib(self):
        rss, peak = parse_proc_status(PROC_STATUS)
        self.assertEqual(rss, 142080)
        self.assertEqual(peak, 151234)

    def test_proc_status_without_vmrss_is_refused(self):
        with self.assertRaises(IdleMemoryError):
            parse_proc_status("Name:\tbun\nState:\tZ (zombie)\n")

    def test_ps_table_rows_are_pid_parent_and_rss(self):
        rows = parse_ps_table(PS_TABLE)
        self.assertEqual(rows[100], (1, 143360))
        self.assertEqual(rows[102], (101, 1024))
        self.assertEqual(len(rows), 4)

    def test_descendants_follow_the_whole_tree_and_skip_strangers(self):
        rows = parse_ps_table(PS_TABLE)
        self.assertEqual(sorted(descendants(rows, 100)), [101, 102])


INIT_CONFIG = """[cluster]
name = "idle"
gossip_port = 9443
raft_port = 9444
reporting_port = 9445

[storage]
data = "/var/lib/reliaburger/data"
images = "/var/lib/reliaburger/images"

[images]
registry_port = 5050
"""


class IsolationTests(unittest.TestCase):
    def test_storage_and_ports_move_off_the_shared_defaults(self):
        text = isolate_config(INIT_CONFIG, Path("/w/state"), port_base=29000)
        self.assertIn('data = "/w/state/data"', text)
        self.assertIn('images = "/w/state/images"', text)
        self.assertNotIn("/var/lib/reliaburger", text)
        self.assertIn("gossip_port = 29443", text)
        self.assertIn("raft_port = 29444", text)
        self.assertIn("reporting_port = 29445", text)
        self.assertIn("registry_port = 29050", text)

    def test_a_config_without_the_storage_root_is_refused(self):
        with self.assertRaises(IdleMemoryError):
            isolate_config("[cluster]\nname = \"x\"\n", Path("/w"), port_base=29000)


class SummaryTests(unittest.TestCase):
    def test_summary_is_min_median_max_in_mib(self):
        samples = [
            Sample(elapsed_secs=600, rss_kib=102400, tree_kib=104448),
            Sample(elapsed_secs=630, rss_kib=104448, tree_kib=106496),
            Sample(elapsed_secs=660, rss_kib=106496, tree_kib=108544),
        ]
        summary = summarise(samples)
        self.assertEqual(summary["rss_mib"], (100.0, 102.0, 104.0))
        self.assertEqual(summary["tree_mib"], (102.0, 104.0, 106.0))

    def test_no_samples_is_an_error_not_a_zero(self):
        with self.assertRaises(IdleMemoryError):
            summarise([])


class RecordTests(unittest.TestCase):
    def test_record_names_the_method_machine_and_every_sample(self):
        samples = [
            Sample(elapsed_secs=600, rss_kib=102400, tree_kib=104448),
            Sample(elapsed_secs=630, rss_kib=104448, tree_kib=106496),
        ]
        metadata = {
            "date": "2026-10-11",
            "mode": "cluster",
            "bun_version": "bun 0.2.0-dev (abc1234)",
            "commit": "abc1234",
            "machine": "Apple M2 Max, 32 GiB",
            "os": "macOS 26.3.1",
            "runtime": "auto",
            "settle_secs": 600,
            "interval_secs": 30,
            "source": "ps -o rss",
            "binary_sha256": "0" * 64,
        }
        record = render_record(metadata, samples)
        self.assertIn("bun 0.2.0-dev (abc1234)", record)
        self.assertIn("Apple M2 Max", record)
        self.assertIn("ps -o rss", record)
        self.assertIn("| 600 | 100.0 | 102.0 |", record)
        self.assertIn("| 630 | 102.0 | 104.0 |", record)
        self.assertIn("median", record)


class ScriptTests(unittest.TestCase):
    def test_script_is_executable_and_documents_its_usage(self):
        script = Path(__file__).resolve().with_name("idle_memory.py")
        self.assertTrue(script.stat().st_mode & 0o111)
        self.assertIn("Usage", script.read_text())


if __name__ == "__main__":
    unittest.main()
