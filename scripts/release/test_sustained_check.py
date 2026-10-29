"""The V02 soak checker judges evidence the way section 4 of the plan says."""
import contextlib
import hashlib
import io
import json
import os
import re
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
        code, verdict = self.evaluate(self.snapshot(**{"writer__file_txt": "MAX 11\nLAST 11\n"}))
        self.assertEqual(code, 1)
        self.assertIn("writer-regression", self.failures(verdict))

    def test_writer_file_with_a_missing_value_is_lost_data(self):
        code, verdict = self.evaluate(self.snapshot(**{"writer__file_txt": "BAD 7: 8\nMISSING 1 first 7\nMAX 9\nLAST 8\n"}))
        self.assertEqual(code, 1)
        self.assertEqual(self.failures(verdict), ["writer-lost"])

    def test_writer_file_with_a_repeated_value_is_two_writers_not_lost_data(self):
        # V02, 28 Sep 2026: line 21927 held 21926 again. The old check read
        # that as a file ending at 21927 below ACK 23539; it held every value.
        self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 23538\nACK 23539\n"}))
        code, verdict = self.evaluate(self.snapshot(**{
            "writer__file_txt": "BAD 21927: 21926\nDUP 1 first 21926\nMAX 23540\nLAST 23541\n"}))
        self.assertEqual(code, 1)
        self.assertEqual(self.failures(verdict), ["writer-duplicate"])
        detail = next(item["detail"] for item in verdict["findings"] if item["check"] == "writer-duplicate")
        self.assertIn("two writers", detail)

    def test_writer_file_with_an_out_of_place_line_alone_is_a_gap(self):
        code, verdict = self.evaluate(self.snapshot(**{"writer__file_txt": "BAD 3: x\nMAX 5\nLAST 5\n"}))
        self.assertEqual(code, 1)
        self.assertEqual(self.failures(verdict), ["writer-gap"])

    def test_writer_file_without_a_summary_warns(self):
        code, verdict = self.evaluate(self.snapshot(**{"writer__file_txt": "LAST 5\n"}))
        self.assertEqual(self.failures(verdict), [])
        self.assertIn("writer-file", [item["check"] for item in verdict["findings"]])

    def test_writer_file_at_or_beyond_the_highest_ack_passes(self):
        self.evaluate(self.snapshot(**{"writer__log_txt": "RESUME 0 after 0 lines\nACK 1\nACK 2\n"}))
        code, verdict = self.evaluate(self.snapshot(**{"writer__file_txt": "MAX 5\nLAST 5\n"}))
        self.assertEqual(self.failures(verdict), [])

    def test_redis_counter_going_backwards_within_one_tail_fails(self):
        code, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 41\nINCR 42\nINCR 1\nINCR 2\n"}))
        self.assertEqual(code, 1)
        self.assertIn("redis-log-order", self.failures(verdict))

    def test_redis_counter_below_an_earlier_tail_fails(self):
        self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 41\nINCR 42\n"}))
        code, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "ERR Could not connect\nINCR 3\nINCR 4\n"}))
        self.assertIn("redis-log-order", self.failures(verdict))

    def test_one_instance_on_two_nodes_is_ordered_per_node(self):
        # V02 final tier, 28 Sep 2026 (ff854cb): the rollback walk moved
        # soak-redis-client-0 from rb-2 to rb-3 under the same name. The tail
        # interleaves the two runs by two nodes' clocks, so 11632 came before
        # 11631. Each run rose; the order across runs isn't the client's.
        self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 11629\n"}))
        code, verdict = self.evaluate(self.snapshot(**{"redis__log_txt":
            "[soak-redis-client-0@rb-a-2] INCR 11630\n"
            "[soak-redis-client-0@rb-a-3] INCR 11632\n"
            "[soak-redis-client-0@rb-a-2] INCR 11631\n"
            "[soak-redis-client-0@rb-a-3] INCR 11633\n"}))
        self.assertEqual(self.failures(verdict), [])
        # The next tail is judged against the newest line of either run.
        _, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 11632\n"}))
        self.assertIn("redis-log-order", self.failures(verdict))

    def test_lines_going_backwards_within_one_labelled_run_still_fail(self):
        _, verdict = self.evaluate(self.snapshot(**{"redis__log_txt":
            "[soak-redis-client-0@rb-a-2] INCR 11631\n"
            "[soak-redis-client-0@rb-a-3] INCR 11633\n"
            "[soak-redis-client-0@rb-a-3] INCR 11632\n"}))
        self.assertIn("redis-log-order", self.failures(verdict))
        detail = next(item["detail"] for item in verdict["findings"] if item["check"] == "redis-log-order")
        self.assertIn("soak-redis-client-0@rb-a-3", detail)

    def test_an_unlabelled_tail_is_still_one_run(self):
        # Without labels the view shows one instance, so any step back fails.
        _, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 11632\nINCR 11631\n"}))
        self.assertIn("redis-log-order", self.failures(verdict))

    def test_writer_log_going_backwards_is_a_log_order_failure_not_data_loss(self):
        self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 10\nACK 11\n"}))
        code, verdict = self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 12\nACK 9\nACK 13\n",
                                                        "writer__file_txt": "MAX 13\nLAST 13\n"}))
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
        code, verdict = self.evaluate(self.snapshot(**{"writer__file_txt": "MAX 40314\nLAST 40314\n"}))
        self.assertEqual(code, 1)
        self.assertIn("writer-regression", self.failures(verdict))

    def test_lines_going_backwards_within_one_tail_still_fail_after_a_power_cut(self):
        self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 41\n"}))
        self.power_cut()
        _, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 50\nINCR 45\n"}))
        self.assertIn("redis-log-order", self.failures(verdict))

    def open_window(self, label="bun-kill-leader"):
        with contextlib.redirect_stdout(io.StringIO()):
            checker.main(["window", str(self.evidence), "open", label])

    def close_window(self):
        with contextlib.redirect_stdout(io.StringIO()):
            checker.main(["window", str(self.evidence), "settled"])

    def test_a_dip_inside_a_fault_window_is_info_and_judged_after_it(self):
        self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 10945\nACK 10946\n"}))
        self.open_window()
        _, verdict = self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 10495\nACK 10496\n"}))
        self.assertEqual(self.failures(verdict), [])
        dip = next(item for item in verdict["findings"] if item["check"] == "writer-log-order")
        self.assertEqual(dip["severity"], "info")
        self.close_window()
        # Caught up: fine. Still below the old baseline: a failure.
        _, verdict = self.evaluate(self.snapshot(**{"writer__log_txt": "ACK 10950\n"}))
        self.assertEqual(self.failures(verdict), [])

    def test_a_dip_that_outlasts_its_fault_window_fails(self):
        self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 2282\n"}))
        self.open_window()
        self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 2133\n"}))
        self.close_window()
        _, verdict = self.evaluate(self.snapshot(**{"redis__log_txt": "INCR 2140\n"}))
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
        checker.main(["expect", str(self.evidence), "restart", "rb-a-1", "--at", str(NOW)])
        _, explained = self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY.replace("nrestarts 0", "nrestarts 1")}))
        _, unexplained = self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY.replace("nrestarts 0", "nrestarts 2")}))
        self.assertEqual(self.failures(explained), [])
        self.assertIn("bun-restart", self.failures(unexplained))

    def test_every_rise_in_unexplained_restarts_is_its_own_failure(self):
        # #985: Bun restarted three times on node 1 without a harness kill
        # in twenty minutes, and only the first became a failure; the other
        # two were repeats inside the half-hour bundle window.
        status = [{"node": "rb-a-1", "id": "default__web-0"}]
        self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY}))
        rises = []
        for minute, count in ((5, 1), (12, 2), (20, 3)):
            _, verdict = self.evaluate(self.snapshot(ts=NOW + 60 * minute, **{
                "status_json": status, "inventory__rb-a-1_txt": INVENTORY.replace("nrestarts 0", f"nrestarts {count}")}))
            rises.append([item for item in verdict["findings"] if item["check"] == "bun-restart"])
        self.assertEqual([[item["severity"] for item in found] for found in rises], [["fail"]] * 3)
        self.assertIn("NRestarts now 3", rises[2][0]["detail"])
        # A count that stays put is not a new restart.
        _, flat = self.evaluate(self.snapshot(ts=NOW + 60 * 25, **{
            "status_json": status, "inventory__rb-a-1_txt": INVENTORY.replace("nrestarts 0", "nrestarts 3")}))
        self.assertNotIn("bun-restart", [item["check"] for item in flat["findings"]])

    def test_a_check_between_the_kill_and_the_restart_keeps_the_expectation(self):
        status = [{"node": "rb-a-1", "id": "default__web-0"}]
        self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY}))
        checker.main(["expect", str(self.evidence), "restart", "rb-a-1", "--at", str(NOW)])
        # systemd hasn't restarted bun yet when this check runs.
        _, before = self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY}))
        _, after = self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY.replace("nrestarts 0", "nrestarts 1")}))
        self.assertEqual(self.failures(before), [])
        self.assertEqual(self.failures(after), [])

    def test_an_expectation_survives_a_check_saving_older_state(self):
        status = [{"node": "rb-a-1", "id": "default__web-0"}]
        self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY}))
        stale = checker.load_state(self.evidence)
        checker.main(["expect", str(self.evidence), "restart", "rb-a-1", "--at", str(NOW)])
        checker.save_state(self.evidence, stale)  # a concurrent check writing back what it loaded
        _, verdict = self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY.replace("nrestarts 0", "nrestarts 1")}))
        self.assertEqual(self.failures(verdict), [])

    def test_an_old_expectation_does_not_excuse_a_later_restart(self):
        status = [{"node": "rb-a-1", "id": "default__web-0"}]
        self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY}))
        checker.main(["expect", str(self.evidence), "restart", "rb-a-1", "--at", str(NOW)])
        # A check well after the kill saw no restart: the kill didn't cause one.
        later = NOW + checker.RESTART_GRACE
        _, quiet = self.evaluate(self.snapshot(ts=later, **{"status_json": status, "inventory__rb-a-1_txt": INVENTORY}))
        _, verdict = self.evaluate(self.snapshot(ts=later + 300, **{"status_json": status, "inventory__rb-a-1_txt": INVENTORY.replace("nrestarts 0", "nrestarts 1")}))
        self.assertEqual(self.failures(quiet), [])
        self.assertIn("bun-restart", self.failures(verdict))

    def test_a_kill_first_checked_long_after_still_explains_its_restart(self):
        # 0.1.1 candidate 2's fast tier: the upgrade slot killed the leader
        # mid-walk, then blocked for 718 s (a walk and a 600 s rollback
        # wait) without a check. The restart was the harness's own, however
        # long it took for a check to look.
        status = [{"node": "rb-a-1", "id": "default__web-0"}]
        self.evaluate(self.snapshot(**{"status_json": status, "inventory__rb-a-1_txt": INVENTORY}))
        killed = NOW + 100
        checker.main(["expect", str(self.evidence), "restart", "rb-a-1", "--at", str(killed)])
        _, verdict = self.evaluate(self.snapshot(ts=killed + 718, **{"status_json": status, "inventory__rb-a-1_txt": INVENTORY.replace("nrestarts 0", "nrestarts 1")}))
        self.assertEqual(self.failures(verdict), [])

    def test_file_descriptors_rising_every_ten_minutes_for_half_an_hour_fail(self):
        # e8c9653's leader: the floor went 206 -> 254 -> 399 -> 531 while
        # sockets piled up. Bun images live an hour or two between kills
        # and upgrade walks, so a six-hour trend never got to run.
        started = NOW - 3000
        floors = [206, 254, 399, 531]
        samples = [[started + 600 + bucket * 600 + minute * 60, floor + minute * 5]
                   for bucket, floor in enumerate(floors) for minute in range(0, 10, 3)]
        now = started + 3000
        self.assertEqual([item["check"] for item in checker.fd_findings(samples, started, now)], ["leak-fd"])
        # Not a complete window yet: the warm-up plus four buckets.
        self.assertEqual(checker.fd_findings(samples, started, now - 60), [])

    def test_bounded_file_descriptors_pass(self):
        started = NOW - 3000
        def samples(floors):
            return [[started + 600 + bucket * 600 + minute * 60, floor + (100 if minute == 3 else 0)]
                    for bucket, floor in enumerate(floors) for minute in range(0, 10, 3)]
        now = started + 3000
        # A settle burst doubles the count for a sample; the floor stays put.
        self.assertEqual(checker.fd_findings(samples([55, 56, 55, 57]), started, now), [])
        # Small steady rises (candidate e8c9653's node 3: 59 -> 72) are not a leak.
        self.assertEqual(checker.fd_findings(samples([59, 63, 68, 72]), started, now), [])
        # One step up (a peer's restart) and then flat is not a leak either.
        self.assertEqual(checker.fd_findings(samples([60, 120, 121, 121]), started, now), [])
        # A dip anywhere breaks the run.
        self.assertEqual(checker.fd_findings(samples([200, 300, 290, 500]), started, now), [])

    def test_rss_over_a_quarter_above_the_warm_sample_fails(self):
        node = {}
        self.assertEqual(checker.rss_findings(node, 100, NOW, NOW - 100), [])
        self.assertEqual(checker.rss_findings(node, 100, NOW, NOW - 3600), [])
        self.assertEqual(checker.rss_findings(node, 125, NOW, NOW - 7200), [])
        self.assertEqual([item["check"] for item in checker.rss_findings(node, 126, NOW, NOW - 7200)], ["leak-rss"])

    def test_an_upgrade_exec_restarts_the_rss_warm_up(self):
        # The candidate-9 soak: an upgrade walk exec'd bun in place (same pid,
        # new image), RSS fell from 594 to 242 MB, and the warm sample was
        # taken ten minutes later, before the new image had filled its
        # working set. A fresh image gets a fresh hour.
        state = {}
        def sample(ts, image, rss_kb):
            text = INVENTORY.replace("bun_rss_kb 300000", f"bun_rss_kb {rss_kb}") + f"bun_image {image}\n"
            return checker.resource_trend_findings(state, "rb-a-2", checker.parse_inventory(text), ts)
        start = NOW - 4 * 3600
        self.assertEqual(sample(start, "a1", 217124), [])
        self.assertEqual(sample(start + 3000, "b2", 242000), [])
        self.assertEqual(sample(start + 3600 + 600, "b2", 479532), [])
        self.assertEqual(sample(start + 5700, "b2", 625572), [])
        self.assertEqual(sample(start + 3000 + 3600, "b2", 657000), [])
        self.assertEqual([item["check"] for item in sample(start + 3000 + 7000, "b2", 830000)], ["leak-rss"])

    def test_inventories_without_an_image_line_still_track_the_pid(self):
        state = {}
        def sample(ts, pid, rss_kb):
            text = INVENTORY.replace("bun_pid 100", f"bun_pid {pid}").replace("bun_rss_kb 300000", f"bun_rss_kb {rss_kb}")
            return checker.resource_trend_findings(state, "rb-a-1", checker.parse_inventory(text), ts)
        self.assertEqual(sample(NOW, 100, 100000), [])
        self.assertEqual(sample(NOW + 3600, 100, 100000), [])
        self.assertEqual([item["check"] for item in sample(NOW + 3700, 100, 130000)], ["leak-rss"])
        self.assertEqual(sample(NOW + 3800, 101, 50000), [])
        self.assertEqual(sample(NOW + 3900, 101, 130000), [])


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

    @staticmethod
    def alerts(*rules):
        return {"critical": [], "ok": [{"id": "replicas", "description": "replicas"}],
                "warnings": [{"id": "alerts-firing", "title": f"{rule}: {rule} fired", "affected_resource": "cluster"}
                             for rule in rules]}

    def test_a_guest_cpu_alert_is_recorded_not_gated(self):
        # The soak drives 2-vCPU guests at the job target rate on purpose,
        # so Mayo's cpu_throttle alert says the load is the load (#1112).
        report = self.alerts("cpu_throttle")
        self.evaluate(self.snapshot(ts=NOW, **{"wtf_json": report}))
        _, verdict = self.evaluate(self.snapshot(ts=NOW + 601, **{"wtf_json": report}))
        self.assertEqual(self.failures(verdict), [])
        self.assertIn("wtf-alert", [item["check"] for item in verdict["findings"] if item["severity"] == "info"])

    def test_a_memory_alert_still_fails_after_ten_minutes(self):
        report = self.alerts("memory_high")
        self.evaluate(self.snapshot(ts=NOW, **{"wtf_json": report}))
        _, verdict = self.evaluate(self.snapshot(ts=NOW + 601, **{"wtf_json": report}))
        self.assertEqual(self.failures(verdict), ["wtf-warning"])

    def test_each_cluster_alert_keeps_its_own_clock(self):
        # Every cluster alert used to share one key, so an alert that had
        # just started inherited the age of one that had been firing.
        self.evaluate(self.snapshot(ts=NOW, **{"wtf_json": self.alerts("cpu_throttle")}))
        _, verdict = self.evaluate(self.snapshot(ts=NOW + 601, **{"wtf_json": self.alerts("cpu_throttle", "memory_high")}))
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


