"""The V02 soak checker judges evidence the way section 4 of the plan says."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
import time
import unittest

import sustained_check as checker

NOW = 1_790_000_000
NODES = ["rb-a-1", "rb-a-2", "rb-a-3"]


def wtf(critical=(), warnings=(), ok=("replicas",)):
    entry = lambda item: {"id": item[0], "title": item[0], "affected_resource": item[1]}
    return {"critical": [entry(item) for item in critical], "warnings": [entry(item) for item in warnings],
            "ok": [{"id": name, "description": name} for name in ok]}


def nodes(leader="rb-a-1", down=()):
    return [{"node_id": name, "state": "dead" if name in down else "alive", "is_council": True,
             "is_leader": name == leader} for name in NODES]


INVENTORY = """boot b1
nrestarts 0
bun_pid 100
bun_fd 300
bun_rss_kb 300000
runc default__web-0
netns rb-default__web-0
veth veth-1
cgroup default/web/0
bpf reliaburger-x/backend_map
listen 0.0.0.0:9117
lease default__web-0
disk logs 20
"""


class Evidence(unittest.TestCase):
    """A temporary evidence directory with the three nodes registered."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.evidence = Path(self.temp.name)
        checker.save_state(self.evidence, {"nodes": NODES})
        self.count = 0

    def snapshot(self, ts=NOW, kind="heavy", **files):
        self.count += 1
        directory = self.evidence / "snapshots" / str(self.count)
        directory.mkdir(parents=True)
        (directory / "meta.json").write_text(json.dumps({"ts": ts, "kind": kind}))
        for name, content in files.items():
            name = name.replace("__", "-").replace("_", ".")
            (directory / name).write_text(content if isinstance(content, str) else json.dumps(content))
        return directory

    def evaluate(self, directory):
        with contextlib.redirect_stdout(io.StringIO()):
            code = checker.evaluate(self.evidence, directory)
        return code, json.loads((directory / "verdict.json").read_text())

    def failures(self, verdict):
        return [item["check"] for item in verdict["findings"] if item["severity"] == "fail"]


