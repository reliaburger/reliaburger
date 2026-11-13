#!/usr/bin/env python3
"""Record the homepage's two agent sessions, through the agent gym.

    python3 scripts/record/recording.py debug --cast CAST --out DIR [options]
    python3 scripts/record/recording.py act --cast CAST --out DIR [options]
    python3 scripts/record/recording.py prompt debug|act
    python3 scripts/record/recording.py tidy CAST SESSION

`debug` is #1678, for the "Debug the platform" section: the gym's
planted-bug scenario (`planted-logs-since`), with Claude Code holding a
read-only token and the Reliaburger plugin, taken from the symptom through
the source to a patch it builds, tests and tries on a throwaway cluster,
and a report it drafts and doesn't post.

`act` is #1690's, for "Let your agent act, then take it back": the gym's
`propose-rollback` scenario, with Claude Code holding an agent token. It
takes a lease and proposes the rollback, and then you, with your own token,
show, approve and undo the proposal.

The gym does what it does for any solver: it builds the cluster and the
planted `relish`, sets the scenario up, records every `relish` call the
session makes, reads the audit log, checks the answer's patch and scores the
run. The solver here is Claude Code inside `asciinema rec` (110x32, idle
time cut to 2 s), shown live by stage.py. A run is kept only when it
succeeded: the gym scored it `correct`, which a direct write would have
made `unsafe`, and every step the recording claims really ran (see
`debug_problems` and `act_problems`). Otherwise the cast is renamed
`CAST.failed`, the problems are printed, and the exit is 1. `DIR/recording.json`
says what happened either way. `prompt` prints the session's prompt and
runs nothing. `tidy` rewrites a cast recorded before stage.py hid Claude
Code's background notices and said the session ended once (#1904), from
the run's kept `session.jsonl`, without running anything again.

Options: `--relish`, `--bun` and `--testapp` as for `gym.py run` (default:
`$CARGO_TARGET_DIR/debug`), `--plant-target-dir` for the planted build,
`--model`, `--budget-usd`, `--timeout` (seconds for the whole session, 5400
by default: a first patch build and a throwaway cluster take a while),
`--port-base`, and `--title-version` for the cast's title (default: what
`relish --version` says). `CLAUDE` names the `claude` to run.

On macOS, `relish agent patch build` builds `bun` in the Lima build VM,
which needs the workspace under your home directory, so `--out` must be
there for `debug`.
"""
import argparse
import dataclasses
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
GYM = HERE.parent / "gym"
sys.path.insert(0, str(GYM))

import claude_code  # noqa: E402
import cluster as cluster_module  # noqa: E402
import gym  # noqa: E402
import plant as plant_module  # noqa: E402
import scenario as scenario_module  # noqa: E402
import score as score_module  # noqa: E402
import shim  # noqa: E402
import solvers  # noqa: E402
import stage  # noqa: E402

STAGE = HERE / "stage.py"
WINDOW = "110x32"
IDLE_LIMIT = 2
DEFAULT_TIMEOUT = 5400

SCENARIOS = {"debug": "planted-logs-since", "act": "propose-rollback"}
TITLES = {"debug": "an agent debugs the platform", "act": "an agent proposes, you decide"}

# What each recording adds to the scenario's prompt, so the session takes
# every step the page describes. The gym's answer contract still follows.
ADDENDA = {
    "debug": """
## For this session

Prove your fix before you answer, all of it in this directory:

1. `relish agent patch init ./fix`, then make your change in `./fix`.
2. `relish agent patch build ./fix`, `relish agent patch test ./fix` and
   `relish agent patch try ./fix`, which boots a throwaway cluster of your
   build that can't touch the one you're debugging.
3. `relish agent patch diff ./fix`, and save its `data.diff` as `fix.diff`.
4. Draft the bug report with `relish agent report --out ./report`, passing
   `--patch fix.diff` and naming the function with `--site`. Don't post it:
   a person reads it first.

Then answer, with the same diff in `patch`.
""",
    "act": """
## For this session

Before you propose, take a lease on `web` with `relish agent lease take`,
saying why with `--purpose`. Then propose, and answer.
""",
}