class ReleaseVersion(unittest.TestCase):
    """The upgrade walk rolls back to the candidate's own version, which the
    cluster reports before the first walk. 0.1.1 candidate 2's fast tier
    rolled back to a hardcoded v0.1.0 and waited 600 s for it."""

    def release_version(self, *versions, soak="bun-v0.1.1-soak.2"):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            code = checker.main(["release-version", "--soak-bun", soak, *versions])
        return code, out.getvalue().strip()

    def test_every_node_agreeing_names_the_release(self):
        self.assertEqual(self.release_version("v0.1.1", "v0.1.1", "v0.1.1"), (0, "0.1.1"))
        self.assertEqual(self.release_version("0.2.0", "v0.2.0", "0.2.0", soak="bun-v0.2.0-soak.1"), (0, "0.2.0"))

    def test_nodes_disagreeing_or_unreachable_name_nothing(self):
        self.assertEqual(self.release_version("v0.1.1", "v0.1.1", "v0.1.0")[0], 1)
        self.assertEqual(self.release_version("v0.1.1", "?", "v0.1.1")[0], 1)
        self.assertEqual(self.release_version()[0], 1)

    def test_a_cluster_already_on_the_soak_build_is_refused(self):
        # Rolling "back" to the soak build itself would prove nothing.
        self.assertEqual(self.release_version("v0.1.1-soak.2", "v0.1.1-soak.2", "v0.1.1-soak.2")[0], 1)


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


def turns(branch="command", within=10, over=(), ts=NOW):
    """bun_agent_loop_turn_seconds_bucket samples as /v1/metrics returns them:
    `within` turns of at most 1 s, plus one for each bound in `over`."""
    bounds = ["0.25", "0.5", "1", "2.5", "5", "10", "+Inf"]
    bigger = {"2.5": 0, "5": 1, "10": 2, "+Inf": 3}
    samples = []
    for bound in bounds:
        count = within
        if bound in bigger:
            count += sum(1 for slot in over if bigger[slot] <= bigger[bound])
        labels = json.dumps({"branch": branch, "le": bound})
        samples.append({"timestamp": ts, "metric_name": "bun_agent_loop_turn_seconds_bucket",
                        "labels": labels, "value": float(count)})
    return samples


