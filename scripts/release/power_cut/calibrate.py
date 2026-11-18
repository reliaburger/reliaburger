#!/usr/bin/env python3
"""What does each kind of power cut lose? Step 0 of the power-cut endurance plan.

One file, two sides. In the guest (copied there by calibrate.sh, run as root):

  calibrate.py prepare --dir DIR           zero and sync the three record files once
  calibrate.py write --dir DIR --run ID    write numbered 4 KiB records to them,
                                           printing an ACK line as each one completes
  calibrate.py scan --dir DIR --run ID     after the cut, report what survived
  calibrate.py write-raw --run ID          the raw-disk mode: O_DIRECT records on the
                                           guest's second disk, alone (see below)
  calibrate.py scan-raw --run ID           after the cut, what survived on it

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

The three run at once on one filesystem, so a direct record's ACK is almost
always followed by another thread's fdatasync (a flush of the whole virtual
disk) or ext4's journal commit before the cut. A surviving direct record
therefore shows that writes covered by a flush survive, not that a completed,
unflushed write does. The raw-disk mode answers that: it writes direct
records alone to a second virtual disk with no filesystem on it
(calibrate-vm.yaml's raw disk, found by its size), reads the disk's
completed-flush count from /sys/block/<disk>/stat before each write, and
prints it with the ACK:

  BOOT <id>
  ACK raw <n> <flushes before record n was written>
  FLUSHES <flushes after the ACK>

Record n is uncovered when its count equals the last FLUSHES reading the
host saw after the last ACK: no flush completed after it was written. A cut
with no uncovered record says nothing and isn't counted. The time between
the last reading and the cut itself (a few milliseconds) isn't observed;
nothing in the guest sends that disk a flush unless asked.

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
RAW = "raw"
# The raw disk's size in 512-byte sectors, as /sys/block/<disk>/size gives
# it: 1 GiB, as calibrate.sh creates it. Nothing else in the guest has it.
RAW_SECTORS = 2097152
# /sys/block/<disk>/stat: "flush I/Os" is the 16th field
# (https://docs.kernel.org/block/stat.html).
STAT_FLUSHES = 15


class RecordError(Exception):
    """A cut whose evidence can't be trusted, so it mustn't be tallied."""


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


def flushes_from_stat(text):
    """The completed-flush count in a /sys/block/<disk>/stat line, or None on a kernel without it."""
    fields = text.split()
    return int(fields[STAT_FLUSHES]) if len(fields) > STAT_FLUSHES else None


def find_raw_device(sys_block, sectors, mounts):
    """The name of the one disk of SECTORS with no partitions and nothing mounted.

    SYS_BLOCK is /sys/block; MOUNTS the text of /proc/mounts. Loop and other
    virtual devices are skipped: the raw disk is a virtio disk, vdX.
    """
    found = []
    for disk in sorted(os.listdir(sys_block)):
        if not disk.startswith("vd"):
            continue
        path = os.path.join(sys_block, disk)
        with open(os.path.join(path, "size")) as file:
            if int(file.read().strip() or 0) != sectors:
                continue
        if any(child.startswith(disk) for child in os.listdir(path)):
            continue
        if any(line.split()[0].startswith(f"/dev/{disk}") for line in mounts.splitlines() if line.strip()):
            continue
        found.append(disk)
    if not found:
        raise RecordError(f"no unpartitioned, unmounted disk of {sectors} sectors")
    if len(found) > 1:
        raise RecordError(f"more than one candidate raw disk: {', '.join(found)}")
    return found[0]


def raw_device():
    with open("/proc/mounts") as file:
        return find_raw_device("/sys/block", RAW_SECTORS, file.read())


def read_flushes(disk):
    with open(f"/sys/block/{disk}/stat") as file:
        count = flushes_from_stat(file.read())
    if count is None:
        raise RecordError(f"/sys/block/{disk}/stat has no flush count")
    return count


def write_raw(run, seconds, pause):
    """Write direct records alone to the raw disk, with the flush count before each."""
    disk = raw_device()
    descriptor = os.open(f"/dev/{disk}", os.O_WRONLY | os.O_DIRECT)
    buffer = mmap.mmap(-1, BLOCK)
    print(f"BOOT {boot_id()}", flush=True)
    deadline = time.monotonic() + seconds
    for seq in range(1, BLOCKS + 1):
        if time.monotonic() > deadline:
            break
        before = read_flushes(disk)
        buffer.seek(0)
        buffer.write(encode(run, seq, RAW))
        os.pwrite(descriptor, buffer, (seq - 1) * BLOCK)
        print(f"ACK {RAW} {seq} {before}", flush=True)
        print(f"FLUSHES {read_flushes(disk)}", flush=True)
        time.sleep(pause)
    print(f"DONE {RAW}", flush=True)
    os.close(descriptor)


def read_raw_acks_text(text):
    """The raw writer's output: its boot id, (n, flushes before n) per ACK, and the last reading after the last ACK."""
    info = {"boot": None, "acks": [], "final_flushes": None}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[0] == "BOOT":
            info["boot"] = parts[1]
        elif len(parts) == 4 and parts[:2] == ["ACK", RAW] and parts[2].isdigit() and parts[3].isdigit():
            info["acks"].append((int(parts[2]), int(parts[3])))
            info["final_flushes"] = None
        elif len(parts) == 2 and parts[0] == "FLUSHES" and parts[1].isdigit():
            info["final_flushes"] = int(parts[1])
    return info


