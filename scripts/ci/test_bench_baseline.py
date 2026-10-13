"""The task-array benchmark fails CI on a regression, not on a slow runner."""
from pathlib import Path
import json
import random
import re
import statistics
import sys
import tempfile
import unittest

import bench_baseline

# Means from the runs of 12 October 2026 that failed the old absolute gate on
# a slow runner (#857), and two earlier runs on a runner with fast CPUs and a
# slow disk. See the "comment" field in the file.
RUNS = json.loads((Path(__file__).parent / "fixtures" / "task-array-runs-2026-10-12.json")
                  .read_text())["runs"]

SUITE = ["executor/fake_64k", "executor/process_true_256/1", "index_set/insert_sequential_1m",
         "index_set/insert_sparse_10k", "leader_tick/64_arrays_3_nodes", "state/json_encode_1m",
         "state/plan_and_complete_1m/3", "storage/ledger_and_index_64k"]


def estimate(root, name, mean_ns):
    """Write what Criterion writes for one benchmark's latest run."""
    directory = Path(root) / name / "new"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "estimates.json").write_text(json.dumps(
        {"mean": {"point_estimate": mean_ns, "standard_error": 1.0},
         "median": {"point_estimate": mean_ns * 0.9, "standard_error": 1.0}}))


def scaled(baseline, factors):
    """This run's means: each baseline mean times its factor (1 when unnamed)."""
    return {name: mean * factors.get(name, 1.0) for name, mean in baseline.items()}


def absolute_gate_fails(current, baseline, max_ratio):
    """What the gate did before #857: any benchmark above `max_ratio` alone fails."""
    return any(mean / baseline[name] > max_ratio for name, mean in current.items()
               if name in baseline)