class AgentLoopTurns(Evidence):
    """The soak fails a tier on any agent-loop turn over 1 s (#351)."""

    def test_turns_within_the_budget_pass(self):
        code, verdict = self.evaluate(self.snapshot(**{"turns__rb-a-1_json": turns(within=500)}))
        self.assertNotEqual(code, 1)
        self.assertEqual(self.failures(verdict), [])

    def test_one_turn_over_a_second_fails_and_says_how_long(self):
        code, verdict = self.evaluate(self.snapshot(**{
            "inventory__rb-a-2_txt": INVENTORY,
            "turns__rb-a-2_json": turns(branch="deploy_op", over=("5",))}))
        self.assertEqual(code, 1)
        failure = next(item for item in verdict["findings"] if item["check"] == "agent-loop-turn")
        self.assertEqual(failure["target"], "rb-a-2")
        self.assertIn("1 deploy_op turn(s) over the 1 s budget, the worst between 2.5 and 5 s", failure["detail"])

    def test_only_turns_since_the_last_check_count(self):
        evidence = {"inventory__rb-a-1_txt": INVENTORY, "turns__rb-a-1_json": turns(over=("2.5",))}
        self.evaluate(self.snapshot(**evidence))
        code, verdict = self.evaluate(self.snapshot(ts=NOW + 3600, **evidence))
        self.assertNotIn("agent-loop-turn", self.failures(verdict))
        code, verdict = self.evaluate(self.snapshot(ts=NOW + 5400, **{
            "inventory__rb-a-1_txt": INVENTORY, "turns__rb-a-1_json": turns(over=("2.5",), ts=NOW + 5390)}))
        self.assertNotIn("agent-loop-turn", self.failures(verdict))
        code, verdict = self.evaluate(self.snapshot(ts=NOW + 7200, **{
            "inventory__rb-a-1_txt": INVENTORY, "turns__rb-a-1_json": turns(over=("2.5", "+Inf"), ts=NOW + 7190)}))
        self.assertEqual(code, 1)
        failure = next(item for item in verdict["findings"] if item["check"] == "agent-loop-turn")
        self.assertIn("1 command turn(s) over the 1 s budget, the worst over 10 s", failure["detail"])

    def test_a_restarted_bun_counts_its_turns_from_zero(self):
        self.evaluate(self.snapshot(**{"inventory__rb-a-1_txt": INVENTORY,
                                       "turns__rb-a-1_json": turns(over=("2.5",))}))
        code, verdict = self.evaluate(self.snapshot(ts=NOW + 3600, **{
            "inventory__rb-a-1_txt": INVENTORY.replace("bun_pid 100", "bun_pid 200"),
            "turns__rb-a-1_json": turns(within=3, over=("2.5",), ts=NOW + 3590)}))
        self.assertEqual(code, 1)
        self.assertIn("agent-loop-turn", self.failures(verdict))

    def test_a_restart_with_no_new_slow_turns_finds_nothing(self):
        # The new bun hasn't run a deploy_op turn yet, so the newest
        # deploy_op samples in the window are still the old process's, with
        # the slow turn the first check already reported (#1261).
        restarted = INVENTORY.replace("bun_pid 100", "bun_pid 200")
        self.evaluate(self.snapshot(**{"inventory__rb-a-2_txt": INVENTORY,
                                       "turns__rb-a-2_json": turns(branch="deploy_op", over=("2.5",))}))
        code, verdict = self.evaluate(self.snapshot(ts=NOW + 1800, **{
            "inventory__rb-a-2_txt": restarted,
            "turns__rb-a-2_json": turns(branch="deploy_op", over=("2.5",), ts=NOW + 1500)
            + turns(branch="command", within=3, ts=NOW + 1790)}))
        self.assertNotIn("agent-loop-turn", self.failures(verdict))
        self.assertEqual(checker.load_state(self.evidence)["turns_over_budget"], {"rb-a-2": 1})

    def test_a_slow_turn_after_a_restart_counts_once_beside_the_old_process_samples(self):
        # The window holds both processes: the old one's last samples, then
        # the new one's counters starting again from zero.
        restarted = INVENTORY.replace("bun_pid 100", "bun_pid 200")
        self.evaluate(self.snapshot(**{"inventory__rb-a-2_txt": INVENTORY,
                                       "turns__rb-a-2_json": turns(branch="deploy_op", over=("2.5",))}))
        code, verdict = self.evaluate(self.snapshot(ts=NOW + 1800, **{
            "inventory__rb-a-2_txt": restarted,
            "turns__rb-a-2_json": turns(branch="deploy_op", over=("2.5",), ts=NOW + 1500)
            + turns(branch="deploy_op", within=3, over=("5",), ts=NOW + 1790)}))
        self.assertEqual(code, 1)
        failure = next(item for item in verdict["findings"] if item["check"] == "agent-loop-turn")
        self.assertIn("1 deploy_op turn(s) over the 1 s budget, the worst between 2.5 and 5 s", failure["detail"])
        self.assertEqual(checker.load_state(self.evidence)["turns_over_budget"], {"rb-a-2": 2})
        # The next check still sees the restart in its window: counted once.
        code, verdict = self.evaluate(self.snapshot(ts=NOW + 3600, **{
            "inventory__rb-a-2_txt": restarted,
            "turns__rb-a-2_json": turns(branch="deploy_op", over=("2.5",), ts=NOW + 1500)
            + turns(branch="deploy_op", within=3, over=("5",), ts=NOW + 1790)
            + turns(branch="deploy_op", within=9, over=("5",), ts=NOW + 3590)}))
        self.assertNotIn("agent-loop-turn", self.failures(verdict))
        self.assertEqual(checker.load_state(self.evidence)["turns_over_budget"], {"rb-a-2": 2})

    def test_a_bun_restarted_under_the_same_pid_counts_from_zero(self):
        self.evaluate(self.snapshot(**{"inventory__rb-a-1_txt": INVENTORY,
                                       "turns__rb-a-1_json": turns(within=50, over=("2.5", "5"))}))
        code, verdict = self.evaluate(self.snapshot(ts=NOW + 1800, **{
            "inventory__rb-a-1_txt": INVENTORY,
            "turns__rb-a-1_json": turns(within=3, over=("2.5",), ts=NOW + 1790)}))
        self.assertEqual(code, 1)
        failure = next(item for item in verdict["findings"] if item["check"] == "agent-loop-turn")
        self.assertIn("1 command turn(s) over the 1 s budget", failure["detail"])

    def slow_fsync_window(self, node):
        with contextlib.redirect_stdout(io.StringIO()):
            checker.main(["window", str(self.evidence), "open", "node-slow-fsync", "--node", node])

    def test_slow_turns_on_a_node_whose_disk_the_soak_throttles_are_info(self):
        # The jobs tier caps one node's writes at 20 IOPS (#972): a turn that
        # journals there is slow by construction, not a regression.
        self.slow_fsync_window("rb-a-2")
        code, verdict = self.evaluate(self.snapshot(**{
            "inventory__rb-a-2_txt": INVENTORY,
            "turns__rb-a-2_json": turns(branch="follow_up", over=("10",))}))
        self.assertNotIn("agent-loop-turn", self.failures(verdict))
        note = next(item for item in verdict["findings"] if item["check"] == "agent-loop-turn")
        self.assertEqual(note["severity"], "info")
        self.assertIn("injected slow-fsync", note["detail"])
        self.assertNotIn("rb-a-2", checker.load_state(self.evidence).get("turns_over_budget", {}))

    def test_slow_turns_on_another_node_still_fail_during_a_slow_fsync_fault(self):
        self.slow_fsync_window("rb-a-1")
        code, verdict = self.evaluate(self.snapshot(**{
            "inventory__rb-a-2_txt": INVENTORY,
            "turns__rb-a-2_json": turns(branch="follow_up", over=("2.5",))}))
        self.assertEqual(code, 1)
        self.assertIn("agent-loop-turn", self.failures(verdict))

    def test_slow_turns_fail_again_once_the_slow_fsync_window_closes(self):
        self.slow_fsync_window("rb-a-2")
        with contextlib.redirect_stdout(io.StringIO()):
            checker.main(["window", str(self.evidence), "settled"])
        code, verdict = self.evaluate(self.snapshot(**{
            "inventory__rb-a-2_txt": INVENTORY,
            "turns__rb-a-2_json": turns(branch="follow_up", over=("2.5",))}))
        self.assertEqual(code, 1)
        self.assertIn("agent-loop-turn", self.failures(verdict))

    def test_the_record_counts_turns_over_the_budget_per_node(self):
        self.evaluate(self.snapshot(**{"turns__rb-a-1_json": turns(over=("2.5", "5")),
                                       "turns__rb-a-2_json": turns()}))
        state = checker.load_state(self.evidence)
        self.assertEqual(state["turns_over_budget"], {"rb-a-1": 2})
        self.assertEqual(sorted(state["turns"]), ["rb-a-1", "rb-a-2"])


SOAK_WORKLOADS = Path(__file__).resolve().parent / "sustained" / "soak-workloads.toml"


def policy_listing(policies, generation=3):
    """`relish --output json policy list` for `policies`, a {name: spec} map
    in the TOML's shape (`match`, `deny`, `action`, `default`)."""
    return {"policies": {name: dict({"action": "deny"}, **spec) for name, spec in policies.items()},
            "generation": generation, "gitops_owned": []}


