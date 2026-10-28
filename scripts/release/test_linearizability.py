"""The council history checker accepts linearizable histories and finds the rest (#1399)."""
import json
from pathlib import Path
import tempfile
import time
import unittest

import linearizability as lin

NS = "rbtest-lin-0"
TTL = 900_000


class History:
    """Hand-made histories: each op names its call and return in seconds."""

    def __init__(self):
        self.events = []
        self.next_id = 0

    def op(self, client, kind, call, ret, outcome, lease=None, expires=None, cleaning=False, key=NS, ttl=TTL):
        self.next_id += 1
        self.events.append({"event": "call", "id": self.next_id, "client": client, "op": kind, "namespace": key,
                            "lease": None if kind == "grant" else lease, "ttl_ms": ttl, "mono": call,
                            "wall_ms": int(call * 1000)})
        if ret is not None:
            self.events.append({"event": "return", "id": self.next_id, "mono": ret, "wall_ms": int(ret * 1000),
                                "outcome": outcome, "lease": lease, "expires_at_unix_ms": expires,
                                "cleaning": cleaning})
        return self

    def check(self, **options):
        return lin.check(lin.operations_from_events(self.events), **options)


def lifecycle(history, client=1, start=0.0, lease="L1", expires=1_000_000):
    """A grant, a renew, two reads and a release, one after another."""
    history.op(client, "grant", start, start + 1, "ok", lease=lease, expires=expires)
    history.op(client, "read", start + 2, start + 3, "ok", lease=lease, expires=expires)
    history.op(client, "renew", start + 4, start + 5, "ok", lease=lease, expires=expires + 5)
    history.op(client, "read", start + 6, start + 7, "ok", lease=lease, expires=expires + 5)
    history.op(client, "release", start + 8, start + 9, "ok", lease=lease)
    history.op(client, "read", start + 10, start + 11, "not_found", lease=lease)
    return history


