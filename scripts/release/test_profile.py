"""The release profile step (#1358): its parsing, medians, rules and README.

The fixtures are the 26 October host-job throughput record's raw windows
(#1030's measurement: the 0.2.3 profile's build against the build with the
#1030 changes), plus small hand-written stacks and snapshots.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "profiling"))

import compare_profile  # noqa: E402
import profile_categories  # noqa: E402
import profile_config  # noqa: E402
import profile_fetch  # noqa: E402
import profile_flame  # noqa: E402
import profile_plan  # noqa: E402
import profile_probe  # noqa: E402
import profile_readme  # noqa: E402
import profile_windows  # noqa: E402

REPOSITORY = HERE.parents[1]
RECORD_1030 = REPOSITORY / "docs" / "qualification" / "2026-10-26-host-job-throughput"


def stage_1030(root, previous="caps-base", candidate="caps-cur"):
    """Lay #1030's cap-27 rounds out as the profile step's raw evidence: LABEL/rN/{host,raw}.*"""
    for label, source in (("previous", previous), ("candidate", candidate)):
        for number in range(1, 6):
            session = Path(root) / label / f"r{number}"
            session.mkdir(parents=True)
            for window in ("host", "raw"):
                for path in (RECORD_1030 / source).glob(f"c27-r{number}-{window}.*"):
                    shutil.copy(path, session / (window + path.name[len(f"c27-r{number}-{window}"):]))


def snapshot(clock, stat, bun_ticks, ctxt, rss_kb, processes=1000):
    return "\n".join([
        "## clock", str(clock),
        "## proc_stat", "cpu  " + " ".join(str(v) for v in stat),
        "## bun_stat", f"{bun_ticks} 0 0 0",
        "## bun_status", f"rss_kb {rss_kb}", "hwm_kb 90000", "threads 40", f"ctxt {ctxt}",
        "## cgroups", "/sys/fs/cgroup/reliaburger 123",
        "## ctxt", "ctxt 5000", f"processes {processes}",
        "## psi_cpu", "some avg10=0.00",
    ])


def write_idle(directory, bun_ticks_used, ctxt_used=600, rss_kb=81920):
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "idle.cpu0").write_text(snapshot(1000.0, [100, 0, 100, 1000, 0, 0, 0, 0], 50, 1000, 80000))
    (directory / "idle.cpu1").write_text(snapshot(1060.0, [200, 0, 200, 25000, 0, 0, 0, 0], 50 + bun_ticks_used,
                                                  1000 + ctxt_used, rss_kb))


def categories_document(bun, system_extra=None):
    """A categories JSON for one perf window, from Bun's category samples."""
    bun_total = sum(bun.values())
    categories = dict(bun)
    categories.update(system_extra or {profile_categories.IDLE: 1000.0})
    return {"total": sum(categories.values()), "bun_total": bun_total, "categories": categories,
            "bun_categories": dict(bun), "other_by_command": {},
            "uncategorised_share": bun.get(profile_categories.BUN_UNCATEGORISED, 0.0) / bun_total if bun_total else 0}


