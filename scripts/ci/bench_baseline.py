#!/usr/bin/env python3
"""Compare a Criterion run with the baseline main saved, and save new baselines.

The task-array benchmarks (benches/task_arrays.rs) run nightly on hosted
runners, whose speed varies from run to run. Criterion's own change report
flags differences of a few per cent, which would make a gate a flake source,
and on 12 October 2026 a runner was uniformly 1.4-2.1x slower than the one
that recorded the baseline (#857). So CI fails only when a benchmark is more
than `--max-ratio` times slower than the rest of the same run (see compare),
or the whole run's median is. Everything is kept as an artefact for the trend.

    bench_baseline.py save    --criterion target/criterion --baseline FILE
    bench_baseline.py compare --criterion target/criterion --baseline FILE \\
                              [--max-ratio 2] [--summary $GITHUB_STEP_SUMMARY]

A benchmark is named by its directory under target/criterion
(`group/function[/parameter]`); its result is `new/estimates.json`.

Fixtures: scripts/ci/test_bench_baseline.py.
"""
import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import statistics
import sys
from typing import Optional

SCHEMA = 1


def read_means(criterion):
    """{benchmark name: mean in nanoseconds} for every benchmark Criterion ran."""
    means = {}
    for estimates in sorted(Path(criterion).rglob("new/estimates.json")):
        name = estimates.parent.parent.relative_to(criterion).as_posix()
        means[name] = float(json.loads(estimates.read_text())["mean"]["point_estimate"])
    if not means:
        raise ValueError(f"no Criterion results under {criterion}")
    return means


def save(criterion, baseline):
    """Write this run's means as the baseline for later runs."""
    baseline = Path(baseline)
    baseline.parent.mkdir(parents=True, exist_ok=True)
    baseline.write_text(json.dumps({"schema": SCHEMA, "benchmarks": read_means(criterion)},
                                   indent=2, sort_keys=True) + "\n")


def load(baseline):
    """The baseline's means, or None when no baseline has been saved yet."""
    path = Path(baseline)
    if not path.exists():
        return None
    document = json.loads(path.read_text())
    if document.get("schema") != SCHEMA:
        raise ValueError(f"{path}: unknown baseline schema {document.get('schema')!r}")
    return {name: float(mean) for name, mean in document["benchmarks"].items()}


@dataclass
class Comparison:
    """One run measured against the baseline.

    `rows` are (name, baseline mean or None, current mean, ratio or None,
    ratio against the rest or None). `runner` is the median ratio, how much
    slower this runner was overall, or None when nothing compares.
    """
    rows: list
    runner: Optional[float]
    regressions: list
    run_too_slow: bool

    @property
    def failed(self):
        return bool(self.regressions) or self.run_too_slow


def compare(current, baseline, max_ratio):
    """Measure each benchmark against its baseline, allowing for the runner's speed.

    A hosted runner can be uniformly slower than the one that recorded the
    baseline, so each benchmark's ratio is divided by the median ratio of the
    other benchmarks in the same run, its "ratio against the rest", and a
    regression is a benchmark more than `max_ratio` times slower than the
    rest. A faster runner never tightens the bound: the median counts as at
    least 1. A slow runner and a regression in most benchmarks look the same
    from inside one run, so the run also fails when its median ratio is above
    `max_ratio`. Benchmarks that are new, or gone, have nothing to compare.
    """
    ratios = {name: mean / baseline[name] for name, mean in current.items()
              if baseline.get(name)}
    rows, regressions = [], []
    for name, mean in sorted(current.items()):
        ratio = ratios.get(name)
        relative = None
        if ratio is not None:
            rest = [other for key, other in ratios.items() if key != name]
            relative = ratio / max(1.0, statistics.median(rest) if rest else 1.0)
            if relative > max_ratio:
                regressions.append(name)
        rows.append((name, baseline.get(name), mean, ratio, relative))
    runner = statistics.median(ratios.values()) if ratios else None
    return Comparison(rows, runner, regressions, runner is not None and runner > max_ratio)


def table(comparison, max_ratio):
    runner = comparison.runner
    runner_text = f"runner factor {runner:.2f}" if runner is not None else "no runner factor"
    lines = [f"Task-array benchmarks against main's baseline, {runner_text} (the median ratio). "
             f"A benchmark fails above {max_ratio:g}x the rest of the run, and the run fails "
             f"when its median is above {max_ratio:g}x.", "",
             "| Benchmark | Baseline mean | This run | Ratio | Against the rest |",
             "|---|---:|---:|---:|---:|"]
    for name, before, mean, ratio, relative in comparison.rows:
        before_text = f"{before / 1e6:.3f} ms" if before is not None else "none"
        ratio_text = f"{ratio:.2f}" if ratio is not None else "new"
        relative_text = f"{relative:.2f}" if relative is not None else "new"
        lines.append(f"| `{name}` | {before_text} | {mean / 1e6:.3f} ms | {ratio_text} "
                     f"| {relative_text} |")
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("save", "compare"):
        command = commands.add_parser(name)
        command.add_argument("--criterion", type=Path, required=True)
        command.add_argument("--baseline", type=Path, required=True)
        if name == "compare":
            command.add_argument("--max-ratio", type=float, default=2.0)
            command.add_argument("--summary", type=Path)
    arguments = parser.parse_args(argv)

    if arguments.command == "save":
        save(arguments.criterion, arguments.baseline)
        print(f"saved the baseline to {arguments.baseline}")
        return 0

    current = read_means(arguments.criterion)
    baseline = load(arguments.baseline)
    if baseline is None:
        print(f"no baseline at {arguments.baseline} yet; nothing to compare")
        baseline = {}
    comparison = compare(current, baseline, arguments.max_ratio)
    report = table(comparison, arguments.max_ratio)
    print(report)
    if arguments.summary:
        with arguments.summary.open("a") as summary:
            summary.write(report)
    for name in comparison.regressions:
        print(f"{name}: more than {arguments.max_ratio:g}x slower than main's baseline, "
              "against the rest of this run", file=sys.stderr)
    if comparison.run_too_slow:
        print(f"the whole run is {comparison.runner:.2f}x slower than main's baseline "
              f"(its median), above {arguments.max_ratio:g}x", file=sys.stderr)
    return 1 if comparison.failed else 0


if __name__ == "__main__":
    sys.exit(main())