class SoakPolicies(Evidence):
    """The soak workload carries policies, and every check makes sure they're
    still in force (#1311): a fault mustn't lose or change one."""

    def setUp(self):
        super().setUp()
        (self.evidence / "workloads.toml").write_text(SOAK_WORKLOADS.read_text())
        self.declared = checker.expected_policies(self.evidence)

    def test_the_soak_workload_carries_a_deny_a_warn_and_a_default(self):
        actions = {spec.get("action", "deny") for spec in self.declared.values() if "deny" in spec}
        self.assertIn("deny", actions)
        self.assertIn("warn", actions)
        self.assertTrue(any("default" in spec for spec in self.declared.values()), self.declared)

    def test_every_soak_policy_in_force_finds_nothing(self):
        code, verdict = self.evaluate(self.snapshot(policies_json=policy_listing(self.declared)))
        self.assertNotEqual(code, 1)
        self.assertNotIn("policies", self.failures(verdict))
        self.assertEqual(checker.load_state(self.evidence)["policy_checks"], 1)

    def test_a_missing_soak_policy_fails_naming_it(self):
        name = sorted(self.declared)[0]
        listing = policy_listing({key: spec for key, spec in self.declared.items() if key != name})
        code, verdict = self.evaluate(self.snapshot(policies_json=listing))
        self.assertEqual(code, 1)
        failure = next(item for item in verdict["findings"] if item["check"] == "policies")
        self.assertIn(f"policy {name} is not in force", failure["detail"])

    def test_a_changed_soak_policy_fails(self):
        name = next(key for key, spec in self.declared.items() if "deny" in spec)
        changed = dict(self.declared, **{name: dict(self.declared[name], deny="true")})
        code, verdict = self.evaluate(self.snapshot(policies_json=policy_listing(changed)))
        self.assertEqual(code, 1)
        failure = next(item for item in verdict["findings"] if item["check"] == "policies")
        self.assertIn(f"policy {name} differs from the soak workload's", failure["detail"])

    def test_the_policy_generation_never_goes_backwards(self):
        self.evaluate(self.snapshot(policies_json=policy_listing(self.declared, generation=9)))
        code, verdict = self.evaluate(self.snapshot(ts=NOW + 600, policies_json=policy_listing(self.declared, generation=6)))
        self.assertEqual(code, 1)
        failure = next(item for item in verdict["findings"] if item["check"] == "policies")
        self.assertIn("policy generation went from 9 back to 6", failure["detail"])

    def test_a_snapshot_without_a_listing_is_not_judged(self):
        # `relish policy list` can fail while there's no leader; the
        # harness then removes the file, and the next check judges it.
        code, verdict = self.evaluate(self.snapshot())
        self.assertNotIn("policies", self.failures(verdict))
        self.assertNotIn("policy_checks", checker.load_state(self.evidence))

    def test_the_turn_budget_is_still_judged_with_the_policies_in_force(self):
        # The apply path runs off the agent loop, so policies shouldn't
        # move the turn budget; the soak checks that they don't.
        code, verdict = self.evaluate(self.snapshot(**{
            "policies_json": policy_listing(self.declared),
            "turns__rb-a-1_json": turns(over=("2.5",))}))
        self.assertEqual(code, 1)
        self.assertEqual(self.failures(verdict), ["agent-loop-turn"])

    def test_the_configure_step_refuses_a_listing_without_the_soak_policies(self):
        listing = self.evidence / "policies.json"
        listing.write_text(json.dumps(policy_listing(self.declared)))
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(checker.check_policies(self.evidence, listing), 0)
        listing.write_text(json.dumps(policy_listing({})))
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            self.assertEqual(checker.check_policies(self.evidence, listing), 1)
        self.assertIn("is not in force", output.getvalue())


class GuestClock(Evidence):
    """One clock source per guest: Lima's guest agent, not timesyncd as well (#608)."""

    def clock(self, ts=NOW, timesyncd="inactive", steps=0):
        return self.snapshot(ts=ts, **{"inventory__rb-a-1_txt":
                                       INVENTORY + f"timesyncd {timesyncd}\nclock_steps {steps}\n"})

    def test_a_guest_with_only_the_lima_agent_passes(self):
        _, verdict = self.evaluate(self.clock())
        self.assertEqual([item for item in verdict["findings"] if item["check"].startswith("guest-clock")], [])

    def test_timesyncd_running_beside_the_guest_agent_fails(self):
        code, verdict = self.evaluate(self.clock(timesyncd="active"))
        self.assertEqual(code, 1)
        failure = next(item for item in verdict["findings"] if item["check"] == "guest-clock")
        self.assertEqual(failure["target"], "rb-a-1")
        self.assertIn("systemd-timesyncd", failure["detail"])

    def test_clock_steps_outside_a_fault_window_warn_and_are_counted(self):
        _, verdict = self.evaluate(self.clock(steps=6))
        self.assertEqual(self.failures(verdict), [])
        warning = next(item for item in verdict["findings"] if item["check"] == "guest-clock-steps")
        self.assertEqual(warning["severity"], "warn")
        self.assertIn("6", warning["detail"])
        self.evaluate(self.clock(ts=NOW + 3600, steps=2))
        self.assertEqual(checker.load_state(self.evidence)["clock_steps"], {"rb-a-1": 8})

    def test_a_step_after_a_power_on_is_expected(self):
        checker.main(["window", str(self.evidence), "open", "power-off", "--down", "1"])
        _, verdict = self.evaluate(self.clock(steps=1))
        steps = [item for item in verdict["findings"] if item["check"] == "guest-clock-steps"]
        self.assertEqual([item["severity"] for item in steps], ["info"])

    def test_the_record_counts_clock_steps_per_node(self):
        self.evaluate(self.clock(steps=3))
        Record.write_run(self)
        record = self.evidence / "record.md"
        checker.render(self.evidence, record)
        self.assertIn("- Guest clock steps by Lima's guest agent: rb-a-1 3", record.read_text())


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
        self.assertIn("the writer file checks (writer-lost, writer-regression) decide data loss", text)
        self.assertIn("not supplied: upgrade slots skipped", text)
        self.assertIn("short leaf lifetimes unavailable", text)
        self.assertIn("- Teardown: `relish local destroy --yes` and `relish uninstall --yes` succeeded", text)

    def test_the_record_says_how_often_the_soak_policies_were_checked(self):
        self.write_run()
        record = self.evidence / "record.md"
        checker.render(self.evidence, record)
        self.assertIn("- Soak policies checked in force: not measured", record.read_text())
        state = checker.load_state(self.evidence)
        state.update(policy_checks=12, policy_generation=27)
        checker.save_state(self.evidence, state)
        again = self.evidence / "again.md"
        checker.render(self.evidence, again)
        self.assertIn("- Soak policies checked in force: 12 times, policy generation 27 at the last check",
                      again.read_text())

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
            run.update(duration_target=60 * 60)
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

    def test_a_clean_final_tier_without_its_execution_inventory_fails(self):
        # #1061: elapsed time, the schedule label and a digest string were
        # enough before; a final tier without walks, faults or specials passed.
        code, text = self.render_tier("final")
        self.assertEqual(code, 1)
        self.assertIn("final tier, full schedule, 8 h 02 min", text)
        self.assertIn("**Final tier: incomplete. It passes neither the V02 gate nor the release gate's final soak:**", text)
        self.assertNotIn("gate passes", text)

    def test_a_final_tier_with_an_open_failure_does_not_pass_the_gate(self):
        code, text = self.render_tier("final", failures=["bun RSS 31% above its warm sample"])
        self.assertEqual(code, 1)
        self.assertIn("**Final tier: not clean. The V02 gate does not pass.**", text)
        self.assertNotIn("gate passes", text)

    def test_a_short_or_unpinned_final_tier_is_not_acceptance(self):
        code, text = self.render_tier("final", finished_at=NOW + 4 * 3600, duration_target=4 * 3600,
                                      candidate_digest="`not checked`")
        self.assertEqual(code, 1)
        self.assertIn("it soaked for 4 h 00 min", text)
        self.assertIn("| candidate.json bytes match the qualified digest | 1 | 0 | MISSING |", text)
        self.assertNotIn("gate passes", text)

    def test_a_run_without_a_tier_is_a_diagnostic(self):
        self.write_run()
        record = self.evidence / "record.md"
        self.assertEqual(checker.render(self.evidence, record), 0)
        text = record.read_text()
        self.assertIn("**Custom run: diagnostic only.**", text)
        self.assertNotIn("gate passes", text)

    def test_render_refuses_to_overwrite(self):
        self.write_run()
        record = self.evidence / "record.md"
        record.write_text("earlier")
        with self.assertRaises(SystemExit):
            checker.render(self.evidence, record)


if __name__ == "__main__":
    unittest.main()


CHAOS_KINDS = ("kill", "delay", "partition", "scenario", "pause", "drop", "dns", "cpu", "memory")
NODE_FAULT_KINDS = ("delay", "disk-full", "slow-fsync")
CANDIDATE = b'{"assets": {}, "version": "0.2.3"}\n'
FINAL = 8 * 3600
QUIET = checker.QUIET_TAIL_SECONDS


def passing_job_report(load, node_faults):
    """A job controller snapshot that drained in its last two minutes and
    covered every mode, node and fault kind."""
    import job_soak
    kinds = job_soak.FAULT_KINDS + (job_soak.NODE_FAULT_KINDS if node_faults else ())
    coverage = {mode: dict({item: True for item in job_soak.REQUIRED},
                           **{"fault:" + node: True for node in NODES},
                           **{"fault-kind:" + kind: True for kind in kinds}) for mode in job_soak.MODES}
    return dict(stopped=True, active=[], errors=[], coverage=coverage, load=load,
                target_rate=1390 if load == "target" else None, started=NOW, heartbeat=NOW + FINAL,
                stop_requested_at=NOW + FINAL - 100,
                progress={mode: {"succeeded": 20_000_000, "retried": 0} for mode in job_soak.MODES},
                # p99 job start lag 32 ms, inside its SLO (#1400).
                metrics=dict(start_lag_ms_counts=[980, 0, 0, 0, 0, 20] + [0] * 10,
                             quiet_start_lag_ms_counts=[980, 0, 0, 0, 0, 20] + [0] * 10))