class CategoriesTests(unittest.TestCase):
    SYSTEM = {
        "swapper;secondary_start_kernel;do_idle": 400,
        "rb-bun;tokio::runtime::scheduler::multi_thread::worker::run;reliaburger::lettuce::runner::spawn_gitops_sync::{closure}": 100,
        "rb-bun;tokio::runtime::scheduler::multi_thread::worker::run;openraft::core::RaftCore::run": 50,
        "rb-bun;tokio::runtime::scheduler::multi_thread::worker::run;tokio::runtime::park": 30,
        "rb-bun;[rb-bun]": 20,
        "helper;el0_svc;__do_sys_clone3;copy_process": 60,
        "helper;el0_svc;invoke_syscall;do_sys_poll": 40,
        ":;el0_svc;do_execve": 25,
        "busybox;do_page_fault": 15,
        "kworker;worker_thread": 10,
    }
    BUN = {stack: n for stack, n in SYSTEM.items() if stack.startswith("rb-bun")}

    def test_every_sample_lands_in_exactly_one_category(self):
        result = profile_categories.categorise(self.SYSTEM, self.BUN)
        self.assertEqual(sum(result["categories"].values()), sum(self.SYSTEM.values()))
        self.assertEqual(result["bun_total"], 200)

    def test_bun_frames_get_bun_categories(self):
        result = profile_categories.categorise(self.SYSTEM, self.BUN)["bun_categories"]
        self.assertEqual(result["Bun: GitOps loop"], 100)
        self.assertEqual(result["Bun: Raft core and log"], 50)
        self.assertEqual(result["Bun: async runtime (scheduler, timers, parking)"], 30)
        self.assertEqual(result[profile_categories.BUN_UNCATEGORISED], 20)

    def test_task_processes_without_a_command_name_count_as_busybox(self):
        result = profile_categories.categorise(self.SYSTEM, self.BUN)["categories"]
        self.assertEqual(result["helper/child: execve"], 25)
        self.assertEqual(result["helper/child: page faults"], 15)
        self.assertEqual(result["helper/child: clone3 (process creation)"], 60)
        self.assertEqual(result["helper: wait, poll, pipes and socket syscalls"], 40)
        self.assertEqual(result[profile_categories.IDLE], 400)

    def test_more_than_five_percent_uncategorised_asks_to_extend_the_map(self):
        result = profile_categories.categorise(self.SYSTEM, self.BUN)
        self.assertAlmostEqual(result["uncategorised_share"], 0.10)
        self.assertIn("extend BUN_RULES", profile_categories.table(result))

    def test_a_mapped_profile_raises_no_warning(self):
        system = {k: v for k, v in self.SYSTEM.items() if k != "rb-bun;[rb-bun]"}
        bun = {k: v for k, v in self.BUN.items() if k != "rb-bun;[rb-bun]"}
        result = profile_categories.categorise(system, bun)
        self.assertEqual(result["uncategorised_share"], 0.0)
        self.assertNotIn("extend", profile_categories.table(result))

    def test_per_task_microseconds_use_the_rate(self):
        result = profile_categories.categorise({"swapper;idle": 50, "busybox;x": 50}, {})
        table = profile_categories.table(result, rate=10000, cpus=4)
        # Half of 4 CPUs at 10,000 tasks a second is 200 us of idle per task.
        self.assertIn("200.0 us/task  idle", table)

    def test_folded_lines_parse_and_merge(self):
        stacks = profile_categories.parse_folded(["a;b 3\n", "a;b 2\n", "garbage\n", "c x\n"])
        self.assertEqual(stacks, {"a;b": 5.0})


PERF_SCRIPT = """\
rb-bun 4242/4250 [001] 100.000100: cpu-clock:
\tffff800080001234 el0_svc+0x10 ([kernel.kallsyms])
\t0000aaaa00001000 _RNvNtCs1234_11reliaburger7lettuce3run (/usr/local/bin/rb-bun)

rb-bun 4242/4251 [002] 100.000200: cpu-clock:
\t0000aaaa00002000 [unknown] (/usr/local/bin/rb-bun)

swapper     0/0     [000] 100.000300: cpu-clock:
\tffff800080009999 do_idle+0x40 ([kernel.kallsyms])

"""


class FlameTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.script = Path(self.directory.name) / "w.script"
        self.script.write_text(PERF_SCRIPT)

    def tearDown(self):
        self.directory.cleanup()

    def test_folds_root_first_and_demangles_rust_v0_names(self):
        stacks = profile_flame.fold_cpu_stacks(str(self.script))
        self.assertEqual(stacks, {
            "rb-bun;reliaburger::lettuce::run;el0_svc": 1,
            "rb-bun;[rb-bun]": 1,
            "swapper;do_idle": 1,
        })

    def test_folding_by_process_keeps_only_that_process(self):
        stacks = profile_flame.fold_cpu_stacks(str(self.script), tgid=4242)
        self.assertEqual(sum(stacks.values()), 2)

    def test_svg_draws_every_visible_frame(self):
        svg = profile_flame.render_svg([(["a", "b"], 3.0), (["a", "c"], 1.0)], "t")
        self.assertTrue(svg.startswith("<svg"))
        for frame in ("a (", "b (", "c ("):
            self.assertIn(frame, svg)

    def test_empty_profile_draws_a_note_not_a_crash(self):
        self.assertIn("no samples", profile_flame.render_svg([], "empty"))

    def test_differential_colours_grown_frames_red_and_shrunk_blue(self):
        before = profile_flame.path_shares([(["a", "b"], 9.0), (["a", "c"], 1.0)])
        after = profile_flame.path_shares([(["a", "b"], 1.0), (["a", "c"], 9.0)])
        fill = profile_flame.diff_fill(before, after)
        self.assertEqual(fill("a;c", "c"), "rgb(230,80,80)")
        self.assertEqual(fill("a;b", "b"), "rgb(80,80,230)")
        self.assertEqual(fill("a", "a"), "rgb(230,230,230)")


class ProbeTests(unittest.TestCase):
    def test_timeline_rows_show_held_accepted_and_start_rate(self):
        sample = {"held": 16000, "succeeded": 34000,
                  "nodes": [{"counters": {"attempts_started": 44000, "active_commands": 24}}]}
        later = dict(sample, nodes=[{"counters": {"attempts_started": 45000, "active_commands": 0}}])
        rows = profile_probe.timeline([f"10.0 {json.dumps(sample)}", "10.5 not json",
                                       f"11.0 {json.dumps(later)}"])
        self.assertEqual(rows[0], profile_probe.HEADER)
        self.assertEqual(len(rows), 3)
        self.assertIn("16000     34000    44000", rows[1])
        self.assertIn("1000", rows[2].split()[4])


class WindowTests(unittest.TestCase):
    def test_cpu_snapshots_give_bun_cores_wakeups_and_rss(self):
        before = profile_windows.parse_cpu(snapshot(1000.0, [100, 0, 100, 1000, 0, 0, 0, 0], 50, 1000, 80000))
        after = profile_windows.parse_cpu(snapshot(1060.0, [200, 0, 200, 1600, 0, 0, 0, 0], 110, 7000, 81920))
        delta = profile_windows.cpu_delta(before, after)
        self.assertAlmostEqual(delta["elapsed_seconds"], 60.0)
        self.assertAlmostEqual(delta["bun_cpu_cores"], 0.6 / 60)
        self.assertAlmostEqual(delta["bun_wakeups_per_second"], 100.0)
        self.assertEqual(delta["bun_rss_kb"], 81920)
        self.assertAlmostEqual(delta["cpu_util"], 200 / 800)

    def test_steady_rate_ignores_the_window_edges(self):
        lines = ["0.0s: 0 unique accepted successes", "15.0s: 1000 unique accepted successes",
                 "40.0s: 6000 unique accepted successes", "58.0s: 9600 unique accepted successes",
                 "60.0s: 99999 unique accepted successes"]
        self.assertAlmostEqual(profile_windows.steady_rate("\n".join(lines)), 200.0)
        self.assertIsNone(profile_windows.steady_rate("5.0s: 10 unique accepted successes"))

    def test_start_lag_percentiles_are_bucket_upper_bounds(self):
        counts = [76, 14, 5, 4, 1]
        self.assertEqual(profile_windows.histogram_percentile(counts, 0.5), 1)
        self.assertEqual(profile_windows.histogram_percentile(counts, 0.99), 8)
        self.assertIsNone(profile_windows.histogram_percentile([0, 0], 0.5))

    def test_a_recorded_window_summarises_as_the_record_says(self):
        summary = profile_windows.summarise_window(str(RECORD_1030 / "caps-base"), "c27-r1-host")
        self.assertEqual(summary["rate"], 5100.0)
        self.assertEqual(summary["scenario"], "jobs-host-processes")
        self.assertTrue(summary["complete"])
        self.assertEqual(round(summary["steady_rate"]), 5161)
        self.assertEqual(round(100 * summary["cpu_util"]), 60)

    def test_collect_reads_every_round_of_every_version(self):
        with tempfile.TemporaryDirectory() as root:
            stage_1030(root)
            collected = profile_windows.collect(root)
            self.assertEqual(sorted(collected), ["candidate", "previous"])
            self.assertEqual(profile_windows.values(collected, "previous", "host", "rate"), [5100.0] * 5)
            self.assertEqual(len(profile_windows.values(collected, "candidate", "raw", "rate")), 5)