class WriterAndRedis(Evidence):
    def test_writer_file_ending_below_an_acknowledged_write_is_a_regression(self):
        self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 10\nACK 11\nACK 12\n"}))
        code, verdict = self.evaluate(self.snapshot(**{"writer__file_txt": "LAST 11\n"}))
        self.assertEqual(code, 1)
        self.assertIn("writer-regression", self.failures(verdict))

    def test_writer_file_with_a_gap_fails(self):
        code, verdict = self.evaluate(self.snapshot(**{"writer__file_txt": "BAD 7: 8\nLAST 7\n"}))
        self.assertEqual(code, 1)
        self.assertIn("writer-gap", self.failures(verdict))

    def test_writer_file_at_or_beyond_the_highest_ack_passes(self):
        self.evaluate(self.snapshot(**{"writer__log_txt": "RESUME 0 after 0 lines\nACK 1\nACK 2\n"}))
        code, verdict = self.evaluate(self.snapshot(**{"writer__file_txt": "LAST 5\n"}))
        self.assertEqual(self.failures(verdict), [])

    def test_redis_counter_going_backwards_within_one_tail_fails(self):
        code, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 41\nINCR 42\nINCR 1\nINCR 2\n"}))
        self.assertEqual(code, 1)
        self.assertIn("redis-log-order", self.failures(verdict))

    def test_redis_counter_below_an_earlier_tail_fails(self):
        self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 41\nINCR 42\n"}))
        code, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "ERR Could not connect\nINCR 3\nINCR 4\n"}))
        self.assertIn("redis-log-order", self.failures(verdict))

    def test_writer_log_going_backwards_is_a_log_order_failure_not_data_loss(self):
        self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 10\nACK 11\n"}))
        code, verdict = self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 12\nACK 9\nACK 13\n",
                                                        "writer__file_txt": "LAST 13\n"}))
        self.assertEqual(code, 1)
        self.assertEqual(self.failures(verdict), ["writer-log-order"])
        detail = next(item["detail"] for item in verdict["findings"] if item["check"] == "writer-log-order")
        self.assertIn("log view", detail)
        self.assertIn("writer file", detail)

    def test_log_order_findings_never_claim_data_loss(self):
        self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 41\nINCR 42\n", "writer__log_txt": "ACK 10\n"}))
        _, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 3\nINCR 2\n", "writer__log_txt": "ACK 5\nACK 4\n"}))
        details = [item["detail"] for item in verdict["findings"] if item["check"].endswith("-log-order")]
        self.assertEqual(len(details), 4)
        for detail in details:
            self.assertTrue(detail.startswith("log view"), detail)
            self.assertNotIn("acknowledged", detail)

    def power_cut(self):
        with contextlib.redirect_stdout(io.StringIO()):
            checker.main(["power-cut", str(self.evidence)])

    def test_a_tail_ending_lower_after_a_power_cut_is_info_once(self):
        self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 40639\nACK 40640\n",
                                        "redis__log_txt": "INCR 2503\nINCR 2504\n"}))
        self.power_cut()
        code, verdict = self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 40313\nACK 40314\n",
                                                        "redis__log_txt": "INCR 781\nINCR 782\n"}))
        self.assertEqual(self.failures(verdict), [])
        excused = [item for item in verdict["findings"] if item["check"].endswith("-log-order")]
        self.assertEqual({item["severity"] for item in excused}, {"info"})
        self.assertTrue(all("power cut" in item["detail"] for item in excused))
        # The baseline restarts from the lower tail; the next tail only has to rise.
        _, verdict = self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 40641\n", "redis__log_txt": "INCR 2600\n"}))
        self.assertEqual(self.failures(verdict), [])
        # Once the tails have advanced outside a window the excuse is spent.
        _, verdict = self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 10\n"}))
        self.assertIn("writer-log-order", self.failures(verdict))

    def test_a_power_cut_never_lowers_what_the_writer_file_must_hold(self):
        self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 40640\n"}))
        self.power_cut()
        self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 40314\n"}))
        code, verdict = self.evaluate(self.snapshot(**{"writer__file_txt": "LAST 40314\n"}))
        self.assertEqual(code, 1)
        self.assertIn("writer-regression", self.failures(verdict))

    def test_lines_going_backwards_within_one_tail_still_fail_after_a_power_cut(self):
        self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 41\n"}))
        self.power_cut()
        _, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 50\nINCR 45\n"}))
        self.assertIn("redis-log-order", self.failures(verdict))

    def test_redis_errors_during_an_outage_do_not_fail(self):
        self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 41\n"}))
        code, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 41\nERR Could not connect\nINCR 42\n"}))
        self.assertEqual(self.failures(verdict), [])


