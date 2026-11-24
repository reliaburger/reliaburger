"""The power-cut endurance harness decides, checks and counts the way the plan says.

`qualify-power-cut-endurance.sh` drives a real VM, so only a real run shows
what a cut does to a cluster. These tests cover everything around that: the
seeded schedule, each invariant check on good and bad evidence, the summary's
arithmetic and rule-of-three bounds, the refusals before anything is
installed, the quiet-host check, and the power and settle functions it shares
with the soak (scripts/release/sustained/power.sh).
"""
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

HERE = Path(__file__).resolve().parent
HARNESS = HERE / "qualify-power-cut-endurance.sh"
POWER = HERE / "sustained" / "power.sh"
sys.path.insert(0, str(HERE / "power_cut"))

import endurance  # noqa: E402
import quiet  # noqa: E402


class Schedule(unittest.TestCase):
    def test_a_seed_gives_the_same_holds_every_time(self):
        first = [endurance.hold(7, cut, "work", 120, 600) for cut in range(1, 20)]
        again = [endurance.hold(7, cut, "work", 120, 600) for cut in range(1, 20)]
        self.assertEqual(first, again)
        self.assertTrue(all(120 <= value <= 600 for value in first))

    def test_different_choices_for_the_same_cut_are_independent(self):
        works = [endurance.hold(7, cut, "work", 0, 10 ** 6) for cut in range(1, 10)]
        offs = [endurance.hold(7, cut, "off", 0, 10 ** 6) for cut in range(1, 10)]
        self.assertNotEqual(works, offs)

    def test_another_seed_gives_another_schedule(self):
        self.assertNotEqual([endurance.hold(1, c, "work", 0, 10 ** 6) for c in range(1, 10)],
                            [endurance.hold(2, c, "work", 0, 10 ** 6) for c in range(1, 10)])

    def test_the_plan_fits_the_duration(self):
        cuts = endurance.plan(3, 3600, 120, 600)
        self.assertGreater(len(cuts), 3)
        self.assertLess(len(cuts), 30)
        self.assertEqual([cut for cut, _, _ in cuts], list(range(1, len(cuts) + 1)))
        self.assertEqual(endurance.plan(3, 60, 120, 600), [])


class Acked(unittest.TestCase):
    def test_takes_the_highest_ack_wherever_it_sits_in_a_line(self):
        log = "node-1 soak-writer RESUME 4 after 4 lines\n2026-11-18T10:00:00Z ACK 5\nACK 7\nACK 6\n"
        self.assertEqual(endurance.acked("writer", log), 7)

    def test_redis_reads_incr_and_ignores_errors(self):
        self.assertEqual(endurance.acked("redis", "INCR 41\nERR Could not connect\nINCR 42\n"), 42)

    def test_nothing_acknowledged_is_zero(self):
        self.assertEqual(endurance.acked("writer", "RESUME 0 after 0 lines\n"), 0)
        self.assertEqual(endurance.acked("writer", "BACK 9\nACK x\n"), 0)

    def test_zero_or_a_value_below_the_last_one_read_is_a_harness_error(self):
        # A log that couldn't be read, or held no ACK, would make I1 and I2
        # hold for anything while the cut still counted towards the bound.
        self.assertIn("no ACK", endurance.acked_problem("writer", 0, None))
        self.assertIn("no INCR", endurance.acked_problem("redis", 0, 40))
        self.assertEqual(endurance.acked_problem("writer", 49, 50), "below the last ACK read (49 < 50)")
        self.assertEqual(endurance.acked_problem("redis", 3, 8), "below the last INCR read (3 < 8)")
        self.assertIsNone(endurance.acked_problem("writer", 50, 50))
        self.assertIsNone(endurance.acked_problem("redis", 7, None))


class SeqCheck(unittest.TestCase):
    def test_a_whole_sequence_past_the_ack_passes(self):
        result = endurance.seq_check("MAX 120\nLAST 120\n", 100)
        self.assertTrue(result["pass"])
        self.assertEqual(result["max"], 120)

    def test_a_file_that_ends_below_the_ack_lost_acknowledged_writes(self):
        result = endurance.seq_check("MAX 90\nLAST 90\n", 100)
        self.assertFalse(result["pass"])
        self.assertIn("below the acknowledged 100", result["reason"])

    def test_gaps_duplicates_and_bad_lines_fail(self):
        for problem in ("MISSING 2 first 50", "DUP 1 first 7", "BAD 12: 13"):
            result = endurance.seq_check(f"{problem}\nMAX 120\nLAST 119\n", 100)
            self.assertFalse(result["pass"], problem)
            self.assertIn(problem.split()[0], result["reason"])

    def test_no_summary_means_the_writer_could_not_be_read(self):
        result = endurance.seq_check("error: no running instance of soak-writer\n", 1)
        self.assertFalse(result["pass"])
        self.assertIn("wasn't reachable", result["reason"])


class ResumeCheck(unittest.TestCase):
    """After a cut, the writer and the Redis client each record where they resumed.

    Checking the file or the counter alone isn't enough: by the time the
    cluster has settled the writer has appended hundreds more lines, so a
    file that lost acknowledged writes and carried on from an earlier number
    is whole again, 1..N with N past the ACK. Where it resumed is the proof.
    """

    def test_resuming_at_or_past_the_ack_after_a_new_start_passes(self):
        result = endurance.resume_check("0\n120\n", 118, 1)
        self.assertTrue(result["pass"], result)
        self.assertEqual((result["resumed_at"], result["starts"]), (120, 2))

    def test_resuming_below_the_ack_lost_acknowledged_writes(self):
        # The case the file check can't see: the file lost 101..118, then grew past them again.
        result = endurance.resume_check("0\n100\n", 118, 1)
        self.assertFalse(result["pass"])
        self.assertIn("resumed at 100, below the acknowledged 118", result["reason"])

    def test_no_new_start_since_the_last_cut_means_nothing_was_checked(self):
        result = endurance.resume_check("0\n120\n", 118, 2)
        self.assertFalse(result["pass"])
        self.assertIn("no new start", result["reason"])

    def test_an_unreadable_record_fails(self):
        self.assertFalse(endurance.resume_check("cat: /data/resumes: No such file or directory\n", 1, 0)["pass"])

    def test_every_start_since_the_last_check_must_be_at_or_past_the_ack(self):
        # The cut's own start lost writes (90 < 100); a second start during
        # the settle, at 400, must not hide it.
        result = endurance.resume_check("0\n90\n400\n", 100, 1)
        self.assertFalse(result["pass"])
        self.assertIn("start 2 resumed at 90, below the acknowledged 100", result["reason"])
        self.assertEqual(result["new_starts"], [90, 400])

    def test_several_new_starts_all_past_the_ack_pass(self):
        result = endurance.resume_check("0\n120\n400\n", 100, 1)
        self.assertTrue(result["pass"], result)
        self.assertEqual(result["new_starts"], [120, 400])

    def test_starts_before_the_last_check_are_not_judged_again(self):
        # A start below this cut's ACK that came before the previous check
        # belongs to an earlier cut, which judged it then.
        self.assertTrue(endurance.resume_check("0\n50\n120\n", 100, 2)["pass"])

    def test_the_baseline_before_the_first_cut_has_no_previous(self):
        result = endurance.resume_check("0\n", 0, None)
        self.assertTrue(result["pass"], result)
        self.assertEqual(result["starts"], 1)

    # A start between the cuts (run8 cut 63: needrestart restarted Bun after
    # an unattended libc upgrade) resumes past the previous cut's ACK, not
    # this one's, which it came before.

    def test_a_start_before_the_cut_is_held_to_the_previous_cuts_ack(self):
        result = endurance.resume_check("0\n100\n150\n230\n", 200, 2, before_cut=3, previous_acked=120)
        self.assertTrue(result["pass"], result)
        self.assertEqual((result["between_cuts"], result["new_starts"]), ([150], [230]))

    def test_a_start_before_the_cut_below_the_previous_ack_fails(self):
        result = endurance.resume_check("0\n100\n110\n230\n", 200, 2, before_cut=3, previous_acked=120)
        self.assertFalse(result["pass"])
        self.assertIn("start 3, before the cut, resumed at 110, below the previous cut's acknowledged 120",
                      result["reason"])

    def test_a_start_after_the_cut_is_still_held_to_this_cuts_ack(self):
        result = endurance.resume_check("0\n100\n150\n190\n", 200, 2, before_cut=3, previous_acked=120)
        self.assertFalse(result["pass"])
        self.assertIn("start 4 resumed at 190, below the acknowledged 200", result["reason"])

    def test_only_a_start_before_the_cut_is_no_new_start(self):
        result = endurance.resume_check("0\n100\n150\n", 200, 2, before_cut=3, previous_acked=120)
        self.assertFalse(result["pass"])
        self.assertIn("no new start since the cut", result["reason"])

    def test_without_a_count_before_the_cut_every_new_start_is_held_to_this_ack(self):
        result = endurance.resume_check("0\n100\n150\n230\n", 200, 2)
        self.assertFalse(result["pass"])
        self.assertIn("start 3 resumed at 150", result["reason"])


