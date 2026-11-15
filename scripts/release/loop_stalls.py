"""Agent loop stall snapshots over a soak (#1707), stdlib only.

When a turn of Bun's agent loop runs past `[node] loop_stall_snapshot_ms`
(750 ms by default), a watchdog thread in Bun writes one block to the
journal: a line saying the loop stalled, how long and in which step, one
line per Bun thread (name, state, wait channel, syscall in flight) with the
kernel stack of every thread that isn't sleeping, and a closing line. Past
its hourly limit the watchdog writes only the first line, ending
"thread snapshot skipped".

Each guest report carries the journal's new stall lines (`@@ loop_stalls`);
the checker appends every block to `<evidence>/loop-stalls.jsonl`, the record
lists them, and a failure bundle gets `loop-stalls-<vm>.txt` cut from the
journal it copies. The blocks are the evidence #1707 needs: which thread held
the loop, and in which syscall.
"""
import json
import re
from pathlib import Path

FILE = "loop-stalls.jsonl"
STALLED = "agent loop stalled"
OPENED = "; thread snapshot ("
SKIPPED = "; thread snapshot skipped"
END = "bun: end of thread snapshot"
# Bun indents every thread and stack line under the opening one.
BODY = "bun:   "
# A block with more lines than this was cut short in the journal.
MAX_LINES = 1000
# Snapshots the record shows; the rest stay in the JSON lines file.
RECORD_LIMIT = 20
THREAD = re.compile(r"bun:   tid \d+ .*? ([A-Za-z])(?= wchan=| syscall=| \[|$)")


def extract(text):
    """Every stall block in TEXT, a journal excerpt in any journalctl format,
    in order. A block cut off before its closing line is kept and marked."""
    blocks, current = [], None
    for line in (text or "").splitlines():
        if STALLED in line and (OPENED in line or SKIPPED in line):
            if current is not None:
                blocks.append("\n".join(current + ["(block cut off)"]))
                current = None
            if SKIPPED in line:
                blocks.append(line)
            else:
                current = [line]
            continue
        if current is None:
            continue
        if END in line:
            blocks.append("\n".join(current + [line]))
            current = None
        elif BODY in line:
            current.append(line)
            if len(current) >= MAX_LINES:
                blocks.append("\n".join(current + ["(block cut off)"]))
                current = None
    if current is not None:
        blocks.append("\n".join(current + ["(block cut off)"]))
    return blocks


def append(evidence, node, now, blocks):
    if not blocks:
        return
    with open(Path(evidence) / FILE, "a") as handle:
        for block in blocks:
            handle.write(json.dumps({"ts": now, "node": node, "block": block}, sort_keys=True) + "\n")


def read(evidence):
    """Rows in the order they were seen. A torn last line is skipped."""
    path = Path(evidence) / FILE
    rows = []
    if not path.exists():
        return rows
    for line in path.read_text().splitlines():
        try:
            rows.append(json.loads(line))
        except ValueError:
            continue
    return rows


def notable(block):
    """The lines of BLOCK worth a record line: the opening one, threads in
    uninterruptible sleep (`D`), and the thread that ran the loop."""
    lines = block.splitlines()
    picked = lines[:1]
    for line in lines[1:]:
        thread = THREAD.search(line.rstrip())
        if thread and (thread.group(1) == "D" or line.rstrip().endswith("[ran the agent loop]")):
            picked.append(line)
    return picked


def section(evidence):
    """The record's section: how many stalls each node had, and the notable
    lines of each snapshot."""
    rows = read(evidence)
    lines = ["## Agent loop stalls", "",
             f"From the stall watchdog's journal blocks (`{FILE}`, #1707). Each snapshot lists every Bun thread; "
             "here are the stall line, threads in uninterruptible sleep and the thread that ran the loop.", ""]
    if not rows:
        return lines + ["- None recorded.", ""]
    counts = {}
    for row in rows:
        counts[row["node"]] = counts.get(row["node"], 0) + 1
    lines.append("- Stalls: " + ", ".join(f"{node} {count}" for node, count in sorted(counts.items())))
    lines.append("")
    for row in rows[:RECORD_LIMIT]:
        lines.append("```")
        lines += notable(row["block"])
        lines.append("```")
    if len(rows) > RECORD_LIMIT:
        lines.append(f"- {len(rows) - RECORD_LIMIT} more in `{FILE}`.")
    lines.append("")
    return lines