class Certificates(Evidence):
    def diagnostics(self, not_after, rotation_state="valid", serial="0a"):
        return {"certificates": {"state": "available", "value": [
            {"certificate_kind": "node", "identity": "rb-a-1", "serial": serial,
             "not_after": not_after, "rotation_state": rotation_state}]}}

    def test_node_certificate_within_five_minutes_of_expiry_fails(self):
        code, verdict = self.evaluate(self.snapshot(**{"diagnostics__rb-a-1_json": self.diagnostics(NOW + 300)}))
        self.assertIn("node-cert", self.failures(verdict))

    def test_node_certificate_with_six_minutes_left_passes(self):
        code, verdict = self.evaluate(self.snapshot(**{"diagnostics__rb-a-1_json": self.diagnostics(NOW + 360)}))
        self.assertEqual(self.failures(verdict), [])

    def test_workload_certificate_needs_ten_minutes(self):
        text = "serial=1F\nnotAfter=" + time.strftime("%b %d %H:%M:%S %Y GMT", time.gmtime(NOW + 540)) + "\n"
        code, verdict = self.evaluate(self.snapshot(**{"identity__rb-a-2_txt": text}))
        self.assertIn("workload-cert", self.failures(verdict))

    def test_retrying_fails_only_after_ten_minutes(self):
        _, first = self.evaluate(self.snapshot(ts=NOW, **{"diagnostics__rb-a-1_json": self.diagnostics(NOW + 9999, "retrying")}))
        _, later = self.evaluate(self.snapshot(ts=NOW + 601, **{"diagnostics__rb-a-1_json": self.diagnostics(NOW + 9999, "retrying")}))
        self.assertEqual(self.failures(first), [])
        self.assertIn("cert-rotation", self.failures(later))

    def test_stopped_rotation_fails_at_once(self):
        _, verdict = self.evaluate(self.snapshot(**{"diagnostics__rb-a-1_json": self.diagnostics(NOW + 9999, "stopped")}))
        self.assertIn("cert-rotation", self.failures(verdict))

    def test_serial_changes_count_as_renewals(self):
        self.evaluate(self.snapshot(**{"diagnostics__rb-a-1_json": self.diagnostics(NOW + 9999, serial="0a")}))
        self.evaluate(self.snapshot(**{"diagnostics__rb-a-1_json": self.diagnostics(NOW + 9999, serial="0b")}))
        state = checker.load_state(self.evidence)
        self.assertEqual(state["renewals"]["node-leaf"]["rb-a-1"], 1)

    def test_ingress_serving_an_old_serial_after_a_reload_fails(self):
        checker.main(["ingress-expect", str(self.evidence), "rb-a-1", "10A0"])
        text = "serial=109F\nnotAfter=" + time.strftime("%b %d %H:%M:%S %Y GMT", time.gmtime(NOW + 2000)) + "\n"
        _, verdict = self.evaluate(self.snapshot(**{"ingress__rb-a-1_txt": text}))
        self.assertIn("ingress-serial", self.failures(verdict))

    def test_diagnostics_and_openssl_serials_compare_as_numbers(self):
        self.assertEqual(checker.normalise_serial("10:a0"), checker.normalise_serial("10A0"))