class CounterCheck(unittest.TestCase):
    def test_at_or_above_the_ack_and_the_last_value_passes(self):
        self.assertTrue(endurance.counter_check("57\n", 50, 40)["pass"])
        self.assertTrue(endurance.counter_check("50", 50, None)["pass"])

    def test_below_the_ack_fails(self):
        self.assertIn("below the acknowledged", endurance.counter_check("49\n", 50, None)["reason"])

    def test_going_backwards_fails(self):
        self.assertIn("went back", endurance.counter_check("60\n", 50, 70)["reason"])

    def test_an_error_instead_of_a_number_fails(self):
        result = endurance.counter_check("Could not connect to Redis\n", 1, None)
        self.assertFalse(result["pass"])


class JournalCheck(unittest.TestCase):
    CLEAN = textwrap.dedent("""\
        2026-11-18T10:00:01+00:00 node-1 systemd[1]: Started reliaburger.service - Reliaburger node agent.
        2026-11-18T10:00:02+00:00 node-1 bun[612]: council: opened the Raft log at index 4123
        """)

    def test_one_start_and_nothing_wrong_passes(self):
        self.assertTrue(endurance.journal_check(self.CLEAN)["pass"])

    def test_a_restart_fails(self):
        text = self.CLEAN + ("2026-11-18T10:00:09+00:00 node-1 systemd[1]: reliaburger.service: Main process exited, "
                             "code=killed, status=6/ABRT\n"
                             "2026-11-18T10:00:10+00:00 node-1 systemd[1]: Started reliaburger.service - x.\n")
        result = endurance.journal_check(text)
        self.assertFalse(result["pass"])
        self.assertEqual(result["starts"], 2)

    def test_a_panic_or_a_store_error_fails(self):
        for line in ("thread 'main' panicked at src/council/durable_log.rs:88:9",
                     "council: the Raft store is corrupted: invalid page checksum",
                     "Error: failed to open metrics store: DB corrupted: invalid root page",
                     "bun: failed to open layer blob: No such file or directory",
                     "raft: Storage(StorageError { kind: Io })",
                     "council: Database repair aborted.",
                     "task ledger: ledger block at byte 8192 is corrupt and isn't the last block"):
            result = endurance.journal_check(self.CLEAN + f"2026-11-18T10:00:03+00:00 node-1 bun[612]: {line}\n")
            self.assertFalse(result["pass"], line)
            self.assertIn("panic" if "panicked" in line else "store", result["problems"], line)

    def test_a_real_recovery_journal_passes(self):
        # Bun 0.2.8's journal after the first smoke run's first cut (17
        # November 2026), trimmed: slow turns and deploy retries while startup
        # cleanup runs are normal after a cut, not store errors.
        text = textwrap.dedent("""\
            2026-11-17T21:06:38+00:00 lima-rb-1 systemd[1]: Starting reliaburger.service - Reliaburger node...
            2026-11-17T21:06:38+00:00 lima-rb-1 systemd[1]: Started reliaburger.service - Reliaburger node.
            2026-11-17T21:06:39+00:00 lima-rb-1 bun[673]: bun: reliaburger node agent v0.2.8 (e423d46)
            2026-11-17T21:06:43+00:00 lima-rb-1 bun[673]: council: no peer advertises a newer recovery epoch than 0; serving raft
            2026-11-17T21:06:51+00:00 lima-rb-1 bun[673]: report worker: snapshot collection failed or timed out, and no recent observation to re-send
            2026-11-17T21:06:57+00:00 lima-rb-1 bun[673]: orchestrator: deploy of soak-redis/default failed (attempt 1), retrying in 5s: startup cleanup still owns runtime allocations; retry after recovery
            2026-11-17T21:06:59+00:00 lima-rb-1 bun[673]: bun: a discovery journal write took 559 ms; the agent loop went on meanwhile
            2026-11-17T21:07:00+00:00 lima-rb-1 bun[673]: scheduler: cannot place every replica of default/hello: placed 0 of 1 replicas: no node is ready
            """)
        self.assertTrue(endurance.journal_check(text)["pass"])

    def test_failures_that_name_no_store_are_not_store_errors(self):
        # Lines 0.2.8's Bun can print that a looser pattern would catch.
        for line in ("lettuce: repaired drift with 2 changes",
                     "onion: failed to open cgroup /sys/fs/cgroup/rb/x: No such file or directory",
                     "grill: failed to open stdout log: Permission denied",
                     "council: opened the Raft log at index 4123"):
            result = endurance.journal_check(self.CLEAN + f"2026-11-18T10:00:03+00:00 node-1 bun[612]: {line}\n")
            self.assertTrue(result["pass"], line)

    def test_no_start_since_boot_fails(self):
        self.assertIn("0 times", endurance.journal_check("-- No entries --\n")["reason"])


class InstancesCheck(unittest.TestCase):
    WANT = {"soak-writer": 1, "soak-redis": 1}

    @staticmethod
    def instance(app, state="running"):
        return {"app_name": app, "state": state, "node": "node-1", "id": f"{app}-0"}

    def test_exactly_the_desired_count_running_passes(self):
        result = endurance.instances_check([self.instance("soak-writer"), self.instance("soak-redis"),
                                            self.instance("soak-redis", "stopped")], self.WANT)
        self.assertTrue(result["pass"])

    def test_a_duplicate_fails(self):
        result = endurance.instances_check([self.instance("soak-writer"), self.instance("soak-writer"),
                                            self.instance("soak-redis")], self.WANT)
        self.assertIn("soak-writer: 2 running", result["reason"])

    def test_a_missing_or_pending_app_fails(self):
        result = endurance.instances_check([self.instance("soak-writer"), self.instance("soak-redis", "pending")],
                                           self.WANT)
        self.assertFalse(result["pass"])
        self.assertIn("soak-redis: 0 running", result["reason"])
        self.assertIn("also pending", result["reason"])


class Judging(unittest.TestCase):
    PASS = {"pass": True}

    def test_a_cut_with_every_check_passing_has_no_violations(self):
        line = endurance.judge(3, {"recovery": {"api_seconds": 40, "settled_seconds": 70}},
                               {"I1": self.PASS, "I2": self.PASS, "I4": self.PASS, "I7": self.PASS})
        self.assertEqual(line["violations"], [])
        self.assertTrue(line["invariants"]["I6"]["pass"])

    def test_slow_recovery_is_an_i6_violation(self):
        for recovery in ({"api_seconds": 121, "settled_seconds": 150}, {"api_seconds": 30, "settled_seconds": 301},
                         {"api_seconds": None, "settled_seconds": None}):
            line = endurance.judge(1, {"recovery": recovery}, {"I1": self.PASS})
            self.assertEqual(line["violations"], ["I6"], recovery)

    def test_each_failed_check_is_named(self):
        line = endurance.judge(1, {"recovery": {"api_seconds": 1, "settled_seconds": 1}},
                               {"I1": {"pass": False}, "I7": {"pass": False}, "I2": self.PASS})
        self.assertEqual(line["violations"], ["I1", "I7"])


