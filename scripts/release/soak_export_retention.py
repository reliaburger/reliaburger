#!/usr/bin/env python3
"""Keep the soak's metrics export to a bounded tail on the guest (#1418).

Bun exports each metrics Parquet file once a minute to
/var/lib/reliaburger/soak-export/metrics/<node>/<sha256>-<name>. The checker
(`export_findings` in sustained_check.py) needs each one at the destination
in the first heavy report after its source has gone. Before this the
destination kept every file, about 600 MiB per node in a day run, on the
same 10 GiB disk as Bun's data. Now a file goes once it is verified and its
source has been gone for KEEP_SECONDS, which spans several heavy reports.

The heavy report runs this after it has listed the export, with the source
and destination directories as arguments.
"""
import json
import os
from pathlib import Path
import sys
import time

KEEP_SECONDS = 30 * 60
STATE_FILE = ".soak-retention.json"
VERIFIED_FILE = ".soak-verified"


def prune(source, destination, now, keep_seconds=KEEP_SECONDS):
    """Delete verified exports whose source has been gone for keep_seconds.

    Returns the deleted files' paths relative to `destination`. The state
    file records when each export was first seen without its source."""
    source, destination = Path(source), Path(destination)
    if not destination.is_dir():
        return []
    sources = {path.name for path in source.glob("*.parquet")}
    try:
        verified = set((destination / VERIFIED_FILE).read_text().split())
    except FileNotFoundError:
        verified = set()
    try:
        state = json.loads((destination / STATE_FILE).read_text())
    except (FileNotFoundError, ValueError):
        state = {}
    kept, removed = {}, []
    for path in sorted(destination.glob("*/*.parquet")):
        relative = str(path.relative_to(destination))
        original = path.name.partition("-")[2]
        if original in sources:
            continue
        gone_since = state.get(relative, now)
        if relative in verified and now - gone_since >= keep_seconds:
            path.unlink()
            removed.append(relative)
        else:
            kept[relative] = gone_since
    temporary = destination / (STATE_FILE + ".tmp")
    temporary.write_text(json.dumps(kept, sort_keys=True))
    os.replace(temporary, destination / STATE_FILE)
    if removed:
        remaining = sorted(verified - set(removed))
        temporary = destination / (VERIFIED_FILE + ".tmp")
        temporary.write_text("".join(name + "\n" for name in remaining))
        os.replace(temporary, destination / VERIFIED_FILE)
    return removed


if __name__ == "__main__":
    prune(sys.argv[1], sys.argv[2], int(time.time()))