class ExecutionInventory(unittest.TestCase):
    """#1061: a final tier passes only on a completed-execution inventory:
    every upgrade walk, fault kind, special, workload check, the candidate's
    bytes and the job campaign's final drain, from what the run did, not
    from its labels."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.runs = 0

    def event(self, phase, **fields):
        checker.append_event(self.evidence, ts=NOW + len(checker.read_events(self.evidence)), phase=phase, **fields)

    def settled(self, phase, injection=None):
        self.event(phase, **(injection or {}))
        self.event(phase, verdict="settled", settle_seconds=40)

    def final_run(self, drop=(), job_final=True, candidate=CANDIDATE, **metadata):
        """Write the evidence of a complete 8-hour final, less the classes in `drop`."""
        self.runs += 1
        self.evidence = Path(self.temp.name) / str(self.runs)
        self.evidence.mkdir()
        digest = hashlib.sha256(CANDIDATE).hexdigest()
        run = dict(tier="final", schedule="full", started_at=NOW, finished_at=NOW + FINAL + QUIET + 120,
                   duration_target=FINAL, candidate_digest=f"`{digest}`", qualified_digest=digest,
                   bootstrapped="true", soak_bun="`bun-v0.2.3-soak.1`, SHA-256 `ff`", release_version="0.2.3",
                   job_soak=True, job_stop_at=NOW + FINAL - 120, job_load="target" if job_final else "calibration",
                   node_faults=job_final, cycles=8, ingress_rotation_secs=None, quiet_tail=QUIET)
        run.update(metadata)
        (self.evidence / "metadata.json").write_text(json.dumps(run))
        if candidate is not None and "candidate" not in drop:
            (self.evidence / "candidate.json").write_bytes(candidate)
        (self.evidence / "jobs").mkdir()
        report = passing_job_report(run["job_load"] if job_final else "calibration", job_final)
        if "drain" in drop:
            report.update(stopped=False, stop_requested_at=None)
        (self.evidence / "jobs" / "snapshot.json").write_text(json.dumps(report))
        armed = {check: dict(armed_seconds=checker.EXPOSURE_ARMED_SECONDS, longest_seconds=checker.EXPOSURE_WINDOW_SECONDS,
                             samples=30, processes_armed=1) for check in ("leak-rss", "leak-fd")}
        checker.save_state(self.evidence, {
            "nodes": NODES, "renewals": {"workload-identity": {name: 24 for name in NODES}},
            "exposure": {} if "exposure" in drop else {name: armed for name in NODES},
            "job_resources": {name: {"complete": True, "ts": NOW + FINAL + QUIET + 60, "live_owners": 0} for name in NODES}})
        if "baseline" not in drop:
            self.event("baseline", verdict="ok", detail="1")
        for phase in ("fault:bun-kill-follower", "fault:bun-kill-leader", "fault:deploy-kill", "fault:power-off"):
            if phase not in drop:
                self.settled(phase, dict(target="rb-a-2"))
        for kind in CHAOS_KINDS:
            if "fault:chaos-" + kind not in drop:
                self.settled("fault:chaos-" + kind, dict(exit=0, command=kind, verdict="ok"))
        if job_final:
            for kind in NODE_FAULT_KINDS:
                if "fault:node-" + kind not in drop:
                    self.settled("fault:node-" + kind, dict(exit=0, command=kind, target="rb-a-1"))
        for _ in range(4 - (1 if "walk" in drop else 0)):
            self.event("upgrade:upgrade", verdict="ok", duration=200)
            self.event("upgrade:rollback", verdict="ok", duration=200)
            self.event("upgrade:settle", verdict="settled", settle_seconds=60)
        for phase, count in (("special:graceful-stop", 2), ("special:quorum-loss", 2), ("special:all-off", 1)):
            for _ in range(count - (1 if phase in drop else 0)):
                self.event(phase)
                if phase == "special:quorum-loss":
                    self.event("special:quorum-back", verdict="ok", duration=30)
                self.event(phase, verdict="settled", settle_seconds=90)
        for _ in range(8 - (1 if "pulse" in drop else 0)):
            self.event("pulse", exit=0, verdict="ok")
        if "export" not in drop:
            self.event("storage:offline-export", target="rb-a-2", verdict="ok")
        if "quiet" not in drop:
            self.quiet_tail()

    def quiet_tail(self, interruption=None):
        """The quiet tail after the faults: nothing injected, walked or stopped."""
        self.event("quiet", detail=f"no faults for {QUIET} s")
        if interruption:
            self.event(interruption, target="rb-a-2")
        self.event("quiet", verdict="ok", duration=QUIET)

    def render(self):
        record = self.evidence / "record.md"
        code = checker.render(self.evidence, record)
        return code, record.read_text()

    def test_a_complete_job_final_passes_the_release_gates_final_soak(self):
        self.final_run()
        code, text = self.render()
        self.assertEqual(code, 0, text)
        self.assertIn("**Final job tier: clean. The release gate's final soak passes** for the candidate whose "
                      f"`candidate.json` has SHA-256 `{hashlib.sha256(CANDIDATE).hexdigest()}`", text)
        self.assertIn("## Execution inventory (gated)", text)
        self.assertIn("| upgrade walk (to the soak build and back) | 4 | 4 | ok |", text)
        self.assertIn("| fault:chaos-memory | 1 | 1 | ok |", text)
        self.assertNotIn("MISSING", text)

    def test_a_complete_v02_final_alone_passes_the_v02_gate_but_not_the_release_gates_final(self):
        self.final_run(job_final=False)
        code, text = self.render()
        self.assertEqual(code, 0, text)
        self.assertIn("**Final tier (V02 alone): clean. The V02 soak gate passes**", text)
        self.assertIn("not the release gate's final soak", text)
        self.assertNotIn("release gate's final soak passes", text)

    def test_each_missing_item_fails_a_final_that_otherwise_looks_clean(self):
        missing = {
            "walk": "| upgrade walk (to the soak build and back) | 4 | 3 | MISSING |",
            "baseline": "| app workloads settled at the baseline | 1 | 0 | MISSING |",
            "candidate": "| candidate.json bytes match the qualified digest | 1 | 0 | MISSING |",
            "drain": "| job campaign drained in its final two minutes | 1 | 0 | MISSING |",
            "pulse": "| catalogue pulse | 8 | 7 | MISSING |",
            "export": "| offline export | 1 | 0 | MISSING |",
            "quiet": "| quiet tail with no faults, walks or specials | 1 | 0 | MISSING |",
            "special:graceful-stop": "| special:graceful-stop | 2 | 1 | MISSING |",
            "special:quorum-loss": "| special:quorum-loss | 2 | 1 | MISSING |",
            "special:all-off": "| special:all-off | 1 | 0 | MISSING |",
        }
        for phase in ("fault:bun-kill-follower", "fault:bun-kill-leader", "fault:deploy-kill", "fault:power-off"):
            missing[phase] = f"| {phase} | 1 | 0 | MISSING |"
        for kind in CHAOS_KINDS:
            missing["fault:chaos-" + kind] = f"| fault:chaos-{kind} | 1 | 0 | MISSING |"
        for kind in NODE_FAULT_KINDS:
            missing["fault:node-" + kind] = f"| fault:node-{kind} | 1 | 0 | MISSING |"
        for item, row in missing.items():
            with self.subTest(item=item):
                self.final_run(drop=(item,))
                code, text = self.render()
                self.assertEqual(code, 1)
                self.assertTrue(text.startswith("# Sustained soak (V02): FAIL"))
                self.assertIn(row, text)
                # A missing drain fails the job gate too, so that record also says "not clean".
                self.assertIn("incomplete", text.split("\n")[4])
                self.assertNotIn("gate passes", text)
                self.assertNotIn("final soak passes", text)

    def test_skipped_upgrade_slots_are_not_walks(self):
        self.final_run(drop=("walk",))
        self.event("upgrade", verdict="skipped", detail="no --soak-bun")
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| upgrade walk (to the soak build and back) | 4 | 3 | MISSING |", text)

    def test_a_walk_whose_rollback_failed_is_not_a_completed_walk(self):
        self.final_run(drop=("walk",))
        self.event("upgrade:upgrade", verdict="ok", duration=200)
        self.event("upgrade:rollback", verdict="fail", duration=600)
        self.event("upgrade:settle", verdict="settled", settle_seconds=60)
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| upgrade walk (to the soak build and back) | 4 | 3 | MISSING |", text)

    def test_a_fault_injected_but_never_settled_is_not_completed(self):
        self.final_run(drop=("fault:power-off",))
        self.event("fault:power-off", target="rb-a-2")
        self.event("fault:power-off", verdict="fail", settle_seconds=480)
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| fault:power-off | 1 | 0 | MISSING |", text)

    def test_a_chaos_fault_the_rails_refused_is_not_completed(self):
        self.final_run(drop=("fault:chaos-pause",))
        self.settled("fault:chaos-pause", dict(exit=1, command="pause", verdict="fail"))
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| fault:chaos-pause | 1 | 0 | MISSING |", text)

    def test_quorum_loss_needs_quorum_back(self):
        self.final_run(drop=("special:quorum-loss",))
        self.event("special:quorum-loss")
        self.event("special:quorum-loss", verdict="settled", settle_seconds=90)
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| special:quorum-loss | 2 | 1 | MISSING |", text)

    def test_candidate_bytes_must_hash_to_the_qualified_digest(self):
        self.final_run(candidate=b"something else\n")
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| candidate.json bytes match the qualified digest | 1 | 0 | MISSING |", text)

    def test_an_adopted_home_never_verified_the_candidate(self):
        self.final_run(bootstrapped="false")
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| candidate.json bytes match the qualified digest | 1 | 0 | MISSING |", text)

    def test_a_final_without_a_job_campaign_has_no_final_drain(self):
        self.final_run(job_final=False, job_soak=None)
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| job campaign drained in its final two minutes | 1 | 0 | MISSING |", text)

    def test_a_fault_inside_the_quiet_tail_breaks_it(self):
        self.final_run(drop=("quiet",))
        self.quiet_tail(interruption="fault:bun-kill-follower")
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| quiet tail with no faults, walks or specials | 1 | 0 | MISSING |", text)

    def test_a_final_whose_leak_checkers_lack_exposure_fails(self):
        # #1062: a final that never armed leak-rss on a node can't claim it
        # found no leak there.
        self.final_run()
        state = checker.load_state(self.evidence)
        state["exposure"]["rb-a-2"]["leak-rss"] = dict(armed_seconds=30 * 60, longest_seconds=30 * 60, samples=7,
                                                      processes_armed=1)
        checker.save_state(self.evidence, state)
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertTrue(text.startswith("# Sustained soak (V02): FAIL"))
        self.assertIn("## Leak-checker exposure (gated)", text)
        self.assertIn("| rb-a-2 | leak-rss | 30 min | 30 min | 7 | 1 | SHORT |", text)
        self.assertIn("leak-checker exposure too short: rb-a-2 leak-rss", text)
        self.assertNotIn("passes**", text)

    def test_a_final_needs_one_unbroken_armed_window(self):
        self.final_run()
        state = checker.load_state(self.evidence)
        state["exposure"]["rb-a-3"]["leak-fd"] = dict(armed_seconds=90 * 60, longest_seconds=20 * 60, samples=30,
                                                     processes_armed=4)
        checker.save_state(self.evidence, state)
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| rb-a-3 | leak-fd | 90 min | 20 min | 30 | 4 | SHORT |", text)

    def test_a_final_with_no_exposure_recorded_fails(self):
        self.final_run(drop=("exposure",))
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| rb-a-1 | leak-rss | 0 min | 0 min | 0 | 0 | SHORT |", text)

    def test_fast_and_custom_runs_record_exposure_without_gating_on_it(self):
        for tier in ("fast", "custom"):
            with self.subTest(tier=tier):
                self.final_run(drop=("walk", "exposure", "quiet"), tier=tier, schedule="compressed",
                               finished_at=NOW + 3600, duration_target=3600, cycles=6)
                code, text = self.render()
                self.assertEqual(code, 0, text)
                self.assertIn("## Leak-checker exposure (recorded, not gated)", text)
                self.assertIn("| rb-a-1 | leak-rss | 0 min | 0 min | 0 | 0 | short |", text)

    def test_relabelling_a_fast_run_as_final_does_not_promote_it(self):
        # A fast run's evidence: two walks, one of each special, one pulse,
        # four cycles. Its labels now say final, full, 8 h.
        self.final_run(drop=("walk", "special:graceful-stop", "special:quorum-loss", "pulse"))
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("**Final tier: incomplete.", text)

    def test_relabelling_a_v02_final_as_a_job_final_does_not_promote_it(self):
        # The labels claim the job final; the controller ran the calibration
        # load and no node fault ever ran.
        self.final_run(job_final=False, job_load="target", node_faults=True)
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("| fault:node-slow-fsync | 1 | 0 | MISSING |", text)
        self.assertNotIn("final soak passes", text)

    def test_a_fast_tier_records_its_inventory_without_gating_on_it(self):
        self.final_run(drop=("walk",), tier="fast", schedule="compressed", finished_at=NOW + 3600,
                       duration_target=3600, cycles=6)
        code, text = self.render()
        self.assertEqual(code, 0, text)
        self.assertIn("## Execution inventory (recorded, not gated)", text)
        self.assertIn("**Fast tier: clean.**", text)

    def test_the_checker_requires_what_the_final_plan_schedules(self):
        # The checker's own copy of the schedule must match the harness's.
        import subprocess
        result = subprocess.run([str(Path(__file__).parent / "qualify-jobs-sustained.sh"), "--tier", "final", "--plan"],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=60,
                                env=dict(os.environ, RELIABURGER_SOAK_CAFFEINATED="1"))
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = [line.split()[1:] for line in result.stdout.splitlines() if line.startswith("plan: cycle ")]
        planned = {"quiet-tail": sum(1 for line in result.stdout.splitlines()
                                     if line.startswith(f"plan: quiet-tail {QUIET} s "))}
        for line in lines:
            slot = line[2]
            if slot == "chaos":
                names = ["fault:chaos-" + kind for kind in line[3:]]
            elif slot == "node-fault":
                names = ["fault:node-" + line[3]]
            elif slot == "special":
                names = ["special:" + line[3]]
            else:
                names = [slot]
            for name in names:
                planned[name] = planned.get(name, 0) + 1
        required = checker.required_execution(dict(tier="final", node_faults=True))
        self.assertEqual(planned.get("upgrade"), required["upgrade walk (to the soak build and back)"])
        self.assertEqual(planned.get("catalogue-pulse"), required["catalogue pulse"])
        self.assertEqual(planned.get("offline-export"), required["offline export"])
        self.assertEqual(planned.get("quiet-tail"), required["quiet tail with no faults, walks or specials"])
        for name in ("special:graceful-stop", "special:quorum-loss", "special:all-off"):
            self.assertEqual(planned.get(name), required[name], name)
        for kind in CHAOS_KINDS:
            self.assertGreaterEqual(planned.get("fault:chaos-" + kind, 0), required["fault:chaos-" + kind], kind)
        for kind in NODE_FAULT_KINDS:
            self.assertGreaterEqual(planned.get("fault:node-" + kind, 0), required["fault:node-" + kind], kind)
        for slot in ("follower-kill", "leader-kill", "deploy-kill", "power-off"):
            self.assertGreaterEqual(planned.get(slot, 0), 1, slot)


class FinalTierDriver(unittest.TestCase):
    """#1061: the harness refuses to start a final tier that could only
    finish incomplete, before it touches a cluster."""

    release = Path(__file__).parent

    def run_script(self, *arguments):
        import subprocess
        return subprocess.run([str(self.release / "qualify-sustained.sh"), *arguments],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=60,
                              env=dict(os.environ, RELIABURGER_SOAK_CAFFEINATED="1"))

    def soak_build(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        build = Path(directory.name) / "bun-v0.2.3-soak.1"
        build.write_bytes(b"bun")
        build.with_name(build.name + ".sig").write_text("{}")
        return str(build)

    def test_a_final_tier_refuses_to_start_without_a_soak_build(self):
        result = self.run_script("--tier", "final", "--base-url", "https://example.invalid/staged",
                                 "--qualified-digest", "a" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("a final tier needs --soak-bun", result.stderr)

    def test_a_final_tier_refuses_to_start_without_a_qualified_digest(self):
        result = self.run_script("--tier", "final", "--base-url", "https://example.invalid/staged",
                                 "--soak-bun", self.soak_build())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("a final tier needs --qualified-digest", result.stderr)

    def test_a_final_tier_refuses_an_adopted_home(self):
        result = self.run_script("--tier", "final", "--home", tempfile.gettempdir(), "--qualified-digest", "a" * 64,
                                 "--soak-bun", self.soak_build())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("a final tier bootstraps its own cluster", result.stderr)

    def test_the_final_plan_walks_four_times_and_its_chaos_reaches_every_kind(self):
        result = self.run_script("--tier", "final", "--plan")
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = [line.split()[1:] for line in result.stdout.splitlines() if line.startswith("plan: cycle ")]
        self.assertEqual(sum(1 for line in lines if line[2] == "upgrade"), 4)
        kinds = {kind for line in lines if line[2] == "chaos" for kind in line[3:]}
        self.assertEqual(kinds, set(CHAOS_KINDS))

    def test_a_final_adds_its_quiet_tail_after_eight_hours_of_faults(self):
        result = self.run_script("--tier", "final", "--plan")
        self.assertEqual(result.returncode, 0, result.stderr)
        header = result.stdout.splitlines()[0].split()[1:]
        header = dict(zip(header[::2], header[1::2]))
        self.assertEqual((header["duration"], header["quiet-tail"]), ("28800", str(QUIET)))
        self.assertIn(f"plan: quiet-tail {QUIET} s with no faults, walks or specials after the last cycle", result.stdout)

    def test_fast_and_custom_tiers_have_no_quiet_tail(self):
        for arguments in (("--tier", "fast"), ("--duration", "2h")):
            result = self.run_script(*arguments, "--plan")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(" quiet-tail 0", result.stdout.splitlines()[0])
            self.assertNotIn("plan: quiet-tail", result.stdout)

    def test_the_hosted_soak_is_an_explicit_diagnostic(self):
        workflow = (self.release.parent.parent / ".github" / "workflows" / "soak.yml").read_text()
        self.assertIn("--tier custom", workflow)
        self.assertIn("diagnostic", workflow)


class Choices(unittest.TestCase):
    """#1062: every fault and node choice comes from a recorded seed, so a
    run's choices can be repeated, and each one is logged."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)

    def evidence(self, name):
        directory = Path(self.temp.name) / name
        directory.mkdir()
        return directory

    def draws(self, evidence, seed):
        checker.start_choices(evidence, seed)
        return [checker.choose(evidence, what, low, high) for what, low, high in
                (("deploy-kill-node", 1, 3), ("deploy-kill-offset", 0, 20), ("power-off-hold", 60, 180),
                 ("follower", 0, 1), ("export-node", 2, 3))]

    def test_the_same_seed_repeats_every_choice(self):
        self.assertEqual(self.draws(self.evidence("a"), 1234), self.draws(self.evidence("b"), 1234))

    def test_another_seed_gives_other_choices(self):
        first = [self.draws(self.evidence(f"a{seed}"), seed) for seed in (1, 2, 3)]
        self.assertGreater(len({tuple(draw) for draw in first}), 1)

    def test_choices_stay_in_their_range(self):
        evidence = self.evidence("range")
        checker.start_choices(evidence, 99)
        values = [checker.choose(evidence, "hold", 60, 180) for _ in range(200)]
        self.assertTrue(all(60 <= value <= 180 for value in values))
        self.assertEqual(len(set(values)) > 50, True)

    def test_each_choice_is_logged_with_what_it_chose(self):
        evidence = self.evidence("log")
        checker.start_choices(evidence, 7)
        value = checker.choose(evidence, "deploy-kill-node", 1, 3)
        events = [event for event in checker.read_events(evidence) if event["phase"] == "choice"]
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["target"], "deploy-kill-node")
        self.assertEqual(events[0]["detail"], f"{value} of 1-3, draw 0, seed 7")

    def test_without_a_seed_one_is_made_and_recorded(self):
        evidence = self.evidence("fresh")
        seed = checker.start_choices(evidence, None)
        self.assertIsInstance(seed, int)
        self.assertEqual(json.loads((evidence / "choices.json").read_text())["seed"], seed)

    def test_a_resumed_run_continues_its_sequence(self):
        whole = self.evidence("whole")
        expected = self.draws(whole, 42)
        resumed = self.evidence("resumed")
        checker.start_choices(resumed, 42)
        got = [checker.choose(resumed, "deploy-kill-node", 1, 3), checker.choose(resumed, "deploy-kill-offset", 0, 20)]
        self.assertEqual(checker.start_choices(resumed, None), 42)
        got += [checker.choose(resumed, "power-off-hold", 60, 180), checker.choose(resumed, "follower", 0, 1),
                checker.choose(resumed, "export-node", 2, 3)]
        self.assertEqual(got, expected)

    def test_a_resumed_run_refuses_a_different_seed(self):
        evidence = self.evidence("conflict")
        checker.start_choices(evidence, 1)
        with self.assertRaises(ValueError):
            checker.start_choices(evidence, 2)

    def test_the_command_line_prints_the_choice(self):
        evidence = self.evidence("cli")
        with contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertEqual(checker.main(["seed", str(evidence), "--seed", "5"]), 0)
            self.assertEqual(checker.main(["choose", str(evidence), "power-off-node", "2", "3"]), 0)
        seed, value = out.getvalue().split()
        self.assertEqual(seed, "5")
        self.assertIn(int(value), (2, 3))


