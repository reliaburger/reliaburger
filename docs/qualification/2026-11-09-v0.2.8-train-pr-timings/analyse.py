"""Train-PR CI timings for #1723: wall time, runner queueing, and run time
without queueing, from the jobs fetch_jobs.py saved.

Run time without queueing is the critical path through ci.yml's `needs`
graph, with each job taking the time it actually ran (started_at to
completed_at) and starting the moment its needs finished. Queueing is wall
time minus that.
"""
import json
import statistics
import sys
from datetime import datetime
from pathlib import Path

HERE = Path(sys.argv[1])

# ci.yml's jobs by display name, and what each needs (from `needs:`).
NEEDS = {
    "select jobs": [], "CI policy": [], "dependency advisory audit": [],
    "closed pull request": [],
    "lint": ["select jobs"], "demo app": ["select jobs"],
    "minimum Rust 1.97.0": ["select jobs"], "source bundle builds": ["select jobs"],
    "portable Linux": ["select jobs"], "doctests and feature tests": ["select jobs"],
    "privileged Linux": ["select jobs"], "privileged Linux (Ubuntu 26.04)": ["select jobs"],
    "build tests": ["select jobs"],
    "multi-node cluster": ["select jobs", "build tests"],
    "acceptance": ["select jobs", "build tests"],
    "optimized contract boundaries": ["select jobs"], "S3 round trip": ["select jobs"],
    "portable macOS": ["select jobs"], "benchmarks": ["select jobs"],
    "task-array benchmarks": ["select jobs"],
}
NEEDS["ignored-test evidence"] = [n for n in NEEDS if n not in ("closed pull request",)]


def at(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def minutes(delta):
    return delta.total_seconds() / 60


def key(name):
    return "portable macOS" if name.startswith("portable macOS") else name


def analyse(run, jobs):
    ran = {}
    for job in jobs:
        if job["conclusion"] in (None, "skipped") or not job["started_at"]:
            continue
        name = key(job["name"])
        span = (at(job["started_at"]), at(job["completed_at"]), at(job["created_at"]))
        # A matrix job's shards run side by side: the slowest one counts.
        if name not in ran or span[1] - span[0] > ran[name][1] - ran[name][0]:
            ran[name] = span
    finish = {}

    def earliest(name):
        if name not in finish:
            before = max((earliest(n) for n in NEEDS.get(name, [])), default=0.0)
            spent = minutes(ran[name][1] - ran[name][0]) if name in ran else 0.0
            finish[name] = before + spent
        return finish[name]

    critical = max(earliest(n) for n in NEEDS)
    created = at(run["created_at"])
    last = max(span[1] for span in ran.values())
    portable = ran.get("portable Linux")
    return {
        "id": run["id"],
        "wall": minutes(last - created),
        "critical": critical,
        "queue": minutes(last - created) - critical,
        "portable": minutes(portable[1] - portable[0]) if portable else None,
        "portable_wait": minutes(portable[0] - portable[2]) if portable else None,
        "heavy": "privileged Linux" in ran,
        "macos": "portable macOS" in ran,
        "split": "doctests and feature tests" in ran,
        "created": run["created_at"],
    }


def median(values):
    values = [v for v in values if v is not None]
    return round(statistics.median(values), 1) if values else None


def main():
    rows = []
    for run in json.loads((HERE / "chosen.json").read_text()):
        if run["conclusion"] != "success" or run["run_attempt"] != 1:
            continue
        jobs = json.loads((HERE / "jobs" / f"{run['id']}-1.json").read_text())
        row = analyse(run, jobs)
        if row["portable"] is None:
            continue  # docs-only: no Rust jobs ran
        row["train"] = run["base"]
        row["event"] = run["event"]
        rows.append(row)
    (HERE / "rows.json").write_text(json.dumps(rows, indent=1))
    groups = {}
    for row in rows:
        path = "heavy" if row["heavy"] else ("light + macOS" if row["macos"] else "light")
        variant = "after #1744" if row["split"] else "before #1744"
        groups.setdefault((row["train"], row["event"], path, variant), []).append(row)
    print("train | event | path | variant | n | wall | critical | queue | portable | portable wait")
    for group, members in sorted(groups.items()):
        print(" | ".join(group), "|", len(members), "|",
              " | ".join(str(median(r[k] for r in members))
                         for k in ("wall", "critical", "queue", "portable", "portable_wait")))


main()