# Stands in for `cargo bench --bench task_arrays -- FILTER`: logs the filter it
# was given and, like Criterion, writes new results only for the benchmarks the
# filter matches, with the means in the JSON file it is pointed at.
FAKE_BENCH = """
import json, re, sys
from pathlib import Path
criterion, means, log, pattern = sys.argv[1:5]
with open(log, "a") as handle:
    handle.write(pattern + "\\n")
for name, mean in json.loads(Path(means).read_text()).items():
    if re.search(pattern, name):
        directory = Path(criterion) / name / "new"
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "estimates.json").write_text(json.dumps(
            {"mean": {"point_estimate": mean, "standard_error": 1.0}}))
"""


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
        self.suite = {name: 1000.0 * (index + 1) for index, name in enumerate(SUITE)}

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
        result = bench_baseline.compare(bench_baseline.read_means(self.criterion),
                                        baseline, max_ratio=2.0)
        self.assertFalse(result.failed)
        self.assertEqual(len(result.rows), 2)

    def test_a_uniformly_slower_runner_passes(self):
        # Every benchmark about twice as slow, scattered the way the runs of
        # 12 October were: some above 2x, which the absolute gate failed on.
        factors = dict(zip(SUITE, [1.9, 2.0, 2.3, 1.8, 2.0, 2.4, 2.0, 2.1]))
        current = scaled(self.suite, factors)
        self.assertTrue(absolute_gate_fails(current, self.suite, 2.0))
        result = bench_baseline.compare(current, self.suite, max_ratio=2.0)
        self.assertAlmostEqual(result.runner, 2.0)
        self.assertEqual(result.regressions, [])
        self.assertFalse(result.failed)

    def test_one_benchmark_more_than_twice_as_slow_as_the_rest_fails(self):
        factors = {name: 2.0 for name in SUITE}
        factors["leader_tick/64_arrays_3_nodes"] = 2.0 * 2.1
        result = bench_baseline.compare(scaled(self.suite, factors), self.suite, max_ratio=2.0)
        self.assertEqual(result.regressions, ["leader_tick/64_arrays_3_nodes"])
        self.assertTrue(result.failed)

    def test_one_benchmark_more_than_twice_as_slow_on_a_steady_runner_fails(self):
        result = bench_baseline.compare(scaled(self.suite, {"state/json_encode_1m": 2.1}),
                                        self.suite, max_ratio=2.0)
        self.assertEqual(result.regressions, ["state/json_encode_1m"])

    def test_a_faster_runner_does_not_tighten_the_bound(self):
        # Fast CPUs and a slow disk: the storage benchmark is 1.7x on its own,
        # but more than 2x the run's median.
        factors = {name: 0.75 for name in SUITE}
        factors["storage/ledger_and_index_64k"] = 1.7
        result = bench_baseline.compare(scaled(self.suite, factors), self.suite, max_ratio=2.0)
        self.assertLess(result.runner, 1.0)
        self.assertFalse(result.failed)

    def test_a_whole_run_more_than_twice_as_slow_fails(self):
        # A slow runner and a broad regression look the same from inside the
        # run, so the median itself keeps the old bound.
        current = scaled(self.suite, {name: 2.5 for name in SUITE})
        result = bench_baseline.compare(current, self.suite, max_ratio=2.0)
        self.assertEqual(result.regressions, [])
        self.assertTrue(result.run_too_slow)
        self.assertTrue(result.failed)

    def test_a_regression_in_most_benchmarks_on_a_steady_runner_fails(self):
        factors = {name: 2.5 for name in SUITE[:5]}
        result = bench_baseline.compare(scaled(self.suite, factors), self.suite, max_ratio=2.0)
        self.assertTrue(result.failed)

    def test_the_runs_of_12_october_pass(self):
        for run in RUNS:
            with self.subTest(run=run["run"], attempt=run["attempt"]):
                self.assertEqual(run["absolute_gate"] == "failure",
                                 absolute_gate_fails(run["current"], run["baseline"], 2.0))
                result = bench_baseline.compare(run["current"], run["baseline"], max_ratio=2.0)
                self.assertEqual(result.regressions, [])
                self.assertFalse(result.failed)

    def test_a_regression_in_any_run_of_12_october_still_fails(self):
        for run in RUNS:
            for name in run["current"]:
                with self.subTest(run=run["run"], attempt=run["attempt"], benchmark=name):
                    ratios = {other: mean / run["baseline"][other]
                              for other, mean in run["current"].items()}
                    ratio = ratios.pop(name)
                    # Push this one benchmark to 2.1x the median of the rest.
                    target = 2.1 * max(1.0, statistics.median(ratios.values()))
                    current = scaled(run["current"], {name: target / ratio})
                    self.assertEqual(
                        bench_baseline.compare(current, run["baseline"], 2.0).regressions, [name])

    def test_a_run_the_absolute_gate_passed_still_passes(self):
        # The relative gate only ever loosens the absolute one.
        generator = random.Random(857)
        for _ in range(2000):
            factors = {name: generator.uniform(0.3, 2.0) for name in SUITE}
            result = bench_baseline.compare(scaled(self.suite, factors), self.suite, 2.0)
            self.assertFalse(result.failed, factors)

    def test_new_and_removed_benchmarks_are_not_regressions(self):
        baseline = {"leader/simulate_1m/3": 5000.0, "gone/bench": 1.0}
        result = bench_baseline.compare(bench_baseline.read_means(self.criterion),
                                        baseline, max_ratio=2.0)
        self.assertFalse(result.failed)
        self.assertIn("index_set/insert_sequential_1m", [row[0] for row in result.rows])

    def test_a_renamed_benchmark_with_a_larger_workload_starts_a_fresh_baseline(self):
        # #872 grew the 0.1 ms JSON encode to 64 encodes an iteration. Under
        # its old name, main's baseline would read that as a 64x regression;
        # under a new name it has no baseline yet and the next main run
        # records one.
        run = RUNS[0]
        grown = dict(run["current"])
        mean = grown.pop("state/json_encode_1m") * 64
        stale = dict(run["current"], **{"state/json_encode_1m": mean})
        self.assertEqual(bench_baseline.compare(stale, run["baseline"], 2.0).regressions,
                         ["state/json_encode_1m"])
        grown["state/json_encode_1m_x64"] = mean
        result = bench_baseline.compare(grown, run["baseline"], 2.0)
        self.assertFalse(result.failed)
        row = next(row for row in result.rows if row[0] == "state/json_encode_1m_x64")
        self.assertEqual(row[1:], (None, mean, None, None))

    def test_nothing_to_compare_has_no_runner_factor(self):
        result = bench_baseline.compare(bench_baseline.read_means(self.criterion), {}, 2.0)
        self.assertIsNone(result.runner)
        self.assertFalse(result.failed)

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
        # Ratio 10 against a rest at 1; the run's median, 5.5, is too slow too.
        self.assertIn("| 10.00 | 10.00 |", text)
        self.assertIn("runner factor 5.50", text)

    def test_compare_passes_a_slow_runner_and_reports_its_factor(self):
        self.baseline.parent.mkdir()
        self.baseline.write_text(json.dumps({"schema": 1, "benchmarks": {
            "index_set/insert_sequential_1m": 450.0, "leader/simulate_1m/3": 3300.0}}))
        summary = self.root / "summary.md"
        status = bench_baseline.main(["compare", "--criterion", str(self.criterion),
                                      "--baseline", str(self.baseline), "--max-ratio", "2",
                                      "--summary", str(summary)])
        self.assertEqual(status, 0)
        self.assertIn("runner factor 1.87", summary.read_text())

    def remeasure_setup(self, first, second):
        """A Criterion run of the whole suite at `first` times the baseline,
        and a fake re-run that measures `second` times it."""
        self.baseline.parent.mkdir()
        self.baseline.write_text(json.dumps({"schema": 1, "benchmarks": self.suite}))
        self.criterion = self.root / "suite"
        for name, mean in scaled(self.suite, first).items():
            estimate(self.criterion, name, mean)
        means = self.root / "remeasured.json"
        means.write_text(json.dumps(scaled(self.suite, second)))
        fake = self.root / "fake_bench.py"
        fake.write_text(FAKE_BENCH)
        self.log = self.root / "filters.log"
        self.summary = self.root / "summary.md"
        command = f"{sys.executable} {fake} {self.criterion} {means} {self.log}"
        return ["compare", "--criterion", str(self.criterion), "--baseline", str(self.baseline),
                "--max-ratio", "2", "--summary", str(self.summary), "--remeasure", command]

    def filters(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_a_benchmark_over_the_limit_once_then_fine_passes_as_remeasured(self):
        argv = self.remeasure_setup({"storage/ledger_and_index_64k": 2.89}, {})
        self.assertEqual(bench_baseline.main(argv), 0)
        self.assertEqual(len(self.filters()), 1)
        text = self.summary.read_text()
        self.assertIn("re-measured", text)
        # Both measurements of the re-measured benchmark, against the rest.
        self.assertRegex(text, r"\| `storage/ledger_and_index_64k` \| 2\.89 \| 1\.00 \| passed")

    def test_a_benchmark_over_the_limit_twice_fails(self):
        argv = self.remeasure_setup({"storage/ledger_and_index_64k": 2.89},
                                    {"storage/ledger_and_index_64k": 2.5})
        self.assertEqual(bench_baseline.main(argv), 1)
        self.assertEqual(len(self.filters()), 1)
        self.assertRegex(self.summary.read_text(),
                         r"\| `storage/ledger_and_index_64k` \| 2\.89 \| 2\.50 \| failed")

    def test_the_remeasure_runs_only_the_benchmarks_over_the_limit(self):
        over = ["executor/process_true_256/1", "storage/ledger_and_index_64k"]
        argv = self.remeasure_setup({name: 3.0 for name in over}, {})
        self.assertEqual(bench_baseline.main(argv), 0)
        [pattern] = self.filters()
        self.assertEqual([name for name in SUITE if re.search(pattern, name)], over)
        # Criterion matches the filter anywhere in the name, so it is anchored:
        # a longer benchmark that merely contains a failing name is not re-run.
        self.assertIsNone(re.search(pattern, "executor/process_true_256/16"))
        self.assertIsNone(re.search(pattern, "x/storage/ledger_and_index_64k"))

    def test_nothing_is_remeasured_when_nothing_is_over_the_limit(self):
        argv = self.remeasure_setup({"storage/ledger_and_index_64k": 1.9}, {})
        self.assertEqual(bench_baseline.main(argv), 0)
        self.assertEqual(self.filters(), [])
        self.assertNotIn("re-measured", self.summary.read_text())

    def test_a_whole_run_too_slow_is_not_remeasured_and_still_fails(self):
        argv = self.remeasure_setup({name: 2.5 for name in SUITE}, {})
        self.assertEqual(bench_baseline.main(argv), 1)
        self.assertEqual(self.filters(), [])

    def test_a_failed_remeasure_fails_the_gate(self):
        argv = self.remeasure_setup({"storage/ledger_and_index_64k": 2.89}, {})
        argv[-1] = f"{sys.executable} -c 'import sys; sys.exit(3)'"
        self.assertEqual(bench_baseline.main(argv), 1)

    def test_an_empty_criterion_directory_is_an_error(self):
        empty = self.root / "empty"
        empty.mkdir()
        with self.assertRaises(ValueError):
            bench_baseline.read_means(empty)


if __name__ == "__main__":
    unittest.main()
