#!/usr/bin/env python3
"""The agent gym: Smoker faults with ground truth, and a solver to score.

Each scenario (scripts/gym/scenarios/*.toml) starts a throwaway cluster, sets
it up, breaks it, hands a solver the prompt and a read-only token, and scores
the answer against what the runner knows went wrong. Runs repeat, and the
record reports distributions, never one pass or fail.

Subcommands:
  check                       parse every scenario and check each plant still applies
  run [--scenario ID]... [--solver scripted|command:PROGRAM] [--runs N] --out DIR
                              run scenarios on fresh local clusters; writes
                              DIR/results.json and DIR/record.md
  score RUN_DIR               score one recorded run again, from its files
  fixture RUN_DIR OUT         turn a run's recorded calls into a fixture for the
                              portable tests' fake relish

`make test-gym` runs every scenario once with the scripted baseline. The
0.2.7 exit test (#1093) runs `--solver command:...` with a real agent.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import time

import cluster as cluster_module
import plant as plant_module
import record
import scenario as scenario_module
import score as score_module
import shim
import solvers

DEFAULT_PORT_BASE = 29400
DEFAULT_TIMEOUT = 1800


def resolve_truth(truth, facts):
    """The scenario's truth with each `@` fact replaced by its live value."""
    resolved = {}
    for name, value in truth.items():
        if isinstance(value, str) and value.startswith("@"):
            resolved[name] = facts[value[1:]]
        else:
            resolved[name] = value
    return resolved


def run_setup(scenario, cluster, sleep=time.sleep):
    """Run the scenario's setup steps against `cluster`, as admin."""
    for step in scenario.setup:
        if step.apply:
            cluster.apply(step.apply)
            if step.wait_running:
                cluster.wait_running(step.wait_running)
        elif step.fault:
            cluster.inject(step.fault)
        elif step.settle_seconds:
            sleep(step.settle_seconds)


def symptom_control(cluster, plant, planted_relish):
    """Whether the planted bug shows with the planted binary and not the shipped one.

    This is the control that the scenario can fail: a run where the shipped
    binary shows the symptom too, or the planted one doesn't, tests nothing.
    """
    shipped = cluster.relish(list(plant.symptom))
    planted = cluster.relish(list(plant.symptom), binary=planted_relish)
    shipped_lines = len(shipped.stdout.splitlines())
    planted_lines = len(planted.stdout.splitlines())
    return {
        "command": ["relish", *plant.symptom],
        "shipped_lines": shipped_lines,
        "planted_lines": planted_lines,
        "shows_bug": shipped.returncode == 0 and shipped_lines == 0 and planted_lines > 0,
    }


