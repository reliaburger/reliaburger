#!/usr/bin/env python3
"""Read a dm-log-writes log and rebuild the disk as of any point in it.

The kernel's `log-writes` device-mapper target
(https://docs.kernel.org/admin-guide/device-mapper/log-writes.html) passes
writes through to a data device and records each one, with every flush, FUA
and named mark, on a log device. Replaying a prefix of the log onto a copy of
the data device as it was when logging began gives the disk as it stood at
that point.

The target logs a plain write only when a later flush completes (a FUA write
at once), so a copy of the log taken at a power cut already leaves out
everything no flush covered: replayed to its end, it's the disk a drive with
a volatile write cache must have kept, fault model 2 in
docs/plans/2026-11-18-plan-power-cut-endurance.md. A prefix ending between
the last two flushes is a state such a drive could also have left, if it
wrote its cache back in order. The calibration record
(docs/qualification/2026-11-18-power-cut-calibration.md) has the proof.

This is the stdlib equivalent of xfstests' `replay-log`, which Ubuntu doesn't
package. The on-disk format, from drivers/md/dm-log-writes.c:

  block 0           super: magic, version, nr_entries (u64 each), sectorsize (u32)
  then, per entry   one block: sector, nr_sectors, flags, data_len (u64 each),
                    followed in the same block by a mark's name;
                    then nr_sectors blocks of data (none for a discard)

`sector` and `nr_sectors` count blocks of `sectorsize` bytes, the data
device's logical block size. The super's nr_entries is updated at each FUA
write and each mark, so write a mark before reading a live log.

  logwrites.py list LOG
  logwrites.py replay LOG --target FILE --to end|last-flush|mark:NAME|entry:N
"""
import argparse
import json
import struct
import sys

MAGIC = 0x6A736677736872
SUPER = struct.Struct("<QQQI")
ENTRY = struct.Struct("<QQQQ")
FLUSH, FUA, DISCARD, MARK, METADATA = 1, 2, 4, 8, 16
FLAG_NAMES = ((FLUSH, "flush"), (FUA, "fua"), (DISCARD, "discard"), (MARK, "mark"), (METADATA, "metadata"))


class LogError(Exception):
    pass


def read_super(log):
    log.seek(0)
    raw = log.read(SUPER.size)
    if len(raw) < SUPER.size:
        raise LogError("log is shorter than its super block")
    magic, version, entries, sectorsize = SUPER.unpack(raw)
    if magic != MAGIC:
        raise LogError(f"not a dm-log-writes log (magic {magic:#x})")
    if version != 1:
        raise LogError(f"unsupported log version {version}")
    if sectorsize == 0 or sectorsize % 512:
        raise LogError(f"bad sector size {sectorsize}")
    return entries, sectorsize


def entries(log):
    """Every entry the super block counts, as dicts with the data's offset in the log."""
    count, sectorsize = read_super(log)
    offset = sectorsize
    found = []
    for index in range(count):
        log.seek(offset)
        header = log.read(sectorsize)
        if len(header) < ENTRY.size:
            raise LogError(f"entry {index} is cut short")
        sector, sectors, flags, data_length = ENTRY.unpack(header[:ENTRY.size])
        entry = {"index": index, "sector": sector, "sectors": sectors, "flags": flags,
                 "data_offset": offset + sectorsize, "sectorsize": sectorsize}
        if flags & MARK:
            entry["mark"] = header[ENTRY.size:ENTRY.size + data_length].split(b"\0")[0].decode(errors="replace")
        found.append(entry)
        offset += sectorsize
        if not flags & DISCARD:
            offset += sectors * sectorsize
    return found


def flag_names(flags):
    return [name for bit, name in FLAG_NAMES if flags & bit]


def durable_point(found):
    """How many entries a disk with a volatile cache must have kept at a cut.

    Everything completed before a flush is durable once the flush is, and a
    FUA write is durable when it completes. A flush entry that carries data
    (REQ_PREFLUSH with a payload) flushes what came before, not its own data,
    so its data isn't counted unless it's also FUA.
    """
    point = 0
    for entry in found:
        if entry["flags"] & FUA:
            point = entry["index"] + 1
        elif entry["flags"] & FLUSH:
            point = max(point, entry["index"])
    return point


def resolve(found, to):
    """The number of entries to replay for --to."""
    if to == "end":
        return len(found)
    if to == "last-flush":
        return durable_point(found)
    if to.startswith("mark:"):
        name = to[len("mark:"):]
        for entry in found:
            if entry.get("mark") == name:
                return entry["index"] + 1
        raise LogError(f"no mark {name!r} in the log")
    if to.startswith("entry:") and to[len("entry:"):].isdigit():
        point = int(to[len("entry:"):])
        if point > len(found):
            raise LogError(f"the log has {len(found)} entries, not {point}")
        return point
    raise LogError(f"don't know how to replay to {to!r}")


def replay(log, target, point):
    """Write the first POINT entries onto TARGET; discards become zeros."""
    written = 0
    for entry in entries(log)[:point]:
        size = entry["sectors"] * entry["sectorsize"]
        if size == 0 or entry["flags"] & MARK:
            continue
        if entry["flags"] & DISCARD:
            data = bytes(size)
        else:
            log.seek(entry["data_offset"])
            data = log.read(size)
            if len(data) != size:
                raise LogError(f"entry {entry['index']}'s data is cut short")
        target.seek(entry["sector"] * entry["sectorsize"])
        target.write(data)
        written += 1
    return written


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    command = commands.add_parser("list")
    command.add_argument("log")
    command = commands.add_parser("replay")
    command.add_argument("log")
    command.add_argument("--target", required=True)
    command.add_argument("--to", required=True)
    arguments = parser.parse_args(argv)
    try:
        with open(arguments.log, "rb") as log:
            found = entries(log)
            if arguments.command == "list":
                for entry in found:
                    print(json.dumps({"index": entry["index"], "sector": entry["sector"],
                                      "sectors": entry["sectors"], "flags": flag_names(entry["flags"]),
                                      **({"mark": entry["mark"]} if "mark" in entry else {})}))
                return 0
            point = resolve(found, arguments.to)
            with open(arguments.target, "r+b") as target:
                written = replay(log, target, point)
                target.flush()
        print(json.dumps({"entries": len(found), "replayed": point, "writes": written,
                          "durable_point": durable_point(found)}))
        return 0
    except LogError as error:
        print(f"logwrites: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