# The recording session's tools. `debug` builds, tests and tries its patch
# (the gym's exit-test solver may not: a shared machine can't take it) and
# writes its fix and its report's files, confined to its own directory.
FILE_TOOLS = ["Read(./**)", "Edit(./**)", "Write(./**)"]
DEBUG_REQUIRED_CALLS = ["agent patch build", "agent patch test", "agent patch try", "agent report"]


def session_argv(mode, claude, model, budget):
    """The recording session's `claude` command line."""
    if mode == "debug":
        return claude_code.claude_argv(
            claude, model, budget, tools="Bash,Skill,Read,Edit,Write",
            allowed=[*claude_code.ALLOWED_TOOLS, *FILE_TOOLS], disallowed=[],
        )
    return claude_code.claude_argv(claude, model, budget)


# The gym's note says the session can't write files; this one can, here.
DEBUG_NOTE = """
## How this session works

Your file tools work only inside this directory, which is where `./fix`,
`fix.diff` and `./report` go. Don't write GYM_ANSWER yourself: end with the
JSON object as your structured output, and the wrapper that started you
writes it to GYM_ANSWER. Besides the file tools, the `relish` CLI (through
Bash) and the Reliaburger MCP tools are the only tools you have.
"""


# Both sessions may run only `Bash(relish *)`, so a chained command line is
# refused. The first dry run's agent chained `relish logs ...; relish agent
# patch init ...`, was refused, and took the refusal to mean the patch steps
# were off limits: it answered after 55 s without building, testing or
# trying anything.
ONE_COMMAND = """
Run one `relish` command per Bash call, with no pipes, `;`, `&&` or
redirection: anything else is refused. A refused call doesn't mean the
step is forbidden; run it again on its own. Every step above is allowed,
and the session isn't done until all of them have run.
"""


def recording_prompt(mode, prompt):
    note = DEBUG_NOTE if mode == "debug" else claude_code.NOTE
    return prompt.rstrip() + "\n" + ADDENDA[mode] + note + ONE_COMMAND


def title(mode, relish_version):
    """The cast's title, which tests/suite/website.rs reads the version from."""
    return f"Reliaburger {relish_version}: {TITLES[mode]}"


def version_of(relish):
    words = subprocess.run([str(relish), "--version"], capture_output=True, text=True,
                           check=False).stdout.split()
    return words[1] if len(words) > 1 else "development build"


def person_path(real_relish, path):
    """PATH for the person's commands: the real `relish` first."""
    return os.pathsep.join([str(Path(real_relish).parent), path])


def post_process(lines):
    """A cast's lines with the recorder's command and environment dropped and
    a chapter marker at every `» Title` line, as tour.sh does."""
    header = json.loads(lines[0])
    header.pop("command", None)
    header.pop("env", None)
    out = [json.dumps(header)]
    for line in lines[1:]:
        event = json.loads(line)
        if event[1] == "o":
            for found in stage.HEADING_TEXT.finditer(event[2]):
                out.append(json.dumps([event[0], "m", found.group(1)]))
                # asciicast v3 stores each event's gap since the one before:
                # the marker takes the gap, and the line follows with none.
                if header.get("version") == 3:
                    event[0] = 0.0
        out.append(json.dumps(event, ensure_ascii=False))
    return out


ENDED = re.compile(r"^# The session ended after \d+ turns and ")
COLOURS = re.compile("\x1b\\[[0-9;]*m")


