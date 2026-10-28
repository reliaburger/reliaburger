"""The council history client records what it asked and what it saw (#1399)."""
import json
import os
from pathlib import Path
import random
import subprocess
import tempfile
import threading
import time
import unittest

import council_history as client
import linearizability as lin
import sustained_check


class FakeCouncil:
    """An in-memory council behind the lease API, with an optional follower-read bug.

    Every write lands on the leader's copy straight away; a follower's copy
    catches up only now and then. Honest reads always use the leader's copy.
    With `follower_reads`, a read sent to a follower answers from its own
    copy without asking the leader: the bug #1399 says the check must catch.
    """

    def __init__(self, nodes=("n1", "n2", "n3"), follower_reads=False, seed=7):
        self.nodes = list(nodes)
        self.leader = self.nodes[0]
        self.copies = {node: {} for node in self.nodes}
        self.follower_reads = follower_reads
        self.random = random.Random(seed)
        self.clock = int(time.time() * 1000)
        self.next_lease = 0
        self.lock = threading.Lock()

    def request(self, node, method, path, body=None, timeout=None):
        with self.lock:
            self.clock += 1
            for follower in self.nodes[1:]:
                if self.random.random() < 0.3:
                    self.copies[follower] = {key: dict(value) for key, value in self.copies[self.leader].items()}
            return self.handle(node, method, path, body)

    def handle(self, node, method, path, body):
        leases = self.copies[self.leader]
        parts = path.strip("/").split("/")
        if method == "POST" and path == "/v1/test/leases":
            if any(lease["namespace"] == body["namespace"] for lease in leases.values()):
                return 409, "lease namespace is already owned"
            self.next_lease += 1
            lease_id = f"lease{self.next_lease}"
            leases[lease_id] = {"lease_id": lease_id, "namespace": body["namespace"],
                                "expires_at_unix_ms": self.clock + body["ttl_seconds"] * 1000,
                                "state": {"status": "active"}}
            return 201, json.dumps(leases[lease_id])
        lease_id = parts[3]
        if method == "GET":
            source = self.copies[node] if self.follower_reads else leases
            if lease_id not in source:
                return 404, json.dumps({"error": "lease not found"})
            return 200, json.dumps(source[lease_id])
        if method == "POST" and parts[-1] == "renew":
            if lease_id not in leases:
                return 404, json.dumps({"error": "lease not found"})
            leases[lease_id]["expires_at_unix_ms"] = self.clock + body["ttl_seconds"] * 1000
            return 200, json.dumps(leases[lease_id])
        if method == "DELETE":
            leases.pop(lease_id, None)
            return 204, ""
        return 400, "unexpected request"


def run_fake(council, operations=400, clients=3):
    with tempfile.TemporaryDirectory() as directory:
        history = Path(directory) / "history.jsonl"
        recorder = client.Recorder(history)
        options = client.Options(namespaces=["rbtest-lin-0", "rbtest-lin-1"], ttl_seconds=300,
                                 pause_seconds=0, timeout_seconds=5, max_operations=operations)
        client.run_clients(recorder, council, council.nodes, options, clients=clients, seed=3,
                           stop=threading.Event())
        recorder.close()
        lines = history.read_text().splitlines()
        return [json.loads(line) for line in lines]


