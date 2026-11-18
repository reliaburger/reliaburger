#!/usr/bin/env python3
"""The power-cut endurance run's decisions and arithmetic, apart from the shell that drives the VMs.

qualify-power-cut-endurance.sh calls one subcommand per decision, so every
rule it applies is here and tested (scripts/release/test_power_cut_endurance.py):

  hold --seed S --cut N --what W --min A --max B   a seeded random whole number
  plan --seed S --duration D --min A --max B       the cut schedule a run would follow
  acked writer|redis FILE [--previous P]          the highest ACK or INCR in a log; fails
                                                   on none, or on less than P (the last read)
  seq FILE --acked N [--resumes F --starts S]      I1 from the writer's sequence check
                                                   and where it resumed (/data/resumes)
  counter FILE --acked N [--previous P] [--resumes F --starts S]
                                                   I2 from `redis-cli GET`, and where the
                                                   client found the counter (soak:starts)
  journal FILE                                     I4 from Bun's journal since boot
  instances FILE --want APP=N[,APP=N]              I7 from `relish apps --instances`
  record --evidence DIR --cut N --detail JSON      judge one cut, append it to cuts.jsonl
  summary --evidence DIR                           summary.json from run.json and cuts.jsonl;
                                                   "aborted" unless every cut made was recorded

Every invariant check prints a JSON object with a `pass` field. The numbering
(I1, I2, I4, I6, I7) is the plan's,
docs/plans/2026-11-18-plan-power-cut-endurance.md.
"""
import argparse
import json
import math
import os
import random
import re
import sys

SCHEMA = "reliaburger.power-cut-endurance/1"
# I6: Bun answers within 120 s of the VM's power-on, and the cluster has
# settled (every desired app running) within 300 s (D-3, provisional).
API_BOUND_SECONDS = 120
SETTLE_BOUND_SECONDS = 300


def hold(seed, cut, what, low, high):
    """A whole number in [LOW, HIGH] that depends only on the seed, the cut and WHAT.

    Seeding each choice by name keeps a resumed run on the same schedule, and
    adding a new choice later doesn't shift the existing ones.
    """
    return random.Random(f"{seed}:{cut}:{what}").randint(low, high)