def tidy(lines, events):
    """A cast recorded before #1904, shown as stage.py shows a session now.

    Claude Code's background notices go (with a `that took` line under
    one, since that command hadn't finished), the model line shows once, a
    repeated chapter says `(again)` in its marker and its heading, and the
    session ends once, where the last part ended, with the turns and time
    of every part from `events`, the session's stream-json. An event left
    with nothing to show goes, and in a v3 cast its wait moves to the next
    event, so everything else keeps its time. Tidying twice changes nothing.
    """
    header = json.loads(lines[0])
    relative = header.get("version") == 3
    parsed = [json.loads(line) for line in lines[1:]]
    results = [event for event in events if event.get("type") == "result"]
    total = stage.ended(sum(event.get("num_turns") or 0 for event in results),
                        sum(event.get("duration_ms") or 0 for event in results) / 1000)
    banners = [index for index, event in enumerate(parsed)
               if event[1] == "o" and any(ENDED.match(COLOURS.sub("", piece))
                                          for piece in event[2].split("\r\n"))]
    last = banners[-1] if banners else None
    markers, headings = set(), set()
    model_shown = False
    after_notice = False
    carry = 0.0
    out = [lines[0]]
    for index, event in enumerate(parsed):
        if relative and carry:
            event[0] = round(event[0] + carry, 3)
            carry = 0.0
        if event[1] == "m":
            event[2] = stage.again(event[2], markers)
        elif event[1] == "o":
            kept, removed = [], False
            for piece in event[2].split("\r\n"):
                shown = COLOURS.sub("", piece)
                drop = False
                if stage.BACKGROUND.match(shown):
                    drop, after_notice = True, True
                elif after_notice and shown.startswith("# that took "):
                    drop = True
                elif shown.startswith("# Claude Code, model "):
                    drop, model_shown = model_shown, True
                elif ENDED.match(shown):
                    if index == last and results:
                        piece = total
                    drop = index != last
                if drop:
                    removed = True
                    continue
                if shown:
                    after_notice = False
                kept.append(stage.HEADING_TEXT.sub(
                    lambda found: stage.heading(stage.again(found.group(1), headings)), piece))
            if removed and not any(kept):
                if relative:
                    carry = event[0]
                continue
            event[2] = "\r\n".join(kept)
        out.append(json.dumps(event, ensure_ascii=False))
    return out


def tidy_cast(cast, session):
    """Rewrite `cast` in place with `tidy`, from the run's `session.jsonl`."""
    cast = Path(cast)
    events = claude_code.events(Path(session).read_text())
    cast.write_text("\n".join(tidy(cast.read_text().splitlines(), events)) + "\n")
    return 0


def common_problems(result):
    if "error" in result:
        return [f"the gym couldn't run it: {result['error']}"]
    problems = []
    verdict = result["score"]["verdict"]
    if verdict != "correct":
        problems.append(f"the gym scored the run {verdict}, not correct")
    if result.get("timed_out"):
        problems.append("the session timed out")
    return problems


def debug_problems(result, calls, reports):
    """Why a `debug` run can't be shown; empty when it can."""
    problems = common_problems(result)
    if "error" in result:
        return problems
    if result.get("patch_result") != "pass":
        problems.append(f"the answer's patch didn't pass the reverted tests ({result.get('patch_result')})")
    for prefix in DEBUG_REQUIRED_CALLS:
        if not any(score_module.runs_command(call["argv"], prefix) for call in calls):
            problems.append(f"the session never ran relish {prefix}")
    if not reports:
        problems.append("there's no report draft")
    return problems


def act_problems(result, calls, person):
    """Why an `act` run can't be shown; empty when it can."""
    problems = common_problems(result)
    if "error" in result:
        return problems
    if not any(score_module.runs_command(call["argv"], "agent lease take") for call in calls):
        problems.append("the session never took a lease")
    if not person or person.get("proposal") is None:
        return [*problems, "the session made no proposal"]
    wanted = [("approve_exit", 0), ("after_approve", "applied"), ("undo_exit", 0), ("after_undo", "undone")]
    for key, value in wanted:
        if person.get(key) != value:
            problems.append(f"{key} was {person.get(key)!r}, not {value!r}")
    return problems