class Classify(unittest.TestCase):
    def test_a_granted_lease_is_ok_with_its_id_and_expiry(self):
        body = json.dumps({"lease_id": "abc", "expires_at_unix_ms": 99, "state": {"status": "active"}})
        self.assertEqual(client.classify("grant", 201, body),
                         {"outcome": "ok", "lease": "abc", "expires_at_unix_ms": 99, "cleaning": False})

    def test_an_owned_namespace_refuses_a_grant(self):
        self.assertEqual(client.classify("grant", 409, "lease namespace is already owned")["outcome"], "owned")

    def test_other_definite_refusals_changed_nothing(self):
        self.assertEqual(client.classify("grant", 409, "too many active test leases")["outcome"], "refused")
        self.assertEqual(client.classify("grant", 429, "")["outcome"], "refused")
        self.assertEqual(client.classify("renew", 403, "")["outcome"], "refused")

    def test_timeouts_and_server_errors_are_unknown(self):
        for kind in ("grant", "renew", "release", "read"):
            self.assertEqual(client.classify(kind, None, "")["outcome"], "unknown")
            self.assertEqual(client.classify(kind, 503, "no leader")["outcome"], "unknown")
            self.assertEqual(client.classify(kind, 504, "leader request timed out")["outcome"], "unknown")

    def test_renew_reports_missing_and_inactive_leases(self):
        self.assertEqual(client.classify("renew", 404, "")["outcome"], "not_found")
        self.assertEqual(client.classify("renew", 409, "lease not found")["outcome"], "not_found")
        self.assertEqual(client.classify("renew", 409, "lease is expired or cleanup has started")["outcome"],
                         "not_active")

    def test_a_read_carries_the_expiry_and_whether_cleanup_started(self):
        body = json.dumps({"lease_id": "abc", "expires_at_unix_ms": 7,
                           "state": {"status": "cleaning", "attempts": 1, "last_error": None}})
        self.assertEqual(client.classify("read", 200, body),
                         {"outcome": "ok", "lease": "abc", "expires_at_unix_ms": 7, "cleaning": True})
        self.assertEqual(client.classify("read", 404, "")["outcome"], "not_found")

    def test_a_release_is_done_pending_or_unknown(self):
        self.assertEqual(client.classify("release", 204, "")["outcome"], "ok")
        self.assertEqual(client.classify("release", 202, "")["outcome"], "pending")
        self.assertEqual(client.classify("release", 409, "busy")["outcome"], "unknown")

    def test_an_unparseable_success_is_unknown(self):
        self.assertEqual(client.classify("grant", 201, "not json")["outcome"], "unknown")


class Recording(unittest.TestCase):
    def test_every_operation_is_a_call_then_a_return(self):
        events = run_fake(FakeCouncil(), operations=60)
        calls = [event for event in events if event["event"] == "call"]
        returns = {event["id"]: event for event in events if event["event"] == "return"}
        self.assertGreaterEqual(len(calls), 60)
        self.assertEqual({op for op in ("grant", "renew", "read", "release") if any(c["op"] == op for c in calls)},
                         {"grant", "renew", "read", "release"})
        for call in calls:
            self.assertIn(call["id"], returns)
            self.assertLessEqual(call["mono"], returns[call["id"]]["mono"])
            self.assertIn(call["node"], ("n1", "n2", "n3"))

    def test_a_transport_error_is_recorded_as_unknown(self):
        class Broken:
            nodes = ["n1"]

            def request(self, node, method, path, body=None, timeout=None):
                raise TimeoutError("timed out")

        events = run_fake(Broken(), operations=3, clients=1)
        returns = [event for event in events if event["event"] == "return"]
        self.assertTrue(returns)
        self.assertTrue(all(event["outcome"] == "unknown" for event in returns))
        self.assertIn("timed out", returns[0]["error"])

    def test_a_resumed_recorder_continues_ids_and_time(self):
        with tempfile.TemporaryDirectory() as directory:
            history = Path(directory) / "history.jsonl"
            history.write_text(json.dumps({"event": "call", "id": 41, "mono": 1e12}) + "\n")
            recorder = client.Recorder(history)
            op_id, mono, _ = recorder.call(client=1, op="read", namespace="ns", lease="x", ttl_ms=0, node="n1")
            recorder.close()
            self.assertEqual(op_id, 42)
            self.assertGreater(mono, 1e12)