class CheckerExposure(Evidence):
    """#1062: the record says how long each leak checker was armed on each
    node. Reported only; nothing is gated on it yet."""

    def sample(self, ts, pid, rss_kb=300000, fd=300):
        inventory = INVENTORY.replace("bun_pid 100", f"bun_pid {pid}").replace(
            "bun_rss_kb 300000", f"bun_rss_kb {rss_kb}").replace("bun_fd 300", f"bun_fd {fd}")
        checker.resource_trend_findings(self.state, "rb-a-1", checker.parse_inventory(inventory), ts)

    def setUp(self):
        super().setUp()
        self.state = {"nodes": NODES}

    def exposure(self, check):
        return self.state["exposure"]["rb-a-1"][check]

    def test_one_long_process_arms_each_checker_after_its_warm_up(self):
        for minute in range(0, 91, 5):
            self.sample(NOW + minute * 60, 100)
        self.assertEqual(self.exposure("leak-rss")["armed_seconds"], 30 * 60)
        self.assertEqual(self.exposure("leak-rss")["samples"], 7)
        self.assertEqual(self.exposure("leak-fd")["armed_seconds"], 40 * 60)
        self.assertEqual(self.exposure("leak-fd")["longest_seconds"], 40 * 60)

    def test_a_restart_ends_the_window_and_starts_the_warm_up_again(self):
        for minute in range(0, 71, 5):
            self.sample(NOW + minute * 60, 100)
        for minute in range(75, 146, 5):
            self.sample(NOW + minute * 60, 200)
        rss = self.exposure("leak-rss")
        self.assertEqual(rss["armed_seconds"], 20 * 60)
        self.assertEqual(rss["longest_seconds"], 10 * 60)
        self.assertEqual(rss["processes_armed"], 2)

    def test_short_lived_processes_never_arm_the_checkers(self):
        for start in range(0, 300, 45):
            for minute in range(start, start + 45, 5):
                self.sample(NOW + minute * 60, 1000 + start)
        self.assertEqual(self.exposure("leak-rss")["armed_seconds"], 0)
        self.assertEqual(self.exposure("leak-fd")["armed_seconds"], 0)

    def test_a_finals_quiet_tail_arms_both_checkers_long_enough_after_the_last_special(self):
        # The last cycle ends with a graceful stop after its heavy sample, so
        # each Bun is first seen up to one heavy interval into the tail, and
        # every heavy observe pushes the next one back by however long it
        # took (wait_until in qualify-sustained.sh). The tail's last sample
        # is the observe after it.
        heavy_every, observe_takes = 300, 30
        tail_end = NOW + checker.QUIET_TAIL_SECONDS
        ts = NOW + heavy_every
        while ts < tail_end:
            self.sample(ts, 200)
            ts += heavy_every + observe_takes
        self.sample(tail_end, 200)
        short = [check for node, check in checker.exposure_shortfalls(self.state) if node == "rb-a-1"]
        self.assertEqual(short, [], self.state["exposure"]["rb-a-1"])

    def test_the_record_reports_armed_minutes_per_node_and_checker(self):
        (self.evidence / "metadata.json").write_text(json.dumps({"started_at": NOW, "finished_at": NOW + 7200}))
        for minute in range(0, 91, 5):
            self.sample(NOW + minute * 60, 100)
        checker.save_state(self.evidence, self.state)
        record = self.evidence / "record.md"
        checker.render(self.evidence, record)
        text = record.read_text()
        self.assertIn("## Leak-checker exposure (recorded, not gated)", text)
        self.assertIn("| rb-a-1 | leak-rss | 30 min | 30 min | 7 | 1 | short |", text)
        self.assertIn("| rb-a-1 | leak-fd | 40 min | 40 min | 9 | 1 | short |", text)
        self.assertIn("| rb-a-2 | leak-rss | 0 min | 0 min | 0 | 0 | short |", text)


