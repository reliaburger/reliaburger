"""The power-cut calibration's logic: the record format, the scan, the tally and the log replay.

`power_cut/calibrate.py` writes numbered records in three modes inside a VM,
the VM loses power, and a scan says what survived; `power_cut/logwrites.py`
rebuilds a disk from a dm-log-writes log as of any point. Only a real VM can
say what a cut loses (docs/qualification/2026-11-18-power-cut-calibration.md);
these tests check the arithmetic and the parsing that turn the guest's bytes
into that record, on files the tests write themselves.
"""
import fcntl
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import time
import unittest

HERE = Path(__file__).resolve().parent
DRIVER = HERE / "power_cut" / "calibrate.sh"
sys.path.insert(0, str(HERE / "power_cut"))

import calibrate  # noqa: E402
import logwrites  # noqa: E402
import quiet  # noqa: E402


class RecordFormat(unittest.TestCase):
    def test_a_record_reads_back_as_present(self):
        block = calibrate.encode("cut-7", 42, "direct")
        self.assertEqual(len(block), calibrate.BLOCK)
        self.assertEqual(calibrate.classify(block, "cut-7", 42, "direct"), "present")

    def test_another_runs_record_is_absent(self):
        block = calibrate.encode("cut-6", 42, "direct")
        self.assertEqual(calibrate.classify(block, "cut-7", 42, "direct"), "absent")

    def test_a_record_in_the_wrong_slot_or_mode_is_absent(self):
        self.assertEqual(calibrate.classify(calibrate.encode("r", 41, "fsync"), "r", 42, "fsync"), "absent")
        self.assertEqual(calibrate.classify(calibrate.encode("r", 42, "fsync"), "r", 42, "direct"), "absent")

    def test_zeros_are_absent(self):
        self.assertEqual(calibrate.classify(bytes(calibrate.BLOCK), "r", 1, "fsync"), "absent")

    def test_a_half_written_record_is_torn(self):
        block = calibrate.encode("r", 3, "fsync")
        torn = block[:2048] + bytes(2048)
        self.assertEqual(calibrate.classify(torn, "r", 3, "fsync"), "torn")


class Scan(unittest.TestCase):
    def scan(self, slots, run="r", mode="fsync"):
        """SLOTS: per block, a sequence number to write there, or None for zeros."""
        with tempfile.NamedTemporaryFile() as file:
            for seq in slots:
                file.write(calibrate.encode(run, seq, mode) if seq else bytes(calibrate.BLOCK))
            file.flush()
            return calibrate.scan_file(file.name, run, mode, limit=len(slots))

    def test_an_unbroken_run_is_all_prefix(self):
        self.assertEqual(self.scan([1, 2, 3, None, None]),
                         {"present": 3, "prefix": 3, "highest": 3, "holes": 0, "torn": 0})

    def test_a_missing_record_ends_the_prefix_and_counts_as_a_hole(self):
        self.assertEqual(self.scan([1, None, 3, 4, None]),
                         {"present": 3, "prefix": 1, "highest": 4, "holes": 1, "torn": 0})

    def test_nothing_written_is_an_empty_prefix(self):
        self.assertEqual(self.scan([None, None])["prefix"], 0)


