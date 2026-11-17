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

    def test_the_baseline_before_the_first_cut_has_no_previous(self):
        result = endurance.resume_check("0\n", 0, None)
        self.assertTrue(result["pass"], result)
        self.assertEqual(result["starts"], 1)


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

    @staticmethod
    def line(cut, violations=(), api=40, settled=60, retries=0):
        return {"cut": cut, "topology": "1", "kind": "site", "violations": list(violations),
                "recovery": {"api_seconds": api, "settled_seconds": settled, "start_retries": retries}}

    def test_no_violations_in_n_cuts_bounds_the_rate_by_the_rule_of_three(self):
        summary = endurance.summarise(self.RUN, [self.line(n) for n in range(1, 13)])
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
        summary = endurance.summarise(self.RUN, [self.line(1), self.line(2, ["I1"])])
        self.assertEqual(summary["verdict"], "fail")
        self.assertIsNone(summary["topologies"]["1"]["bound95"])
        self.assertEqual(summary["topologies"]["1"]["violated_cuts"], [{"cut": 2, "violations": ["I1"]}])

    def test_recovery_percentiles_and_retries(self):
        lines = [self.line(n, settled=s, retries=r) for n, (s, r) in enumerate([(10, 0), (20, 1), (30, 0), (100, 1)], 1)]
        summary = endurance.summarise(self.RUN, lines)
        self.assertEqual(summary["topologies"]["1"]["recovery_seconds"], {"p50": 20, "p95": 100, "max": 100})
        self.assertEqual(summary["environment_events"]["vz_start_retries"], 2)

    def test_no_cuts_is_not_a_pass(self):
        self.assertEqual(endurance.summarise(self.RUN, [])["verdict"], "no cuts")


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
            Path(evidence, "run.json").write_text(json.dumps({"seed": 1}))
            result = self.run_endurance("summary", "--evidence", evidence)
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads(Path(evidence, "summary.json").read_text())["verdict"], "fail")

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

    def test_resume_refuses_an_adopted_home_it_did_not_make(self):
        with tempfile.TemporaryDirectory() as evidence:
            Path(evidence, "run.json").write_text(json.dumps({"seed": 1, "home": os.path.expanduser("~/.reliaburger"),
                                                              "duration_seconds": 60}))
            self.assert_refused(self.run_harness("--resume", "--evidence", evidence), "only adopts a home it made")


def shell_function(script, name):
    """The text of one shell function from SCRIPT, `name() {` to the `}` that closes it."""
    lines = script.read_text().splitlines()
    start = next(i for i, line in enumerate(lines) if line.startswith(f"{name}() {{"))
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


class SharedPowerFunctions(unittest.TestCase):
    """sustained/power.sh: the soak and the endurance harness cut and settle the same way."""

    def run_bash(self, script):
        return subprocess.run(["bash", "-c", f". {POWER}\n{textwrap.dedent(script)}"], stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True, timeout=60)

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