class LinearizableHistoriesPass(unittest.TestCase):
    def test_a_sequential_lease_lifecycle_passes(self):
        result = lifecycle(History()).check()
        self.assertEqual(result["verdict"], "linearizable")
        self.assertEqual(result["operations"], 6)
        self.assertEqual(result["checked"], 6)

    def test_two_clients_taking_turns_on_one_namespace_pass(self):
        history = lifecycle(History(), client=1, lease="L1")
        history.op(2, "grant", 3, 4, "owned")
        lifecycle(history, client=2, start=12, lease="L2", expires=2_000_000)
        self.assertEqual(history.check()["verdict"], "linearizable")

    def test_a_read_concurrent_with_a_renew_may_see_either_expiry(self):
        for seen in (100, 200):
            history = History()
            history.op(1, "grant", 0, 1, "ok", lease="L1", expires=100)
            history.op(1, "renew", 2, 6, "ok", lease="L1", expires=200)
            history.op(2, "read", 3, 4, "ok", lease="L1", expires=seen)
            self.assertEqual(history.check()["verdict"], "linearizable", seen)

    def test_concurrent_grants_where_one_wins_pass(self):
        history = History()
        history.op(1, "grant", 0, 3, "ok", lease="L1", expires=100)
        history.op(2, "grant", 1, 2, "owned")
        self.assertEqual(history.check()["verdict"], "linearizable")

    def test_a_timed_out_grant_may_not_have_happened(self):
        history = History()
        history.op(1, "grant", 0, None, "unknown")
        history.op(2, "grant", 5, 6, "ok", lease="L2", expires=100_000)
        self.assertEqual(history.check()["verdict"], "linearizable")

    def test_a_timed_out_grant_may_have_happened(self):
        history = History()
        history.op(1, "grant", 0, None, "unknown")
        history.op(2, "grant", 5, 6, "owned")
        history.op(2, "grant", 7, 8, "owned")
        self.assertEqual(history.check()["verdict"], "linearizable")

    def test_a_timed_out_renew_may_or_may_not_have_moved_the_expiry(self):
        for seen in (1_000_000, TTL + 2_000 + 5):
            history = History()
            history.op(1, "grant", 0, 1, "ok", lease="L1", expires=1_000_000)
            history.op(1, "renew", 2, None, "unknown", lease="L1")
            history.op(1, "read", 5, 6, "ok", lease="L1", expires=seen)
            self.assertEqual(history.check()["verdict"], "linearizable", seen)

    def test_a_lease_past_its_expiry_may_be_reaped(self):
        history = History()
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=10_000)
        history.op(1, "read", 100, 101, "not_found", lease="L1")
        history.op(2, "grant", 102, 103, "ok", lease="L2", expires=500_000)
        self.assertEqual(history.check()["verdict"], "linearizable")

    def test_a_release_still_cleaning_keeps_the_namespace(self):
        history = History()
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=1_000_000)
        history.op(1, "release", 2, 3, "pending", lease="L1")
        history.op(1, "read", 4, 5, "ok", lease="L1", expires=1_000_000, cleaning=True)
        history.op(2, "grant", 6, 7, "owned")
        history.op(1, "release", 8, 9, "ok", lease="L1")
        history.op(2, "grant", 10, 11, "ok", lease="L2", expires=1_000_000)
        self.assertEqual(history.check()["verdict"], "linearizable")

    def test_a_read_during_a_release_may_see_cleanup_already_started(self):
        history = History()
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=1_000_000)
        history.op(1, "release", 2, 6, "ok", lease="L1")
        history.op(2, "read", 3, 4, "ok", lease="L1", expires=1_000_000, cleaning=True)
        history.op(2, "read", 7, 8, "not_found", lease="L1")
        self.assertEqual(history.check()["verdict"], "linearizable")

    def test_a_read_after_a_release_returned_may_not_see_cleanup(self):
        history = History()
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=1_000_000)
        history.op(1, "release", 2, 3, "ok", lease="L1")
        history.op(2, "read", 4, 5, "ok", lease="L1", expires=1_000_000, cleaning=True)
        self.assertEqual(history.check()["verdict"], "violation")

    def test_namespaces_are_checked_apart(self):
        history = lifecycle(History(), client=1, lease="L1")
        lifecycle(history, client=2, lease="M1")
        for event in history.events[len(history.events) // 2:]:
            if event["event"] == "call":
                event["namespace"] = "rbtest-lin-1"
        result = history.check()
        self.assertEqual(result["verdict"], "linearizable")
        self.assertEqual(result["partitions"], 2)


class ViolationsFail(unittest.TestCase):
    def assert_violation(self, history):
        result = history.check()
        self.assertEqual(result["verdict"], "violation", json.dumps(result, indent=1))
        return result

    def test_a_stale_read_fails(self):
        history = History()
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=100)
        history.op(1, "renew", 2, 3, "ok", lease="L1", expires=200)
        history.op(2, "read", 4, 5, "ok", lease="L1", expires=100)
        self.assert_violation(history)

    def test_a_lost_grant_fails(self):
        history = History()
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=1_000_000)
        history.op(2, "read", 2, 3, "not_found", lease="L1")
        self.assert_violation(history)

    def test_a_lost_release_fails(self):
        history = History()
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=1_000_000)
        history.op(1, "release", 2, 3, "ok", lease="L1")
        history.op(2, "read", 4, 5, "ok", lease="L1", expires=1_000_000)
        self.assert_violation(history)

    def test_a_lost_renew_fails(self):
        history = History()
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=100)
        history.op(1, "renew", 2, 3, "ok", lease="L1", expires=200)
        history.op(1, "read", 4, 5, "ok", lease="L1", expires=200)
        history.op(2, "read", 6, 7, "ok", lease="L1", expires=100)
        self.assert_violation(history)

    def test_a_lease_held_by_two_clients_at_once_fails(self):
        history = History()
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=1_000_000)
        history.op(2, "grant", 2, 3, "ok", lease="L2", expires=1_000_000)
        self.assert_violation(history)

    def test_overlapping_grants_that_both_win_fail(self):
        history = History()
        history.op(1, "grant", 0, 3, "ok", lease="L1", expires=1_000_000)
        history.op(2, "grant", 1, 2, "ok", lease="L2", expires=1_000_000)
        self.assert_violation(history)

    def test_a_refused_grant_with_no_holder_fails(self):
        history = lifecycle(History())
        history.op(2, "grant", 20, 21, "owned")
        self.assert_violation(history)

    def test_the_record_carries_the_smallest_failing_slice(self):
        history = History()
        for round_number in range(30):
            lifecycle(history, client=1 + round_number % 2, start=round_number * 20.0,
                      lease=f"L{round_number}", expires=1_000_000 + round_number)
        history.op(1, "grant", 700, 701, "ok", lease="Z", expires=5_000_000)
        history.op(1, "renew", 702, 703, "ok", lease="Z", expires=5_000_100)
        history.op(2, "read", 704, 705, "ok", lease="Z", expires=5_000_000)
        for round_number in range(40, 50):
            history.op(1, "read", round_number * 20.0, round_number * 20.0 + 1, "ok", lease="Z", expires=5_000_100)
        result = self.assert_violation(history)
        violation = result["violation"]
        self.assertEqual(violation["namespace"], NS)
        ids = [entry["id"] for entry in violation["slice"]]
        self.assertLessEqual(len(ids), 3, violation["slice"])
        kinds = [entry["op"] for entry in violation["slice"]]
        self.assertIn("renew", kinds)
        self.assertEqual(kinds[-1], "read")

    def test_a_violation_in_one_namespace_fails_the_whole_history(self):
        history = lifecycle(History(), client=1, lease="L1")
        for event in history.events:
            event["namespace"] = "rbtest-lin-1"
        history.op(1, "grant", 0, 1, "ok", lease="Z", expires=100)
        history.op(1, "renew", 2, 3, "ok", lease="Z", expires=200)
        history.op(2, "read", 4, 5, "ok", lease="Z", expires=100)
        result = history.check()
        self.assertEqual(result["verdict"], "violation")
        self.assertEqual(result["violation"]["namespace"], NS)