class Summary(unittest.TestCase):
    RUN = {"release": "0.2.8", "seed": 5, "tier": "quick", "host": "macOS", "started": "s", "ended": "e",
           "paused_seconds": 0}

    def run_with(self, cuts, **extra):
        return {**self.RUN, "cuts_started": cuts, **extra}

    @staticmethod
    def line(cut, violations=(), api=40, settled=60, retries=0):
        return {"cut": cut, "topology": "1", "kind": "site", "violations": list(violations),
                "recovery": {"api_seconds": api, "settled_seconds": settled, "start_retries": retries}}

    def test_no_violations_in_n_cuts_bounds_the_rate_by_the_rule_of_three(self):
        summary = endurance.summarise(self.run_with(12), [self.line(n) for n in range(1, 13)])
        topology = summary["topologies"]["1"]
        self.assertEqual(topology["cuts"], 12)
        self.assertAlmostEqual(topology["bound95"], 1 - 0.05 ** (1 / 12))
        self.assertEqual(summary["verdict"], "pass")
        self.assertEqual(summary["pooled"]["cuts"], 12)
        self.assertEqual(summary["schema"], "reliaburger.power-cut-endurance/1")

    def test_the_bound_matches_the_published_table(self):
        self.assertAlmostEqual(endurance.bound95(25), 0.1129, places=4)
        self.assertAlmostEqual(endurance.bound95(1000), 0.002991, places=5)
        self.assertIsNone(endurance.bound95(0))

    def test_a_violation_fails_the_run_and_drops_the_bound(self):
        summary = endurance.summarise(self.run_with(2), [self.line(1), self.line(2, ["I1"])])
        self.assertEqual(summary["verdict"], "fail")
        self.assertIsNone(summary["topologies"]["1"]["bound95"])
        self.assertEqual(summary["topologies"]["1"]["violated_cuts"], [{"cut": 2, "violations": ["I1"]}])

    def test_recovery_percentiles_and_retries(self):
        lines = [self.line(n, settled=s, retries=r) for n, (s, r) in enumerate([(10, 0), (20, 1), (30, 0), (100, 1)], 1)]
        summary = endurance.summarise(self.run_with(4), lines)
        self.assertEqual(summary["topologies"]["1"]["recovery_seconds"], {"p50": 20, "p95": 100, "max": 100})
        self.assertEqual(summary["environment_events"]["vz_start_retries"], 2)

    def test_no_cuts_is_not_a_pass(self):
        self.assertEqual(endurance.summarise(self.run_with(0), [])["verdict"], "no cuts")

    def test_a_cut_made_but_not_recorded_aborts_the_run(self):
        # Three cuts made, two lines: the third's record never reached cuts.jsonl.
        summary = endurance.summarise(self.run_with(3), [self.line(1), self.line(2)])
        self.assertEqual(summary["verdict"], "aborted")
        self.assertIn("3 cuts made, 2 recorded", summary["aborted"])
        self.assertIsNone(summary["pooled"]["bound95"])

    def test_a_run_that_stopped_early_is_aborted_not_passed(self):
        summary = endurance.summarise(self.run_with(2, aborted="could not read the last ACK"),
                                      [self.line(1), self.line(2)])
        self.assertEqual(summary["verdict"], "aborted")
        self.assertEqual(summary["aborted"], "could not read the last ACK")

    def test_a_violation_still_fails_an_aborted_run(self):
        summary = endurance.summarise(self.run_with(2, aborted="x"), [self.line(1), self.line(2, ["I4"])])
        self.assertEqual(summary["verdict"], "fail")


class RecordAndSummaryCommands(unittest.TestCase):
    def run_endurance(self, *arguments):
        return subprocess.run([sys.executable, str(HERE / "power_cut" / "endurance.py"), *arguments],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def test_record_reads_each_checks_file_and_appends_a_line(self):
        with tempfile.TemporaryDirectory() as evidence:
            cut = Path(evidence, "cuts", "00001")
            cut.mkdir(parents=True)
            for name in ("I1", "I2", "I4"):
                (cut / f"{name}.json").write_text(json.dumps({"pass": True}))
            detail = json.dumps({"topology": "1", "kind": "site",
                                 "recovery": {"api_seconds": 30, "settled_seconds": 50, "start_retries": 0}})
            result = self.run_endurance("record", "--evidence", evidence, "--cut", "1", "--detail", detail)
            self.assertEqual(result.returncode, 1, "I7 was never checked, so the cut must fail")
            line = json.loads(Path(evidence, "cuts.jsonl").read_text())
            self.assertEqual(line["violations"], ["I7"])
            self.assertEqual(line["invariants"]["I7"]["reason"], "not checked")
            Path(evidence, "run.json").write_text(json.dumps({"seed": 1, "cuts_started": 1}))
            result = self.run_endurance("summary", "--evidence", evidence)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads(Path(evidence, "summary.json").read_text())["verdict"], "fail")

    def test_record_turns_unreadable_inputs_into_failures_and_still_appends(self):
        with tempfile.TemporaryDirectory() as evidence:
            cut = Path(evidence, "cuts", "00001")
            cut.mkdir(parents=True)
            for name in ("I1", "I2", "I4", "I7"):
                (cut / f"{name}.json").write_text(json.dumps({"pass": True}))
            (cut / "I2.json").write_text("")
            result = self.run_endurance("record", "--evidence", evidence, "--cut", "1", "--detail", "")
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertNotIn("Traceback", result.stderr)
            line = json.loads(Path(evidence, "cuts.jsonl").read_text())
            self.assertEqual(line["violations"], ["I2", "I6"])
            self.assertIn("unreadable", line["invariants"]["I2"]["reason"])
            self.assertIn("detail_error", line)
            Path(evidence, "run.json").write_text(json.dumps({"seed": 1, "cuts_started": 1}))
            result = self.run_endurance("summary", "--evidence", evidence)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads(Path(evidence, "summary.json").read_text())["verdict"], "fail")

    def test_summary_of_a_torn_cuts_file_is_aborted(self):
        with tempfile.TemporaryDirectory() as evidence:
            Path(evidence, "run.json").write_text(json.dumps({"seed": 1, "cuts_started": 1}))
            Path(evidence, "cuts.jsonl").write_text('{"cut": 1, "violations": []')
            result = self.run_endurance("summary", "--evidence", evidence)
            self.assertEqual(result.returncode, 1, result.stderr)
            summary = json.loads(Path(evidence, "summary.json").read_text())
            self.assertEqual(summary["verdict"], "aborted")
            self.assertIn("unreadable", summary["aborted"])

    def test_acked_exits_non_zero_on_nothing_or_a_regression(self):
        with tempfile.NamedTemporaryFile("w") as file:
            file.write("error: no running instance of soak-writer\n")
            file.flush()
            result = self.run_endurance("acked", "writer", file.name)
            self.assertEqual((result.returncode, result.stdout.strip()), (1, "0"))
            file.write("ACK 40\n")
            file.flush()
            self.assertEqual(self.run_endurance("acked", "writer", file.name).returncode, 0)
            self.assertEqual(self.run_endurance("acked", "writer", file.name, "--previous", "41").returncode, 1)

    def test_an_invariant_command_exits_non_zero_on_failure(self):
        with tempfile.NamedTemporaryFile("w") as file:
            file.write("MAX 3\nLAST 3\n")
            file.flush()
            self.assertEqual(self.run_endurance("seq", file.name, "--acked", "3").returncode, 0)
            self.assertEqual(self.run_endurance("seq", file.name, "--acked", "4").returncode, 1)

    def test_a_whole_file_still_fails_when_the_writer_resumed_below_the_ack(self):
        with tempfile.TemporaryDirectory() as directory:
            seq, resumes = Path(directory, "seq.txt"), Path(directory, "resumes.txt")
            seq.write_text("MAX 500\nLAST 500\n")
            resumes.write_text("0\n90\n")
            result = self.run_endurance("seq", str(seq), "--acked", "100", "--resumes", str(resumes), "--starts", "1")
            self.assertEqual(result.returncode, 1)
            self.assertIn("resumed at 90", json.loads(result.stdout)["reason"])
            resumes.write_text("0\n120\n")
            result = self.run_endurance("seq", str(seq), "--acked", "100", "--resumes", str(resumes), "--starts", "1")
            self.assertEqual(result.returncode, 0, result.stdout)


