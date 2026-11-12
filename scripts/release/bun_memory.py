"""Bun's memory by kind over a soak (#1417), stdlib only.

Every guest inventory carries `bun_mem_*` lines from sustained/bun-memory.awk:
smaps_rollup's totals, the thread count and smaps classified into the main
heap, glibc thread arenas, thread stacks, other anonymous memory and
file-backed memory. The checker appends one row per node per sample to
`<evidence>/memory.jsonl`, and the record gets an hourly table per node and
a line saying which kind grew most after the first hour, so a run shows what
grows, not only that RSS does.
"""
import json
from pathlib import Path

FILE = "memory.jsonl"
# memory.jsonl's keys, each from the inventory line `bun_mem_<key>`.
KEYS = ("rss_kb", "anonymous_kb", "private_dirty_kb", "swap_kb", "threads", "heap_kb", "arena_kb", "arenas",
        "stack_kb", "stacks", "other_anon_kb", "file_kb")
# The kinds the growth line compares, with the names the record gives them.
KINDS = (("heap_kb", "heap"), ("arena_kb", "arenas"), ("stack_kb", "stacks"), ("other_anon_kb", "other anonymous"),
         ("file_kb", "file"))
HOUR = 3600
# The window of the RSS slope the record gives each node (#1417): recorded,
# not gated, until a run says what a levelled-off slope is. Nodes 1 and 3
# of the 0.2.7 memory run, which levelled off, give 0.6 to 0.7 MiB an hour.
SLOPE_WINDOW = 6 * HOUR


def number(inventory, kind):
    try:
        return int(inventory[kind][0])
    except (KeyError, IndexError, ValueError):
        return None


def sample(inventory, node, now):
    """One memory.jsonl row from a node's parsed inventory, or None when the
    guest reported no breakdown (Bun wasn't running)."""
    if "bun_mem_heap_kb" not in inventory:
        return None
    row = {"ts": now, "node": node, "pid": number(inventory, "bun_pid")}
    for key in KEYS:
        row[key] = number(inventory, "bun_mem_" + key)
    top = []
    for line in inventory.get("bun_mem_top", []):
        parts = line.split()
        if len(parts) == 3 and parts[1].isdigit():
            top.append([parts[0], int(parts[1]), parts[2]])
    row["top"] = top
    return row


def append(evidence, row):
    with open(Path(evidence) / FILE, "a") as handle:
        handle.write(json.dumps(row, sort_keys=True) + "\n")


def read(evidence):
    """Rows by node, each node's in time order. A torn last line is skipped."""
    path = Path(evidence) / FILE
    nodes = {}
    if not path.exists():
        return nodes
    for line in path.read_text().splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(row, dict) and "node" in row and "ts" in row:
            nodes.setdefault(row["node"], []).append(row)
    for rows in nodes.values():
        rows.sort(key=lambda row: row["ts"])
    return nodes


def hourly(rows):
    """The first sample, the first at or after each whole hour since it, and
    the last sample."""
    if not rows:
        return []
    first = rows[0]["ts"]
    picked, hour = [], 0
    for row in rows:
        if row["ts"] >= first + hour * HOUR:
            picked.append(row)
            hour = (row["ts"] - first) // HOUR + 1
    if picked[-1] is not rows[-1]:
        picked.append(rows[-1])
    return picked


def mib(kb):
    return "n/a" if kb is None else f"{kb / 1024:.1f}"


def elapsed(seconds):
    return f"{seconds // HOUR}:{seconds % HOUR // 60:02d}"