class Recording(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)

    def cut(self, acks, scan, boot_after="b2"):
        (self.path / "acks").write_text(acks)
        (self.path / "scan.json").write_text(json.dumps({"boot": boot_after, "modes": scan}))
        return calibrate.record("stop", 1, self.path / "acks", self.path / "scan.json", {"cut_seconds": 1.5})

    @staticmethod
    def survived(prefix, highest=None):
        return {"present": prefix, "prefix": prefix, "highest": highest or prefix, "holes": 0, "torn": 0}

    def test_the_highest_ack_per_mode_counts_and_noise_is_ignored(self):
        acks, boot = calibrate.read_acks_text("BOOT b1\nACK fsync 1\nACK fsync 2\nACK direct 9\ngarbage\nACK fsync x\n")
        self.assertEqual(acks, {"fsync": 2, "direct": 9, "buffered": 0})
        self.assertEqual(boot, "b1")

    def test_records_lost_below_the_last_ack_are_lost_acked(self):
        line = self.cut("BOOT b1\nACK fsync 10\nACK direct 20\nACK buffered 30\n",
                        {"fsync": self.survived(10), "direct": self.survived(20, 22), "buffered": self.survived(4)})
        self.assertEqual(line["modes"]["fsync"]["lost_acked"], 0)
        self.assertEqual(line["modes"]["direct"]["beyond_ack"], 2)
        self.assertEqual(line["modes"]["buffered"]["lost_acked"], 26)
        self.assertTrue(line["rebooted"])
        self.assertEqual(line["cut_seconds"], 1.5)

    def test_a_cut_that_never_rebooted_the_guest_is_refused(self):
        # The same boot id after the cut: the cut never fired, and a writer
        # that's still running has lost nothing, which would tally as clean.
        with self.assertRaisesRegex(calibrate.RecordError, "never rebooted"):
            self.cut("BOOT b1\nACK fsync 1\n", {m: self.survived(1) for m in calibrate.MODES}, boot_after="b1")

    def test_a_cut_whose_writer_never_reported_its_boot_is_refused(self):
        with self.assertRaisesRegex(calibrate.RecordError, "boot id"):
            self.cut("ACK fsync 1\n", {m: self.survived(1) for m in calibrate.MODES})

    def test_the_command_line_fails_on_a_cut_that_never_fired(self):
        (self.path / "acks").write_text("BOOT b1\nACK fsync 1\n")
        (self.path / "scan.json").write_text(json.dumps({"boot": "b1", "modes": {m: self.survived(1) for m in calibrate.MODES}}))
        result = subprocess.run([sys.executable, str(HERE / "power_cut" / "calibrate.py"), "record", "--kind", "sysrq",
                                 "--cut", "1", "--acks", str(self.path / "acks"), "--scan", str(self.path / "scan.json")],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("never rebooted", result.stderr)


class Tally(unittest.TestCase):
    @staticmethod
    def line(kind, lost, acked=10):
        return {"kind": kind, "boot_before": "b1", "rebooted": True, "modes": {
            "fsync": {"acked": acked, "lost_acked": 0, "torn": 0, "holes": 0},
            "buffered": {"acked": acked, "lost_acked": lost, "torn": 0, "holes": 0},
        }}

    def test_counts_cuts_and_losses_per_kind_and_mode(self):
        summary = calibrate.tally([self.line("stop", 0), self.line("stop", 5), self.line("kill", 7)])
        stop = summary["kinds"]["stop"]
        self.assertEqual(stop["cuts"], 2)
        self.assertEqual(stop["modes"]["buffered"]["cuts_losing_acked"], 1)
        self.assertEqual(stop["modes"]["buffered"]["lost_acked_max"], 5)
        self.assertEqual(stop["modes"]["buffered"]["verdict"], "lost acknowledged records in 1 of 2 cuts")
        self.assertEqual(stop["modes"]["fsync"]["verdict"], "kept every acknowledged record")
        self.assertEqual(summary["kinds"]["kill"]["modes"]["buffered"]["verdict"],
                         "lost acknowledged records in every cut")

    def test_a_line_that_didnt_reboot_is_refused(self):
        line = self.line("sysrq", 0)
        line["rebooted"] = False
        with self.assertRaisesRegex(calibrate.RecordError, "cut 3 .*never rebooted"):
            calibrate.tally([dict(line, cut=3)])
        del line["rebooted"]
        with self.assertRaisesRegex(calibrate.RecordError, "never rebooted"):
            calibrate.tally([line])

    def test_a_line_without_a_boot_id_before_the_cut_is_refused(self):
        line = self.line("stop", 0)
        del line["boot_before"]
        with self.assertRaisesRegex(calibrate.RecordError, "no boot id"):
            calibrate.tally([line])

    def test_a_mode_that_acknowledged_nothing_says_nothing(self):
        summary = calibrate.tally([self.line("stop", 0, acked=0)])
        self.assertEqual(summary["kinds"]["stop"]["modes"]["fsync"]["cuts"], 0)
        self.assertEqual(summary["kinds"]["stop"]["modes"]["fsync"]["verdict"], "no data")

    def test_the_command_line_tallies_a_file(self):
        with tempfile.NamedTemporaryFile("w", suffix=".jsonl") as file:
            file.write(json.dumps(self.line("stop", 0)) + "\n\n")
            file.flush()
            result = subprocess.run([sys.executable, str(HERE / "power_cut" / "calibrate.py"), "tally", file.name],
                                    check=True, stdout=subprocess.PIPE, text=True)
        self.assertEqual(json.loads(result.stdout)["kinds"]["stop"]["cuts"], 1)


def stat_line(flushes):
    """A /sys/block/<dev>/stat line: 17 fields, flush I/Os completed is the 16th."""
    return " ".join(str(value) for value in [10, 0, 80, 5, 200, 0, 1600, 40, 0, 50, 45, 0, 0, 0, 0, flushes, 3]) + "\n"


class RawDevice(unittest.TestCase):
    """The raw-disk mode: O_DIRECT records on a second disk with no filesystem, counted only with no flush after them."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)

    def test_the_flush_count_is_the_sixteenth_field(self):
        self.assertEqual(calibrate.flushes_from_stat(stat_line(7)), 7)

    def test_a_kernel_without_flush_fields_has_no_flush_count(self):
        self.assertIsNone(calibrate.flushes_from_stat("1 2 3 4 5 6 7 8 9 10 11\n"))

    def fake_block(self, disks):
        """DISKS: name -> (sectors, partitions)."""
        for name, (sectors, partitions) in disks.items():
            disk = self.path / "block" / name
            disk.mkdir(parents=True)
            (disk / "size").write_text(f"{sectors}\n")
            for number in range(1, partitions + 1):
                (disk / f"{name}{number}").mkdir()
        return self.path / "block"

    def test_the_raw_disk_is_the_one_unpartitioned_disk_of_its_size(self):
        block = self.fake_block({"vda": (20971520, 2), "vdb": (2097152, 0), "vdc": (1024, 0), "loop0": (2097152, 0)})
        self.assertEqual(calibrate.find_raw_device(block, 2097152, ""), "vdb")

    def test_no_disk_of_that_size_is_an_error(self):
        block = self.fake_block({"vda": (20971520, 2)})
        with self.assertRaisesRegex(calibrate.RecordError, "no unpartitioned"):
            calibrate.find_raw_device(block, 2097152, "")

    def test_two_candidates_or_a_mounted_one_is_an_error(self):
        block = self.fake_block({"vdb": (2097152, 0), "vdc": (2097152, 0)})
        with self.assertRaisesRegex(calibrate.RecordError, "vdb, vdc"):
            calibrate.find_raw_device(block, 2097152, "")
        with self.assertRaisesRegex(calibrate.RecordError, "no unpartitioned"):
            calibrate.find_raw_device(block, 2097152, "/dev/vdb /mnt ext4 rw 0 0\n/dev/vdc /x ext4 rw 0 0\n")

    def test_the_writer_reports_acks_with_the_flush_count_before_each_write(self):
        info = calibrate.read_raw_acks_text("BOOT b1\nACK raw 1 4\nFLUSHES 4\nACK raw 2 4\nFLUSHES 5\nACK raw 3 5\nFLUSHES 5\njunk\n")
        self.assertEqual(info["boot"], "b1")
        self.assertEqual(info["acks"], [(1, 4), (2, 4), (3, 5)])
        self.assertEqual(info["final_flushes"], 5)

    def test_without_a_reading_after_the_last_ack_the_final_count_is_unknown(self):
        info = calibrate.read_raw_acks_text("BOOT b1\nACK raw 1 4\nFLUSHES 4\nACK raw 2 4\n")
        self.assertIsNone(info["final_flushes"])

    @staticmethod
    def survived(prefix):
        return {"present": prefix, "prefix": prefix, "highest": prefix, "holes": 0, "torn": 0}

    def test_records_written_after_the_last_flush_are_uncovered(self):
        info = calibrate.read_raw_acks_text("BOOT b\n" + "".join(
            f"ACK raw {n} {4 if n <= 3 else 5}\nFLUSHES {4 if n < 3 else 5}\n" for n in range(1, 7)))
        result = calibrate.raw_result(info, self.survived(6))
        self.assertTrue(result["counted"])
        self.assertEqual(result["uncovered"], 3)
        self.assertEqual((result["lost_acked"], result["lost_uncovered"]), (0, 0))

    def test_losing_uncovered_records_is_counted_as_a_loss(self):
        info = calibrate.read_raw_acks_text("BOOT b\nACK raw 1 0\nFLUSHES 0\nACK raw 2 0\nFLUSHES 0\nACK raw 3 0\nFLUSHES 0\n")
        result = calibrate.raw_result(info, self.survived(1))
        self.assertTrue(result["counted"])
        self.assertEqual((result["acked"], result["uncovered"], result["lost_acked"], result["lost_uncovered"]), (3, 3, 2, 2))

    def test_a_flush_after_the_last_ack_means_the_cut_isnt_counted(self):
        info = calibrate.read_raw_acks_text("BOOT b\nACK raw 1 0\nFLUSHES 0\nACK raw 2 0\nFLUSHES 1\n")
        result = calibrate.raw_result(info, self.survived(2))
        self.assertFalse(result["counted"])
        self.assertEqual(result["uncovered"], 0)

    def test_an_unknown_final_count_means_the_cut_isnt_counted(self):
        info = calibrate.read_raw_acks_text("BOOT b\nACK raw 1 0\nFLUSHES 0\nACK raw 2 0\n")
        self.assertFalse(calibrate.raw_result(info, self.survived(2))["counted"])

    def test_a_raw_cut_records_and_tallies_only_when_counted(self):
        (self.path / "scan.json").write_text(json.dumps({"boot": "b2", "modes": {"raw": self.survived(1)}}))
        (self.path / "counted").write_text("BOOT b1\nACK raw 1 0\nFLUSHES 0\nACK raw 2 0\nFLUSHES 0\n")
        (self.path / "flushed").write_text("BOOT b1\nACK raw 1 0\nFLUSHES 0\nACK raw 2 0\nFLUSHES 1\n")
        lines = [calibrate.record("kill", n, self.path / name, self.path / "scan.json", {})
                 for n, name in ((1, "counted"), (2, "flushed"))]
        self.assertEqual(set(lines[0]["modes"]), {"raw"})
        self.assertEqual(lines[0]["modes"]["raw"]["lost_uncovered"], 1)
        row = calibrate.tally(lines)["kinds"]["kill"]["modes"]["raw"]
        self.assertEqual((row["cuts"], row["not_counted"], row["cuts_losing_acked"]), (1, 1, 1))
        self.assertEqual(row["verdict"], "lost acknowledged records in every cut")

    def test_a_raw_run_where_every_cut_saw_a_flush_has_no_data(self):
        (self.path / "scan.json").write_text(json.dumps({"boot": "b2", "modes": {"raw": self.survived(1)}}))
        (self.path / "acks").write_text("BOOT b1\nACK raw 1 0\nFLUSHES 1\n")
        row = calibrate.tally([calibrate.record("stop", 1, self.path / "acks", self.path / "scan.json", {})])["kinds"]["stop"]["modes"]["raw"]
        self.assertEqual(row["verdict"], "no data")


class Quiet(unittest.TestCase):
    """quiet.py: a power-cut script runs only while no release gate holds the host."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)
        self.lock = self.path / "quiet.lock"
        self.chains = str(self.path / "gate-*" / "chain.log")

    def chain(self, name, text, age):
        log = self.path / name / "chain.log"
        log.parent.mkdir()
        log.write_text(text)
        stamp = time.time() - age
        os.utime(log, (stamp, stamp))

    def hold(self, operation):
        descriptor = os.open(self.lock, os.O_RDWR | os.O_CREAT)
        self.addCleanup(os.close, descriptor)
        fcntl.flock(descriptor, operation)

    def test_a_missing_lock_and_no_chains_leave_the_host_free(self):
        self.assertIsNone(quiet.busy_reason(self.lock, self.chains, 3600))

    def test_a_lock_nobody_holds_is_free(self):
        self.lock.write_text("")
        self.assertFalse(quiet.lock_held(self.lock))

    def test_an_exclusive_holder_makes_the_host_busy(self):
        self.hold(fcntl.LOCK_EX)
        self.assertIn("quiet lock", quiet.busy_reason(self.lock, self.chains, 3600))

    def test_a_shared_holder_makes_the_host_busy_too(self):
        self.hold(fcntl.LOCK_SH)
        self.assertTrue(quiet.lock_held(self.lock))

    def test_a_fresh_chain_log_still_running_makes_the_host_busy(self):
        self.chain("gate-030-c4", "step 3: soak\n", age=60)
        self.assertIn("gate-030-c4", quiet.busy_reason(self.lock, self.chains, 3600))

    def test_a_stale_chain_log_is_ignored(self):
        self.chain("gate-029", "step 3: soak\n", age=7200)
        self.assertIsNone(quiet.busy_reason(self.lock, self.chains, 3600))

    def test_a_finished_chain_log_is_ignored(self):
        self.chain("gate-030-c3", "step 9\n12:00 chain done\n", age=60)
        self.assertIsNone(quiet.busy_reason(self.lock, self.chains, 3600))

    def test_the_command_line_says_why_and_exits_one(self):
        self.chain("gate-030-c4", "step 1\n", age=1)
        result = subprocess.run([sys.executable, str(HERE / "power_cut" / "quiet.py"), "check",
                                 "--lock", str(self.lock), "--chains", self.chains],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("a gate chain is running", result.stdout)


def build_log(items, sectorsize=512, count=None, tail=b""):
    """A dm-log-writes log: ITEMS are (sector, data_or_None, flags, mark_name).

    COUNT is the super block's nr_entries (all of ITEMS by default); TAIL is
    appended after the last entry, as whatever the log device held there.
    """
    log = io.BytesIO()
    count = len(items) if count is None else count
    log.write(struct.pack("<QQQI", logwrites.MAGIC, 1, count, sectorsize).ljust(sectorsize, b"\0"))
    for sector, data, flags, mark in items:
        sectors = 0 if data is None else len(data) // sectorsize
        name = (mark or "").encode()
        if mark:
            flags |= logwrites.MARK
        header = struct.pack("<QQQQ", sector, sectors, flags, len(name)) + name
        log.write(header.ljust(sectorsize, b"\0"))
        if data is not None and not flags & logwrites.DISCARD:
            log.write(data)
    log.write(tail)
    log.seek(0)
    return log


class LogWrites(unittest.TestCase):
    A = b"A" * 512
    B = b"B" * 1024
    C = b"C" * 512

    def log(self):
        return build_log([
            (0, None, 0, "start"),
            (0, self.A, 0, None),
            (0, None, logwrites.FLUSH, None),
            (1, self.B, 0, None),
            (0, None, 0, "after-b"),
            (4, self.C, 0, None),
        ])

    def replayed(self, to):
        log = self.log()
        target = io.BytesIO(bytes(512 * 8))
        logwrites.replay(log, target, logwrites.resolve(logwrites.entries(log), to))
        return target.getvalue()

    def test_entries_carry_sectors_flags_and_mark_names(self):
        found = logwrites.entries(self.log())
        self.assertEqual(len(found), 6)
        self.assertEqual(found[0]["mark"], "start")
        self.assertEqual((found[3]["sector"], found[3]["sectors"]), (1, 2))
        self.assertEqual(logwrites.flag_names(found[2]["flags"]), ["flush"])

    def test_replay_to_the_end_applies_every_write(self):
        disk = self.replayed("end")
        self.assertEqual(disk[:512], self.A)
        self.assertEqual(disk[512:1536], self.B)
        self.assertEqual(disk[2048:2560], self.C)

    def test_replay_to_the_last_flush_drops_what_came_after_it(self):
        disk = self.replayed("last-flush")
        self.assertEqual(disk[:512], self.A)
        self.assertEqual(disk[512:], bytes(512 * 7))

    def test_replay_to_a_mark_stops_after_it(self):
        disk = self.replayed("mark:after-b")
        self.assertEqual(disk[512:1536], self.B)
        self.assertEqual(disk[2048:2560], bytes(512))

    def test_a_fua_write_is_durable_but_a_flush_carrying_data_is_not(self):
        fua = build_log([(0, self.A, logwrites.FUA, None), (1, self.C, 0, None)])
        self.assertEqual(logwrites.durable_point(logwrites.entries(fua)), 1)
        preflush = build_log([(0, self.A, 0, None), (1, self.C, logwrites.FLUSH, None)])
        self.assertEqual(logwrites.durable_point(logwrites.entries(preflush)), 1)
        commit = build_log([(0, self.A, 0, None), (1, self.C, logwrites.FLUSH | logwrites.FUA, None)])
        self.assertEqual(logwrites.durable_point(logwrites.entries(commit)), 2)

    def test_a_log_with_no_flush_keeps_nothing(self):
        self.assertEqual(logwrites.durable_point(logwrites.entries(build_log([(0, self.A, 0, None)]))), 0)

    def test_a_discard_replays_as_zeros_and_takes_no_data_in_the_log(self):
        log = build_log([(0, self.A, 0, None), (0, b"\0" * 512, logwrites.DISCARD, None), (1, self.C, 0, None)])
        target = io.BytesIO(bytes(512 * 2))
        logwrites.replay(log, target, 3)
        self.assertEqual(target.getvalue(), bytes(512) + self.C)

    def test_four_kibibyte_sectors_are_read_in_their_own_units(self):
        data = b"D" * 4096
        log = build_log([(2, data, 0, None)], sectorsize=4096)
        target = io.BytesIO(bytes(4096 * 4))
        logwrites.replay(log, target, 1)
        self.assertEqual(target.getvalue()[8192:12288], data)

    def test_refuses_a_file_that_isnt_a_log(self):
        with self.assertRaisesRegex(logwrites.LogError, "not a dm-log-writes log"):
            logwrites.entries(io.BytesIO(bytes(4096)))

    def test_refuses_an_unknown_mark_and_an_entry_past_the_end(self):
        found = logwrites.entries(self.log())
        with self.assertRaisesRegex(logwrites.LogError, "no mark"):
            logwrites.resolve(found, "mark:nope")
        with self.assertRaisesRegex(logwrites.LogError, "6 entries"):
            logwrites.resolve(found, "entry:7")
        with self.assertRaisesRegex(logwrites.LogError, "don't know"):
            logwrites.resolve(found, "sideways")

    def test_a_truncated_log_is_an_error_not_a_short_replay(self):
        whole = self.log().getvalue()
        log = io.BytesIO(whole[:-100])
        with self.assertRaisesRegex(logwrites.LogError, "cut short"):
            logwrites.replay(log, io.BytesIO(bytes(4096)), 6)

    def test_the_command_line_replays_onto_a_file(self):
        with tempfile.TemporaryDirectory() as directory:
            log_path = os.path.join(directory, "log")
            target = os.path.join(directory, "disk")
            Path(log_path).write_bytes(self.log().getvalue())
            Path(target).write_bytes(bytes(512 * 8))
            result = subprocess.run([sys.executable, str(HERE / "power_cut" / "logwrites.py"), "replay", log_path,
                                     "--target", target, "--to", "last-flush"],
                                    check=True, stdout=subprocess.PIPE, text=True)
            self.assertEqual(json.loads(result.stdout)["replayed"], 2)
            self.assertEqual(Path(target).read_bytes()[:512], self.A)


class LogWalk(unittest.TestCase):
    """A log copied at an unannounced cut: the super block counts entries only up to the last FUA write or mark."""

    A = b"A" * 512
    C = b"C" * 512
    ITEMS = [(0, A, logwrites.FUA, None), (0, None, logwrites.FLUSH, None), (1, C, 0, None), (2, A, 0, None)]

    def test_without_walking_only_the_counted_entries_are_read(self):
        self.assertEqual(len(logwrites.entries(build_log(self.ITEMS, count=1))), 1)

    def test_walking_reads_the_entries_past_the_count_and_says_so(self):
        found = logwrites.entries(build_log(self.ITEMS, count=1, tail=bytes(4096)), walk=True)
        self.assertEqual(len(found), 4)
        self.assertEqual([entry["counted"] for entry in found], [True, False, False, False])
        self.assertEqual(found[2]["sector"], 1)

    def test_walking_stops_at_a_zeroed_block(self):
        # A zeroed log device past the last entry: an all-zero header is no entry.
        found = logwrites.entries(build_log(self.ITEMS[:2], count=1, tail=bytes(4096)), walk=True)
        self.assertEqual(len(found), 2)

    def test_walking_stops_at_unknown_flags(self):
        junk = struct.pack("<QQQQ", 0, 1, 1 << 20, 0).ljust(512, b"\0") + self.A
        self.assertEqual(len(logwrites.entries(build_log(self.ITEMS, count=1, tail=junk), walk=True)), 4)

    def test_walking_stops_at_a_mark_name_too_long_for_its_block(self):
        junk = struct.pack("<QQQQ", 0, 0, logwrites.MARK, 4096).ljust(512, b"x")
        self.assertEqual(len(logwrites.entries(build_log(self.ITEMS, count=1, tail=junk), walk=True)), 4)

    def test_walking_stops_quietly_at_an_entry_whose_data_is_cut_short(self):
        whole = build_log(self.ITEMS, count=1).getvalue()
        found = logwrites.entries(io.BytesIO(whole[:-100]), walk=True)
        self.assertEqual(len(found), 3)

    def test_a_counted_entry_cut_short_is_still_an_error(self):
        whole = build_log(self.ITEMS).getvalue()
        with self.assertRaisesRegex(logwrites.LogError, "cut short"):
            logwrites.replay(io.BytesIO(whole[:-100]), io.BytesIO(bytes(4096)), 4)

    def test_the_command_line_walks_when_asked(self):
        with tempfile.TemporaryDirectory() as directory:
            log_path = os.path.join(directory, "log")
            target = os.path.join(directory, "disk")
            Path(log_path).write_bytes(build_log(self.ITEMS, count=1, tail=bytes(1024)).getvalue())
            Path(target).write_bytes(bytes(512 * 4))
            result = subprocess.run([sys.executable, str(HERE / "power_cut" / "logwrites.py"), "replay", log_path,
                                     "--target", target, "--to", "end", "--walk"],
                                    check=True, stdout=subprocess.PIPE, text=True)
            summary = json.loads(result.stdout)
            self.assertEqual((summary["entries"], summary["counted"], summary["replayed"]), (4, 1, 4))
            self.assertEqual(Path(target).read_bytes()[512:1536], self.C + self.A)


class Bounded(unittest.TestCase):
    """calibrate.sh's `bounded` must stop a command that ignores SIGALRM, as Go's limactl does."""

    def run_bounded(self, seconds, command):
        lines = DRIVER.read_text().splitlines()
        start = next(i for i, line in enumerate(lines) if line.startswith("bounded() {"))
        end = next(i for i in range(start, len(lines)) if lines[i] == "}")
        script = "\n".join(lines[start:end + 1]) + f'\nbounded {seconds} {command}\necho "status=$?"\n'
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


class OwnGroup(unittest.TestCase):
    """calibrate.sh's writer runs under `in_own_group`, so killing its group after a cut reaches the ssh under limactl."""

    def test_killing_the_group_kills_what_the_command_started(self):
        lines = DRIVER.read_text().splitlines()
        helper = next(line for line in lines if line.startswith("in_own_group=("))
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "late"
            script = (helper + f"\n\"${{in_own_group[@]}}\" sh -c '(sleep 2; touch {marker}) & sleep 20' &\n"
                      "writer=$!\nsleep 0.5\nkill -KILL -- \"-$writer\"\nwait \"$writer\"\necho \"status=$?\"\n")
            result = subprocess.run(["bash", "-c", script], capture_output=True, text=True, timeout=30)
            time.sleep(3)
            self.assertIn("status=137", result.stdout)
            self.assertFalse(marker.exists(), "a process the writer started outlived the kill")


if __name__ == "__main__":
    unittest.main()