def plan(seed, duration, low, high, cycle_overhead=180):
    """The cuts a run of DURATION seconds would make: (cut, minute it falls at, hold).

    CYCLE_OVERHEAD is a guess at the off time, recovery and checks per cut;
    the real schedule runs by the clock, so this is for --plan only.
    """
    elapsed, cut, cuts = 0, 0, []
    while True:
        cut += 1
        seconds = hold(seed, cut, "work", low, high)
        if elapsed + seconds > duration:
            return cuts
        elapsed += seconds
        cuts.append((cut, elapsed // 60, seconds))
        elapsed += cycle_overhead


def acked(kind, text):
    """The highest acknowledged value in a log: `ACK n` (writer) or `INCR n` (Redis client)."""
    pattern = re.compile(r"\bACK (\d+)\b" if kind == "writer" else r"\bINCR (\d+)\b")
    values = [int(match.group(1)) for match in pattern.finditer(text)]
    return max(values) if values else 0


def acked_problem(kind, value, previous):
    """Why VALUE can't be the last acknowledged value before a cut, or None when it can.

    Zero means the log couldn't be read or held no ACK (INCR) at all, and a
    value below PREVIOUS, the last one read, means the log was cut short or
    came from somewhere else. Either would make I1 or I2 hold whatever the cut
    did, so the harness mustn't cut on it.
    """
    word = "ACK" if kind == "writer" else "INCR"
    if value == 0:
        return f"no {word} in the log"
    if previous is not None and value < previous:
        return f"the last {word} read, {value}, is below the {previous} read before"
    return None


def seq_check(text, acked_value):
    """I1: the writer's awk summary says /data/seq is 1..N with N at least the last ACK.

    The awk (from qualify-sustained.sh) prints `BAD line: value` for the first
    line that isn't its own number, `DUP`, `MISSING`, `MAX n` and `LAST n`.
    """
    fields = {}
    for line in text.splitlines():
        parts = line.split(None, 1)
        if parts and parts[0] in ("BAD", "DUP", "MISSING", "MAX", "LAST"):
            fields[parts[0]] = parts[1] if len(parts) > 1 else ""
    result = {"acked": acked_value}
    if "MAX" not in fields or "LAST" not in fields:
        return {**result, "pass": False, "reason": "no sequence summary (the writer wasn't reachable)"}
    maximum, last = int(fields["MAX"]), int(fields["LAST"])
    result.update({"max": maximum, "lines": last})
    for problem in ("BAD", "DUP", "MISSING"):
        if problem in fields:
            return {**result, "pass": False, "reason": f"{problem} {fields[problem]}"}
    if maximum < acked_value:
        return {**result, "pass": False, "reason": f"the file ends at {maximum}, below the acknowledged {acked_value}"}
    return {**result, "pass": True}


def counter_check(text, acked_value, previous):
    """I2: the Redis counter is at least the last acknowledged INCR and never went backwards."""
    value = text.strip().splitlines()[-1].strip() if text.strip() else ""
    result = {"acked": acked_value, "previous": previous}
    if not value.isdigit():
        return {**result, "pass": False, "reason": f"GET answered {value!r}"}
    counter = int(value)
    result["counter"] = counter
    if counter < acked_value:
        return {**result, "pass": False, "reason": f"counter {counter} is below the acknowledged {acked_value}"}
    if previous is not None and counter < previous:
        return {**result, "pass": False, "reason": f"counter {counter} went back from {previous}"}
    return {**result, "pass": True}


def resume_check(text, acked_value, previous_starts):
    """Where the writer (or the Redis client) resumed at each start since the previous check.

    TEXT is one number per start, oldest first: the last whole line the
    writer found in /data/seq, or the counter the client read before its
    first INCR. There must be a start since the previous check
    (PREVIOUS_STARTS starts), or nothing restarted and the check proves
    nothing, and every one of those new starts must be at least ACKED_VALUE:
    a start during the settle that resumed higher doesn't make up for the
    cut's own start losing writes. PREVIOUS_STARTS is None for the baseline
    before the first cut, when every start is new.

    The file and counter checks alone can't see a loss: by the time the
    cluster settles, the writer has appended hundreds of lines past the lost
    ones and the client has counted past them.
    """
    values = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        if not line.isdigit():
            return {"acked": acked_value, "pass": False, "reason": f"the resume record holds {line!r}"}
        values.append(int(line))
    result = {"acked": acked_value, "starts": len(values), "previous_starts": previous_starts}
    if not values:
        return {**result, "pass": False, "reason": "no start recorded"}
    result["resumed_at"] = values[-1]
    first_new = previous_starts or 0
    result["new_starts"] = values[first_new:]
    if previous_starts is not None and len(values) <= previous_starts:
        return {**result, "pass": False,
                "reason": f"no new start since the last check ({len(values)} starts, {previous_starts} before)"}
    for number, value in enumerate(values[first_new:], first_new + 1):
        if value < acked_value:
            return {**result, "pass": False,
                    "reason": f"start {number} resumed at {value}, below the acknowledged {acked_value}"}
    return {**result, "pass": True}


def with_resume(result, resume):
    """One invariant from a value check and a resume check: both must pass."""
    merged = {**result, "resume": resume}
    if not result.get("pass"):
        return merged
    if not resume.get("pass"):
        return {**merged, "pass": False, "reason": resume["reason"]}
    return merged


# Lines in Bun's journal that mean a store didn't open cleanly or Bun crashed.
# The store pattern names stores, not any failure to open: 0.2.8's Bun also
# logs "failed to open cgroup ..." (eBPF), "failed to open stdout log"
# (process workloads) and Lettuce's "repaired drift with ...", none of which
# is a store that a cut damaged. redb's errors read "DB corrupted: ...",
# "Database repair aborted." and, through `{:?}`, `StorageError`/`DatabaseError`.
JOURNAL_PROBLEMS = (
    ("panic", re.compile(r"panicked at|RUST_BACKTRACE")),
    ("store", re.compile(
        r"(?i)\bcorrupt\w*"
        r"|\b(DatabaseError|StorageError)\b"
        r"|\b(database|store|db) repair"
        r"|\brepair(ing|ed)? (the )?(database|store|raft|log|ledger)\b"
        r"|\bfailed to open\b[^:]{0,40}?\b(store|database|ledger|index|catalog|blob|snapshot|raft)\b")),
)
JOURNAL_START = re.compile(r"Started reliaburger\.service|Started .*[Rr]eliaburger")
JOURNAL_EXIT = re.compile(r"reliaburger\.service: (Main process exited|Failed with result|Scheduled restart)")


def journal_check(text):
    """I4: Bun started once since boot, never exited, and logged no panic or store error."""
    starts = len(JOURNAL_START.findall(text))
    exits = len(JOURNAL_EXIT.findall(text))
    problems = {}
    for name, pattern in JOURNAL_PROBLEMS:
        lines = [line.strip() for line in text.splitlines() if pattern.search(line)]
        if lines:
            problems[name] = lines[:5]
    result = {"starts": starts, "exits": exits, "problems": problems}
    if starts != 1:
        return {**result, "pass": False, "reason": f"Bun started {starts} times since boot, not once"}
    if exits:
        return {**result, "pass": False, "reason": f"Bun exited {exits} times since boot"}
    if problems:
        return {**result, "pass": False, "reason": "problem lines in Bun's journal: " + ", ".join(problems)}
    return {**result, "pass": True}


def instances_check(instances, want):
    """I7: every desired app has exactly its replica count running, and nothing else of it runs.

    An instance in any state but running or stopped (pending, failed,
    unhealthy) means the app hasn't settled.
    """
    running, other = {}, {}
    for instance in instances:
        app = instance.get("app_name")
        state = instance.get("state")
        if state == "running":
            running[app] = running.get(app, 0) + 1
        elif state != "stopped":
            other.setdefault(app, []).append(state)
    problems = []
    for app, count in sorted(want.items()):
        if running.get(app, 0) != count:
            problems.append(f"{app}: {running.get(app, 0)} running, want {count}")
        if app in other:
            problems.append(f"{app}: also {', '.join(sorted(other[app]))}")
    result = {"running": running, "want": want}
    if problems:
        return {**result, "pass": False, "reason": "; ".join(problems)}
    return {**result, "pass": True}


def timing_check(recovery):
    """I6: Bun answered and the cluster settled within the provisional bounds (D-3)."""
    api, settled = recovery.get("api_seconds"), recovery.get("settled_seconds")
    result = {"api_seconds": api, "settled_seconds": settled,
              "api_bound": API_BOUND_SECONDS, "settle_bound": SETTLE_BOUND_SECONDS}
    if api is None or api > API_BOUND_SECONDS:
        return {**result, "pass": False, "reason": f"Bun answered after {api} s (bound {API_BOUND_SECONDS} s)"}
    if settled is None or settled > SETTLE_BOUND_SECONDS:
        return {**result, "pass": False,
                "reason": f"the cluster settled after {settled} s (bound {SETTLE_BOUND_SECONDS} s)"}
    return {**result, "pass": True}


def judge(cut, detail, invariants):
    """One cut's line for cuts.jsonl: the detail, every invariant's result and the violations."""
    invariants = dict(invariants)
    invariants["I6"] = timing_check(detail.get("recovery", {}))
    violations = sorted(name for name, result in invariants.items() if not result.get("pass"))
    return {"cut": cut, **detail, "invariants": invariants, "violations": violations}


def bound95(cuts):
    """The 95% upper bound on the per-cut failure rate after CUTS cuts with no failure."""
    return None if cuts == 0 else 1 - 0.05 ** (1 / cuts)


def percentile(values, fraction):
    """Nearest-rank percentile; None for no values."""
    if not values:
        return None
    ordered = sorted(values)
    return ordered[max(0, math.ceil(fraction * len(ordered)) - 1)]


def summarise(run, lines, unreadable=0):
    """summary.json: per topology and pooled counts and bounds, recovery times and the verdict.

    The run is aborted, not passed, when the harness stopped early (run.json's
    `aborted`), when a cut it made (`cuts_started`) has no line in cuts.jsonl,
    or when UNREADABLE lines of it couldn't be parsed. A violation still
    fails it.
    """
    topologies = {}
    retries = 0
    for line in lines:
        topology = topologies.setdefault(str(line.get("topology", "1")), {
            "cuts": 0, "violations": 0, "by_kind": {}, "api": [], "settled": [], "violated": []})
        topology["cuts"] += 1
        topology["by_kind"][line.get("kind", "site")] = topology["by_kind"].get(line.get("kind", "site"), 0) + 1
        recovery = line.get("recovery", {})
        if recovery.get("api_seconds") is not None:
            topology["api"].append(recovery["api_seconds"])
        if recovery.get("settled_seconds") is not None:
            topology["settled"].append(recovery["settled_seconds"])
        retries += recovery.get("start_retries", 0)
        if line.get("violations"):
            topology["violations"] += 1
            topology["violated"].append({"cut": line["cut"], "violations": line["violations"]})
    result = {}
    for name, topology in sorted(topologies.items()):
        result[name] = {
            "cuts": topology["cuts"],
            "violations": topology["violations"],
            "bound95": bound95(topology["cuts"]) if topology["violations"] == 0 else None,
            "by_kind": topology["by_kind"],
            "api_seconds": {"p50": percentile(topology["api"], 0.5), "p95": percentile(topology["api"], 0.95),
                            "max": max(topology["api"], default=None)},
            "recovery_seconds": {"p50": percentile(topology["settled"], 0.5),
                                 "p95": percentile(topology["settled"], 0.95),
                                 "max": max(topology["settled"], default=None)},
            "violated_cuts": topology["violated"],
        }
    cuts = sum(t["cuts"] for t in result.values())
    violations = sum(t["violations"] for t in result.values())
    aborted = run.get("aborted") or None
    if aborted is None and unreadable:
        aborted = f"{unreadable} unreadable lines in cuts.jsonl"
    if aborted is None and run.get("cuts_started", 0) != cuts:
        aborted = f"{run.get('cuts_started', 0)} cuts made, {cuts} recorded"
    if violations:
        verdict = "fail"
    elif aborted:
        verdict = "aborted"
    elif cuts == 0:
        verdict = "no cuts"
    else:
        verdict = "pass"
    return {
        "schema": SCHEMA,
        **{key: run.get(key) for key in ("release", "relish", "bun", "host", "hypervisor", "tier", "seed",
                                         "started", "ended", "paused_seconds")},
        "topologies": result,
        "pooled": {"cuts": cuts, "violations": violations,
                   "bound95": bound95(cuts) if violations == 0 and not aborted else None},
        "environment_events": {"vz_start_retries": retries},
        "aborted": aborted,
        "verdict": verdict,
    }


def read_lines(path):
    """cuts.jsonl's lines, and how many of them couldn't be parsed."""
    if not os.path.exists(path):
        return [], 0
    lines, unreadable = [], 0
    with open(path) as file:
        for text in file:
            if not text.strip():
                continue
            try:
                lines.append(json.loads(text))
            except json.JSONDecodeError:
                unreadable += 1
    return lines, unreadable


def read_json(path, default):
    """The JSON in PATH; DEFAULT for a missing file, and a failing check for an unreadable one."""
    if not os.path.exists(path):
        return default
    try:
        value = json.loads(read_text(path))
    except (OSError, json.JSONDecodeError) as error:
        return {"pass": False, "reason": f"unreadable check output: {error}"}
    if not isinstance(value, dict):
        return {"pass": False, "reason": f"unreadable check output: {value!r}"}
    return value


def read_text(path):
    with open(path, errors="replace") as file:
        return file.read()


def parse_want(text):
    want = {}
    for item in text.split(","):
        app, _, count = item.partition("=")
        if not app or not count.isdigit():
            raise SystemExit(f"--want takes APP=N[,APP=N], not {text!r}")
        want[app] = int(count)
    return want


def emit(result):
    print(json.dumps(result))
    return 0 if result.get("pass", True) else 1


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    command = commands.add_parser("hold")
    for name in ("seed", "cut", "min", "max"):
        command.add_argument(f"--{name}", type=int, required=True)
    command.add_argument("--what", required=True)
    command = commands.add_parser("plan")
    for name in ("seed", "duration", "min", "max"):
        command.add_argument(f"--{name}", type=int, required=True)
    command = commands.add_parser("acked")
    command.add_argument("kind", choices=("writer", "redis"))
    command.add_argument("file")
    command.add_argument("--previous", type=int, help="the value read before this one")
    command = commands.add_parser("seq")
    command.add_argument("file")
    command.add_argument("--acked", type=int, required=True)
    command.add_argument("--resumes", help="one resume point per start, oldest first")
    command.add_argument("--starts", type=int, help="starts recorded at the previous check")
    command = commands.add_parser("counter")
    command.add_argument("file")
    command.add_argument("--acked", type=int, required=True)
    command.add_argument("--previous", type=int)
    command.add_argument("--resumes", help="the counter each client start found, oldest first")
    command.add_argument("--starts", type=int, help="starts recorded at the previous check")
    command = commands.add_parser("resumed")
    command.add_argument("file")
    command.add_argument("--acked", type=int, required=True)
    command.add_argument("--starts", type=int)
    command = commands.add_parser("journal")
    command.add_argument("file")
    command = commands.add_parser("instances")
    command.add_argument("file")
    command.add_argument("--want", required=True)
    command = commands.add_parser("record")
    command.add_argument("--evidence", required=True)
    command.add_argument("--cut", type=int, required=True)
    command.add_argument("--detail", required=True, help="JSON: kind, timings, acked values, recovery")
    command = commands.add_parser("summary")
    command.add_argument("--evidence", required=True)
    arguments = parser.parse_args(argv)

    if arguments.command == "hold":
        print(hold(arguments.seed, arguments.cut, arguments.what, arguments.min, arguments.max))
        return 0
    if arguments.command == "plan":
        for cut, minute, seconds in plan(arguments.seed, arguments.duration, arguments.min, arguments.max):
            print(f"cut {cut}: after {seconds} s of work, about minute {minute}")
        return 0
    if arguments.command == "acked":
        value = acked(arguments.kind, read_text(arguments.file))
        print(value)
        problem = acked_problem(arguments.kind, value, arguments.previous)
        if problem:
            print(f"power-cut: {problem}", file=sys.stderr)
            return 1
        return 0
    if arguments.command == "seq":
        result = seq_check(read_text(arguments.file), arguments.acked)
        if arguments.resumes:
            result = with_resume(result, resume_check(read_text(arguments.resumes), arguments.acked,
                                                      arguments.starts))
        return emit(result)
    if arguments.command == "counter":
        result = counter_check(read_text(arguments.file), arguments.acked, arguments.previous)
        if arguments.resumes:
            result = with_resume(result, resume_check(read_text(arguments.resumes), arguments.acked,
                                                      arguments.starts))
        return emit(result)
    if arguments.command == "resumed":
        return emit(resume_check(read_text(arguments.file), arguments.acked, arguments.starts))
    if arguments.command == "journal":
        return emit(journal_check(read_text(arguments.file)))
    if arguments.command == "instances":
        try:
            instances = json.loads(read_text(arguments.file))
        except json.JSONDecodeError:
            return emit({"pass": False, "reason": "`relish apps --instances` gave no JSON"})
        return emit(instances_check(instances, parse_want(arguments.want)))
    if arguments.command == "record":
        cut_directory = os.path.join(arguments.evidence, "cuts", f"{arguments.cut:05d}")
        # Nothing here may stop the line reaching cuts.jsonl: an input that
        # can't be read is a failing check, not a crash.
        invariants = {name: read_json(os.path.join(cut_directory, f"{name}.json"),
                                      {"pass": False, "reason": "not checked"})
                      for name in ("I1", "I2", "I4", "I7")}
        try:
            detail = json.loads(arguments.detail)
            if not isinstance(detail, dict):
                raise ValueError(f"not an object: {detail!r}")
        except ValueError as error:
            detail = {"detail_error": f"unreadable --detail: {error}"}
        line = judge(arguments.cut, detail, invariants)
        with open(os.path.join(arguments.evidence, "cuts.jsonl"), "a") as file:
            file.write(json.dumps(line) + "\n")
            file.flush()
            os.fsync(file.fileno())
        print(json.dumps({"cut": line["cut"], "violations": line["violations"]}))
        return 1 if line["violations"] else 0
    if arguments.command == "summary":
        with open(os.path.join(arguments.evidence, "run.json")) as file:
            run = json.load(file)
        lines, unreadable = read_lines(os.path.join(arguments.evidence, "cuts.jsonl"))
        summary = summarise(run, lines, unreadable)
        with open(os.path.join(arguments.evidence, "summary.json"), "w") as file:
            json.dump(summary, file, indent=2)
            file.write("\n")
        print(json.dumps({"verdict": summary["verdict"], "cuts": summary["pooled"]["cuts"]}))
        return 0 if summary["verdict"] == "pass" else 1
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
