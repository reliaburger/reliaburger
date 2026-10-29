"""Soak charts draw a run's resource and job time series from its evidence (#979)."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
import time
import unittest
import xml.etree.ElementTree as ET

import soak_charts as charts
import sustained_check as checker

START = 1_790_000_000
NODES = ["rb-a-1", "rb-a-2", "rb-a-3"]


def stamp(ts):
    return time.strftime("%Y%m%dT%H%M%S", time.gmtime(ts))


class Evidence(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.evidence = Path(self.directory.name) / "evidence"
        (self.evidence / "snapshots").mkdir(parents=True)
        self.number = 0
        self.write_metadata()

    def tearDown(self):
        self.directory.cleanup()

    def write_metadata(self, **extra):
        metadata = {"started_at": START, "finished_at": START + 7200, "tier": "final"}
        metadata.update(extra)
        (self.evidence / "metadata.json").write_text(json.dumps(metadata))
        (self.evidence / "state.json").write_text(json.dumps({"nodes": NODES}))

    def snapshot(self, ts, kind="heavy", inventories=None, jobs=None):
        self.number += 1
        directory = self.evidence / "snapshots" / f"{stamp(ts)}-{kind}-{self.number}"
        directory.mkdir()
        (directory / "meta.json").write_text(json.dumps({"ts": ts, "kind": kind}))
        for node, values in (inventories or {}).items():
            text = "".join(f"{key} {value}\n" for key, value in values.items())
            (directory / f"inventory-{node}.txt").write_text("boot b1\nclk_tck 100\n" + text + "runc x\n")
        if jobs is not None:
            (directory / "jobs.json").write_text(json.dumps(jobs))
        return directory

    def inventory(self, rss_mib, fds=100, ticks=0, pid=10):
        return {"bun_pid": pid, "bun_rss_kb": int(rss_mib * 1024), "bun_fd": fds, "bun_cpu_ticks": ticks}

    def jobs(self, succeeded, target=1390):
        return {"target_rate": target, "progress": {"process": {"succeeded": succeeded}, "runc": {"succeeded": 0}}}

    def event(self, ts, phase, **fields):
        with open(self.evidence / "events.jsonl", "a") as out:
            out.write(json.dumps(dict(ts=ts, phase=phase, **fields)) + "\n")

    def full_run(self):
        for minute in range(0, 120, 10):
            ts = START + minute * 60
            self.snapshot(ts, inventories={node: self.inventory(200 + minute + index, 90 + index, ticks=minute * 6000,
                                                                pid=10 + index)
                                           for index, node in enumerate(NODES)})
            self.snapshot(ts + 30, kind="light", jobs=self.jobs(minute * 60 * 1500))
        self.event(START + 600, "fault:power-off", target="rb-a-3", command="limactl stop --force")
        self.event(START + 700, "fault:power-off", verdict="settled", settle_seconds=20)
        self.event(START + 1800, "upgrade:upgrade", target="all")
        self.event(START + 3600, "special:quorum-loss")
        self.event(START + 3000, "failure", target="settle", verdict="fail")


class Collect(Evidence):
    def test_resource_series_come_from_snapshots_that_carry_inventories(self):
        self.full_run()
        series = charts.collect(self.evidence)
        self.assertEqual(series.nodes, NODES)
        rss = series.rss["rb-a-1"]
        self.assertEqual(len(rss), 12)
        self.assertEqual(rss[0], (0, 200.0))
        self.assertEqual(rss[-1], (110 * 60, 310.0))
        self.assertEqual(series.fds["rb-a-2"][0], (0, 91))

    def test_cpu_is_the_tick_rate_between_samples_of_the_same_process(self):
        self.full_run()
        series = charts.collect(self.evidence)
        # 6,000 ticks at 100 a second over 600 s is one whole core: 100%.
        self.assertEqual(series.cpu["rb-a-1"][0], (600, 100.0))
        self.assertEqual(len(series.cpu["rb-a-1"]), 11)

    def test_a_restarted_process_starts_a_new_cpu_line_instead_of_a_negative_rate(self):
        self.snapshot(START, inventories={"rb-a-1": self.inventory(200, ticks=5000, pid=10)})
        self.snapshot(START + 600, inventories={"rb-a-1": self.inventory(150, ticks=100, pid=11)})
        self.snapshot(START + 1200, inventories={"rb-a-1": self.inventory(160, ticks=700, pid=11)})
        cpu = charts.collect(self.evidence).cpu["rb-a-1"]
        self.assertEqual(cpu, [(1200, 1.0)])

    def test_accepted_jobs_a_second_is_windowed_from_the_cumulative_successes(self):
        self.full_run()
        series = charts.collect(self.evidence)
        self.assertEqual(series.target_rate, 1390.0)
        self.assertTrue(series.jobs)
        for _, rate in series.jobs:
            self.assertAlmostEqual(rate, 1500.0, places=6)

    def test_faults_upgrades_and_specials_become_marks_and_settles_do_not(self):
        self.full_run()
        marks = charts.collect(self.evidence).marks
        self.assertEqual([(offset, kind) for offset, kind, _ in marks],
                         [(600, "fault"), (1800, "upgrade"), (3000, "failure"), (3600, "special")])

    def test_an_empty_evidence_directory_has_no_series(self):
        series = charts.collect(self.evidence)
        self.assertEqual(series.rss, {})
        self.assertEqual(series.jobs, [])
        self.assertEqual(charts.write_charts(self.evidence, Path(self.directory.name) / "out"), [])


class Draw(Evidence):
    def written(self):
        out = Path(self.directory.name) / "out"
        return out, charts.write_charts(self.evidence, out)

    def test_a_job_soak_draws_four_charts_that_parse_as_svg(self):
        self.full_run()
        out, written = self.written()
        self.assertEqual([chart.name for chart in written], ["rss", "fds", "cpu", "jobs"])
        for chart in written:
            root = ET.parse(out / chart.file).getroot()
            self.assertEqual(root.tag, "{http://www.w3.org/2000/svg}svg")
            self.assertTrue(root.find("{http://www.w3.org/2000/svg}title").text)

    def test_charts_colour_through_css_variables_with_a_dark_scheme(self):
        self.full_run()
        out, _ = self.written()
        text = (out / "rss.svg").read_text()
        self.assertIn("var(--series-1,#2a78d6)", text)
        self.assertIn("prefers-color-scheme: dark", text)
        self.assertNotIn('stroke="#', text)

    def test_the_memory_chart_draws_both_bun_targets(self):
        self.full_run()
        out, _ = self.written()
        text = (out / "rss.svg").read_text()
        self.assertIn("512 MiB", text)
        self.assertIn("384 MiB", text)

    def test_the_jobs_chart_draws_the_target_rate(self):
        self.full_run()
        out, _ = self.written()
        self.assertIn("target 1,390/s", (out / "jobs.svg").read_text())

    def test_every_chart_marks_faults_upgrades_and_specials(self):
        self.full_run()
        out, written = self.written()
        for chart in written:
            text = (out / chart.file).read_text()
            for kind in ("fault", "upgrade", "special", "failure"):
                self.assertIn(f'class="mark {kind}"', text, chart.name)

    def test_a_single_series_chart_has_no_legend(self):
        self.full_run()
        out, _ = self.written()
        self.assertNotIn(">accepted jobs</text>", (out / "jobs.svg").read_text())
        self.assertIn(">node 3</text>", (out / "rss.svg").read_text())

    def test_output_is_byte_for_byte_deterministic(self):
        self.full_run()
        out, written = self.written()
        first = {chart.file: (out / chart.file).read_bytes() for chart in written}
        again = Path(self.directory.name) / "again"
        charts.write_charts(self.evidence, again)
        self.assertEqual(first, {name: (again / name).read_bytes() for name in first})

    def test_an_app_only_soak_has_no_jobs_chart(self):
        self.snapshot(START, inventories={"rb-a-1": self.inventory(200)})
        self.snapshot(START + 600, inventories={"rb-a-1": self.inventory(210, ticks=60)})
        _, written = self.written()
        self.assertEqual([chart.name for chart in written], ["rss", "fds", "cpu"])

    def test_a_single_sample_draws_nothing_rather_than_a_degenerate_chart(self):
        self.snapshot(START, inventories={"rb-a-1": self.inventory(200)})
        _, written = self.written()
        self.assertEqual(written, [])


class ShortRuns(unittest.TestCase):
    """The demos (#671) draw minutes, not hours, with their steps as marks."""

    LINE = [("bun", 1, [(0, 40.0), (90, 42.0), (180, 41.0)])]

    def test_a_run_of_a_few_minutes_labels_its_time_axis_in_seconds(self):
        svg = charts.draw("Bun memory", "MiB", self.LINE, 180, [], key=())
        self.assertIn(">60 s</text>", svg)
        self.assertNotIn(" min</text>", svg)

    def test_a_soak_keeps_its_minute_and_hour_axis(self):
        self.assertEqual(charts.time_step(3600), 300)
        self.assertEqual(charts.time_step(8 * 3600), 3600)

    def test_a_chart_marks_demo_steps_under_its_own_key(self):
        svg = charts.draw("Bun memory", "MiB", self.LINE, 180, [(30, "step", "Deploy v1")],
                          key=(("step", "demo step"),))
        self.assertIn('class="mark step"', svg)
        self.assertIn("<title>step: Deploy v1 at 30 s</title>", svg)
        self.assertIn(">demo step</text>", svg)
        self.assertNotIn(">upgrade walk</text>", svg)
        ET.fromstring(svg)


class Record(Evidence):
    def test_render_writes_charts_beside_the_record_and_links_them(self):
        self.full_run()
        record = Path(self.directory.name) / "docs" / "2026-10-17-run.md"
        checker.render(self.evidence, record)
        text = record.read_text()
        self.assertIn("## Charts", text)
        self.assertIn("![Bun resident memory per node](2026-10-17-run-charts/rss.svg)", text)
        self.assertTrue((record.parent / "2026-10-17-run-charts" / "jobs.svg").exists())

    def test_render_without_samples_says_so_instead_of_linking(self):
        record = Path(self.directory.name) / "record.md"
        checker.render(self.evidence, record)
        text = record.read_text()
        self.assertIn("## Charts\n\nNo resource samples were recorded.", text)
        self.assertFalse((Path(self.directory.name) / "record-charts").exists())

    def test_render_charts_rebuilds_from_existing_evidence_and_prints_the_section(self):
        self.full_run()
        out = Path(self.directory.name) / "qualification" / "run-charts"
        printed = io.StringIO()
        with contextlib.redirect_stdout(printed):
            self.assertEqual(checker.main(["render-charts", str(self.evidence), str(out)]), 0)
        self.assertTrue((out / "rss.svg").exists())
        self.assertIn("![Accepted jobs a second](run-charts/jobs.svg)", printed.getvalue())


if __name__ == "__main__":
    unittest.main()