class EndToEnd(unittest.TestCase):
    def test_an_honest_council_gives_a_linearizable_history(self):
        events = run_fake(FakeCouncil())
        result = lin.check(lin.operations_from_events(events))
        self.assertEqual(result["verdict"], "linearizable")
        self.assertGreater(result["checked"], 300)

    def test_follower_reads_without_asking_the_leader_fail_the_check(self):
        events = run_fake(FakeCouncil(follower_reads=True))
        result = lin.check(lin.operations_from_events(events))
        self.assertEqual(result["verdict"], "violation")
        # The clients run flat out here, so a quiet moment to cut at is rare.
        self.assertLessEqual(len(result["violation"]["slice"]), 20, result["violation"])
        self.assertIn("read", [entry["op"] for entry in result["violation"]["slice"]])


class SoakRecord(unittest.TestCase):
    """The V02 record says how many council operations were checked, and fails on a violation."""

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.evidence = Path(temp.name)
        sustained_check.save_state(self.evidence, {"nodes": ["rb-a-1", "rb-a-2", "rb-a-3"]})
        (self.evidence / "metadata.json").write_text(json.dumps({
            "tier": "custom", "schedule": "compressed", "started_at": 1_790_000_000,
            "finished_at": 1_790_000_060, "council_history": True}))

    def render(self, events=None):
        if events is not None:
            history = self.evidence / "council-history.jsonl"
            history.write_text("".join(json.dumps(event) + "\n" for event in events))
            result = lin.check(lin.load_history(history))
            (self.evidence / "council-history-check.json").write_text(json.dumps(result))
        record = self.evidence / "record.md"
        code = sustained_check.render(self.evidence, record)
        return code, record.read_text()

    def test_a_linearizable_history_passes_and_says_how_many_operations_were_checked(self):
        events = run_fake(FakeCouncil(), operations=80)
        operations = sum(1 for op in lin.operations_from_events(events) if op.kind in lin.KINDS)
        code, text = self.render(events)
        self.assertEqual(code, 0, text)
        self.assertIn("## Council history (linearizability)", text)
        self.assertIn(f"- Operations checked: {operations} of {operations}", text)
        self.assertIn("- Verdict: linearizable", text)

    def test_a_violation_fails_the_record_and_shows_the_failing_slice(self):
        code, text = self.render(run_fake(FakeCouncil(follower_reads=True)))
        self.assertEqual(code, 1)
        self.assertTrue(text.startswith("# Sustained soak (V02): FAIL"))
        self.assertIn("- Verdict: **not linearizable**", text)
        self.assertIn("| Id | Client | Operation | Lease | Call | Return | Outcome | Expiry | Cleaning |", text)

    def test_a_run_that_owed_a_check_and_has_none_fails(self):
        code, text = self.render()
        self.assertEqual(code, 1)
        self.assertIn("- Not checked: the soak recorded a council history but never checked it", text)

    def test_an_inconclusive_check_is_reported_but_does_not_fail(self):
        (self.evidence / "council-history-check.json").write_text(json.dumps({
            "verdict": "inconclusive", "operations": 900, "checked": 600, "partitions": 2,
            "inconclusive": ["rbtest-lin-x-1"], "unanswered_writes": 4}))
        code, text = self.render()
        self.assertEqual(code, 0, text)
        self.assertIn("- Verdict: inconclusive: the search ran out of budget in rbtest-lin-x-1", text)

    def test_a_run_without_a_history_says_nothing_about_one(self):
        (self.evidence / "metadata.json").write_text(json.dumps({"tier": "day", "schedule": "none",
                                                                 "started_at": 1, "finished_at": 2}))
        code, text = self.render()
        self.assertNotIn("Council history", text)


class FastTierPlan(unittest.TestCase):
    def plan(self, *arguments):
        script = Path(__file__).parent / "qualify-sustained.sh"
        return subprocess.run([str(script), *arguments, "--plan"], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              text=True, timeout=60, env=dict(os.environ, RELIABURGER_SOAK_CAFFEINATED="1"))

    def test_the_fast_and_final_tiers_record_and_check_a_council_history(self):
        for tier in ("fast", "final"):
            result = self.plan("--tier", tier)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("plan: council-history 4 clients, checked for linearizability after the soak",
                          result.stdout)


if __name__ == "__main__":
    unittest.main()