class Leaks(Evidence):
    def setUp(self):
        super().setUp()
        baseline = self.snapshot(kind="baseline", **{
            "status_json": [{"node": "rb-a-1", "id": "default__web-0"}],
            "inventory__rb-a-1_txt": INVENTORY, "inventory__rb-a-2_txt": INVENTORY.replace("web", "api"),
            "inventory__rb-a-3_txt": "boot b3\n"})
        (baseline / "status.json").write_text(json.dumps([
            {"node": "rb-a-1", "id": "default__web-0"}, {"node": "rb-a-2", "id": "default__api-0"}]))
        checker.record_baseline(self.evidence, baseline)

    def test_the_baseline_itself_has_no_leaks(self):
        _, verdict = self.evaluate(self.snapshot(**{
            "status_json": [{"node": "rb-a-1", "id": "default__web-0"}], "inventory__rb-a-1_txt": INVENTORY}))
        self.assertEqual(self.failures(verdict), [])

    def test_a_container_without_an_instance_is_a_leak(self):
        _, verdict = self.evaluate(self.snapshot(**{
            "status_json": [{"node": "rb-a-1", "id": "default__web-0"}],
            "inventory__rb-a-1_txt": INVENTORY + "runc default__old-0\nnetns rb-default__old-0\n"}))
        self.assertIn("leak-runc", self.failures(verdict))
        self.assertIn("leak-netns", self.failures(verdict))

    def test_extra_veths_beyond_new_instances_are_leaks(self):
        _, verdict = self.evaluate(self.snapshot(**{
            "status_json": [{"node": "rb-a-1", "id": "default__web-0"}],
            "inventory__rb-a-1_txt": INVENTORY + "veth veth-2\n"}))
        self.assertIn("leak-veth", self.failures(verdict))

    def test_a_new_instance_may_bring_its_own_veth(self):
        _, verdict = self.evaluate(self.snapshot(**{
            "status_json": [{"node": "rb-a-1", "id": "default__web-0"}, {"node": "rb-a-1", "id": "default__web-1"}],
            "inventory__rb-a-1_txt": INVENTORY + "veth veth-2\nrunc default__web-1\n"}))
        self.assertEqual(self.failures(verdict), [])

    def test_new_listeners_and_bpf_pins_are_leaks(self):
        _, verdict = self.evaluate(self.snapshot(**{
            "status_json": [{"node": "rb-a-1", "id": "default__web-0"}],
            "inventory__rb-a-1_txt": INVENTORY + "listen 0.0.0.0:31337\nbpf reliaburger-y/old_map\n"}))
        self.assertIn("leak-listen", self.failures(verdict))
        self.assertIn("leak-bpf", self.failures(verdict))

    def test_leaks_during_a_fault_window_are_progress_not_failures(self):
        checker.main(["window", str(self.evidence), "open", "power-off", "--down", "1"])
        _, verdict = self.evaluate(self.snapshot(**{
            "status_json": [{"node": "rb-a-1", "id": "default__web-0"}],
            "inventory__rb-a-1_txt": INVENTORY + "runc default__old-0\n"}))
        self.assertEqual(self.failures(verdict), [])

    def test_unexplained_systemd_restart_fails_but_a_harness_kill_does_not(self):
        status = [{"node": "rb-a-1", "id": "default__web-0"}]
        self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY}))
        checker.main(["expect", str(self.evidence), "restart", "rb-a-1"])
        _, explained = self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY.replace("nrestarts 0", "nrestarts 1")}))
        _, unexplained = self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY.replace("nrestarts 0", "nrestarts 2")}))
        self.assertEqual(self.failures(explained), [])
        self.assertIn("bun-restart", self.failures(unexplained))

    def test_file_descriptors_growing_every_hour_for_six_hours_fail(self):
        samples = [[NOW - 6 * 3600 + hour * 3600 + minute * 300, 300 + hour * 10] for hour in range(6) for minute in range(12)]
        self.assertEqual([item["check"] for item in checker.fd_findings(samples, NOW)], ["leak-fd"])
        samples[-5][1] = 250
        self.assertEqual(checker.fd_findings(samples, NOW), [])

    def test_rss_over_a_quarter_above_the_warm_sample_fails(self):
        node = {}
        self.assertEqual(checker.rss_findings(node, 100, NOW, NOW - 100), [])
        self.assertEqual(checker.rss_findings(node, 100, NOW, NOW - 3600), [])
        self.assertEqual(checker.rss_findings(node, 125, NOW, NOW - 7200), [])
        self.assertEqual([item["check"] for item in checker.rss_findings(node, 126, NOW, NOW - 7200)], ["leak-rss"])


class Exports(Evidence):
    def test_a_file_pruned_after_export_is_fine_and_one_lost_fails(self):
        self.evaluate(self.snapshot(**{"export__rb-a-1_txt": "src logs logs_000001.parquet aa11\nsrc logs logs_000002.parquet bb22\n"}))
        _, verdict = self.evaluate(self.snapshot(**{"export__rb-a-1_txt": "dest logs rb-a-1/aa11-logs_000001.parquet aa11\n"}))
        self.assertEqual(self.failures(verdict), ["export-lost"])
        self.assertIn("logs_000002", verdict["findings"][0]["detail"])

    def test_a_destination_file_whose_bytes_do_not_match_its_name_is_corrupt(self):
        _, verdict = self.evaluate(self.snapshot(**{"export__rb-a-1_txt": "dest metrics rb-a-1/aa11-metrics_000001.parquet ff00\n"}))
        self.assertEqual(self.failures(verdict), ["export-corrupt"])

    def test_retained_snapshot_archives_must_not_disappear(self):
        two = "snap default/soak-writer/data/s1.tar.gz ok\nsnap default/soak-writer/data/s2.tar.gz ok\n"
        self.evaluate(self.snapshot(**{"export__rb-a-1_txt": two}))
        _, verdict = self.evaluate(self.snapshot(**{"export__rb-a-1_txt": "snap default/soak-writer/data/s2.tar.gz ok\n"}))
        self.assertEqual(self.failures(verdict), ["snapshot-missing"])


