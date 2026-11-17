#!/usr/bin/env python3
"""What does each kind of power cut lose? Step 0 of the power-cut endurance plan.

One file, two sides. In the guest (copied there by calibrate.sh, run as root):

  calibrate.py prepare --dir DIR           zero and sync the three record files once
  calibrate.py write --dir DIR --run ID    write numbered 4 KiB records to them,
                                           printing an ACK line as each one completes
  calibrate.py scan --dir DIR --run ID     after the cut, report what survived

On the host:

  calibrate.py record --kind K --cut N --acks FILE --scan FILE [--detail JSON]
                                           one JSON line for cuts.jsonl
  calibrate.py tally CUTS.jsonl            the summary: per kind and mode, how many
                                           cuts lost acknowledged records

The three modes differ only in how far a record has gone when its ACK prints:

  fsync     buffered pwrite then fdatasync: on stable storage as far as the
            guest can tell (a flush reached the virtual disk)
  direct    O_DIRECT pwrite, no flush: the virtual disk completed the write,
            but nobody asked it to make the write durable
  buffered  buffered pwrite, no sync: only in the guest's page cache

Each record carries the run id, its sequence number, its mode and a CRC over
the block, so a scan tells a record of this run from a stale one (absent) and
from a half-written one (torn). Record N lives in block N - 1, so a scan needs
no index.
"""
import argparse
import json
import mmap
import os
import struct
import sys
import threading
import time
import zlib

BLOCK = 4096
MAGIC = b"RBPCUT01"
MODES = ("fsync", "direct", "buffered")
# 128 MiB per file: enough for a 20-second run of the fastest mode.
BLOCKS = 32768
HEADER = struct.Struct("<8s16sQ8s")


def file_for(directory, mode):
    return os.path.join(directory, f"records-{mode}.bin")