class SeedInTheRecord(Evidence):
    def test_the_record_names_the_seed_and_the_observed_effects(self):
        (self.evidence / "metadata.json").write_text(json.dumps({"started_at": NOW, "finished_at": NOW + 600}))
        checker.start_choices(self.evidence, 31337)
        checker.choose(self.evidence, "power-off-node", 2, 3)
        checker.append_event(self.evidence, ts=NOW + 5, phase="effect:node-disk-full", target="rb-a-2",
                             detail="available 67108864 bytes")
        record = self.evidence / "record.md"
        checker.render(self.evidence, record)
        text = record.read_text()
        self.assertIn("- Fault and node choices: 1, from seed 31337 (`--seed 31337` repeats them)", text)
        self.assertIn("| effect:node-disk-full | rb-a-2 | available 67108864 bytes |", text)


class SeededHarness(unittest.TestCase):
    """#1062: the harness draws nothing from bash's unseeded $RANDOM."""

    release = Path(__file__).parent

    def run_script(self, *arguments):
        import subprocess
        return subprocess.run([str(self.release / "qualify-sustained.sh"), *arguments],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=60,
                              env=dict(os.environ, RELIABURGER_SOAK_CAFFEINATED="1"))

    def test_the_harness_never_uses_bash_random(self):
        script = (self.release / "qualify-sustained.sh").read_text()
        code = [line for line in script.splitlines() if not line.lstrip().startswith("#")]
        self.assertFalse([line for line in code if "RANDOM" in line])

    def test_the_plan_prints_the_seed_it_was_given(self):
        result = self.run_script("--tier", "fast", "--plan", "--seed", "77")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("plan: seed 77", result.stdout)

    def test_a_seed_must_be_a_whole_number(self):
        result = self.run_script("--tier", "fast", "--plan", "--seed", "lucky")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--seed must be a whole number", result.stderr)

    def test_every_node_fault_has_a_parseable_effect_probe(self):
        import subprocess
        script = (self.release / "qualify-sustained.sh").read_text()
        function = script[script.index("node_fault_script() {"):]
        function = function[:function.index("\n}\n") + 3]
        for kind in ("delay", "disk-full", "slow-fsync"):
            probe = subprocess.run(["bash", "-c", function + f'node_fault_script {kind} effect'],
                                   stdout=subprocess.PIPE, text=True, check=True).stdout
            self.assertTrue(probe.strip(), kind)
            subprocess.run(["bash", "-n"], input=probe, text=True, check=True)