class CompareTests(unittest.TestCase):
    def test_bootstrap_interval_is_repeatable_and_brackets_the_ratio(self):
        previous, candidate = [100.0, 102.0, 98.0], [120.0, 118.0, 125.0]
        first = compare_profile.bootstrap_ratio(previous, candidate)
        self.assertEqual(first, compare_profile.bootstrap_ratio(previous, candidate))
        self.assertLessEqual(first[0], 120 / 100)
        self.assertGreaterEqual(first[1], 120 / 100)

    def test_a_clear_slowdown_is_a_regression(self):
        row = compare_profile.compare_series([100.0, 101.0, 99.0], [80.0, 81.0, 79.0])
        self.assertEqual(row["verdict"], "regression")
        self.assertLess(row["interval"][1], 1.0)

    def test_a_slowdown_inside_the_noise_is_not_a_regression(self):
        row = compare_profile.compare_series([100.0, 60.0, 140.0], [85.0, 140.0, 60.0])
        self.assertLess(row["ratio"], 0.9)
        self.assertEqual(row["verdict"], "no clear change")

    def test_a_big_speed_up_needs_explaining(self):
        self.assertEqual(compare_profile.compare_series([100.0] * 3, [115.0] * 3)["verdict"], "improvement")

    def test_lower_is_better_metrics_turn_the_rule_round(self):
        slower = compare_profile.compare_series([1.0, 1.0, 1.0], [1.3, 1.3, 1.3], higher_is_better=False)
        self.assertEqual(slower["verdict"], "regression")
        faster = compare_profile.compare_series([1.0, 1.0, 1.0], [0.8, 0.8, 0.8], higher_is_better=False)
        self.assertEqual(faster["verdict"], "improvement")

    def test_1030_measurement_flags_both_speed_ups_for_explanation(self):
        with tempfile.TemporaryDirectory() as root:
            stage_1030(root)
            result = compare_profile.compare(profile_windows.collect(root), {})
        rows = {row["window"]: row for row in result["rates"]}
        self.assertAlmostEqual(rows["host"]["ratio"], 7500.0 / 5100.0)
        self.assertEqual(rows["host"]["verdict"], "improvement")
        self.assertEqual(rows["raw"]["verdict"], "improvement")
        self.assertAlmostEqual(rows["host"]["relative_previous"], 5100.0 / 23519.3, places=3)
        flagged = {(f["rule"], f["subject"]) for f in result["findings"]}
        self.assertIn(("improvement", "Host jobs, default cap"), flagged)
        self.assertTrue(all(f["severity"] == compare_profile.EXPLAIN for f in result["findings"]))

    def test_1030_measurement_backwards_would_fail_as_a_regression(self):
        with tempfile.TemporaryDirectory() as root:
            stage_1030(root, previous="caps-cur", candidate="caps-base")
            result = compare_profile.compare(profile_windows.collect(root), {})
        failing = [f for f in result["findings"] if f["severity"] == compare_profile.WOULD_FAIL]
        self.assertEqual({f["subject"] for f in failing}, {"Host jobs, default cap", "Raw VM processes (calibration)"})

    def test_a_failed_window_is_left_out_and_flagged_not_read_as_a_regression(self):
        with tempfile.TemporaryDirectory() as root:
            for label, error, rate in (("previous", None, 1600.0), ("candidate", "no successful outcomes", 0.0)):
                session = Path(root) / label / "r1"
                session.mkdir(parents=True)
                (session / "shared.json").write_text(json.dumps(
                    {"scenario": "jobs-shared-containers", "successes_per_second": rate,
                     "measurement_complete": True, "error": error}))
            result = compare_profile.compare(profile_windows.collect(root), {})
        self.assertEqual([(f["rule"], f["subject"]) for f in result["findings"]],
                         [("failed-window", "Shared containers, candidate")])
        self.assertEqual(result["rates"][0]["verdict"], "not compared")

    def test_bun_busy_while_idle_would_fail(self):
        with tempfile.TemporaryDirectory() as root:
            write_idle(Path(root) / "previous" / "r1", bun_ticks_used=30)
            write_idle(Path(root) / "candidate" / "r1", bun_ticks_used=300)
            result = compare_profile.compare(profile_windows.collect(root), {})
        rules = [f["rule"] for f in result["findings"] if f["severity"] == compare_profile.WOULD_FAIL]
        self.assertIn("idle-cpu", rules)
        idle = {row["field"]: row for row in result["idle"]}
        self.assertAlmostEqual(idle["bun_cpu_cores"]["candidate"]["median"], 0.05)

    def test_a_quiet_idle_cluster_passes(self):
        with tempfile.TemporaryDirectory() as root:
            write_idle(Path(root) / "previous" / "r1", bun_ticks_used=30)
            write_idle(Path(root) / "candidate" / "r1", bun_ticks_used=30)
            result = compare_profile.compare(profile_windows.collect(root), {})
        self.assertEqual(result["findings"], [])

    def test_category_rules_flag_new_moving_and_unmapped_categories(self):
        previous = categories_document({"Bun: Raft core and log": 90.0, "Bun: GitOps loop": 10.0})
        candidate = categories_document({"Bun: Raft core and log": 60.0, "Bun: GitOps loop": 10.0,
                                         "Bun: metrics (Mayo)": 20.0,
                                         profile_categories.BUN_UNCATEGORISED: 10.0})
        categories = {"previous": {"perfcpu-idle": previous}, "candidate": {"perfcpu-idle": candidate}}
        result = compare_profile.compare({}, categories)
        by_rule = {}
        for finding in result["findings"]:
            by_rule.setdefault(finding["rule"], []).append(finding)
        self.assertEqual([f["subject"] for f in by_rule["new-category"]],
                         ["Bun: metrics (Mayo) (idle cluster, share of Bun)"])
        moved = {f["subject"].split(" (")[0] for f in by_rule["category-move"]}
        self.assertIn("Bun: Raft core and log", moved)
        self.assertEqual(by_rule["uncategorised"][0]["severity"], compare_profile.EXTEND_MAP)

    def test_markdown_lists_every_flag_with_an_explanation_placeholder(self):
        with tempfile.TemporaryDirectory() as root:
            stage_1030(root)
            result = compare_profile.compare(profile_windows.collect(root), {})
        text = compare_profile.markdown(result, "v0.2.2", "v0.2.3")
        self.assertIn("## Compared with v0.2.2", text)
        self.assertIn("| Host jobs, default cap | 5,100.0 (5,100.0-5,100.0)/s | 7,500.0 (7,483.3-7,500.0)/s", text)
        self.assertEqual(text.count("*Explanation: to be written.*"), len(result["findings"]))