class Registry(Evidence):
    """Pickle refuses tag reads without a leader; that's not the same as losing an image."""

    MANIFEST = b'{"schemaVersion": 2}'
    BLOB = b"layer"

    def push_state(self):
        state = checker.load_state(self.evidence)
        state["registry_pushed"] = [{"tag": "baseline",
                                     "manifest": "sha256:" + checker.hashlib.sha256(self.MANIFEST).hexdigest(),
                                     "blobs": ["sha256:" + checker.hashlib.sha256(self.BLOB).hexdigest()]}]
        checker.save_state(self.evidence, state)

    def verify(self, manifest_answers):
        """Run `registry verify` against canned answers for the manifest GETs."""
        self.push_state()
        answers = list(manifest_answers)
        calls = []

        def request(args, method, path, body=None, headers=None):
            calls.append(path)
            if "/manifests/" in path:
                status, body = answers.pop(0)
                return status, {}, body
            return 200, {}, self.BLOB

        args = type("Args", (), {"evidence": self.evidence, "action": "verify"})()
        original, sleep = checker.registry_request, checker.time.sleep
        checker.registry_request, checker.time.sleep = request, lambda _: None
        try:
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                checker.registry_command(args)
        finally:
            checker.registry_request, checker.time.sleep = original, sleep
        return json.loads(out.getvalue()), calls

    def test_a_brief_503_is_retried_and_not_reported(self):
        result, calls = self.verify([(503, b""), (200, self.MANIFEST)])
        self.assertEqual((result["problems"], result["unavailable"]), ([], []))
        self.assertEqual(sum("/manifests/" in path for path in calls), 2)

    def test_a_lasting_503_is_unavailable_not_a_problem(self):
        result, calls = self.verify([(503, b"")] * checker.REGISTRY_UNAVAILABLE_TRIES)
        self.assertEqual(result["problems"], [])
        self.assertEqual(len(result["unavailable"]), 1)
        self.assertEqual(sum("/manifests/" in path for path in calls), checker.REGISTRY_UNAVAILABLE_TRIES)

    def test_changed_or_missing_bytes_are_problems_at_once(self):
        for answer, words in (((200, b"other"), "changed"), ((404, b""), "returned 404")):
            result, calls = self.verify([answer])
            self.assertEqual(len(result["problems"]), 1)
            self.assertIn(words, result["problems"][0])
            self.assertEqual(sum("/manifests/" in path for path in calls), 1)

    def test_unavailable_registry_is_info_inside_a_fault_window(self):
        state = checker.load_state(self.evidence)
        state["window"] = "quorum-loss"
        checker.save_state(self.evidence, state)
        _, verdict = self.evaluate(self.snapshot(registry_json={"problems": [], "unavailable": ["x returned 503"]}))
        self.assertNotIn("registry", self.failures(verdict))

    def test_unavailable_registry_fails_outside_a_fault_window(self):
        _, verdict = self.evaluate(self.snapshot(registry_json={"problems": [], "unavailable": ["x returned 503"]}))
        self.assertIn("registry", self.failures(verdict))

    def test_changed_image_fails_even_inside_a_fault_window(self):
        state = checker.load_state(self.evidence)
        state["window"] = "quorum-loss"
        checker.save_state(self.evidence, state)
        _, verdict = self.evaluate(self.snapshot(registry_json={"problems": ["x changed"], "unavailable": []}))
        self.assertIn("registry", self.failures(verdict))