def encode(run, seq, mode):
    """One record: header, a fill that depends on the sequence number, CRC last."""
    header = HEADER.pack(MAGIC, run.encode().ljust(16, b"\0")[:16], seq, mode.encode().ljust(8, b"\0"))
    fill_length = BLOCK - len(header) - 4
    fill = (struct.pack("<Q", seq) * (fill_length // 8 + 1))[:fill_length]
    body = header + fill
    return body + struct.pack("<I", zlib.crc32(body))


def classify(block, run, seq, mode):
    """What a block holds: "present", "absent" or "torn".

    A block counts as torn when it starts like this run's record but its CRC
    doesn't match: part of the write reached the disk.
    """
    if len(block) != BLOCK:
        return "absent"
    body, (crc,) = block[:-4], struct.unpack("<I", block[-4:])
    if zlib.crc32(body) == crc:
        magic, block_run, block_seq, block_mode = HEADER.unpack(body[:HEADER.size])
        wanted = (MAGIC, run.encode().ljust(16, b"\0")[:16], seq, mode.encode().ljust(8, b"\0"))
        return "present" if (magic, block_run, block_seq, block_mode) == wanted else "absent"
    return "torn" if block.startswith(MAGIC + run.encode().ljust(16, b"\0")[:16]) else "absent"


def scan_file(path, run, mode, limit=BLOCKS):
    """Summarise one record file after a cut.

    `prefix` is the highest N such that records 1..N are all present; `holes`
    counts records missing below the highest present one.
    """
    present = []
    torn = 0
    with open(path, "rb") as file:
        for index in range(limit):
            block = file.read(BLOCK)
            if not block:
                break
            kind = classify(block, run, index + 1, mode)
            if kind == "present":
                present.append(index + 1)
            elif kind == "torn":
                torn += 1
    highest = present[-1] if present else 0
    prefix = 0
    for seq in present:
        if seq != prefix + 1:
            break
        prefix = seq
    return {"present": len(present), "prefix": prefix, "highest": highest,
            "holes": highest - len(present), "torn": torn}


def prepare(directory):
    os.makedirs(directory, exist_ok=True)
    zeros = bytes(BLOCK * 256)
    for mode in MODES:
        path = file_for(directory, mode)
        if os.path.exists(path) and os.path.getsize(path) == BLOCK * BLOCKS:
            continue
        with open(path, "wb") as file:
            for _ in range(BLOCKS // 256):
                file.write(zeros)
            file.flush()
            os.fsync(file.fileno())
    descriptor = os.open(directory, os.O_RDONLY)
    os.fsync(descriptor)
    os.close(descriptor)


def boot_id():
    try:
        with open("/proc/sys/kernel/random/boot_id") as file:
            return file.read().strip()
    except OSError:
        return "unknown"


def write(directory, run, seconds, pause):
    """Write records in all three modes at once until SECONDS pass or the files fill."""
    lock = threading.Lock()

    def say(line):
        with lock:
            sys.stdout.write(line + "\n")
            sys.stdout.flush()

    def one(mode):
        flags = os.O_WRONLY | (os.O_DIRECT if mode == "direct" else 0)
        descriptor = os.open(file_for(directory, mode), flags)
        # O_DIRECT needs an aligned buffer; an anonymous map is page-aligned.
        buffer = mmap.mmap(-1, BLOCK)
        deadline = time.monotonic() + seconds
        for seq in range(1, BLOCKS + 1):
            if time.monotonic() > deadline:
                break
            buffer.seek(0)
            buffer.write(encode(run, seq, mode))
            os.pwrite(descriptor, buffer, (seq - 1) * BLOCK)
            if mode == "fsync":
                os.fdatasync(descriptor)
            say(f"ACK {mode} {seq}")
            if mode != "fsync":
                time.sleep(pause)
        say(f"DONE {mode}")
        os.close(descriptor)

    say(f"BOOT {boot_id()}")
    threads = [threading.Thread(target=one, args=(mode,)) for mode in MODES]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()


def read_acks_text(text):
    """The highest ACK the host saw per mode, and the boot id the writer reported."""
    acks = {mode: 0 for mode in MODES}
    boot = None
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[0] == "BOOT":
            boot = parts[1]
        elif len(parts) == 3 and parts[0] == "ACK" and parts[1] in acks and parts[2].isdigit():
            acks[parts[1]] = max(acks[parts[1]], int(parts[2]))
    return acks, boot


def record(kind, cut, acks_path, scan_path, detail):
    """One cut: per mode, what the host saw acknowledged and what survived."""
    with open(acks_path, errors="replace") as file:
        acks, boot = read_acks_text(file.read())
    with open(scan_path) as file:
        scan = json.load(file)
    modes = {}
    for mode in MODES:
        survived = scan["modes"][mode]
        acknowledged = acks[mode]
        modes[mode] = {
            "acked": acknowledged,
            **survived,
            # Records the host saw acknowledged that aren't on the disk now.
            "lost_acked": max(0, acknowledged - survived["prefix"]),
            # Records that survived beyond the last ACK the host saw: in flight.
            "beyond_ack": max(0, survived["highest"] - acknowledged),
        }
    return {"cut": cut, "kind": kind, "boot_before": boot, "boot_after": scan.get("boot"),
            "rebooted": boot is not None and boot != scan.get("boot"), "modes": modes, **detail}


def tally(lines):
    """Per kind and mode: cuts, cuts that lost acknowledged records, and how much.

    A cut whose writer never acknowledged anything in a mode says nothing about
    that mode, so it's left out of that mode's count.
    """
    kinds = {}
    for entry in lines:
        kind = kinds.setdefault(entry["kind"], {"cuts": 0, "rebooted": 0, "modes": {}})
        kind["cuts"] += 1
        kind["rebooted"] += 1 if entry.get("rebooted") else 0
        for mode, result in entry["modes"].items():
            row = kind["modes"].setdefault(mode, {"cuts": 0, "cuts_losing_acked": 0, "lost_acked_total": 0,
                                                  "lost_acked_max": 0, "torn": 0, "holes": 0, "acked_total": 0})
            if result["acked"] == 0:
                continue
            row["cuts"] += 1
            row["acked_total"] += result["acked"]
            row["cuts_losing_acked"] += 1 if result["lost_acked"] else 0
            row["lost_acked_total"] += result["lost_acked"]
            row["lost_acked_max"] = max(row["lost_acked_max"], result["lost_acked"])
            row["torn"] += result["torn"]
            row["holes"] += result["holes"]
    for kind in kinds.values():
        for row in kind["modes"].values():
            row["verdict"] = verdict(row)
    return {"schema": "reliaburger.power-cut-calibration/1", "kinds": kinds}


def verdict(row):
    """In words: does this mode keep what it acknowledged under this cut?"""
    if row["cuts"] == 0:
        return "no data"
    if row["cuts_losing_acked"] == 0:
        return "kept every acknowledged record"
    if row["cuts_losing_acked"] == row["cuts"]:
        return "lost acknowledged records in every cut"
    return f"lost acknowledged records in {row['cuts_losing_acked']} of {row['cuts']} cuts"


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("prepare", "write", "scan"):
        command = commands.add_parser(name)
        command.add_argument("--dir", required=True)
        if name != "prepare":
            command.add_argument("--run", required=True)
    commands.choices["write"].add_argument("--seconds", type=float, default=60)
    commands.choices["write"].add_argument("--pause", type=float, default=0.002)
    command = commands.add_parser("record")
    command.add_argument("--kind", required=True)
    command.add_argument("--cut", type=int, required=True)
    command.add_argument("--acks", required=True)
    command.add_argument("--scan", required=True)
    command.add_argument("--detail", default="{}")
    command = commands.add_parser("tally")
    command.add_argument("cuts")
    arguments = parser.parse_args(argv)

    if arguments.command == "prepare":
        prepare(arguments.dir)
    elif arguments.command == "write":
        write(arguments.dir, arguments.run, arguments.seconds, arguments.pause)
    elif arguments.command == "scan":
        modes = {mode: scan_file(file_for(arguments.dir, mode), arguments.run, mode) for mode in MODES}
        print(json.dumps({"run": arguments.run, "boot": boot_id(), "modes": modes}))
    elif arguments.command == "record":
        print(json.dumps(record(arguments.kind, arguments.cut, arguments.acks, arguments.scan,
                                json.loads(arguments.detail))))
    elif arguments.command == "tally":
        with open(arguments.cuts) as file:
            print(json.dumps(tally(json.loads(line) for line in file if line.strip()), indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
