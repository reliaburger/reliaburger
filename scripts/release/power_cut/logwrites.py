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
device's logical block size. The super's nr_entries is updated only at each
FUA write and each mark, so a log copied at a cut nobody announced can hold
entries past the count. Write a mark at the cut when you can (the calibration
proof did). When you can't, `--walk` reads on past the count while the
entries still look like entries: a header with known flags, a mark name that
fits its block, and data that's all there. The format has no checksum, so a
walk trusts the log device's tail: zero the log device before logging, or a
stale entry from an earlier session reads as a real one.

  logwrites.py list LOG [--walk]
  logwrites.py replay LOG --target FILE --to end|last-flush|mark:NAME|entry:N [--walk]
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
KNOWN_FLAGS = FLUSH | FUA | DISCARD | MARK | METADATA


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


def plausible(header, sectorsize, data_available):
    """Does a block past the counted entries look like an entry the kernel wrote?

    The kernel never logs an empty plain write, so an all-zero header (a
    zeroed log device's tail) isn't one; nor is a header with flags it doesn't
    set, a mark name longer than its block, or a write whose data isn't all
    in the log yet.
    """
    if len(header) < sectorsize:
        return False
    _, sectors, flags, data_length = ENTRY.unpack(header[:ENTRY.size])
    if flags & ~KNOWN_FLAGS or (flags == 0 and sectors == 0):
        return False
    if flags & MARK:
        return data_length <= sectorsize - ENTRY.size
    if data_length != 0:
        return False
    return flags & DISCARD or sectors * sectorsize <= data_available


def entries(log, walk=False):
    """The entries in the log, as dicts with the data's offset in the log.

    Without WALK, exactly the entries the super block counts. With WALK, those
    and then every further entry until one isn't `plausible`; each entry's
    `counted` says which side of the count it's on.
    """
    count, sectorsize = read_super(log)
    log.seek(0, 2)
    size = log.tell()
    offset = sectorsize
    found = []
    index = 0
    while index < count or walk:
        log.seek(offset)
        header = log.read(sectorsize)
        if index >= count and not plausible(header, sectorsize, size - offset - sectorsize):
            break
        if len(header) < ENTRY.size:
            raise LogError(f"entry {index} is cut short")
        sector, sectors, flags, data_length = ENTRY.unpack(header[:ENTRY.size])
        entry = {"index": index, "sector": sector, "sectors": sectors, "flags": flags,
                 "data_offset": offset + sectorsize, "sectorsize": sectorsize, "counted": index < count}
        if flags & MARK:
            entry["mark"] = header[ENTRY.size:ENTRY.size + data_length].split(b"\0")[0].decode(errors="replace")
        found.append(entry)
        offset += sectorsize
        if not flags & DISCARD:
            offset += sectors * sectorsize
        index += 1
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


def replay(log, target, point, walk=False):
    """Write the first POINT entries onto TARGET; discards become zeros."""
    written = 0
    for entry in entries(log, walk)[:point]:
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
    command.add_argument("--walk", action="store_true", help="read on past the super block's entry count")
    command = commands.add_parser("replay")
    command.add_argument("log")
    command.add_argument("--target", required=True)
    command.add_argument("--to", required=True)
    command.add_argument("--walk", action="store_true", help="read on past the super block's entry count")
    arguments = parser.parse_args(argv)
    try:
        with open(arguments.log, "rb") as log:
            found = entries(log, arguments.walk)
            if arguments.command == "list":
                for entry in found:
                    print(json.dumps({"index": entry["index"], "sector": entry["sector"],
                                      "sectors": entry["sectors"], "flags": flag_names(entry["flags"]),
                                      "counted": entry["counted"],
                                      **({"mark": entry["mark"]} if "mark" in entry else {})}))
                return 0
            point = resolve(found, arguments.to)
            with open(arguments.target, "r+b") as target:
                written = replay(log, target, point, arguments.walk)
                target.flush()
        counted = sum(1 for entry in found if entry["counted"])
        print(json.dumps({"entries": len(found), "counted": counted, "replayed": point, "writes": written,
                          "durable_point": durable_point(found)}))
        return 0
    except LogError as error:
        print(f"logwrites: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
