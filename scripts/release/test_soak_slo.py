"""The soak's latency SLOs (#1400): what a user feels on a quiet three-node
cluster, measured in every soak tier, gated in the fast and final tiers."""
import contextlib
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest

import soak_slo
import sustained_check as checker

NOW = 1_790_000_000
NODES = ["rb-a-1", "rb-a-2", "rb-a-3"]


def lag_counts(p99_bucket, samples=1000):
    """Start-lag bucket counts whose 99th percentile falls in `p99_bucket` ms."""
    counts = [0] * 16
    index = soak_slo.job_bucket_index(p99_bucket)
    counts[0] = samples - samples // 50
    counts[index] = samples // 50
    return counts


class Histogram(unittest.TestCase):

    def test_the_p99_is_the_upper_bound_of_its_bucket(self):
        histogram = soak_slo.new_histogram()
        for ms in [10] * 98 + [140, 600]:
            soak_slo.observe(histogram, ms)
        self.assertEqual(histogram["count"], 100)
        self.assertEqual(histogram["max"], 600)
        self.assertEqual(soak_slo.p99(histogram), 150)

    def test_an_empty_histogram_has_no_p99(self):
        self.assertIsNone(soak_slo.p99(soak_slo.new_histogram()))

    def test_a_sample_past_the_last_bucket_reports_the_maximum(self):
        histogram = soak_slo.new_histogram()
        soak_slo.observe(histogram, 9_000_000)
        self.assertEqual(soak_slo.p99(histogram), 9_000_000)

    def test_every_threshold_is_a_bucket_bound(self):
        for slo in soak_slo.SLOS:
            if slo.source == "job":
                self.assertIn(slo.threshold_ms, soak_slo.JOB_BOUNDS_MS, slo.name)
            else:
                self.assertIn(slo.threshold_ms, soak_slo.BOUNDS_MS, slo.name)


class Rows(unittest.TestCase):
    """The checker's verdict for each SLO: pass, fail, and missing data."""

    def measured(self, read=20, write=300, start=2000, ready=9000):
        data = {}
        for name, ms in (("api-read", read), ("api-write", write), ("app-start", start), ("deploy-ready", ready)):
            for _ in range(50):
                soak_slo.record(data, name, ms)
        return data

    def job_report(self, p99_ms=32, samples=1000):
        counts = lag_counts(p99_ms, samples) if samples else [0] * 16
        return {"m8": {"p99_start_lag_ms": soak_slo.job_p99(counts), "start_lag_samples": sum(counts)}}

    def verdicts(self, rows):
        return {row["name"]: row["verdict"] for row in rows}

    def test_a_quiet_fast_tier_inside_every_slo_passes(self):
        rows = soak_slo.rows(self.measured(), self.job_report(), "fast")
        self.assertEqual(soak_slo.failures(rows), [])
        self.assertEqual(self.verdicts(rows)["job-start"], "PASS")

    def test_a_job_start_over_its_threshold_fails_a_gated_tier_with_its_own_reason(self):
        rows = soak_slo.rows(self.measured(), self.job_report(p99_ms=512), "final")
        self.assertEqual(self.verdicts(rows)["job-start"], "FAIL")
        self.assertEqual(soak_slo.failures(rows),
                         ["job-start: job task start p99 512 ms is over the 256 ms SLO"])

    def test_a_job_start_past_the_last_bucket_fails(self):
        counts = [0] * 16
        counts[-1] = 100
        report = {"m8": {"p99_start_lag_ms": soak_slo.job_p99(counts), "start_lag_samples": 100}}
        rows = soak_slo.rows(self.measured(), report, "fast")
        self.assertEqual(self.verdicts(rows)["job-start"], "FAIL")
        self.assertIn("over every start-lag bucket", soak_slo.failures(rows)[0])

    def test_a_gated_slo_with_no_samples_fails(self):
        for report in (None, self.job_report(samples=0)):
            with self.subTest(report=report):
                rows = soak_slo.rows(self.measured(), report, "fast")
                self.assertEqual(self.verdicts(rows)["job-start"], "FAIL")
                self.assertEqual(soak_slo.failures(rows), ["job-start: no job task start samples"])

    def test_record_only_slos_never_fail_the_tier(self):
        slow = self.measured(read=5000, write=30000, start=120000, ready=600000)
        rows = soak_slo.rows(slow, self.job_report(), "final")
        self.assertEqual(soak_slo.failures(rows), [])
        verdicts = self.verdicts(rows)
        for name in ("api-read", "api-write", "app-start", "deploy-ready"):
            self.assertEqual(verdicts[name], "over (record only)", name)

    def test_record_only_slos_with_no_samples_say_so(self):
        rows = soak_slo.rows({}, self.job_report(), "fast")
        self.assertEqual(soak_slo.failures(rows), [])
        self.assertEqual(self.verdicts(rows)["api-read"], "not measured")

    def test_a_run_without_a_job_campaign_has_no_job_starts_to_gate(self):
        rows = soak_slo.rows(self.measured(), None, "final", job_campaign=False)
        self.assertEqual(soak_slo.failures(rows), [])
        self.assertEqual(self.verdicts(rows)["job-start"], "not measured")

    def test_a_day_or_custom_tier_records_job_start_without_gating_it(self):
        for tier in ("day", "custom"):
            with self.subTest(tier=tier):
                rows = soak_slo.rows({}, self.job_report(p99_ms=4096), tier)
                self.assertEqual(soak_slo.failures(rows), [])
                self.assertEqual(self.verdicts(rows)["job-start"], "over (record only)")

    def test_a_failed_call_or_rollout_is_named_in_the_table(self):
        data = self.measured()
        soak_slo.record_failure(data, "deploy-ready", "not ready within 180 s")
        rows = soak_slo.rows(data, self.job_report(), "fast")
        row = [row for row in rows if row["name"] == "deploy-ready"][0]
        self.assertEqual(row["failed"], 1)
        line = [line for line in soak_slo.table_lines(rows) if line.startswith("| Rolling update p99 |")][0]
        self.assertIn("50 (1 failed)", line)