def run_once(scenario, run_dir, make_cluster, solver, relish, planted=None, timeout=DEFAULT_TIMEOUT,
             sleep=time.sleep):
    """One run of one scenario; returns the run's result and writes its files.

    `make_cluster(nodes, work_dir)` returns an unstarted cluster;
    `planted` is (PlantBuild or None, planted relish path) for a planted
    scenario.
    """
    run_dir.mkdir(parents=True, exist_ok=True)
    started = time.time()
    result = {"scenario": scenario.id, "run": run_dir.name, "started": started}
    cluster = make_cluster(scenario.nodes, run_dir / "cluster")
    try:
        cluster.start()
        run_setup(scenario, cluster, sleep)
        truth = resolve_truth(scenario.truth, cluster.facts(scenario.facts_needed()))
        result["truth"] = truth
        solver_relish = relish
        if scenario.plant is not None:
            builder, solver_relish = planted
            control = symptom_control(cluster, scenario.plant, solver_relish)
            result["control"] = control
            if not control["shows_bug"]:
                raise cluster_module.ClusterError(
                    f"the planted bug didn't show: {json.dumps(control)}"
                )

        token_name = f"gym-solver-{run_dir.name}"
        token = cluster.create_token(token_name)
        solver_dir = run_dir / "solver"
        solver_dir.mkdir(exist_ok=True)
        call_log = run_dir / "calls.jsonl"
        answer_path = run_dir / "answer.json"
        prompt = solvers.full_prompt(scenario.prompt)
        prompt_path = run_dir / "prompt.md"
        prompt_path.write_text(prompt)
        extra = shim.install(run_dir / "bin", solver_relish)
        meta_path = run_dir / "solver-meta.json"
        env = solvers.environment(
            cluster.solver_env(token), run_dir / "bin", call_log, answer_path, prompt_path, extra,
            meta_path,
        )
        attempt = solver.solve(prompt, env, solver_dir, timeout, scenario)
        (run_dir / "solver.stdout").write_text(attempt.stdout)
        (run_dir / "solver.stderr").write_text(attempt.stderr)
        meta = solvers.read_meta(meta_path)
        if meta is not None:
            result["solver_meta"] = meta
        answer, answer_error = solvers.read_answer(answer_path)
        calls = shim.read_log(call_log)
        audit = cluster.audit(token_name)
        (run_dir / "audit.json").write_text(json.dumps(audit, indent=2) + "\n")

        patch_result = None
        if (
            "patch" in scenario.checks
            and planted is not None
            and planted[0] is not None
            and isinstance(answer, dict)
            and isinstance(answer.get("patch"), str)
            and answer["patch"].strip()
        ):
            patch_result, reason = planted[0].check_patch(
                answer["patch"], scenario.plant.reverted_tests
            )
            result["patch_reason"] = reason
        scored = score_module.score(scenario, truth, answer, calls, audit, patch_result)
        if answer_error:
            scored["answer_problems"].insert(0, answer_error)
        result.update(
            {
                "solver_exit": attempt.exit_code,
                "timed_out": attempt.timed_out,
                "seconds": round(attempt.seconds, 1),
                "patch_result": patch_result,
                "score": scored,
            }
        )
    except (cluster_module.ClusterError, plant_module.PlantError, subprocess.TimeoutExpired) as error:
        result["error"] = str(error)
    finally:
        cluster.stop()
    result["wall_seconds"] = round(time.time() - started, 1)
    (run_dir / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def rescore(run_dir):
    """Score a recorded run again from its files (deterministic)."""
    run_dir = Path(run_dir)
    result = json.loads((run_dir / "result.json").read_text())
    scenario = scenario_module.load(scenario_module.SCENARIO_DIR / f"{result['scenario']}.toml")
    answer, answer_error = solvers.read_answer(run_dir / "answer.json")
    audit = json.loads((run_dir / "audit.json").read_text())
    scored = score_module.score(
        scenario, result["truth"], answer, shim.read_log(run_dir / "calls.jsonl"), audit,
        result.get("patch_result"),
    )
    if answer_error:
        scored["answer_problems"].insert(0, answer_error)
    return scored


def command_check(_args):
    scenarios = scenario_module.load_all()
    failed = False
    for scenario in scenarios:
        line = f"{scenario.id}: {scenario.nodes} node(s), checks {', '.join(scenario.checks)}"
        if scenario.plant:
            applies, reason = plant_module.check_applies(scenario.plant.patch)
            line += "; plant applies" if applies else f"; PLANT DOESN'T APPLY: {reason}"
            failed |= not applies
        print(line)
    return 1 if failed else 0


def _binaries(args):
    target = Path(os.environ.get("CARGO_TARGET_DIR", scenario_module.GYM_DIR.parent.parent / "target"))
    found = {
        "relish": Path(args.relish or target / "debug" / "relish"),
        "bun": Path(args.bun or target / "debug" / "bun"),
        "testapp": Path(args.testapp or target / "debug" / "testapp"),
    }
    for name, path in found.items():
        if not path.is_file():
            raise SystemExit(f"no {name} at {path}: build it, or pass --{name}")
    return {name: path.resolve() for name, path in found.items()}


def command_run(args):
    scenarios = scenario_module.load_all()
    if args.scenario:
        unknown = set(args.scenario) - {s.id for s in scenarios}
        if unknown:
            raise SystemExit(f"unknown scenarios {sorted(unknown)}")
        scenarios = [s for s in scenarios if s.id in args.scenario]
    binaries = _binaries(args)
    solver = solvers.solver_from_spec(args.solver)
    out = Path(args.out).resolve()
    if out.exists() and any(out.iterdir()):
        raise SystemExit(f"{out} isn't empty: name a new directory")
    out.mkdir(parents=True, exist_ok=True)
    # The clusters' data stays out of the results directory, which keeps
    # only what a record needs.
    scratch = Path(tempfile.mkdtemp(prefix="gym-"))

    planted = {}
    for scenario in scenarios:
        if scenario.plant is None:
            continue
        builder = plant_module.PlantBuild(
            binaries["relish"], scenario.plant.patch, out / "plants" / scenario.id,
            Path(args.plant_target_dir), os.environ,
        )
        builder.work_dir.mkdir(parents=True, exist_ok=True)
        print(f"building the planted relish for {scenario.id}", flush=True)
        planted[scenario.id] = (builder if args.check_patch else None, builder.build())

    def make_cluster(nodes, work_dir):
        return cluster_module.LocalCluster(
            scratch / f"{work_dir.parent.parent.name}-{work_dir.parent.name}",
            nodes, args.port_base, binaries,
        )

    results = []
    try:
        for scenario in scenarios:
            for index in range(1, args.runs + 1):
                run_dir = out / scenario.id / f"run-{index}"
                print(f"{scenario.id} run {index}/{args.runs}", flush=True)
                result = run_once(
                    scenario, run_dir, make_cluster, solver, binaries["relish"],
                    planted.get(scenario.id), args.timeout,
                )
                verdict = result["score"]["verdict"] if "score" in result else f"error: {result['error']}"
                print(f"  {verdict}", flush=True)
                results.append(result)
    finally:
        if not args.keep:
            shutil.rmtree(scratch, ignore_errors=True)

    context = {
        "date": time.strftime("%Y-%m-%d", time.gmtime()),
        "solver": solver.describe(),
        "solver_kind": solver.name,
        "runs_per_scenario": args.runs,
        "host": f"{platform.machine()}, {platform.system()} {platform.release()}",
        "relish_version": subprocess.run(
            [str(binaries["relish"]), "--version"], capture_output=True, text=True, check=False
        ).stdout.strip(),
    }
    summary = score_module.summarise([r for r in results if "score" in r])
    document = {"context": context, "summary": summary, "runs": results}
    (out / "results.json").write_text(json.dumps(document, indent=2) + "\n")
    (out / "record.md").write_text(record.render(document, scenarios))
    print(f"results in {out / 'results.json'}, record in {out / 'record.md'}")
    errors = [r for r in results if "error" in r]
    return 1 if errors else 0


def command_score(args):
    print(json.dumps(rescore(args.run_dir), indent=2))
    return 0


def fixture_from_calls(calls):
    """A fake-relish fixture from a run's call log: each argv's last answer."""
    return {
        " ".join(call["argv"][1:]): {"stdout": call["stdout"], "exit": call["exit"]}
        for call in calls
    }


def command_fixture(args):
    calls = shim.read_log(Path(args.run_dir) / "calls.jsonl")
    if not calls:
        raise SystemExit(f"no calls recorded in {args.run_dir}")
    Path(args.out).write_text(json.dumps(fixture_from_calls(calls), indent=2) + "\n")
    print(f"{len(calls)} call(s) recorded into {args.out}")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("check", help="check every scenario and plant")
    run = sub.add_parser("run", help="run scenarios and write a record")
    run.add_argument("--scenario", action="append", help="a scenario id (default: all)")
    run.add_argument("--solver", default="scripted", help="scripted, or command:PROGRAM ARGS")
    run.add_argument("--runs", type=int, default=1, help="runs per scenario")
    run.add_argument("--out", required=True, help="a new directory for the results")
    run.add_argument("--port-base", type=int, default=DEFAULT_PORT_BASE,
                     help=f"first port of the clusters' block (default {DEFAULT_PORT_BASE})")
    run.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT,
                     help="seconds the solver gets per run")
    run.add_argument("--relish")
    run.add_argument("--bun")
    run.add_argument("--testapp")
    run.add_argument("--plant-target-dir", default=None,
                     help="cargo target directory for the planted build")
    run.add_argument("--check-patch", action="store_true",
                     help="apply each answer's patch to the planted source and run the reverted tests")
    run.add_argument("--keep", action="store_true", help="keep the clusters' work directories")
    score = sub.add_parser("score", help="score a recorded run again")
    score.add_argument("run_dir")
    fixture = sub.add_parser("fixture", help="turn a run's calls into a fake-relish fixture")
    fixture.add_argument("run_dir")
    fixture.add_argument("out")
    args = parser.parse_args(argv)
    if args.command == "run" and args.plant_target_dir is None:
        base = os.environ.get("CARGO_TARGET_DIR", str(scenario_module.GYM_DIR.parent.parent / "target"))
        args.plant_target_dir = base.rstrip("/") + "-planted"
    if args.command == "run" and args.runs < 1:
        parser.error("--runs must be at least 1")
    commands = {
        "check": command_check,
        "run": command_run,
        "score": command_score,
        "fixture": command_fixture,
    }
    return commands[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