class Ports(unittest.TestCase):
    """The cluster's host ports stay clear of every port the host hands out or tests take.

    Run 6 lost cut 77 to a cargo test binary holding the API port
    (127.0.0.1:29277): the harness's old ports sat inside TEST_PORTS
    (27000..30000, tests/support/bun_process.rs), and the in-process suites
    hard-code 15000-26999 and 30000-30999. Ephemeral ports are out too: Linux
    hands them out from 32768, macOS from 49152, so 50000 and up would race
    every outgoing connection on a Mac.
    """

    def test_every_offset_keeps_the_ports_in_the_documented_block(self):
        for offset in range(0, endurance.MAX_PORT_OFFSET + 1):
            ports = endurance.ports(offset)
            for port in ports.values():
                self.assertGreaterEqual(port, 31000, (offset, ports))
                self.assertLess(port, 32768, (offset, ports))
            self.assertEqual(len(set(ports.values())), 3)

    def test_no_offset_gives_one_runs_port_to_another_kind(self):
        seen = {}
        for offset in range(0, endurance.MAX_PORT_OFFSET + 1):
            for kind, port in endurance.ports(offset).items():
                self.assertEqual(seen.setdefault(port, kind), kind, (offset, kind, port))

    def test_an_offset_past_the_block_is_refused(self):
        with self.assertRaises(ValueError):
            endurance.ports(endurance.MAX_PORT_OFFSET + 1)

    def test_the_harness_takes_its_ports_from_the_checker(self):
        result = subprocess.run([sys.executable, str(HERE / "power_cut" / "endurance.py"), "ports", "--offset", "120"],
                                capture_output=True, text=True)
        self.assertEqual(result.stdout.split(), ["31120", "31720", "32320"], result.stderr)


