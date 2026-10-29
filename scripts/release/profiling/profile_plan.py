#!/usr/bin/env python3
"""The release profile step's session plan and its time estimate.

  profile_plan.py --scenarios raw,host,shared,idle --rounds 3 --window 60
      [--budget 7200] [--format text|sessions]

Every session runs on a fresh boot of the bench VM (a Bun that follows another
on the same boot fails on the old node's mounts). A rate session runs one
version's scenarios once; round r runs both versions back to back, and the
order alternates every round (previous first in odd rounds), so drift on the
host hits both builds alike. Then one profiling session per version runs the
perf windows with the profiling build.

If the estimate is over the budget (2 hours, maintainer decision, 26 October
2026), rounds are dropped, never scenarios, down to one, and the plan says so.

`--format sessions` prints one line per session for qualify-profile.sh:
"rates LABEL ROUND" or "perf LABEL".
"""
import argparse

LABELS = ("previous", "candidate")
RATE_SCENARIOS = ("raw", "host", "shared", "fresh")
KNOWN_SCENARIOS = RATE_SCENARIOS + ("idle", "control")
# The maintainer's default set (26 October 2026): fresh containers move to the
# nightly run, the control plane comes later. Both still run on request.
DEFAULT_SCENARIOS = ("raw", "host", "shared", "idle")
# The windows of a profiling session, in order.
PERF_WINDOWS = ("perfcpu-host", "perfoff-host", "perfsched-host", "probe-host", "perfcpu-idle")
BUDGET_SECONDS = 2 * 3600

# Seconds, measured on the 0.2.3 record's rig and rounded up.
VM_CREATE = 420  # create, first boot, apt
BOOT = 45  # stop and start for a fresh boot
SETUP = 45  # init, Bun up, token, web app applied
SETTLE = 30  # the web app's settle before the first window
WINDOW_GAP = 12  # bench's drain and the pause between windows
CONTROL = 240  # relish bench --quick and the API round trips
PERF_POST = 180  # perf script and folding, inside the VM
COPY = 60  # evidence back to the host, and the analysis


def parse_scenarios(text):
    names = [name.strip() for name in text.split(",") if name.strip()]
    unknown = [name for name in names if name not in KNOWN_SCENARIOS]
    if unknown:
        raise ValueError(f"unknown scenario {unknown[0]!r}; known: {', '.join(KNOWN_SCENARIOS)}")
    if not names:
        raise ValueError("no scenarios")
    if len(set(names)) != len(names):
        raise ValueError("a scenario is named twice")
    return names


def session_seconds(scenarios, window, settle=SETTLE):
    seconds = BOOT + SETUP + settle
    for name in scenarios:
        if name in RATE_SCENARIOS:
            seconds += window + WINDOW_GAP
        elif name == "idle":
            seconds += window + 5
        elif name == "control":
            seconds += CONTROL
    return seconds


def perf_session_seconds(window, settle=SETTLE):
    return BOOT + SETUP + settle + 4 * (window + WINDOW_GAP) + window + 5 + PERF_POST


def estimate(scenarios, rounds, window, perf=True, settle=SETTLE):
    """Wall time in seconds for the whole step, both versions."""
    seconds = VM_CREATE + COPY + rounds * len(LABELS) * session_seconds(scenarios, window, settle)
    if perf:
        seconds += len(LABELS) * perf_session_seconds(window, settle)
    return seconds


def fit(scenarios, rounds, window, budget=BUDGET_SECONDS, perf=True, settle=SETTLE):
    """Rounds that fit the budget: (rounds kept, rounds dropped). Never fewer than one."""
    kept = rounds
    while kept > 1 and estimate(scenarios, kept, window, perf, settle) > budget:
        kept -= 1
    return kept, rounds - kept


def sessions(rounds, perf=True):
    """("rates", label, round) for every round, alternating order; then ("perf", label, None)."""
    out = []
    for number in range(1, rounds + 1):
        order = LABELS if number % 2 == 1 else tuple(reversed(LABELS))
        out += [("rates", label, number) for label in order]
    if perf:
        out += [("perf", label, None) for label in LABELS]
    return out


def describe(scenarios, rounds, window, budget, perf, settle):
    kept, dropped = fit(scenarios, rounds, window, budget, perf, settle)
    total = estimate(scenarios, kept, window, perf, settle)
    lines = [f"scenarios: {', '.join(scenarios)}",
             f"rounds: {kept} per version, alternating order, {window} s windows"]
    if dropped:
        lines.append(f"dropped: {dropped} of {rounds} rounds, to fit the {budget // 60}-minute budget")
    lines.append(f"estimated wall time: {total // 60} min {total % 60} s (budget {budget // 60} min)")
    for kind, label, number in sessions(kept, perf):
        if kind == "rates":
            lines.append(f"  round {number}: {label}, fresh boot, {', '.join(scenarios)}")
        else:
            lines.append(f"  profile: {label}, fresh boot, profiling build, {', '.join(PERF_WINDOWS)}")
    return lines, kept, dropped, total


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--scenarios", default=",".join(DEFAULT_SCENARIOS))
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--window", type=int, default=60)
    parser.add_argument("--settle", type=int, default=SETTLE)
    parser.add_argument("--budget", type=int, default=BUDGET_SECONDS)
    parser.add_argument("--no-perf", action="store_true")
    parser.add_argument("--format", choices=["text", "sessions", "shell"], default="text")
    args = parser.parse_args()
    if args.rounds < 1 or args.window < 10:
        parser.error("--rounds must be at least 1 and --window at least 10")
    try:
        scenarios = parse_scenarios(args.scenarios)
    except ValueError as error:
        parser.error(str(error))
    perf = not args.no_perf
    lines, kept, dropped, total = describe(scenarios, args.rounds, args.window, args.budget, perf, args.settle)
    if args.format == "text":
        print("\n".join(lines))
    elif args.format == "shell":
        print(f"plan_rounds={kept}\nplan_dropped={dropped}\nplan_estimate={total}")
    else:
        for kind, label, number in sessions(kept, perf):
            print(kind, label, number or "-")


if __name__ == "__main__":
    main()