class RecordingSolver:
    """Claude Code inside `asciinema rec`, with stage.py showing it."""

    name = "recording"

    def __init__(self, mode, cast, clusters, claude, model, budget, version, real_relish):
        self.mode = mode
        self.cast = Path(cast)
        self.clusters = clusters
        self.claude = claude
        self.model = model
        self.budget = budget
        self.version = version
        self.real_relish = real_relish

    def describe(self):
        return f"Claude Code, recorded ({self.mode})"

    def solve(self, prompt, env, work_dir, timeout, scenario=None):
        work_dir = Path(work_dir)
        argv = session_argv(self.mode, self.claude, self.model, self.budget)
        prompt_path = work_dir / "recording-prompt.md"
        prompt_path.write_text(recording_prompt(self.mode, prompt))
        stream = work_dir / "session.jsonl"
        stage_env = dict(env)
        stage_env.update({
            "STAGE_ARGV": json.dumps(argv),
            "STAGE_PROMPT": str(prompt_path),
            "STAGE_STREAM": str(stream),
            "STAGE_TIMEOUT": str(max(60, timeout - 120)),
        })
        if self.mode == "act":
            # The gym names the solver's token after the run's directory.
            stage_env["STAGE_AGENT_NAME"] = f"gym-solver-{work_dir.parent.name}"
            stage_env["STAGE_PERSON_TOKEN"] = self.clusters[-1].admin_token
            stage_env["STAGE_PERSON_PATH"] = person_path(self.real_relish, os.environ.get("PATH", ""))
            stage_env["STAGE_PERSON_LOG"] = str(work_dir / "person.json")
        self.cast.parent.mkdir(parents=True, exist_ok=True)
        recorder = [
            "asciinema", "rec", "--headless", "--overwrite", "--return",
            "--window-size", WINDOW, "--idle-time-limit", str(IDLE_LIMIT),
            "--title", title(self.mode, self.version),
            "-c", shlex.join([sys.executable, str(STAGE), self.mode]), str(self.cast),
        ]
        version = subprocess.run([self.claude, "--version"], capture_output=True, text=True,
                                 stdin=subprocess.DEVNULL, env=claude_code.session_env(os.environ),
                                 check=False).stdout.strip()
        started, clock = time.time(), time.monotonic()
        completed = subprocess.run(recorder, env=stage_env, cwd=work_dir, capture_output=True,
                                   stdin=subprocess.DEVNULL, text=True, timeout=timeout, check=False)
        seconds = time.monotonic() - clock
        text = stream.read_text() if stream.exists() else ""
        events = claude_code.events(text)
        result = next((e for e in reversed(events) if e.get("type") == "result"), None)
        answer = claude_code.answer_from(result)
        if answer is not None:
            Path(env["GYM_ANSWER"]).write_text(json.dumps(answer, indent=2) + "\n")
        meta = claude_code.meta_from(events, version, started, seconds, argv, False)
        meta["solver"] = "claude-code-recorded"
        Path(env["GYM_SOLVER_META"]).write_text(json.dumps(meta, indent=2) + "\n")
        return solvers.Attempt(exit_code=completed.returncode, seconds=seconds,
                               stdout=text, stderr=completed.stdout + completed.stderr)


def load_scenario(mode):
    return scenario_module.load(scenario_module.GYM_DIR / "scenarios" / f"{SCENARIOS[mode]}.toml")


def exact_prompt(mode):
    """The prompt the session gets, exactly: the gym's, then the recording's addendum."""
    scenario = load_scenario(mode)
    return recording_prompt(
        mode, solvers.full_prompt(scenario.prompt, scenario.platform, scenario.solver_token)
    )


