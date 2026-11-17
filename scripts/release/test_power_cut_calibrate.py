"""The power-cut calibration's logic: the record format, the scan, the tally and the log replay.

`power_cut/calibrate.py` writes numbered records in three modes inside a VM,
the VM loses power, and a scan says what survived; `power_cut/logwrites.py`
rebuilds a disk from a dm-log-writes log as of any point. Only a real VM can
say what a cut loses (docs/qualification/2026-11-18-power-cut-calibration.md);
these tests check the arithmetic and the parsing that turn the guest's bytes
into that record, on files the tests write themselves.
"""
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "power_cut"))

import calibrate  # noqa: E402
import logwrites  # noqa: E402


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

    def test_the_same_boot_id_after_the_cut_means_no_reboot(self):
        line = self.cut("BOOT b1\nACK fsync 1\n", {m: self.survived(1) for m in calibrate.MODES}, boot_after="b1")
        self.assertFalse(line["rebooted"])


class Tally(unittest.TestCase):
    @staticmethod
    def line(kind, lost, acked=10):
        return {"kind": kind, "rebooted": True, "modes": {
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


def build_log(items, sectorsize=512):
    """A dm-log-writes log: ITEMS are (sector, data_or_None, flags, mark_name)."""
    log = io.BytesIO()
    log.write(struct.pack("<QQQI", logwrites.MAGIC, 1, len(items), sectorsize).ljust(sectorsize, b"\0"))
    for sector, data, flags, mark in items:
        sectors = 0 if data is None else len(data) // sectorsize
        name = (mark or "").encode()
        if mark:
            flags |= logwrites.MARK
        header = struct.pack("<QQQQ", sector, sectors, flags, len(name)) + name
        log.write(header.ljust(sectorsize, b"\0"))
        if data is not None and not flags & logwrites.DISCARD:
            log.write(data)
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


if __name__ == "__main__":
    unittest.main()