class Rejudge(unittest.TestCase):
    """`rejudge` runs today's checker over a run's recorded cuts, offline.

    Old evidence has no start counts from before each cut and no record of a
    start that came after verify read the list, so rejudge infers the one
    and looks for the other in the next cut's record, and says so.
    """

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.evidence = Path(directory.name)
        self.lines = []

    def cut(self, number, acked, writer, redis, previous_starts, **detail):
        cut = self.evidence / "cuts" / f"{number:05d}"
        cut.mkdir(parents=True)
        (cut / "seq.txt").write_text(f"MAX {acked + 50}\nLAST {acked + 50}\n")
        (cut / "counter.txt").write_text(f"{acked + 5}\n")
        (cut / "resumes-writer.txt").write_text("".join(f"{value}\n" for value in writer))
        (cut / "resumes-redis.txt").write_text("".join(f"{value}\n" for value in redis))
        passed = {"pass": True}
        resume = {"previous_starts": previous_starts}
        self.lines.append({
            "cut": number, "topology": "1", "kind": "site", "acked": {"writer": acked, "redis": acked},
            "recovery": {"api_seconds": 10, "settled_seconds": 30, "start_retries": 0}, **detail,
            "invariants": {"I1": {"pass": False, "resume": resume}, "I2": {"pass": False, "previous": None,
                                                                          "resume": resume},
                           "I4": passed, "I6": passed, "I7": passed},
            "violations": ["I1", "I2"]})

    def rejudge(self):
        (self.evidence / "cuts.jsonl").write_text("".join(json.dumps(line) + "\n" for line in self.lines))
        return endurance.rejudge(str(self.evidence))

    def test_a_restart_between_the_cuts_is_no_violation(self):
        # Run 8 cut 63, without the counts the harness now records.
        self.cut(1, 120, [0, 130], [0, 130], 1)
        self.cut(2, 200, [0, 130, 150, 230], [0, 130, 150, 230], 2)
        report = self.rejudge()
        self.assertEqual(report["verdict"], "pass", report)
        cut = report["cuts"][1]
        self.assertEqual((cut["was"], cut["now"]), (["I1", "I2"], []))
        self.assertTrue(cut["inferred_before_cut"])
        self.assertEqual(report["restarts_between_cuts"], [2])

    def test_a_late_start_is_found_in_the_next_cuts_record(self):
        # Run 10 cut 13: verify read soak:starts before the client pushed.
        self.cut(1, 120, [0, 130], [0, 130], 1)
        self.cut(2, 200, [0, 130, 230], [0, 130], 2)
        self.cut(3, 300, [0, 130, 230, 310], [0, 130, 205, 310], 3, starts_before_cut={"writer": 3, "redis": 3})
        report = self.rejudge()
        self.assertEqual(report["verdict"], "pass", report)
        self.assertEqual(report["cuts"][1]["late_start_from"], {"I2": 3})

    def test_a_late_start_with_no_later_record_is_undecided(self):
        # Run 12 cut 4: the run stopped at the violation, so nothing later shows the start.
        self.cut(1, 120, [0, 130], [0, 130], 1)
        self.cut(2, 200, [0, 130, 230], [0, 130], 2)
        report = self.rejudge()
        self.assertEqual(report["verdict"], "undecided", report)
        self.assertEqual(report["cuts"][1]["undecided"], ["I2"])

    def test_a_real_loss_is_still_a_violation(self):
        self.cut(1, 120, [0, 130], [0, 130], 1)
        self.cut(2, 200, [0, 130, 190], [0, 130, 230], 2)
        report = self.rejudge()
        self.assertEqual(report["verdict"], "fail", report)
        self.assertEqual(report["cuts"][1]["now"], ["I1"])
        self.assertIn("resumed at 190", report["cuts"][1]["reasons"]["I1"])

    def test_rejudge_writes_nothing_into_the_evidence(self):
        self.cut(1, 120, [0, 130], [0, 130], 1)
        (self.evidence / "cuts.jsonl").write_text(json.dumps(self.lines[0]) + "\n")
        before = sorted(str(path) for path in self.evidence.rglob("*"))
        result = subprocess.run([sys.executable, str(HERE / "power_cut" / "endurance.py"), "rejudge",
                                 "--evidence", str(self.evidence)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["verdict"], "pass")
        self.assertEqual(sorted(str(path) for path in self.evidence.rglob("*")), before)


class Quiet(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)
        self.lock = self.path / "quiet.lock"
        self.chains = str(self.path / "gate-*" / "chain.log")

    def chain(self, name, text, age=0):
        directory = self.path / name
        directory.mkdir()
        log = directory / "chain.log"
        log.write_text(text)
        stamp = time.time() - age
        os.utime(log, (stamp, stamp))

    def test_a_quiet_host_is_free(self):
        self.assertIsNone(quiet.busy_reason(str(self.lock), self.chains, 172800))

    def test_a_held_lock_means_busy(self):
        with open(self.lock, "w") as holder:
            fcntl.flock(holder, fcntl.LOCK_EX)
            self.assertIn("quiet lock", quiet.busy_reason(str(self.lock), self.chains, 172800))
        self.assertIsNone(quiet.busy_reason(str(self.lock), self.chains, 172800))

    def test_a_recent_unfinished_chain_means_busy(self):
        self.chain("gate-030-data", "2026-11-19T14:00:00Z start v02-fast\n")
        self.assertIn("gate-030-data", quiet.busy_reason(str(self.lock), self.chains, 172800))

    def test_a_finished_or_old_chain_is_history(self):
        self.chain("gate-030-rehearsal", "start a\nend a exit=0\n2026-11-16T11:16:45Z chain done\n")
        self.chain("gate-028-data", "2026-11-11T00:00:00Z start v02-final\n", age=5 * 86400)
        self.assertIsNone(quiet.busy_reason(str(self.lock), self.chains, 172800))


class HarnessRefusals(unittest.TestCase):
    """The harness refuses bad arguments before it installs or starts anything."""

    def run_harness(self, *arguments, env=None):
        environment = {**os.environ, "RELIABURGER_QUIET_LOCK": "/nonexistent/quiet.lock",
                       "RELIABURGER_GATE_CHAINS": "/nonexistent/gate-*/chain.log", **(env or {})}
        return subprocess.run(["bash", str(HARNESS), *arguments], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              text=True, env=environment, timeout=60)

    def assert_refused(self, result, message):
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(message, result.stderr)

    def test_needs_a_release_or_a_candidate(self):
        with tempfile.TemporaryDirectory() as evidence:
            self.assert_refused(self.run_harness("--tier", "quick", "--evidence", evidence),
                                "give --release or --base-url")

    def test_refuses_both_sources_a_bad_version_and_an_undigested_candidate(self):
        with tempfile.TemporaryDirectory() as evidence:
            self.assert_refused(self.run_harness("--tier", "quick", "--evidence", evidence, "--release", "0.2.8",
                                                 "--base-url", "https://example.com/x"), "not both")
            self.assert_refused(self.run_harness("--tier", "quick", "--evidence", evidence, "--release", "latest"),
                                "a version like")
            self.assert_refused(self.run_harness("--tier", "quick", "--evidence", evidence,
                                                 "--base-url", "https://example.com/x"), "--qualified-digest")

    def test_refuses_an_unknown_tier_and_a_custom_tier_without_a_duration(self):
        self.assert_refused(self.run_harness("--tier", "week", "--release", "0.2.8"), "unknown tier")
        self.assert_refused(self.run_harness("--release", "0.2.8"), "needs --duration")

    def test_refuses_bad_ranges(self):
        self.assert_refused(self.run_harness("--tier", "quick", "--release", "0.2.8", "--work", "600-120"),
                            "a range is")

    def test_never_overwrites_a_run(self):
        with tempfile.TemporaryDirectory() as evidence:
            Path(evidence, "run.json").write_text("{}")
            self.assert_refused(self.run_harness("--tier", "quick", "--release", "0.2.8", "--evidence", evidence),
                                "already holds a run")

    def test_does_not_start_while_a_gate_holds_the_host(self):
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, "gate-030-data").mkdir()
            Path(directory, "gate-030-data", "chain.log").write_text("start v02-final\n")
            result = self.run_harness("--tier", "quick", "--release", "0.2.8", "--evidence", directory + "/evidence",
                                      env={"RELIABURGER_GATE_CHAINS": directory + "/gate-*/chain.log"})
            self.assert_refused(result, "not starting: a gate chain is running")

    def test_plan_prints_the_seeded_schedule_and_touches_nothing(self):
        first = self.run_harness("--tier", "quick", "--seed", "11", "--plan")
        again = self.run_harness("--tier", "quick", "--seed", "11", "--plan")
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(first.stdout, again.stdout)
        self.assertRegex(first.stdout, r"^cut 1: after \d+ s of work")

    @staticmethod
    def aborted_run(evidence, cuts_started, lines, between):
        Path(evidence, "run.json").write_text(json.dumps({
            "seed": 1, "home": "/tmp/rbpc.gone", "cuts_started": cuts_started,
            "aborted": "cut 3 wasn't made", "aborted_between_cuts": between}))
        Path(evidence, "cuts.jsonl").write_text("".join('{"cut": %d}\n' % n for n in range(1, lines + 1)))

    def test_resume_refuses_a_run_aborted_mid_cut(self):
        with tempfile.TemporaryDirectory() as evidence:
            self.aborted_run(evidence, 3, 2, False)
            self.assert_refused(self.run_harness("--resume", "--evidence", evidence), "this run was aborted")

    def test_resume_refuses_a_cut_made_but_never_recorded(self):
        # Even when nothing marked the run aborted (a kill -9, a host crash).
        with tempfile.TemporaryDirectory() as evidence:
            Path(evidence, "run.json").write_text(json.dumps({"seed": 1, "home": "/tmp/rbpc.gone",
                                                              "cuts_started": 3}))
            Path(evidence, "cuts.jsonl").write_text('{"cut": 1}\n{"cut": 2}\n')
            self.assert_refused(self.run_harness("--resume", "--evidence", evidence),
                                "cut 3 was made but never recorded")

    def test_resume_carries_on_after_an_abort_between_cuts(self):
        with tempfile.TemporaryDirectory() as evidence:
            self.aborted_run(evidence, 2, 2, True)
            result = self.run_harness("--resume", "--evidence", evidence)
            # It gets past the abort check to adopting the cluster, which isn't there.
            self.assert_refused(result, "no quickstart cluster in /tmp/rbpc.gone")
            self.assertNotIn("aborted", result.stderr)

    def test_resume_refuses_a_run_with_a_violation(self):
        # Run 10 carried on past cut 13's violation and so lost the state
        # kept for diagnosing it.
        with tempfile.TemporaryDirectory() as evidence:
            Path(evidence, "run.json").write_text(json.dumps({"seed": 1, "home": "/tmp/rbpc.gone",
                                                              "cuts_started": 2}))
            Path(evidence, "cuts.jsonl").write_text('{"cut": 1, "violations": []}\n'
                                                    '{"cut": 2, "violations": ["I2"]}\n')
            result = self.run_harness("--resume", "--evidence", evidence)
            self.assert_refused(result, "cut 2 violated I2")
            self.assertIn("--rejudge", result.stderr)

    def test_refuses_a_port_offset_past_the_block(self):
        self.assert_refused(self.run_harness("--tier", "quick", "--release", "0.2.8", "--port-offset", "501"),
                            "--port-offset takes a whole number up to 500")

    def test_rejudge_needs_an_evidence_directory_with_cuts(self):
        with tempfile.TemporaryDirectory() as evidence:
            self.assert_refused(self.run_harness("--rejudge", evidence), "no cuts.jsonl")

    def test_resume_refuses_an_adopted_home_it_did_not_make(self):
        with tempfile.TemporaryDirectory() as evidence:
            Path(evidence, "run.json").write_text(json.dumps({"seed": 1, "home": os.path.expanduser("~/.reliaburger"),
                                                              "duration_seconds": 60}))
            self.assert_refused(self.run_harness("--resume", "--evidence", evidence), "only adopts a home it made")


def shell_function(script, name):
    """The text of one shell function from SCRIPT, `name() {` to the `}` that closes it."""
    lines = script.read_text().splitlines()
    start = next(i for i, line in enumerate(lines) if line.startswith(f"{name}() {{"))
    if lines[start].rstrip().endswith("}"):
        return lines[start] + "\n"
    end = next(i for i in range(start, len(lines)) if lines[i] == "}")
    return "\n".join(lines[start:end + 1]) + "\n"


class BoundedCommands(unittest.TestCase):
    """with_timeout must stop a command that ignores SIGALRM, as Go's limactl does."""

    def run_bounded(self, seconds, command):
        script = shell_function(HARNESS, "with_timeout") + f'with_timeout {seconds} {command}\necho "status=$?"\n'
        started = time.monotonic()
        result = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
        return result.stdout, time.monotonic() - started

    def test_a_command_that_ignores_sigalrm_is_killed_at_the_deadline(self):
        # A grandchild holding stdout must not keep the caller waiting either.
        output, took = self.run_bounded(1, "sh -c 'trap \"\" ALRM TERM; (sleep 20; echo late) & sleep 20'")
        self.assertIn("status=124", output)
        self.assertNotIn("late", output)
        self.assertLess(took, 10)

    def test_a_command_that_finishes_keeps_its_status_and_output(self):
        output, _ = self.run_bounded(5, "sh -c 'echo hello; exit 3'")
        self.assertIn("hello\nstatus=3", output)