class Recovery(Evidence):
    def test_one_leader_and_three_voters_settle(self):
        code, verdict = self.evaluate(self.snapshot(kind="settle", **{
            "wtf_json": wtf(), "nodes_json": nodes(), "http_txt": "200 0.01\n", "status_json": []}))
        self.assertEqual(code, 0)
        self.assertTrue(verdict["settle_clean"])

    def test_missing_evidence_is_not_settled(self):
        code, verdict = self.evaluate(self.snapshot(kind="settle", **{"http_txt": "200 0.01\n", "status_json": []}))
        self.assertEqual(code, 3)
        code, verdict = self.evaluate(self.snapshot(kind="settle", **{"wtf_json": wtf(), "nodes_json": nodes()}))
        self.assertEqual(code, 3)

    def test_a_light_check_says_nothing_about_settling(self):
        code, verdict = self.evaluate(self.snapshot(kind="light", **{"http_txt": "200 0.01\n"}))
        self.assertEqual((code, verdict["settle_clean"]), (0, None))

    def test_missing_replicas_are_not_settled(self):
        code, verdict = self.evaluate(self.snapshot(**{"wtf_json": wtf(ok=()), "nodes_json": nodes()}))
        self.assertEqual(code, 3)
        self.assertFalse(verdict["settle_clean"])

    def test_a_dead_node_outside_a_fault_window_fails(self):
        code, verdict = self.evaluate(self.snapshot(**{"wtf_json": wtf(), "nodes_json": nodes(down=("rb-a-3",))}))
        self.assertIn("node-alive", self.failures(verdict))

    def test_a_dead_node_inside_a_fault_window_is_not_a_failure(self):
        checker.main(["window", str(self.evidence), "open", "power-off", "--down", "1"])
        code, verdict = self.evaluate(self.snapshot(**{"wtf_json": wtf(), "nodes_json": nodes(down=("rb-a-3",))}))
        self.assertEqual(self.failures(verdict), [])
        self.assertFalse(verdict["settle_clean"])

    def test_no_leader_for_over_thirty_seconds_with_one_node_down_fails(self):
        self.evaluate(self.snapshot(ts=NOW, **{"nodes_json": nodes(leader=None)}))
        _, verdict = self.evaluate(self.snapshot(ts=NOW + 31, **{"nodes_json": nodes(leader=None)}))
        self.assertIn("no-leader", self.failures(verdict))

    def test_wtf_critical_fails_after_five_minutes_and_warning_after_ten(self):
        report = wtf(critical=[("no-backends", "app.x")], warnings=[("under-replicated", "app.y")])
        _, first = self.evaluate(self.snapshot(ts=NOW, **{"wtf_json": report}))
        _, five = self.evaluate(self.snapshot(ts=NOW + 301, **{"wtf_json": report}))
        _, ten = self.evaluate(self.snapshot(ts=NOW + 601, **{"wtf_json": report}))
        self.assertEqual(self.failures(first), [])
        self.assertEqual(self.failures(five), ["wtf-critical"])
        self.assertEqual(self.failures(ten), ["wtf-warning"])

    def test_a_cleared_wtf_finding_starts_counting_again(self):
        report = wtf(warnings=[("under-replicated", "app.y")])
        self.evaluate(self.snapshot(ts=NOW, **{"wtf_json": report}))
        self.evaluate(self.snapshot(ts=NOW + 500, **{"wtf_json": wtf()}))
        _, verdict = self.evaluate(self.snapshot(ts=NOW + 700, **{"wtf_json": report}))
        self.assertEqual(self.failures(verdict), [])

    def test_a_repeated_failure_bundles_once_per_half_hour(self):
        _, first = self.evaluate(self.snapshot(ts=NOW, kind="light", **{"http_txt": "502 0.10\n"}))
        code, again = self.evaluate(self.snapshot(ts=NOW + 30, kind="light", **{"http_txt": "502 0.10\n"}))
        _, later = self.evaluate(self.snapshot(ts=NOW + 1800, kind="light", **{"http_txt": "502 0.10\n"}))
        self.assertEqual(self.failures(first), ["ingress-http"])
        self.assertEqual((code, self.failures(again)), (0, []))
        self.assertEqual([item["severity"] for item in again["findings"]], ["repeat"])
        self.assertEqual(self.failures(later), ["ingress-http"])

    def test_a_harness_failure_key_is_new_once_then_a_repeat(self):
        self.assertEqual(checker.main(["seen", str(self.evidence), "ingress-reload|rb-a-3"]), 1)
        self.assertEqual(checker.main(["seen", str(self.evidence), "ingress-reload|rb-a-3"]), 0)
        self.assertEqual(checker.main(["seen", str(self.evidence), "ingress-reload|rb-a-2"]), 1)

    def test_ingress_errors_outside_a_fault_window_fail(self):
        _, verdict = self.evaluate(self.snapshot(kind="light", **{"http_txt": "502 0.10\n"}))
        self.assertEqual(self.failures(verdict), ["ingress-http"])


