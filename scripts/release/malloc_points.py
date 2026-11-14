"""malloc_stats and malloc_trim on Bun at fixed soak hours (#1417), stdlib only.

The 0.2.7 memory run showed that all of Bun's growth is glibc malloc's (the
main heap and its thread arena), but not whether malloc holds live data or
only free memory it hasn't given back. So the day tier stops each node's Bun
with gdb at hours 6, 12, 18 and 24 and calls two glibc functions in it:

- `malloc_stats()`, which prints each arena's system bytes (what malloc took
  from the kernel) and in-use bytes (what the program holds) to Bun's
  stderr, which the journal keeps;
- `malloc_trim(0)`, which hands free pages back to the kernel.

`sustained/malloc-point.sh` does it in the guest and prints Bun's memory
before and after the trim; the checker turns that into one row a node in
`<evidence>/malloc.jsonl`, and the record tables them. In-use bytes that keep
rising between points are live heap; RSS that a trim brings back down is
malloc's high-water mark.

The attach stops every Bun thread for a moment, so a check that fails inside
an attach window is put down to the attach in the record. The checks
themselves don't change.
"""
import json
import re
import time
from pathlib import Path

FILE = "malloc.jsonl"
# The hours the harness asks for while the soak runs; hour 24 runs once the
# soak time is up, before the drain (qualify-sustained.sh).
HOURS = (6, 12, 18)
FINAL_HOUR = 24
HOUR = 3600
# A trim before a node's leak-rss warm sample would lower that node's
# baseline, so a point waits for every warm sample, but no longer than this.
WARM_WAIT = 30 * 60
# A slow agent-loop turn shows up at the next check, up to two light checks
# (30 s apart) later; a failure first seen this long after an attach ends is
# still the attach's.
EXCUSE_AFTER = 180
# The memory kinds kept before and after the trim, from bun-memory.awk.
KINDS = ("rss_kb", "anonymous_kb", "heap_kb", "arena_kb", "other_anon_kb")

_FIELD = re.compile(r"(system bytes|in use bytes|max mmap regions|max mmap bytes)\s*=\s*(\d+)\s*$")
_ARENA = re.compile(r"Arena (\d+):\s*$")
_KEYS = {"system bytes": "system_bytes", "in use bytes": "in_use_bytes",
         "max mmap regions": "mmap_regions", "max mmap bytes": "mmap_bytes"}


def parse_stats(text):
    """malloc_stats' arenas and totals from a journal excerpt, or None when it
    holds no complete block. Other lines may sit among them."""
    arenas, total, current, result = [], None, None, None
    for raw in text.splitlines():
        line = raw.strip()
        arena = _ARENA.search(line)
        if arena:
            if int(arena.group(1)) == 0:
                arenas, total = [], None
            current = {"arena": int(arena.group(1)), "system_bytes": None, "in_use_bytes": None}
            arenas.append(current)
            continue
        if line.endswith("Total (incl. mmap):"):
            current = total = {}
            continue
        field = _FIELD.search(line)
        if field and current is not None:
            current[_KEYS[field.group(1)]] = int(field.group(2))
        if total is not None and "mmap_bytes" in total and arenas:
            result = dict(arenas=arenas, system_bytes=total.get("system_bytes"),
                          in_use_bytes=total.get("in_use_bytes"), mmap_regions=total.get("mmap_regions"),
                          mmap_bytes=total["mmap_bytes"])
            arenas, total, current = [], None, None
    return result


def sections(text):
    """The report's `@@ name` sections, by name."""
    parts, name = {}, None
    for line in text.splitlines():
        if line.startswith("@@ "):
            name = line[3:].strip()
            parts[name] = []
        elif name is not None:
            parts[name].append(line)
    return {key: "\n".join(lines) + "\n" for key, lines in parts.items()}


def _values(text):
    values = {}
    for line in text.splitlines():
        key, _, value = line.strip().partition(" ")
        values.setdefault(key, value.strip())
    return values


def _number(values, key, kind=int):
    try:
        return kind(values[key])
    except (KeyError, ValueError):
        return None


def _memory(text):
    values = _values(text)
    return {kind: _number(values, "bun_mem_" + kind) for kind in KINDS}


def row(node, hour, report):
    """One malloc.jsonl row from a node's malloc-point report."""
    parts = sections(report)
    attach = _values(parts.get("attach", ""))
    before = _values(parts.get("before", ""))
    stats = parse_stats(parts.get("stats", "")) or {}
    return dict(node=node, hour=hour, pid=_number(before, "bun_pid"),
                attach_start=_number(attach, "start", float), attach_end=_number(attach, "end", float),
                gdb_exit=_number(attach, "gdb_exit"),
                before=_memory(parts.get("before", "")), after=_memory(parts.get("after", "")),
                arenas=stats.get("arenas", []), system_bytes=stats.get("system_bytes"),
                in_use_bytes=stats.get("in_use_bytes"), mmap_regions=stats.get("mmap_regions"),
                mmap_bytes=stats.get("mmap_bytes"))