class HarnessShell(unittest.TestCase):
    """The harness's own shell functions, run against a fake limactl and relish.

    The fakes log every call to `calls` and answer from files in the
    temporary directory, the way the shared power functions' tests do.
    """

    FUNCTIONS = ("with_timeout", "now", "endurance", "run_get", "run_set", "rel", "read_resumes", "starts_in",
                 "acked_now", "read_acked", "acked_problems", "abort_run", "take_baseline", "power_back", "await_start",
                 "verify", "one_cut", "cuts_done", "cleanup", "hold_guest_updates")

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)
        self.evidence = self.path / "evidence"
        (self.evidence / "logs").mkdir(parents=True)
        (self.evidence / "cuts").mkdir()
        self.calls = self.path / "calls"
        self.calls.touch()
        self.fake("limactl", """
            echo "limactl $*" >> "$FAKE/calls"
            if [ "$1" = start ]; then
                n=$(grep -c '^limactl start' "$FAKE/calls")
                if [ -f "$FAKE/start-$n" ]; then cat "$FAKE/start-$n" >&2; exit 1; fi
            fi
            """)
        self.fake("relish", """
            echo "relish $*" >> "$FAKE/calls"
            answer() { [ -f "$FAKE/$1" ] || exit 1; cat "$FAKE/$1"; }
            # NAME-N answers the Nth call matching PATTERN, when it exists.
            numbered() {
                local n; n=$(grep -c "$2" "$FAKE/calls")
                if [ -f "$FAKE/$1-$n" ]; then cat "$FAKE/$1-$n"; else answer "$1"; fi
            }
            case "$*" in
                "--output json apps --instances")
                    n=$(grep -c 'apps --instances' "$FAKE/calls")
                    [ ! -f "$FAKE/api-after" ] || [ "$n" -ge "$(cat "$FAKE/api-after")" ] || exit 1
                    answer instances ;;
                "logs soak-writer"*) answer log-writer ;;
                "logs soak-redis-client"*) answer log-redis ;;
                "exec soak-writer awk"*) answer seq ;;
                "exec soak-writer cat /data/resumes") numbered resumes-writer 'cat /data/resumes' ;;
                "exec soak-redis redis-cli GET soak:counter") answer counter ;;
                "exec soak-redis redis-cli LRANGE soak:starts 0 -1") numbered resumes-redis 'LRANGE soak:starts' ;;
                "local stop --yes") answer local-stop ;;
                *) exit 1 ;;
            esac
            """)

    def fake(self, name, body):
        path = self.path / name
        path.write_text("#!/usr/bin/env bash\n" + textwrap.dedent(body))
        path.chmod(0o755)

    def answer(self, name, text):
        (self.path / name).write_text(text)

    def run_json(self):
        return json.loads((self.evidence / "run.json").read_text())

    def run_shell(self, script, run=None, cuts=0, fast_clock=False):
        if run is not None:
            (self.evidence / "run.json").write_text(json.dumps(run))
        if cuts:
            (self.evidence / "cuts.jsonl").write_text("".join(json.dumps({"cut": n, "violations": []}) + "\n"
                                                              for n in range(1, cuts + 1)))
        functions = "".join(shell_function(HARNESS, name) for name in self.FUNCTIONS)
        prelude = textwrap.dedent(f"""
            set -uo pipefail
            export FAKE={self.path}
            here={HERE / "power_cut"}; evidence={self.evidence}; power_log_dir={self.evidence}/logs
            limactl={self.path}/limactl; relish={self.path}/relish; vm=(none node-a); down=(0 1)
            api_bound=120; start_wait=60; want='soak-writer=1,soak-redis=1,soak-redis-client=1'
            fail() {{ printf 'power-cut: %s\\n' "$*" >&2; exit 1; }}
            say() {{ printf '%s\\n' "$*"; }}
            sleep() {{ :; }}
            gsh() {{ printf '%s\\n' '2026-11-18T10:00:01+00:00 n systemd[1]: Started reliaburger.service - x.'; }}
            """)
        # A clock that moves 100 s at every reading, so deadlines pass at once.
        clock = 'now() { local t; t=$(( $(cat "$FAKE/clock" 2>/dev/null || echo 0) + 100 )); echo "$t" > "$FAKE/clock"; echo "$t"; }\n'
        if fast_clock:
            functions += clock
        return subprocess.run(["bash", "-c", prelude + functions + textwrap.dedent(script)], stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True, timeout=60)

    def calls_made(self):
        return self.calls.read_text().splitlines()

    # run_get and run_set

    def test_a_stored_zero_reads_back_as_zero(self):
        result = self.run_shell("""
            run_set seed 0; run_set off_min 0; run_set paused_at ''
            echo "seed=$(run_get seed) off_min=$(run_get off_min) paused=$(run_get paused_at) none=$(run_get nothing)"
            """, run={})
        self.assertEqual(result.stdout.strip(), "seed=0 off_min=0 paused= none=", result.stderr)

    # power_back (finding 1)

    def test_power_back_starts_the_vm_and_waits_for_the_api_without_relish_local_start(self):
        self.answer("instances", "[]")
        self.answer("api-after", "3")
        result = self.run_shell("""
            powered_on_at=$(now)
            power_back && echo "back retries=$start_retries down=${down[1]}"
            """)
        self.assertEqual(result.stdout.strip(), "back retries=0 down=0", result.stderr)
        calls = self.calls_made()
        self.assertEqual(calls[0], "limactl start --tty=false node-a")
        self.assertEqual(calls.count("relish --output json apps --instances"), 3)
        self.assertFalse([call for call in calls if call.startswith("relish local")], calls)

    def test_power_back_retries_only_the_known_vz_disk_refusal(self):
        self.answer("instances", "[]")
        self.answer("start-1", "level=fatal msg=\"VZErrorDomain: The storage device attachment is invalid.\"\n")
        result = self.run_shell("powered_on_at=$(now)\npower_back && echo \"back retries=$start_retries\"\n")
        self.assertEqual(result.stdout.strip(), "back retries=1", result.stderr)
        self.assertEqual([call for call in self.calls_made() if call.startswith("limactl")],
                         ["limactl start --tty=false node-a", "limactl stop --force node-a",
                          "limactl start --tty=false node-a"])

    def test_any_other_start_failure_is_not_retried(self):
        self.answer("instances", "[]")
        self.answer("start-1", "level=fatal msg=\"exiting, status={Running:false Degraded:false}\"\n")
        result = self.run_shell("powered_on_at=$(now)\npower_back || echo \"failed retries=$start_retries\"\n")
        self.assertEqual(result.stdout.strip(), "failed retries=0", result.stderr)
        self.assertEqual(self.calls_made(), ["limactl start --tty=false node-a"])

    def test_bun_not_answering_by_the_bound_fails_without_a_second_power_cycle(self):
        # No instances answer: the API never comes up.
        result = self.run_shell("""
            api_bound=2; powered_on_at=$(now)
            power_back || echo failed
            """)
        self.assertEqual(result.stdout.strip(), "failed", result.stderr)
        self.assertEqual([call for call in self.calls_made() if call.startswith("limactl")],
                         ["limactl start --tty=false node-a"])

    # The last ACK and INCR (finding 3)

    def test_read_acked_keeps_the_last_value_and_refuses_none_or_less(self):
        self.answer("log-writer", "RESUME 40\nACK 41\nACK 42\n")
        result = self.run_shell('read_acked writer && echo "kept $(run_get writer_acked)"\n', run={"writer_acked": 30})
        self.assertEqual(result.stdout.split(), ["42", "kept", "42"], result.stderr)
        result = self.run_shell("read_acked writer || echo refused\n", run={"writer_acked": 50})
        self.assertEqual(result.stdout.split(), ["refused"])
        self.assertIn("below the last ACK read (42 < 50)", result.stderr)
        self.assertEqual(self.run_json()["writer_acked"], 50)
        (self.path / "log-writer").unlink()
        result = self.run_shell("read_acked writer || echo refused\n", run={})
        self.assertEqual(result.stdout.split(), ["refused"])

    def test_a_cut_with_no_ack_to_hold_is_not_made_and_aborts_the_run(self):
        self.answer("log-writer", "")
        self.answer("log-redis", "INCR 9\n")
        result = self.run_shell("""
            seed=1 work_min=1 work_max=1 off_min=1 off_max=1
            power_off() { echo "power_off $1" >> "$FAKE/calls"; }
            one_cut 4
            echo "returned $?"
            """, run={"cuts_started": 3, "writer_acked": 100, "redis_acked": 8})
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("returned", result.stdout)
        self.assertIn("wasn't made", result.stderr)
        self.assertNotIn("power_off 1", self.calls_made())
        run = self.run_json()
        self.assertEqual(run["cuts_started"], 3)
        self.assertIn("wasn't made", run["aborted"])

    # Where each start resumed (finding 2)

    def test_verify_judges_every_new_start_and_records_how_many_there_are(self):
        self.answer("seq", "MAX 500\nLAST 500\n")
        self.answer("resumes-writer", "0\n90\n400\n")
        self.answer("counter", "130\n")
        self.answer("resumes-redis", "0\n100\n")
        self.answer("instances", json.dumps([{"app_name": app, "state": "running"} for app in
                                             ("soak-writer", "soak-redis", "soak-redis-client")]))
        cut = self.evidence / "cuts" / "00002"
        cut.mkdir()
        result = self.run_shell(f"cut_dir={cut}\nverify 100 100 1 1 90 90\n",
                                run={"writer_starts": 1, "redis_starts": 1, "counter": 110})
        self.assertEqual(result.returncode, 0, result.stderr)
        writer = json.loads((cut / "I1.json").read_text())
        self.assertFalse(writer["pass"])
        self.assertIn("start 2 resumed at 90", writer["reason"])
        self.assertTrue(json.loads((cut / "I2.json").read_text())["pass"])
        self.assertTrue(json.loads((cut / "I7.json").read_text())["pass"])
        run = self.run_json()
        self.assertEqual((run["writer_starts"], run["redis_starts"], run["counter"]), (3, 2, 130))

    def running(self):
        self.answer("instances", json.dumps([{"app_name": app, "state": "running"} for app in
                                             ("soak-writer", "soak-redis", "soak-redis-client")]))

    def test_verify_waits_for_a_start_recorded_after_the_cluster_settled(self):
        # Run 12 cut 4: every instance ran, but the client hadn't yet reached
        # Redis and pushed its start onto soak:starts. (Its start at 90 came
        # before the cut, held to the previous cut's 90.)
        self.answer("seq", "MAX 500\nLAST 500\n")
        self.answer("resumes-writer", "0\n120\n")
        self.answer("counter", "130\n")
        self.answer("resumes-redis-1", "0\n90\n")
        self.answer("resumes-redis-2", "0\n90\n")
        self.answer("resumes-redis", "0\n90\n100\n")
        self.running()
        cut = self.evidence / "cuts" / "00002"
        cut.mkdir()
        result = self.run_shell(f"cut_dir={cut}\nverify 100 100 1 2 90 90\n",
                                run={"writer_starts": 1, "redis_starts": 1, "counter": 110})
        self.assertEqual(result.returncode, 0, result.stderr)
        redis = json.loads((cut / "I2.json").read_text())
        self.assertTrue(redis["pass"], redis)
        self.assertEqual(redis["resume"]["new_starts"], [100])
        self.assertEqual(self.calls_made().count("relish exec soak-redis redis-cli LRANGE soak:starts 0 -1"), 3)
        self.assertEqual(self.run_json()["redis_starts"], 3)

    def test_verify_gives_up_on_a_start_that_never_comes(self):
        self.answer("seq", "MAX 500\nLAST 500\n")
        self.answer("resumes-writer", "0\n120\n")
        self.answer("counter", "130\n")
        self.answer("resumes-redis", "0\n90\n")
        self.running()
        cut = self.evidence / "cuts" / "00002"
        cut.mkdir()
        result = self.run_shell(f"cut_dir={cut}\nverify 100 100 1 2 90 90\n",
                                run={"writer_starts": 1, "redis_starts": 1, "counter": 110}, fast_clock=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        redis = json.loads((cut / "I2.json").read_text())
        self.assertFalse(redis["pass"])
        self.assertIn("no new start since the cut", redis["reason"])

    def test_a_cut_records_each_apps_starts_just_before_the_power_off(self):
        # Run 8 cut 63: needrestart restarted Bun between the cuts, so the
        # writer started once (at 150) before the cut and once (at 230) after it.
        self.running()
        self.answer("log-writer", "ACK 200\n")
        self.answer("log-redis", "INCR 200\n")
        self.answer("seq", "MAX 300\nLAST 300\n")
        self.answer("counter", "210\n")
        self.answer("resumes-writer", "0\n100\n150\n")
        self.answer("resumes-redis", "0\n100\n")
        result = self.run_shell("""
            seed=1 work_min=1 work_max=1 off_min=1 off_max=1
            power_off() {
                echo "power_off $1" >> "$FAKE/calls"
                printf '0\\n100\\n150\\n230\\n' > "$FAKE/resumes-writer"
                printf '0\\n100\\n205\\n' > "$FAKE/resumes-redis"
            }
            power_back() { start_retries=0; }
            settle() { settled_seconds=20; }
            one_cut 2
            echo "returned $?"
            """, run={"cuts_started": 1, "writer_acked": 120, "redis_acked": 90, "writer_starts": 2,
                      "redis_starts": 2, "counter": 95}, cuts=1)
        self.assertIn("returned 0", result.stdout, result.stderr)
        line = json.loads((self.evidence / "cuts.jsonl").read_text().splitlines()[-1])
        self.assertEqual(line["starts_before_cut"], {"writer": 3, "redis": 2})
        self.assertEqual(line["previous_acked"], {"writer": 120, "redis": 90})
        self.assertEqual(line["invariants"]["I1"]["resume"]["between_cuts"], [150])
        self.assertEqual(line["violations"], [])
        calls = self.calls_made()
        self.assertLess(max(i for i, call in enumerate(calls) if "cat /data/resumes" in call
                            and i < calls.index("power_off 1")), calls.index("power_off 1"))

    def test_the_guest_stops_updating_itself(self):
        # Cutting power mid-dpkg can damage the guest, and needrestart
        # restarted Bun after an unattended libc upgrade (run 8).
        result = self.run_shell("""
            gsh() { printf '%s\\n' "$1" > "$FAKE/guest-script"; }
            hold_guest_updates && echo held
            """, run={})
        self.assertEqual(result.stdout.strip(), "held", result.stderr)
        script = (self.path / "guest-script").read_text()
        for unit in ("apt-daily.timer", "apt-daily-upgrade.timer", "unattended-upgrades.service"):
            self.assertIn(unit, script)
        self.assertIn("mask", script)
        self.assertIn("needrestart", script)

    def test_the_baseline_records_each_workloads_starts_and_last_values(self):
        self.answer("instances", json.dumps([{"app_name": app, "state": "running"} for app in
                                             ("soak-writer", "soak-redis", "soak-redis-client")]))
        self.answer("log-writer", "ACK 7\n")
        self.answer("log-redis", "INCR 3\n")
        self.answer("resumes-writer", "0\n")
        self.answer("resumes-redis", "0\n2\n")
        result = self.run_shell("take_baseline\n", run={})
        self.assertEqual(result.returncode, 0, result.stderr)
        run = self.run_json()
        self.assertEqual((run["writer_starts"], run["redis_starts"], run["writer_acked"], run["redis_acked"]),
                         (1, 2, 7, 3))

    def test_a_baseline_below_the_last_ack_says_so(self):
        # After a resume the writer's log tail read below the ACK kept before
        # the pause: that's a regression, not "nothing acknowledged".
        self.answer("instances", json.dumps([{"app_name": app, "state": "running"} for app in
                                             ("soak-writer", "soak-redis", "soak-redis-client")]))
        self.answer("log-writer", "ACK 40\n")
        self.answer("log-redis", "INCR 9\n")
        result = self.run_shell("take_baseline\n", run={"writer_acked": 50, "redis_acked": 8}, fast_clock=True)
        self.assertNotEqual(result.returncode, 0)
        aborted = self.run_json()["aborted"]
        self.assertIn("below the last ACK read (40 < 50)", aborted)
        self.assertNotIn("acknowledged nothing", aborted)

    def test_a_baseline_without_resume_records_aborts(self):
        self.answer("instances", json.dumps([{"app_name": app, "state": "running"} for app in
                                             ("soak-writer", "soak-redis", "soak-redis-client")]))
        self.answer("log-writer", "ACK 7\n")
        self.answer("log-redis", "INCR 3\n")
        result = self.run_shell("take_baseline\n", run={})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resumed", self.run_json()["aborted"])

    # Stopping early (finding 5)

    def test_an_exit_mid_cut_forces_the_vm_off_and_says_where_everything_is(self):
        # Cut 3 was made and never recorded: the run can't resume, so a forced stop costs nothing.
        self.answer("local-stop", "")
        result = self.run_shell("""
            home=/tmp/rbpc.test keep=false
            trap cleanup EXIT
            fail 'something broke'
            """, run={"cuts_started": 3}, cuts=2)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("limactl stop --force node-a", self.calls_made())
        self.assertNotIn("relish local stop --yes", self.calls_made())
        self.assertFalse(self.run_json()["aborted_between_cuts"])
        self.assertIn("/tmp/rbpc.test", result.stderr)
        self.assertIn(str(self.evidence), result.stderr)
        self.assertIn("status 1", self.run_json()["aborted"])
        self.assertEqual(json.loads((self.evidence / "summary.json").read_text())["verdict"], "aborted")

    def test_an_exit_between_cuts_stops_the_cluster_cleanly_and_can_resume(self):
        # A forced stop here would itself be a cut nobody recorded.
        self.answer("local-stop", "")
        result = self.run_shell("""
            home=/tmp/rbpc.test keep=false
            trap cleanup EXIT
            abort_run "cut 3 wasn't made: no ACK in the log"
            """, run={"cuts_started": 2}, cuts=2)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("relish local stop --yes", self.calls_made())
        self.assertNotIn("limactl stop --force node-a", self.calls_made())
        run = self.run_json()
        self.assertTrue(run["aborted_between_cuts"])
        self.assertIn("--resume", result.stderr)

    def test_a_clean_stop_that_fails_falls_back_to_the_forced_one_and_cannot_resume(self):
        result = self.run_shell("""
            home=/tmp/rbpc.test keep=false
            trap cleanup EXIT
            fail 'interrupted'
            """, run={"cuts_started": 2}, cuts=2)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([call for call in self.calls_made() if "stop" in call],
                         ["relish local stop --yes", "limactl stop --force node-a"])
        self.assertFalse(self.run_json()["aborted_between_cuts"])

    def test_a_violation_or_a_pause_keeps_the_cluster_as_it_is(self):
        result = self.run_shell("""
            home=/tmp/rbpc.test keep=false stopping=keep-off
            trap cleanup EXIT
            exit 75
            """, run={})
        self.assertEqual(result.returncode, 75)
        self.assertEqual(self.calls_made(), [])

    def test_a_refusal_before_any_cluster_touches_nothing(self):
        result = self.run_shell("""
            unset home; keep=false
            trap cleanup EXIT
            fail 'give --release or --base-url'
            """, run={})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls_made(), [])
        self.assertNotIn("aborted", self.run_json())