class BoundedSearch(unittest.TestCase):
    def test_a_search_over_its_budget_is_inconclusive_not_a_hang(self):
        history = History()
        # Many timed-out renews and concurrent reads: a wide search.
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=1_000_000)
        for index in range(12):
            history.op(10 + index, "renew", 2 + index * 0.01, None, "unknown", lease="L1")
        for index in range(12):
            history.op(30 + index, "read", 3, 100, "ok", lease="L1", expires=1_000_000)
        history.op(1, "grant", 200, 201, "ok", lease="L2", expires=1_000_000)
        started = time.monotonic()
        result = history.check(budget=50)
        self.assertEqual(result["verdict"], "inconclusive")
        self.assertLess(time.monotonic() - started, 10)
        self.assertEqual(result["inconclusive"], [NS])


class Events(unittest.TestCase):
    def test_an_operation_with_no_return_is_unknown_and_never_returns(self):
        history = History()
        history.op(1, "renew", 1, None, "unknown", lease="L1")
        [operation] = lin.operations_from_events(history.events)
        self.assertEqual(operation.outcome, "unknown")
        self.assertEqual(operation.ret, lin.INF)

    def test_a_read_without_an_answer_says_nothing_and_is_dropped(self):
        history = History()
        history.op(1, "read", 1, 2, "unknown", lease="L1")
        history.op(1, "read", 3, None, "unknown", lease="L1")
        self.assertEqual(lin.operations_from_events(history.events), [])

    def test_a_refused_write_changed_nothing_and_is_dropped(self):
        history = History()
        history.op(1, "grant", 1, 2, "refused")
        self.assertEqual(lin.operations_from_events(history.events), [])

    def test_the_cli_writes_a_result_and_exits_one_on_a_violation(self):
        history = History()
        history.op(1, "grant", 0, 1, "ok", lease="L1", expires=1_000_000)
        history.op(2, "read", 2, 3, "not_found", lease="L1")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "history.jsonl"
            path.write_text("".join(json.dumps(event) + "\n" for event in history.events))
            out = Path(directory) / "result.json"
            self.assertEqual(lin.main(["check", str(path), "--out", str(out)]), 1)
            result = json.loads(out.read_text())
            self.assertEqual(result["verdict"], "violation")
            self.assertEqual(result["operations"], 2)

    def test_a_torn_last_line_from_a_killed_client_is_ignored(self):
        history = lifecycle(History())
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "history.jsonl"
            path.write_text("".join(json.dumps(event) + "\n" for event in history.events) + '{"event": "ca')
            self.assertEqual(lin.check(lin.load_history(path))["verdict"], "linearizable")


if __name__ == "__main__":
    unittest.main()