def append(evidence, entry):
    with open(Path(evidence) / FILE, "a") as handle:
        handle.write(json.dumps(entry, sort_keys=True) + "\n")


def read(evidence):
    """Every row in the order written. A torn last line is skipped."""
    path = Path(evidence) / FILE
    rows = []
    if not path.exists():
        return rows
    for line in path.read_text().splitlines():
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(entry, dict) and "node" in entry and "hour" in entry:
            rows.append(entry)
    return rows


def due(rows, state, started, now):
    """The hour whose point should run now, or None: the earliest of HOURS
    not yet run whose time has come, once every node has its leak-rss warm
    sample or WARM_WAIT after the hour."""
    done = {entry["hour"] for entry in rows}
    for hour in HOURS:
        if hour in done:
            continue
        at = started + hour * HOUR
        if now < at:
            return None
        resources = state.get("resources", {})
        warm = all("rss_warm_kb" in resources.get(node, {}) for node in state.get("nodes", []))
        return hour if warm or now >= at + WARM_WAIT else None
    return None


def windows(rows):
    """(start, end, row) for every attach whose times are known."""
    return [(entry["attach_start"], entry["attach_end"] + EXCUSE_AFTER, entry) for entry in rows
            if isinstance(entry.get("attach_start"), (int, float)) and isinstance(entry.get("attach_end"), (int, float))]


def excuse(failures, rows):
    """The failure rows, with each one first seen inside an attach window put
    down to the attach: not open, so it doesn't count against the run."""
    spans = windows(rows)
    result = []
    for failure in failures:
        ts = failure.get("ts")
        hit = next((entry for start, end, entry in spans if isinstance(ts, (int, float)) and int(start) <= ts <= end),
                   None)
        if hit is None or failure.get("disposition", "open") != "open":
            result.append(failure)
            continue
        result.append(dict(failure, disposition="attach", **{"class": "diagnostic"},
                           cause=f"malloc_stats/malloc_trim attach on {hit['node']} at hour {hit['hour']} paused Bun",
                           action="none: diagnostic, not counted against the run"))
    return result


def _mib(kb):
    return "n/a" if not isinstance(kb, int) else f"{kb / 1024:.1f}"


def _mib_bytes(value):
    return "n/a" if not isinstance(value, int) else f"{value / 1048576:.1f}"


def _change(entry, kind):
    return f"{_mib(entry['before'].get(kind))} → {_mib(entry['after'].get(kind))}"


def section(evidence):
    """The record's malloc section; empty when the run has no malloc.jsonl."""
    rows = read(evidence)
    if not rows:
        return []
    lines = ["## malloc_stats and malloc_trim", "",
             "At hours 6, 12, 18 and 24 the harness attached gdb to each node's Bun and called glibc's "
             "`malloc_stats()` and `malloc_trim(0)` (`malloc.jsonl`, #1417). In use is what Bun's allocations "
             "held; system is what malloc had taken from the kernel. RSS and the heap and arena split are from "
             "smaps just before the attach and about 5 s after the trim. Sizes in MiB; times UTC. Pause is the "
             "whole gdb run, attach and symbol loading included.", "",
             "| Node | Hour | Pid | Attach | Pause | RSS before | RSS after | Given back | Heap | Arenas | In use | System |",
             "|---|---|---|---|---|---|---|---|---|---|---|---|"]
    notes = []
    for entry in sorted(rows, key=lambda item: (item["hour"], item["node"])):
        start, end = entry.get("attach_start"), entry.get("attach_end")
        when = time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(start)) if isinstance(start, (int, float)) else "n/a"
        pause = f"{end - start:.1f} s" if isinstance(start, (int, float)) and isinstance(end, (int, float)) else "n/a"
        before, after = entry["before"].get("rss_kb"), entry["after"].get("rss_kb")
        given = _mib(before - after) if isinstance(before, int) and isinstance(after, int) else "n/a"
        lines.append(f"| {entry['node']} | {entry['hour']} | {entry.get('pid')} | {when} | {pause} | {_mib(before)} | "
                     f"{_mib(after)} | {given} | {_change(entry, 'heap_kb')} | {_change(entry, 'arena_kb')} | "
                     f"{_mib_bytes(entry.get('in_use_bytes'))} | {_mib_bytes(entry.get('system_bytes'))} |")
        if entry.get("gdb_exit") != 0:
            notes.append(f"- {entry['node']}, hour {entry['hour']}: gdb exited {entry.get('gdb_exit')}; "
                         "see malloc/ in the evidence.")
        elif entry.get("arenas"):
            notes.append(f"- {entry['node']}, hour {entry['hour']}: " + "; ".join(
                f"arena {arena['arena']} {_mib_bytes(arena.get('in_use_bytes'))} of "
                f"{_mib_bytes(arena.get('system_bytes'))} MiB in use" for arena in entry["arenas"]))
        else:
            notes.append(f"- {entry['node']}, hour {entry['hour']}: no malloc_stats output in Bun's journal.")
    return lines + [""] + notes + [""]