class GuestMemory(unittest.TestCase):
    """#985: the OOM report that explained #982 was only found by hand on
    the live VM. The soak keeps guest memory evidence itself."""

    script = Path(__file__).parent / "sustained" / "guest-memory.sh"
    KERNEL = """Oct 16 10:00:00 rb-a-1 kernel: eth0: link up
Oct 16 10:01:00 rb-a-1 kernel: bun invoked oom-killer: gfp_mask=0x140cca(GFP_HIGHUSER_MOVABLE|__GFP_COMP), order=0, oom_score_adj=0
Oct 16 10:01:00 rb-a-1 kernel: Mem-Info:
Oct 16 10:01:00 rb-a-1 kernel: [  pid  ]   uid  tgid total_vm      rss pgtables_bytes swapents oom_score_adj name
Oct 16 10:01:00 rb-a-1 kernel: [    812]     0   812   400000   262144   2400000        0          -999 lima-guestagent
Oct 16 10:01:00 rb-a-1 kernel: [   1200]     0  1200   300000    97000   1100000        0             0 bun
Oct 16 10:01:00 rb-a-1 kernel: oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),task=bun,pid=1200,uid=0
Oct 16 10:01:00 rb-a-1 kernel: Out of memory: Killed process 1200 (bun) total-vm:1200000kB, anon-rss:388000kB
Oct 16 10:02:00 rb-a-1 kernel: eth0: carrier ok
"""

    def run_script(self, mode):
        import os, subprocess
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            proc = root / "proc"; (proc / "812" / "fd").mkdir(parents=True)
            (proc / "meminfo").write_text("MemTotal: 2014000 kB\nMemAvailable: 51200 kB\n")
            (proc / "812" / "status").write_text("Name: lima-guestagent\nVmRSS:\t1048576 kB\n")
            for fd in range(5): (proc / "812" / "fd" / str(fd)).write_text("")
            bin_dir = root / "bin"; bin_dir.mkdir()
            calls = root / "calls"
            stubs = {"ps": "printf '  PID   RSS COMMAND\\n  812 1048576 lima-guestagent\\n 1200 388000 bun\\n'",
                     "systemctl": "echo 812",
                     "journalctl": f"echo \"$*\" >> {calls}; cat <<'LOG'\n{self.KERNEL}LOG"}
            for name, body in stubs.items():
                (bin_dir / name).write_text("#!/bin/sh\n" + body + "\n"); (bin_dir / name).chmod(0o755)
            result = subprocess.run(["bash", str(self.script), mode], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                    timeout=30, env=dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}", SOAK_PROC=str(proc)))
            return result, calls.read_text()

    def test_the_report_has_meminfo_top_rss_the_guest_agent_and_the_oom_report(self):
        result, _ = self.run_script("recent")
        self.assertEqual(result.returncode, 0, result.stderr)
        text = result.stdout
        self.assertIn("MemAvailable: 51200 kB", text)
        self.assertIn("1048576 lima-guestagent", text)
        self.assertIn("lima_guestagent_rss_kb 1048576", text)
        self.assertIn("lima_guestagent_fd 5", text)
        self.assertIn("invoked oom-killer", text)
        self.assertIn("-999 lima-guestagent", text)
        self.assertIn("Out of memory: Killed process 1200 (bun)", text)
        # Only the OOM report, not the rest of the kernel log.
        self.assertNotIn("link up", text)
        self.assertNotIn("carrier ok", text)

    def test_heavy_checks_read_new_kernel_messages_and_bundles_the_last_half_hour(self):
        _, heavy = self.run_script("cursor")
        _, bundle = self.run_script("recent")
        self.assertIn("--cursor-file=/var/tmp/soak-kernel.cursor", heavy)
        self.assertIn("--since=-30min", bundle)

    def test_heavy_checks_and_failure_bundles_collect_it(self):
        script = (Path(__file__).parent / "qualify-sustained.sh").read_text()
        heavy = script[script.index("collect() {"):script.index("registry() {")]
        bundle = script[script.index("record_failure() {"):script.index("setup_fail() {")]
        self.assertIn('memory_report "$node" cursor > "$directory/memory-${vm[node]}.txt"', heavy)
        self.assertIn('memory_report "$node" recent > "$directory/memory-${vm[node]}.txt"', bundle)


class TestFailures(unittest.TestCase):
    """#1068: the chaos scenario recorded the JSON report's last three lines,
    `} ] }`, which hid why `relish test` failed."""

    REASON = "apply failed: API error (status 400): timeout (2s) must not be longer than interval (1s)"

    def report(self):
        return json.dumps({"schema_version": 1, "results": [
            {"name": "dead_worker_node_has_workloads_rescheduled", "outcome": {"status": "fail", "reason": self.REASON}},
            {"name": "passing_case", "outcome": {"status": "pass"}},
        ]}, indent=2)

    def test_a_report_names_each_failing_case_and_its_reason(self):
        self.assertEqual(checker.test_failure_reasons(self.report()),
                         f"dead_worker_node_has_workloads_rescheduled: fail: {self.REASON}")

    def test_without_a_report_it_keeps_the_tail_of_stderr(self):
        self.assertEqual(checker.test_failure_reasons("", "connecting\nerror: cluster unreachable\n"),
                         "connecting error: cluster unreachable")

    def test_the_subcommand_reads_the_report_file(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "chaos-3.log"
            report.write_text(self.report())
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(checker.main(["test-failures", str(report), str(report) + ".err"]), 0)
        self.assertIn(self.REASON, out.getvalue())

    def test_the_chaos_scenario_records_the_reason_not_the_tail(self):
        script = (Path(__file__).parent / "qualify-sustained.sh").read_text()
        chaos = script[script.index("chaos_one() {"):script.index("catalogue_pulse() {")]
        self.assertIn('--timeout 600s > "$output" 2> "$output.err"', chaos)
        self.assertIn('record_failure "chaos-$kind" "$(check test-failures "$output" "$output.err")"', chaos)


DAY = 24 * 3600


class DayRecord(unittest.TestCase):
    """#1032: a day tier is gated on the day itself: the host-job rate, each
    node's share, the last hour against the first, the drain, the candidate
    and every leak checker armed. It owes no faults, walks or specials."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.runs = 0

    def day_run(self, rate=23149, per_node=None, last_hour=None, drop=()):
        import job_soak
        self.runs += 1
        self.evidence = Path(self.temp.name) / str(self.runs)
        self.evidence.mkdir()
        digest = hashlib.sha256(CANDIDATE).hexdigest()
        (self.evidence / "metadata.json").write_text(json.dumps(dict(
            tier="day", schedule="none", started_at=NOW, finished_at=NOW + DAY + 300, duration_target=DAY,
            candidate_digest=f"`{digest}`", qualified_digest=digest, bootstrapped="true",
            soak_bun="not supplied: upgrade slots skipped", job_soak=True, job_stop_at=NOW + DAY, job_load="day",
            target_rate=str(rate if rate >= 23149 else 23149), node_faults=False, cycles=0,
            ingress_rotation_secs=None, quiet_tail=0)))
        (self.evidence / "candidate.json").write_bytes(CANDIDATE)
        (self.evidence / "jobs").mkdir()
        host = rate * DAY
        marks = [[NOW + hour * 3600, hour * 3600 * rate] for hour in range(25)]
        if last_hour is not None:
            marks[-1][1] = marks[-2][1] + 3600 * last_hour
        coverage = {mode: {item: True for item in job_soak.REQUIRED if item != "unknown"} for mode in job_soak.MODES}
        third = host // 3
        snapshot = dict(stopped=True, active=[], errors=[], coverage=coverage, load="day", target_rate=23149,
                        started=NOW, heartbeat=NOW + DAY + 100, stop_requested_at=NOW + DAY,
                        progress={mode: {"succeeded": host if mode == "process" else 1_000_000, "retried": 0}
                                  for mode in job_soak.MODES},
                        per_node=per_node if per_node is not None else {
                            name: dict(succeeded=third, slots=27, restarted=False) for name in NODES},
                        hour_marks=marks)
        if "drain" in drop:
            snapshot.update(stopped=False)
        (self.evidence / "jobs" / "snapshot.json").write_text(json.dumps(snapshot))
        armed = {check: dict(armed_seconds=DAY - 3600, longest_seconds=DAY - 3600, samples=2000, processes_armed=1)
                 for check in ("leak-rss", "leak-fd")}
        checker.save_state(self.evidence, {
            "nodes": NODES, "renewals": {"workload-identity": {name: 50 for name in NODES}},
            "exposure": {} if "exposure" in drop else {name: armed for name in NODES},
            "job_resources": {name: {"complete": True, "ts": NOW + DAY + 200, "live_owners": 0} for name in NODES}})
        checker.append_event(self.evidence, ts=NOW, phase="baseline", verdict="ok", detail="1")

    def render(self):
        record = self.evidence / "record.md"
        code = checker.render(self.evidence, record)
        return code, record.read_text()

    def test_a_clean_day_passes_and_records_every_node_and_hour(self):
        self.day_run()
        code, text = self.render()
        self.assertEqual(code, 0, text)
        self.assertIn("**Day tier: clean.** 2,000,073,600 host-job tasks accepted in", text)
        self.assertIn("## Execution inventory (gated)", text)
        self.assertNotIn("MISSING", text)
        self.assertNotIn("fault:", text.split("## Execution inventory")[1].split("##")[0])
        self.assertIn("## Day run (#1032)", text)
        self.assertIn("| rb-a-1 | 27 | 666,691,200 | 7,716.3/s | 33.3% | 33.3% | ok |", text)
        hours = [line for line in text.splitlines() if re.match(r"\| Hour \d+ \|", line)]
        self.assertEqual(len(hours), 24)
        self.assertIn("Last hour against the first: 23,149.0/s against 23,149.0/s, ok", text)
        self.assertIn("## Leak-checker exposure (gated)", text)

    def test_a_slow_day_fails_and_says_whether_it_made_the_floor(self):
        self.day_run(rate=15000)
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("host-job rate 15,000.0/s is below the 23149/s target (the billion-a-day floor holds", text)
        self.assertIn("**Day tier: not clean.**", text)

    def test_an_unbalanced_or_slowing_day_fails(self):
        self.day_run(per_node={"rb-a-1": dict(succeeded=50, slots=27), "rb-a-2": dict(succeeded=25, slots=27),
                               "rb-a-3": dict(succeeded=25, slots=27)})
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("rb-a-1 took 50.0% of the host-job successes", text)
        self.day_run(last_hour=20000)
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("the last hour ran at 20,000.0/s, more than 10% below the first hour's 23,149.0/s", text)

    def test_a_day_without_its_drain_or_armed_leak_checkers_fails(self):
        for item in ("drain", "exposure"):
            with self.subTest(item=item):
                self.day_run(drop=(item,))
                code, text = self.render()
                self.assertEqual(code, 1)
                self.assertNotIn("**Day tier: clean.**", text)