class PlanTests(unittest.TestCase):
    def test_rounds_alternate_which_version_goes_first(self):
        sessions = profile_plan.sessions(3)
        self.assertEqual(sessions[:6], [
            ("rates", "previous", 1), ("rates", "candidate", 1),
            ("rates", "candidate", 2), ("rates", "previous", 2),
            ("rates", "previous", 3), ("rates", "candidate", 3)])
        self.assertEqual(sessions[6:], [("perf", "previous", None), ("perf", "candidate", None)])

    def test_the_default_run_fits_two_hours(self):
        scenarios = list(profile_plan.DEFAULT_SCENARIOS)
        self.assertLessEqual(profile_plan.estimate(scenarios, 3, 60), profile_plan.BUDGET_SECONDS)
        self.assertEqual(profile_plan.fit(scenarios, 3, 60), (3, 0))

    def test_an_oversized_run_drops_rounds_never_scenarios(self):
        scenarios = list(profile_plan.KNOWN_SCENARIOS)
        kept, dropped = profile_plan.fit(scenarios, 10, 60)
        self.assertGreaterEqual(kept, 1)
        self.assertEqual(kept + dropped, 10)
        self.assertLessEqual(profile_plan.estimate(scenarios, kept, 60), profile_plan.BUDGET_SECONDS)
        self.assertEqual(profile_plan.fit(scenarios, 10, 60, budget=1), (1, 9))

    def test_unknown_or_repeated_scenarios_are_refused(self):
        with self.assertRaises(ValueError):
            profile_plan.parse_scenarios("raw,jobs")
        with self.assertRaises(ValueError):
            profile_plan.parse_scenarios("raw,raw")
        self.assertEqual(profile_plan.parse_scenarios("raw, idle"), ["raw", "idle"])


class FetchTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        root = Path(self.directory.name)
        self.bun = root / "bun"
        self.bun.write_bytes(b"bun bytes")
        self.record = root / "candidate.json"
        self.record.write_text(json.dumps({
            "version": "v0.2.4", "commit": "a" * 40, "run_id": 7, "run_attempt": 1,
            "repository": "reliaburger/reliaburger",
            "assets": {"bun-linux-aarch64": {"sha256": profile_fetch.sha256_file(self.bun), "size": 9}}}))

    def tearDown(self):
        self.directory.cleanup()

    def test_matching_assets_give_the_identity(self):
        identity = profile_fetch.verify(self.record, {"bun-linux-aarch64": self.bun},
                                        profile_fetch.sha256_file(self.record))
        self.assertEqual(identity["version"], "v0.2.4")
        self.assertIn("run_id=7", profile_fetch.shell_assignments(identity))

    def test_a_changed_asset_is_refused(self):
        self.bun.write_bytes(b"other bytes")
        with self.assertRaisesRegex(profile_fetch.VerifyError, "bun-linux-aarch64 has SHA-256"):
            profile_fetch.verify(self.record, {"bun-linux-aarch64": self.bun})

    def test_a_manifest_that_is_not_the_qualified_one_is_refused(self):
        with self.assertRaisesRegex(profile_fetch.VerifyError, "qualified digest"):
            profile_fetch.verify(self.record, {}, "0" * 64)

    def test_an_asset_the_manifest_does_not_list_is_refused(self):
        with self.assertRaisesRegex(profile_fetch.VerifyError, "lists no relish"):
            profile_fetch.verify(self.record, {"relish-linux-aarch64": self.bun})