def growth_line(node, rows):
    """Which kind grew most in the last Bun process seen, from its first
    sample an hour or more after it was first seen to its last sample. The
    first hour is left out: that's Bun warming up."""
    if not rows:
        return f"- {node}: no memory samples."
    process = [row for row in rows if row.get("pid") == rows[-1].get("pid")]
    start = next((row for row in process if row["ts"] >= process[0]["ts"] + HOUR), None)
    if start is None or start is process[-1]:
        return f"- {node}: not enough data: the last Bun process (pid {rows[-1].get('pid')}) has no samples after its first hour."
    end = process[-1]
    growth = [(name, end[key] - start[key]) for key, name in KINDS
              if isinstance(end.get(key), int) and isinstance(start.get(key), int)]
    span = elapsed(end["ts"] - start["ts"])
    if not growth or max(delta for _, delta in growth) <= 0:
        return f"- {node}: nothing grew after the first hour ({span} h:mm compared)."
    name, delta = max(growth, key=lambda item: item[1])
    others = ", ".join(f"{other} {other_delta / 1024:+.1f}" for other, other_delta in growth if other != name)
    return (f"- {node}: **{name}** grew most after the first hour: {delta / 1024:+.1f} MiB over {span} h:mm "
            f"(others, MiB: {others}).")


def rss_slope(rows):
    """The least-squares slope of the last Bun process's RSS over its last
    SLOPE_WINDOW, in MiB an hour, or None when that process hasn't run for
    SLOPE_WINDOW."""
    process = [row for row in rows if row.get("pid") == rows[-1].get("pid") and isinstance(row.get("rss_kb"), int)]
    if not process or process[-1]["ts"] - process[0]["ts"] < SLOPE_WINDOW:
        return None
    window = [row for row in process if row["ts"] >= process[-1]["ts"] - SLOPE_WINDOW]
    hours = [(row["ts"] - window[0]["ts"]) / HOUR for row in window]
    mib = [row["rss_kb"] / 1024 for row in window]
    mean_hour, mean_mib = sum(hours) / len(hours), sum(mib) / len(mib)
    return (sum((hour - mean_hour) * (size - mean_mib) for hour, size in zip(hours, mib))
            / sum((hour - mean_hour) ** 2 for hour in hours))


def slope_line(node, rows):
    slope = rss_slope(rows)
    if slope is None:
        process = [row for row in rows if row.get("pid") == rows[-1].get("pid")]
        return (f"- {node}: no {SLOPE_WINDOW // HOUR}-hour RSS slope: the last Bun process (pid {rows[-1].get('pid')}) "
                f"ran for {elapsed(process[-1]['ts'] - process[0]['ts'])} h:mm.")
    return (f"- {node}: RSS slope over the last {SLOPE_WINDOW // HOUR} hours: {slope:+.2f} MiB/h "
            "(least squares; recorded, not gated).")


def section(evidence):
    """The record's `Bun memory by kind` section; empty when the run has no
    memory.jsonl."""
    nodes = read(evidence)
    if not nodes:
        return []
    lines = ["## Bun memory by kind", "",
             "From each node's `/proc/PID/smaps` at the settle and heavy checks (`memory.jsonl`, #1417). Sizes in "
             "MiB; arenas are glibc thread arenas, with the number of 64 MiB arena heaps in brackets. Time is h:mm "
             "since the node's first sample.", ""]
    for node, rows in sorted(nodes.items()):
        lines += [f"### {node}", "",
                  "| Time | Pid | RSS | Heap | Arenas | Stacks | Other anonymous | File | Threads |",
                  "|---|---|---|---|---|---|---|---|---|"]
        first = rows[0]["ts"]
        for row in hourly(rows):
            arenas = mib(row.get("arena_kb")) + ("" if row.get("arenas") is None else f" ({row['arenas']})")
            threads = "n/a" if row.get("threads") is None else str(row["threads"])
            lines.append(f"| {elapsed(row['ts'] - first)} | {row.get('pid')} | {mib(row.get('rss_kb'))} | "
                         f"{mib(row.get('heap_kb'))} | {arenas} | {mib(row.get('stack_kb'))} | "
                         f"{mib(row.get('other_anon_kb'))} | {mib(row.get('file_kb'))} | {threads} |")
        lines += ["", growth_line(node, rows), slope_line(node, rows), ""]
    return lines