class TomlEditing(unittest.TestCase):
    NODE = """[node]
name = "rb-a-2"

[node.labels]

[logs]
retention_days = 7
export_interval_secs = 3600

[testing]
safety_class = "development"
allowed_operations = [
    "inject_workload_faults",
    "alter_node_state",
]
max_lease_seconds = 3600
"""

    def test_sets_replaces_and_appends_keys(self):
        text = checker.toml_set(self.NODE, [
            ("node.labels.soak-volume", '"writer"'),
            ("logs.export_interval_secs", "60"),
            ("logs.export_path", '"file:///x"'),
            ("testing.allowed_operations", '["a", "b"]'),
            ("security.leaf_lifetime_override_secs", "3600"),
        ])
        self.assertIn('[node.labels]\nsoak-volume = "writer"\n', text)
        self.assertIn('export_interval_secs = 60\nexport_path = "file:///x"\n', text)
        self.assertIn('allowed_operations = ["a", "b"]\nmax_lease_seconds = 3600\n', text)
        self.assertTrue(text.endswith('[security]\nleaf_lifetime_override_secs = 3600\n'))
        self.assertNotIn('"alter_node_state",', text)

    def test_setting_twice_is_idempotent(self):
        once = checker.toml_set(self.NODE, [("logs.max_storage_mb", "8")])
        self.assertEqual(checker.toml_set(once, [("logs.max_storage_mb", "8")]), once)