def raw_result(info, survived):
    """One raw-disk cut: what was acknowledged, what no flush covered, and what of each was lost."""
    acked = max((seq for seq, _ in info["acks"]), default=0)
    final = info["final_flushes"]
    uncovered = [seq for seq, before in info["acks"] if final is not None and before == final]
    return {
        "acked": acked,
        **survived,
        "lost_acked": max(0, acked - survived["prefix"]),
        "beyond_ack": max(0, survived["highest"] - acked),
        "flushes_final": final,
        "uncovered": len(uncovered),
        "lost_uncovered": sum(1 for seq in uncovered if seq > survived["prefix"]),
        # Only a cut that left some acknowledged record uncovered says
        # anything about completed, unflushed writes.
        "counted": bool(uncovered),
    }


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
    """One cut: per mode, what the host saw acknowledged and what survived.

    Raises RecordError when the cut can't be shown to have happened: the
    writer reported no boot id, or the guest's boot id didn't change. A cut
    that never fired loses nothing and would tally as clean.
    """
    with open(acks_path, errors="replace") as file:
        text = file.read()
    acks, boot = read_acks_text(text)
    with open(scan_path) as file:
        scan = json.load(file)
    if boot is None:
        raise RecordError(f"cut {cut}: the writer reported no boot id, so the cut can't be shown to have fired")
    if boot == scan.get("boot"):
        raise RecordError(f"cut {cut}: the guest never rebooted (boot id {boot} before and after)")
    modes = {}
    if RAW in scan["modes"]:
        modes[RAW] = raw_result(read_raw_acks_text(text), scan["modes"][RAW])
    for mode in MODES:
        if mode not in scan["modes"]:
            continue
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
            "rebooted": True, "modes": modes, **detail}


def tally(lines):
    """Per kind and mode: cuts, cuts that lost acknowledged records, and how much.

    A cut whose writer never acknowledged anything in a mode says nothing about
    that mode, so it's left out of that mode's count; so is a raw-disk cut
    that left no record uncovered (`not_counted`). A line that doesn't show
    the guest rebooted raises RecordError.
    """
    kinds = {}
    for entry in lines:
        if entry.get("rebooted") is not True or not entry.get("boot_before"):
            raise RecordError(f"cut {entry.get('cut', '?')} of {entry.get('kind', '?')}: "
                              "no boot id before the cut, or the guest never rebooted")
        kind = kinds.setdefault(entry["kind"], {"cuts": 0, "rebooted": 0, "modes": {}})
        kind["cuts"] += 1
        kind["rebooted"] += 1 if entry.get("rebooted") else 0
        for mode, result in entry["modes"].items():
            row = kind["modes"].setdefault(mode, {"cuts": 0, "cuts_losing_acked": 0, "lost_acked_total": 0,
                                                  "lost_acked_max": 0, "torn": 0, "holes": 0, "acked_total": 0})
            if result["acked"] == 0:
                continue
            if "counted" in result:
                row.setdefault("not_counted", 0)
                row.setdefault("uncovered_total", 0)
                if not result["counted"]:
                    row["not_counted"] += 1
                    continue
                row["uncovered_total"] += result["uncovered"]
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
    for name in ("write-raw", "scan-raw"):
        commands.add_parser(name).add_argument("--run", required=True)
    for name in ("write", "write-raw"):
        commands.choices[name].add_argument("--seconds", type=float, default=60)
        commands.choices[name].add_argument("--pause", type=float, default=0.002)
    command = commands.add_parser("record")
    command.add_argument("--kind", required=True)
    command.add_argument("--cut", type=int, required=True)
    command.add_argument("--acks", required=True)
    command.add_argument("--scan", required=True)
    command.add_argument("--detail", default="{}")
    command = commands.add_parser("tally")
    command.add_argument("cuts")
    arguments = parser.parse_args(argv)
    try:
        run_command(arguments)
    except RecordError as error:
        print(f"calibrate: {error}", file=sys.stderr)
        return 1
    return 0


def run_command(arguments):
    if arguments.command == "prepare":
        prepare(arguments.dir)
    elif arguments.command == "write":
        write(arguments.dir, arguments.run, arguments.seconds, arguments.pause)
    elif arguments.command == "scan":
        modes = {mode: scan_file(file_for(arguments.dir, mode), arguments.run, mode) for mode in MODES}
        print(json.dumps({"run": arguments.run, "boot": boot_id(), "modes": modes}))
    elif arguments.command == "write-raw":
        write_raw(arguments.run, arguments.seconds, arguments.pause)
    elif arguments.command == "scan-raw":
        disk = raw_device()
        modes = {RAW: scan_file(f"/dev/{disk}", arguments.run, RAW)}
        print(json.dumps({"run": arguments.run, "boot": boot_id(), "disk": disk, "modes": modes}))
    elif arguments.command == "record":
        print(json.dumps(record(arguments.kind, arguments.cut, arguments.acks, arguments.scan,
                                json.loads(arguments.detail))))
    elif arguments.command == "tally":
        with open(arguments.cuts) as file:
            print(json.dumps(tally(json.loads(line) for line in file if line.strip()), indent=2))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