class SharedPowerFunctions(unittest.TestCase):
    """sustained/power.sh: the soak and the endurance harness cut and settle the same way."""

    # The callers' with_timeout, standing in: it says what bound it was given.
    # It writes to descriptor 3 (the test's stderr), which the functions' log
    # redirections leave alone.
    BOUND = 'exec 3>&2\nwith_timeout() { echo "bound $1" >&3; shift; "$@"; }\n'

    def run_bash(self, script):
        return subprocess.run(["bash", "-c", f". {POWER}\n{self.BOUND}{textwrap.dedent(script)}"],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=60)

    def test_both_vm_commands_are_bounded(self):
        # limactl stop --force and relish local start each run under with_timeout,
        # so a hung one can't stall the soak or the endurance run.
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_bash(f"""
                limactl=true; relish=true; power_log_dir={directory}; vm=(none node-a); down=(0 0)
                power_before_off() {{ :; }}; power_after_on() {{ :; }}
                power_off 1; power_on 1
                """)
            self.assertEqual(result.stderr.split("\n")[:2], ["bound 120", "bound 600"])

    def test_power_off_runs_the_hook_marks_the_node_down_and_force_stops_its_vm(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_bash(f"""
                limactl=echo; power_log_dir={directory}; vm=(none node-a node-b); down=(0 0 0)
                power_before_off() {{ echo "before $1 down=${{down[$1]}}"; }}
                power_off 2
                echo "down=${{down[2]}}"
                cat {directory}/power-off-2-*.log
                """)
            self.assertEqual(result.stdout.split("\n")[:3], ["before 2 down=0", "down=1", "stop --force node-b"])

    def test_power_on_starts_the_node_and_returns_relishs_status(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self.run_bash(f"""
                relish=false; power_log_dir={directory}; down=(0 1)
                power_after_on() {{ echo "after $1 down=${{down[$1]}}"; }}
                power_on 1 && echo started || echo "failed $?"
                """)
            self.assertEqual(result.stdout.split("\n")[:2], ["after 1 down=0", "failed 1"])

    def test_settle_needs_the_streak_in_a_row(self):
        result = self.run_bash("""
            answers=(0 1 0 0); i=0; settle_interval=0
            settle_probe() { local a=${answers[i]}; i=$((i + 1)); return "$a"; }
            settle_between() { echo "check $i"; }
            settle_succeeded() { echo "settled $1"; }
            settle_timed_out() { echo "timed out"; }
            settle fault 60 && echo ok
            """)
        self.assertEqual(result.stdout.split(), ["check", "1", "check", "2", "check", "3", "check", "4",
                                                 "settled", "fault", "ok"])

    def test_settle_gives_up_at_the_deadline(self):
        result = self.run_bash("""
            settle_interval=1
            settle_probe() { return 1; }
            settle_between() { :; }
            settle_succeeded() { echo settled; }
            settle_timed_out() { echo "timed out $1 after $3 of $2"; }
            settle slow 1 || echo "status $?"
            """)
        self.assertRegex(result.stdout, r"timed out slow after [12] of 1\nstatus 1")

    def test_the_soak_uses_the_shared_functions_and_defines_its_hooks(self):
        soak = (HERE / "qualify-sustained.sh").read_text()
        self.assertIn('. "$repository/scripts/release/sustained/power.sh"', soak)
        for function in ("power_off", "power_on", "settle"):
            self.assertNotRegex(soak, rf"(?m)^{function}\(\) \{{", f"the soak redefines {function}")
        for hook in ("power_before_off", "power_after_on", "settle_probe", "settle_between", "settle_succeeded",
                     "settle_timed_out"):
            self.assertRegex(soak, rf"(?m)^{hook}\(\) \{{", f"the soak lacks {hook}")
        self.assertIn('. "$repository/scripts/release/sustained/power.sh"', HARNESS.read_text())


if __name__ == "__main__":
    unittest.main()