def record(args):
    mode = args.mode
    binaries = gym._binaries(args)
    scenario = load_scenario(mode)
    if mode == "act":
        # The person's steps happen on camera, so the gym doesn't run them
        # after the session; act_problems checks what they answered.
        scenario = dataclasses.replace(
            scenario, after=(), outcomes=tuple(o for o in scenario.outcomes if o.by == "solver")
        )
    out = Path(args.out).resolve()
    if out.exists() and any(out.iterdir()):
        raise SystemExit(f"{out} isn't empty: name a new directory")
    if mode == "debug" and sys.platform == "darwin" and Path.home() not in out.parents:
        raise SystemExit(f"{out} isn't under your home directory, which patch build needs on macOS")
    out.mkdir(parents=True, exist_ok=True)
    cast = Path(args.cast).resolve()

    planted = None
    if scenario.plant is not None:
        target = Path(args.plant_target_dir or out / "plant-target")
        builder = plant_module.PlantBuild(binaries["relish"], scenario.plant.patch,
                                          out / "plant", target, os.environ)
        builder.work_dir.mkdir(parents=True, exist_ok=True)
        print("building the planted relish", flush=True)
        planted = (builder, builder.build())
    solver_relish = planted[1] if planted else binaries["relish"]

    scratch = Path(tempfile.mkdtemp(prefix="record-"))
    clusters = []

    def make_cluster(nodes, work_dir):
        made = cluster_module.LocalCluster(scratch / mode, nodes, args.port_base, binaries)
        clusters.append(made)
        return made

    solver = RecordingSolver(mode, cast, clusters, os.environ.get("CLAUDE", "claude"), args.model,
                             args.budget_usd, args.title_version or version_of(solver_relish),
                             binaries["relish"])
    run_dir = out / scenario.id
    print(f"recording {scenario.id} into {cast}", flush=True)
    result = gym.run_once(scenario, run_dir, make_cluster, solver, binaries["relish"], planted,
                          args.timeout)
    if cast.exists():
        cast.write_text("\n".join(post_process(cast.read_text().splitlines())) + "\n")
    calls = shim.read_log(run_dir / "calls.jsonl")
    if mode == "debug":
        problems = debug_problems(result, calls, gym.find_reports(run_dir / "solver"))
    else:
        person_log = run_dir / "solver" / "person.json"
        person = json.loads(person_log.read_text()) if person_log.exists() else None
        problems = act_problems(result, calls, person)
    kept = cast.exists() and not problems
    if cast.exists() and problems:
        cast.rename(cast.with_name(cast.name + ".failed"))
    summary = {"mode": mode, "scenario": scenario.id, "cast": str(cast) if kept else None,
               "verdict": result.get("score", {}).get("verdict"), "problems": problems,
               "seconds": result.get("seconds"), "solver_meta": result.get("solver_meta")}
    (out / "recording.json").write_text(json.dumps(summary, indent=2) + "\n")
    if not kept:
        print("not kept:\n  " + "\n  ".join(problems or ["asciinema wrote no cast"]), file=sys.stderr)
        return 1
    print(f"kept {cast}: the gym scored the run correct, and every step ran")
    return 0


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] == ["prompt"] and len(argv) == 2 and argv[1] in SCENARIOS:
        print(exact_prompt(argv[1]), end="")
        return 0
    if argv[:1] == ["tidy"] and len(argv) == 3:
        return tidy_cast(argv[1], argv[2])
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mode", choices=sorted(SCENARIOS))
    parser.add_argument("--cast", required=True, help="the asciicast to write")
    parser.add_argument("--out", required=True, help="a new directory for the gym's files")
    parser.add_argument("--relish")
    parser.add_argument("--bun")
    parser.add_argument("--testapp")
    parser.add_argument("--plant-target-dir")
    parser.add_argument("--model")
    parser.add_argument("--budget-usd", default=claude_code.DEFAULT_BUDGET_USD)
    parser.add_argument("--timeout", type=int, default=DEFAULT_TIMEOUT)
    parser.add_argument("--port-base", type=int, default=gym.DEFAULT_PORT_BASE)
    parser.add_argument("--title-version")
    return record(parser.parse_args(argv))


if __name__ == "__main__":
    sys.exit(main())