class ReadmeTests(unittest.TestCase):
    def render(self, mode):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        record = Path(directory.name)
        stage_1030(record / "raw")
        write_idle(record / "raw" / "previous" / "r1", bun_ticks_used=30)
        write_idle(record / "raw" / "candidate" / "r1", bun_ticks_used=30)
        perf = record / "raw" / "candidate" / "perf"
        perf.mkdir()
        (perf / "perfcpu-idle.categories.json").write_text(json.dumps(
            categories_document({profile_categories.BUN_UNCATEGORISED: 50.0, "Bun: GitOps loop": 50.0})))
        (record / "flamegraphs").mkdir()
        (record / "flamegraphs" / "diff-host-oncpu-vm.svg").write_text("<svg/>")
        (record / "metadata.tsv").write_text("\n".join(f"{k}\t{v}" for k, v in {
            "mode": mode, "date": "2026-10-26", "previous_version": "v0.2.2", "candidate_version": "v0.2.3",
            "rounds_planned": "5", "rounds_run": "3", "rounds_dropped": "2", "estimate_seconds": "6000",
            "wall_seconds": "6600", "command": "scripts/release/qualify-profile.sh --smoke"}.items()) + "\n")
        (record / "host-load.tsv").write_text(
            "2026-10-26T10:00:00Z\tstart previous r1\t3.10\t0\n"
            "2026-10-26T10:00:05Z\tsample\t3.20\t2\n"
            "2026-10-26T10:05:00Z\tend previous r1\t4.20\t0\n")
        comparison = profile_readme.render(str(record))
        return (record / "README.md").read_text(), comparison, record

    def test_readme_has_the_answer_comparison_rates_and_rig(self):
        text, comparison, record = self.render("release")
        for heading in ("## Answer", "## Compared with v0.2.2", "### Would flag", "## Rates",
                        "## Where the time goes", "## The rig", "### Host load", "### Wall time",
                        "## Reproduce", "## Files"):
            self.assertIn(heading, text)
        self.assertIn(profile_readme.ANSWER_PLACEHOLDER, text)
        self.assertIn("| previous r1 | 3.10 | 4.20 | 0 |", text)
        self.assertIn("compilers running in 1 of 1 samples", text)
        self.assertIn("2 of the 5 planned rounds dropped", text)
        self.assertIn("flamegraphs/diff-host-oncpu-vm.svg", text)
        self.assertIn("extend BUN_RULES", text)
        self.assertTrue((record / "comparison.json").exists())
        self.assertTrue((record / "summary.json").exists())
        self.assertGreater(len(comparison["findings"]), 0)

    def test_a_smoke_readme_says_it_is_not_a_record(self):
        text, _, _ = self.render("smoke")
        self.assertIn("Smoke run, not a qualification record", text)


class ConfigTests(unittest.TestCase):
    def test_existing_keys_are_set_in_their_own_sections(self):
        text = ("[node]\nenabled = false\n\n[process_workloads]\nmount_isolation = true\nallowed_binaries = []\n\n"
                "[ebpf]\nsweep_interval_secs = 5\nenabled = false\n")
        edited = profile_config.edit(text, "/opt/bench/busybox")
        profile_config.check(edited, "/opt/bench/busybox")
        self.assertIn("[node]\nenabled = false", edited)

    def test_missing_keys_and_sections_are_added(self):
        edited = profile_config.edit("[node]\nname = \"n\"\n", "/opt/bench/busybox")
        profile_config.check(edited, "/opt/bench/busybox")

    def test_a_config_that_did_not_take_is_refused(self):
        with self.assertRaises(ValueError):
            profile_config.check("[ebpf]\nenabled = false\n", "/opt/bench/busybox")


class ScriptTests(unittest.TestCase):
    SCRIPT = HERE / "qualify-profile.sh"

    def run_script(self, *arguments):
        env = dict(os.environ, RELIABURGER_PROFILE_CAFFEINATED="1")
        return subprocess.run(["bash", str(self.SCRIPT), *arguments], capture_output=True, text=True, env=env)

    def test_plan_prints_the_sessions_and_touches_nothing(self):
        result = self.run_script("--plan", "--candidate", "https://example.invalid/v0.2.4", "--previous", "v0.2.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("round 2: candidate", result.stdout)
        self.assertIn("estimated wall time", result.stdout)

    def test_smoke_plan_is_one_short_round(self):
        result = self.run_script("--dry-run", "--smoke", "--candidate", "https://example.invalid/v0.2.4")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("rounds: 1 per version, alternating order, 30 s windows", result.stdout)

    def test_a_candidate_is_required(self):
        result = self.run_script("--plan")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--candidate", result.stderr)

    def test_unknown_scenarios_are_refused_before_anything_runs(self):
        result = self.run_script("--plan", "--candidate", "https://example.invalid/x", "--scenarios", "raw,nope")
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
