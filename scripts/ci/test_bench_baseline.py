"""The task-array benchmark fails CI only on a large regression against main's baseline."""
from pathlib import Path
import json
import tempfile
import unittest

import bench_baseline


def estimate(root, name, mean_ns):
    """Write what Criterion writes for one benchmark's latest run."""
    directory = Path(root) / name / "new"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "estimates.json").write_text(json.dumps(
        {"mean": {"point_estimate": mean_ns, "standard_error": 1.0},
         "median": {"point_estimate": mean_ns * 0.9, "standard_error": 1.0}}))


class BenchBaselineTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.criterion = self.root / "criterion"
        estimate(self.criterion, "index_set/insert_sequential_1m", 1000.0)
        estimate(self.criterion, "leader/simulate_1m/3", 5000.0)
        # Criterion's previous run and its HTML report are not results.
        (self.criterion / "index_set" / "insert_sequential_1m" / "base").mkdir()
        (self.criterion / "report").mkdir()
        self.baseline = self.root / "baseline" / "baseline.json"

    def tearDown(self):
        self.tmp.cleanup()

    def test_each_benchmark_is_named_by_its_criterion_path(self):
        self.assertEqual(bench_baseline.read_means(self.criterion),
                         {"index_set/insert_sequential_1m": 1000.0, "leader/simulate_1m/3": 5000.0})

    def test_a_saved_baseline_reads_back_as_the_means(self):
        bench_baseline.save(self.criterion, self.baseline)
        self.assertEqual(bench_baseline.load(self.baseline),
                         bench_baseline.read_means(self.criterion))

    def test_noise_below_the_ratio_passes(self):
        baseline = {"index_set/insert_sequential_1m": 700.0, "leader/simulate_1m/3": 5000.0}
        rows, regressions = bench_baseline.compare(bench_baseline.read_means(self.criterion),
                                                   baseline, max_ratio=2.0)
        self.assertEqual(regressions, [])
        self.assertEqual(len(rows), 2)

    def test_a_large_regression_is_reported(self):
        baseline = {"index_set/insert_sequential_1m": 400.0, "leader/simulate_1m/3": 5000.0}
        _, regressions = bench_baseline.compare(bench_baseline.read_means(self.criterion),
                                                baseline, max_ratio=2.0)
        self.assertEqual(regressions, ["index_set/insert_sequential_1m"])

    def test_new_and_removed_benchmarks_are_not_regressions(self):
        baseline = {"leader/simulate_1m/3": 5000.0, "gone/bench": 1.0}
        rows, regressions = bench_baseline.compare(bench_baseline.read_means(self.criterion),
                                                   baseline, max_ratio=2.0)
        self.assertEqual(regressions, [])
        self.assertIn("index_set/insert_sequential_1m", [row[0] for row in rows])

    def test_the_first_run_without_a_baseline_passes(self):
        status = bench_baseline.main(["compare", "--criterion", str(self.criterion),
                                      "--baseline", str(self.baseline), "--max-ratio", "2"])
        self.assertEqual(status, 0)

    def test_compare_fails_on_a_large_regression_and_writes_the_summary(self):
        self.baseline.parent.mkdir()
        self.baseline.write_text(json.dumps({"schema": 1, "benchmarks": {
            "index_set/insert_sequential_1m": 100.0, "leader/simulate_1m/3": 5000.0}}))
        summary = self.root / "summary.md"
        status = bench_baseline.main(["compare", "--criterion", str(self.criterion),
                                      "--baseline", str(self.baseline), "--max-ratio", "2",
                                      "--summary", str(summary)])
        self.assertEqual(status, 1)
        text = summary.read_text()
        self.assertIn("| `index_set/insert_sequential_1m` |", text)
        self.assertIn("10.00", text)

    def test_an_empty_criterion_directory_is_an_error(self):
        empty = self.root / "empty"
        empty.mkdir()
        with self.assertRaises(ValueError):
            bench_baseline.read_means(empty)


if __name__ == "__main__":
    unittest.main()