class ApiReads(unittest.TestCase):
    """measure_api is the function #1401 calls to time the same API reads."""

    def test_each_endpoint_answers_each_read_and_is_timed(self):
        ticks = iter(x / 1000 for x in range(0, 10_000, 7))
        calls = []

        def request(endpoint, path, token, ca, timeout):
            calls.append((endpoint["name"], path, token))
            return 200

        rows = soak_slo.measure_api([dict(name="rb-a-1", port=9117), dict(name="rb-a-2", port=9118)], "secret",
                                    "/ca.pem", request=request, clock=lambda: next(ticks))
        self.assertEqual([(row["node"], row["path"]) for row in rows],
                         [("rb-a-1", path) for path in soak_slo.READ_PATHS] +
                         [("rb-a-2", path) for path in soak_slo.READ_PATHS])
        self.assertTrue(all(row["ok"] and row["ms"] == 7 for row in rows))
        self.assertEqual({call[2] for call in calls}, {"secret"})

    def test_a_refused_or_failing_read_counts_with_the_time_the_user_waited(self):
        def request(endpoint, path, token, ca, timeout):
            if path == soak_slo.READ_PATHS[0]:
                raise TimeoutError("timed out")
            return 503

        rows = soak_slo.measure_api([dict(name="rb-a-1", port=9117)], "t", "/ca.pem", request=request,
                                    clock=iter([0, 10, 10, 10.2]).__next__)
        self.assertEqual([row["ok"] for row in rows], [False, False])
        self.assertEqual(rows[0]["ms"], 10000)
        self.assertEqual(rows[0]["error"], "TimeoutError: timed out")
        self.assertEqual(rows[1]["error"], "HTTP 503")

    def test_reads_fold_outside_fault_windows_only_and_skip_cut_forwards(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            rows = [dict(node="rb-a-1", path="/v1/cluster/nodes", ms=12, ok=True),
                    dict(node="rb-a-2", path="/v1/cluster/nodes", ms=9000, ok=False, error="TimeoutError: x")]
            soak_slo.fold_reads(evidence, rows, fault_window=True, cut=set())
            self.assertEqual(soak_slo.load(evidence), {})
            soak_slo.fold_reads(evidence, rows, fault_window=False, cut={"rb-a-2"})
            data = soak_slo.load(evidence)
            self.assertEqual(data["api-read"]["count"], 1)
            soak_slo.fold_reads(evidence, rows, fault_window=False, cut=set())
            data = soak_slo.load(evidence)
            self.assertEqual(data["api-read"]["count"], 3)
            self.assertEqual(data["api-read"]["failed"], 1)

    def test_the_subcommand_writes_the_snapshot_file(self):
        with tempfile.TemporaryDirectory() as directory:
            snapshot = Path(directory)
            header = snapshot / ".auth-header"
            header.write_text("Authorization: Bearer abc\n")
            seen = []
            original = soak_slo.https_get
            soak_slo.https_get = lambda endpoint, path, token, ca, timeout: seen.append((endpoint, token)) or 200
            try:
                code = soak_slo.main(["api-latency", str(snapshot), "--api-port", "9200", "--ca", "/ca.pem",
                                      "--auth-header", str(header), "--node", "rb-a-1", "--node", "rb-a-2",
                                      "--node", "rb-a-3", "--down", "2"])
            finally:
                soak_slo.https_get = original
            self.assertEqual(code, 0)
            rows = json.loads((snapshot / "api-latency.json").read_text())
            self.assertEqual({row["node"] for row in rows}, {"rb-a-1", "rb-a-3"})
            self.assertEqual({endpoint["port"] for endpoint, _ in seen}, {9200, 9202})
            self.assertEqual({token for _, token in seen}, {"abc"})


def history(*entries):
    return {"app": "soak-identity", "namespace": "default", "warnings": [], "history": [
        {"node": node, "id": deploy, "result": result,
         "created_at": {"secs_since_epoch": created // 1000, "nanos_since_epoch": created % 1000 * 1_000_000},
         "completed_at": {"secs_since_epoch": done // 1000, "nanos_since_epoch": done % 1000 * 1_000_000}}
        for node, deploy, result, created, done in entries]}


class Rollouts(unittest.TestCase):
    """App start and deploy-to-ready come from one quiet rolling update."""

    BEFORE = history(("rb-a-1", "d1", "Completed", 1_000_000, 1_002_000))

    def test_a_rollout_is_done_once_every_node_has_a_new_completed_entry(self):
        after = history(("rb-a-1", "d1", "Completed", 1_000_000, 1_002_000),
                        ("rb-a-1", "d2", "Completed", 2_000_000, 2_001_500),
                        ("rb-a-2", "d3", "Completed", 2_000_100, 2_003_100))
        self.assertEqual(soak_slo.rollout_progress(self.BEFORE, after, 3), ("waiting", []))
        after["history"].append(history(("rb-a-3", "d4", "Completed", 2_000_200, 2_002_450))["history"][0])
        self.assertEqual(soak_slo.rollout_progress(self.BEFORE, after, 3), ("done", [1500, 3000, 2250]))

    def test_a_new_entry_that_did_not_complete_fails_the_rollout(self):
        after = history(("rb-a-1", "d1", "Completed", 1_000_000, 1_002_000),
                        ("rb-a-2", "d5", "RolledBack", 2_000_000, 2_010_000))
        state, detail = soak_slo.rollout_progress(self.BEFORE, after, 3)
        self.assertEqual(state, "failed")
        self.assertIn("rb-a-2 RolledBack", detail)

    def test_a_rollout_records_write_start_and_ready_samples(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            soak_slo.record_rollout(evidence, apply_ms=400, ready_ms=8000, starts=[1500, 3000, 2250])
            data = soak_slo.load(evidence)
            self.assertEqual(data["api-write"]["count"], 1)
            self.assertEqual(data["app-start"]["count"], 3)
            self.assertEqual(data["deploy-ready"]["max"], 8000)
            soak_slo.record_rollout(evidence, apply_ms=200, failed="not ready within 180 s")
            data = soak_slo.load(evidence)
            self.assertEqual(data["deploy-ready"]["failed"], 1)
            self.assertEqual(data["deploy-ready"]["failures"], ["not ready within 180 s"])
            self.assertEqual(data["api-write"]["count"], 2)

    def test_the_rolled_subcommand_exits_by_state(self):
        with tempfile.TemporaryDirectory() as directory:
            before, after = Path(directory) / "before.json", Path(directory) / "after.json"
            before.write_text(json.dumps(self.BEFORE))
            after.write_text(json.dumps(self.BEFORE))
            self.assertEqual(soak_slo.main(["rolled", str(before), str(after), "1"]), 1)
            after.write_text(json.dumps(history(("rb-a-1", "d2", "Completed", 2_000_000, 2_001_000))))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(soak_slo.main(["rolled", str(before), str(after), "1"]), 0)
            self.assertEqual(out.getvalue().split(), ["1000"])
            after.write_text(json.dumps(history(("rb-a-1", "d2", "Failed", 2_000_000, 2_001_000))))
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(soak_slo.main(["rolled", str(before), str(after), "1"]), 2)


CANDIDATE = b'{"candidate": true}\n'


class Record(unittest.TestCase):
    """Every record gets the SLO table; a gated breach fails the tier."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.evidence = Path(self.temp.name) / "evidence"
        self.evidence.mkdir()

    def run_with(self, tier="fast", lag=lag_counts(32)):
        digest = hashlib.sha256(CANDIDATE).hexdigest()
        (self.evidence / "metadata.json").write_text(json.dumps(dict(
            tier=tier, schedule="compressed", started_at=NOW, job_soak=True, finished_at=NOW + 3600, duration_target=3600,
            candidate_digest=f"`{digest}`", qualified_digest=digest, ingress_rotation_secs=None)))
        checker.save_state(self.evidence, {"nodes": NODES})
        checker.append_event(self.evidence, ts=NOW, phase="baseline", verdict="ok", detail="1")
        (self.evidence / "jobs").mkdir(exist_ok=True)
        (self.evidence / "jobs" / "snapshot.json").write_text(json.dumps(dict(
            started=NOW, heartbeat=NOW + 3500, metrics=dict(start_lag_ms_counts=lag))))
        for _ in range(20):
            soak_slo.record_rollout(self.evidence, apply_ms=300, ready_ms=9000, starts=[2000, 2500, 3000])
        for _ in range(20):
            soak_slo.fold_reads(self.evidence, [dict(node=node, path="/v1/status?cluster=true", ms=40, ok=True)
                                                for node in NODES], fault_window=False, cut=set())

    def render(self):
        record = self.evidence / "record.md"
        code = checker.render(self.evidence, record)
        return code, record.read_text()

    def test_every_record_has_the_slo_table(self):
        self.run_with()
        _, text = self.render()
        section = text.split("## Latency SLOs")[1].split("\n## ")[0]
        self.assertIn("| API read p99 | 50 ms | ≤ 1,000 ms | 60 | record only | PASS |", section)
        self.assertIn("| Job task start p99 | 32 ms | ≤ 256 ms |", section)
        self.assertIn("gated", section)

    def test_a_breached_slo_fails_the_record_with_its_own_reason(self):
        self.run_with(lag=lag_counts(1024))
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("# Sustained soak (V02): FAIL", text)
        self.assertIn("job-start: job task start p99 1,024 ms is over the 256 ms SLO", text)
        self.assertIn("**Fast tier: not clean.**", text)

    def test_a_record_with_no_job_start_samples_fails_a_gated_tier(self):
        self.run_with(tier="final", lag=[0] * 16)
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("job-start: no job task start samples", text)


class Trend(unittest.TestCase):
    """A plain listing of every record's SLO values, oldest first."""

    def test_the_trend_reads_tables_and_falls_back_to_older_job_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            (folder / "2026-10-15-v0.2.0-sustained-v02-fast.md").write_text("# Sustained soak (V02): PASS\n")
            (folder / "2026-10-15-v0.2.0-sustained-v02-fast.jobs.json").write_text(json.dumps(
                {"m8": {"p99_start_lag_ms": 4, "start_lag_samples": 54122}}))
            (folder / "2026-11-02-v0.2.4-job-qualification-fast.md").write_text("\n".join([
                "# Sustained soak (V02): PASS", "", "## Latency SLOs", "",
                "| SLO | p99 | Threshold | Samples | Mode | Verdict |", "|---|---|---|---|---|---|",
                "| API read p99 | 50 ms | ≤ 1,000 ms | 600 | record only | PASS |",
                "| Job task start p99 | 32 ms | ≤ 256 ms | 5,000 | gated | PASS |", "", "## Data", ""]))
            (folder / "2026-10-20-dependency-exceptions.md").write_text("# Dependencies\n")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(soak_slo.main(["trend", str(folder)]), 0)
        lines = out.getvalue().splitlines()
        self.assertEqual(len(lines), 3, lines)
        self.assertIn("API read p99", lines[0])
        self.assertTrue(lines[1].startswith("2026-10-15-v0.2.0-sustained-v02-fast"))
        self.assertIn("4 ms", lines[1])
        self.assertIn("50 ms", lines[2])
        self.assertIn("32 ms", lines[2])

    def test_the_repository_trend_runs(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(soak_slo.main(["trend", str(Path(__file__).parents[2] / "docs" / "qualification")]), 0)
        self.assertIn("2026-10-25-v0.2.2-job-qualification", out.getvalue())


class Harness(unittest.TestCase):
    """The harness measures the SLOs in every tier, outside fault windows."""

    script = (Path(__file__).parent / "qualify-sustained.sh").read_text()

    def test_light_checks_time_api_reads_outside_fault_windows(self):
        light = self.script[self.script.index("collect_light() {"):self.script.index("probe_forwards() {")]
        # After the forwards probe, so the checker knows which forwards are cut.
        self.assertIn('probe_forwards "$directory"\n    [ -n "$window_label" ] || slo_reads "$directory"', light)
        reads = self.script[self.script.index("slo_reads() {"):]
        self.assertIn('slo api-latency "$directory"', reads[:reads.index("\n}\n")])

    def test_the_rollouts_roll_their_own_app_with_a_health_check(self):
        template = (Path(__file__).parent / "sustained" / "soak-workloads.toml").read_text()
        app = template[template.index("[app.soak-slo]"):]
        self.assertIn('replicas = "*"', app)
        self.assertIn('env = { SOAK_SLO_GENERATION = "0" }', app)
        self.assertIn("[app.soak-slo.health]", app)
        # The health check serves a file the command writes itself, so it
        # never depends on what the image ships (/etc/hostname is a Docker
        # runtime mount, not part of the image).
        self.assertIn('path = "/health"', app)
        self.assertIn("echo ok > /srv/soak-slo/health", app)
        self.assertIn("httpd -f -p 8080 -h /srv/soak-slo", app)
        self.assertNotIn("/etc", app.split("[app.soak-slo.health]")[0].split("command =")[1])
        generations = self.script[self.script.index("soak_workloads() {"):self.script.index("write_workloads() {")]
        self.assertIn("SOAK_SLO_GENERATION", generations)
        self.assertIn('"soak-slo": 3', self.script)

    def test_the_waits_run_a_quiet_rolling_update_on_their_own_cadence(self):
        wait = self.script[self.script.index("wait_until() {"):self.script.index("run_slot() {")]
        self.assertIn("run_slot slo_rollout", wait)
        rollout = self.script[self.script.index("slo_rollout() {"):]
        rollout = rollout[:rollout.index("\n}\n")]
        self.assertIn('[ -z "$window_label" ] || return 0', rollout)
        self.assertIn("slo rolled", rollout)
        self.assertIn("slo rollout", rollout)
        # The probe's apply never carries the job fixtures (#963).
        self.assertNotIn("write_workloads", rollout)
        self.assertNotIn("soak-jobs.toml", rollout)

    def test_evaluate_folds_the_reads(self):
        source = (Path(__file__).parent / "sustained_check.py").read_text()
        self.assertIn("soak_slo.fold_snapshot(", source)


if __name__ == "__main__":
    unittest.main()