class Record(Evidence):
    def write_run(self, failures=()):
        (self.evidence / "metadata.json").write_text(json.dumps({
            "schedule": "compressed", "started_at": NOW, "finished_at": NOW + 2400,
            "base_url": "<https://example.invalid/staging>", "candidate_digest": "`abc`",
            "soak_bun": "not supplied: upgrade slots skipped", "deviations": ["provision_isolated_workloads"],
            "cycles": 4, "ingress_rotation_secs": 300,
            "teardown": "`relish local destroy --yes` and `relish uninstall --yes` succeeded"}))
        for offset, seconds in ((0, 40), (100, 60)):
            checker.append_event(self.evidence, ts=NOW + offset, phase="fault:bun-kill-follower", target="rb-a-2")
            checker.append_event(self.evidence, ts=NOW + offset + seconds, phase="fault:bun-kill-follower",
                                 verdict="settled", settle_seconds=seconds)
        checker.append_event(self.evidence, ts=NOW + 30, phase="upgrade", verdict="skipped")
        state = checker.load_state(self.evidence)
        state.update({"writer-ack": 20000, "renewals": {"workload-identity": {name: 2 for name in NODES}},
                      "counters": {"ingress-reload": 21}})
        checker.save_state(self.evidence, state)
        for number, detail in enumerate(failures, 1):
            directory = self.evidence / "failures" / str(number)
            directory.mkdir(parents=True)
            (directory / "summary.json").write_text(json.dumps({"ts": NOW + 100, "check": "settle", "detail": detail}))

    def test_a_clean_run_renders_pass_with_counts(self):
        self.write_run()
        record = self.evidence / "record.md"
        self.assertEqual(checker.render(self.evidence, record), 0)
        text = record.read_text()
        self.assertTrue(text.startswith("# Sustained soak (V02): PASS"))
        self.assertIn("| fault:bun-kill-follower | 2 | 2 | 0 | 0 | 60 s / 60 s |", text)
        self.assertIn("| upgrade | 1 | 0 | 0 | 1 | n/a |", text)
        self.assertIn("highest ACK 20000", text)
        self.assertIn("the writer file checks (writer-gap, writer-regression) decide data loss", text)
        self.assertIn("not supplied: upgrade slots skipped", text)
        self.assertIn("short leaf lifetimes unavailable", text)
        self.assertIn("- Teardown: `relish local destroy --yes` and `relish uninstall --yes` succeeded", text)

    def test_an_open_failure_renders_fail_with_its_row(self):
        self.write_run(failures=["fault:power-off did not settle within 480 s"])
        record = self.evidence / "record.md"
        self.assertEqual(checker.render(self.evidence, record), 1)
        text = record.read_text()
        self.assertTrue(text.startswith("# Sustained soak (V02): FAIL"))
        self.assertIn("| 1 | ", text)
        self.assertIn("did not settle within 480 s", text)

    def test_too_few_rotations_fail_the_gate(self):
        self.write_run()
        metadata = json.loads((self.evidence / "metadata.json").read_text())
        metadata.update(finished_at=NOW + 3 * 3600, ingress_rotation_secs=None)
        (self.evidence / "metadata.json").write_text(json.dumps(metadata))
        state = checker.load_state(self.evidence)
        state["renewals"]["workload-identity"]["rb-a-3"] = 1
        checker.save_state(self.evidence, state)
        record = self.evidence / "record.md"
        self.assertEqual(checker.render(self.evidence, record), 1)
        self.assertIn("| workload identity rotations (soak-identity, per node) | 1 | ≥ 5 | FAIL |", record.read_text())

    def render_tier(self, tier, failures=(), **metadata):
        self.write_run(failures=failures)
        run = json.loads((self.evidence / "metadata.json").read_text())
        if tier == "final":
            run.update(schedule="full", finished_at=NOW + 8 * 3600 + 120, duration_target=8 * 3600,
                       ingress_rotation_secs=None)
            state = checker.load_state(self.evidence)
            state["renewals"]["workload-identity"] = {name: 16 for name in NODES}
            checker.save_state(self.evidence, state)
        else:
            run.update(duration_target=90 * 60)
        run.update(tier=tier, **metadata)
        (self.evidence / "metadata.json").write_text(json.dumps(run))
        record = self.evidence / "record.md"
        code = checker.render(self.evidence, record)
        return code, record.read_text()

    def test_a_clean_fast_tier_says_clean_but_never_claims_acceptance(self):
        code, text = self.render_tier("fast")
        self.assertEqual(code, 0)
        self.assertIn("fast tier, compressed schedule", text)
        self.assertIn("**Fast tier: clean.**", text)
        self.assertIn("not acceptance", text)
        self.assertNotIn("gate passes", text)

    def test_a_clean_final_tier_passes_the_gate(self):
        code, text = self.render_tier("final")
        self.assertEqual(code, 0)
        self.assertIn("final tier, full schedule, 8 h 02 min", text)
        self.assertIn("**Final tier: clean. The V02 soak gate passes** for the candidate whose `candidate.json` "
                      "has SHA-256 `abc`", text)

    def test_a_final_tier_with_an_open_failure_does_not_pass_the_gate(self):
        code, text = self.render_tier("final", failures=["bun RSS 31% above its warm sample"])
        self.assertEqual(code, 1)
        self.assertIn("**Final tier: not clean. The V02 gate does not pass.**", text)
        self.assertNotIn("gate passes", text)

    def test_a_short_or_unpinned_final_tier_is_not_acceptance(self):
        code, text = self.render_tier("final", finished_at=NOW + 4 * 3600, duration_target=4 * 3600,
                                      candidate_digest="`not checked`")
        self.assertEqual(code, 0)
        self.assertIn("**Final tier: clean, but not acceptance:** it soaked for 4 h 00 min; "
                      "the candidate digest was not checked", text)
        self.assertNotIn("gate passes", text)

    def test_a_run_without_a_tier_says_nothing_about_the_gate(self):
        self.write_run()
        record = self.evidence / "record.md"
        self.assertEqual(checker.render(self.evidence, record), 0)
        self.assertIn("**No tier (a custom run).**", record.read_text())

    def test_render_refuses_to_overwrite(self):
        self.write_run()
        record = self.evidence / "record.md"
        record.write_text("earlier")
        with self.assertRaises(SystemExit):
            checker.render(self.evidence, record)


if __name__ == "__main__":
    unittest.main()
