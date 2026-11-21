#!/usr/bin/env python3
"""Nightly qualification of a merge train's tip, and the code-freeze check against it (#1363).

A nightly runs the long tiers on the current tip of a train, one after
another on the quiet host: the power-cut and reboot runs, the V02 and jobs
fast soaks and, for a release that needs the full gate, the 8-hour job final.
It runs only when a release is close: the evening before a planned freeze,
`arm` loads a launchd job for that one night, which disarms itself after it
runs, whatever the result (maintainer decision, 20 November 2026, #1721).
Each night leaves a dated directory under `--records` with `nightly.json` and
a README naming the exact commit. At code freeze, `freeze-check` asks whether
the latest green nightly ran the candidate's code; if it did, the gate only
adds the staged installs.

    nightly.py arm --train release-0-3-2 [--at 18:00] [--date YYYY-MM-DD] [run options]
    nightly.py disarm
    nightly.py run --train release-0-2-4 [--dispatch] [--previous vX.Y.Z] [--gate full] [options]
    nightly.py freeze-check --records DIR --commit SHA [--previous vX.Y.Z] [--gate full]

`run --dispatch` first builds the tip with build.yml and stages that build
with stage.yml on the nightly channel, then qualifies it; the armed job runs
it with `--armed-for DATE`, which removes the job before the night starts and
unloads it after. If stage.yml fails, the night stages the build itself with
`candidate.py stage-fetch` and `stage` and the operator's gh login. Every run
checks `power_cut/quiet.py` first and holds its quiet lock while the tiers
run, so the power-cut endurance run pauses and a release gate isn't joined.

Both work out the gate the commit's release needs with `release_tier.py`:
the release is the version in the commit's Cargo.toml, the previous release
the nearest `v*` tag behind it (`--previous` names another). `--gate` may only
raise it: asking for the patch gate on a release that needs the full one is
refused (#1428).

The soaks install a staged build of the tip: normally the train's nightly
build, signed with the nightly key and staged under a `nightly-` tag
(docs/releasing.md). Before any tier runs, `run` checks that its
candidate.json hashes to the digest, names the tip's code, and that its guest
images are signed with the key its channel names.

`scripts/release/nightly-qualify.sh` wraps `run` and keeps the Mac awake;
its `--once` is `arm` and its `--disarm` is `disarm`. `run --plan` (or
`--dry-run`) resolves the tip and prints the tiers it would run, touching
nothing. Only the parts that need no GitHub, launchd or Lima are tested here
(`test_nightly.py`), with fakes for `gh` and `launchctl`; the tiers
themselves need Lima and hours.
"""
from __future__ import annotations

import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shlex
import subprocess
import sys
import tempfile
import time
import urllib.request

import candidate
import release_tier
from package import GUEST_METADATA, guest_image_statement, trusted_keys, verify_signature

HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[1]
sys.path.insert(0, str(HERE / "power_cut"))
import quiet  # noqa: E402

PLATFORM = sys.platform
GITHUB_REPOSITORY = "reliaburger/reliaburger"
# One launchd job per armed night, named for its date, so a night that's
# running is never unloaded by arming or disarming another.
LABEL = "com.reliaburger.nightly-qualify"
DEFAULT_CUT_OFF = "18:00"
# A train's build waits up to two hours for its CI run to finish (build.yml,
# #1722) before building for about an hour; staging takes minutes.
BUILD_TIMEOUT = 5 * 3600
STAGE_TIMEOUT = 3600
POLL = 60
ACCEPT_NIGHTLY = "RELIABURGER_ACCEPT_NIGHTLY"
RECORD = "nightly.json"
README = "README.md"
SCHEMA = 1

POWER_CUT = "power-cut-reboot"
V02_FAST = "v02-fast"
JOBS_FAST = "jobs-fast"
JOBS_FINAL = "jobs-final"
# The tiers each gate needs from a nightly, in the order a night runs them:
# the cheapest and most likely to fail first, the 10½-hour final last.
GATE_TIERS = {
    "patch": (POWER_CUT, V02_FAST, JOBS_FAST),
    "full": (POWER_CUT, V02_FAST, JOBS_FAST, JOBS_FINAL),
}
SOAKS = (V02_FAST, JOBS_FAST, JOBS_FINAL)
PASS, FAIL, NOT_RUN = "pass", "fail", "not run"


def plan_steps(gate, commit, run_dir, checkout, iterations, staged, soak_bun):
    """The tiers a night runs, as [(name, argv or None, note)]: None means it can't run.

    `staged` is (base URL, qualified digest) of a staged build of `commit`, or
    None. The soaks install through `curl | sh` from a staged build, so
    without one they're recorded as not run and the night isn't green.
    """
    scripts = Path(checkout) / "scripts" / "release"
    run_dir = Path(run_dir)
    steps = [(POWER_CUT, [str(scripts / "qualify-power-cut-reboot.sh"), "--commit", commit,
                          "--iterations", str(iterations), "--record-dir", str(run_dir / POWER_CUT)], "")]
    for name in GATE_TIERS[gate][1:]:
        if staged is None:
            steps.append((name, None, f"no staged build of {commit[:12]} (--base-url, --qualified-digest)"))
            continue
        if name == JOBS_FINAL and not soak_bun:
            steps.append((name, None, "the job final needs --soak-bun"))
            continue
        script = "qualify-sustained.sh" if name == V02_FAST else "qualify-jobs-sustained.sh"
        tier = "final" if name == JOBS_FINAL else "fast"
        argv = [str(scripts / script), "--tier", tier, "--base-url", staged[0],
                "--qualified-digest", staged[1], "--record", str(run_dir / f"{name}.md")]
        if soak_bun:
            argv += ["--soak-bun", str(soak_bun)]
        steps.append((name, argv, ""))
    return steps


def run_tiers(steps, runner, clock=time.monotonic, load=os.getloadavg):
    """Run each step in order and return its result; the final is skipped after a failure.

    Every fast tier runs even when an earlier one failed, so one night shows
    every problem. The job final doesn't: it's 10½ hours on code already
    known to be red.
    """
    results = []
    for name, argv, note in steps:
        if argv is None:
            results.append(dict(name=name, status=NOT_RUN, seconds=0, note=note))
            continue
        if name == JOBS_FINAL and any(result["status"] != PASS for result in results):
            results.append(dict(name=name, status=NOT_RUN, seconds=0,
                                note="skipped: an earlier tier didn't pass"))
            continue
        load_before = round(load()[0], 2)
        started = clock()
        status = runner(argv)
        results.append(dict(name=name, status=PASS if status == 0 else FAIL,
                            seconds=round(clock() - started), exit_status=status,
                            load_before=load_before, load_after=round(load()[0], 2),
                            command=shlex.join(argv), note=note))
    return results


def green(record, gate="patch"):
    """A nightly is green for a gate when every tier that gate needs passed."""
    statuses = {tier["name"]: tier["status"] for tier in record.get("tiers", [])}
    return all(statuses.get(name) == PASS for name in GATE_TIERS[gate])


def render_readme(record):
    """The night's record, for a person: what ran on which commit, and the verdict."""
    verdict = {gate: green(record, gate) for gate in GATE_TIERS}
    if verdict["full"]:
        summary = "green for the full gate (and the patch gate)"
    elif verdict["patch"]:
        summary = "green for the patch gate"
    else:
        summary = "red: not green for any gate"
    lines = [
        f"# Nightly qualification: {record['train']}, {record['date']}",
        "",
        f"Commit `{record['commit']}` (tree `{record['tree']}`), the tip of `{record['train']}` "
        f"when the night started. Verdict: **{summary}**.",
        "",
        f"Started {record['started']}, finished {record['finished']}, on {record['host']}.",
    ]
    staged = record.get("staged")
    if staged:
        lines += ["", f"Soaks installed the staged build `{staged['base_url']}`, "
                      f"`candidate.json` SHA-256 `{staged['qualified_digest']}`."]
    lines += ["", "| Tier | Result | Time | Load before / after | Note |", "| --- | --- | ---: | --- | --- |"]
    for tier in record["tiers"]:
        minutes = f"{tier['seconds'] // 60} min" if tier["seconds"] else ""
        loads = f"{tier['load_before']} / {tier['load_after']}" if "load_before" in tier else ""
        lines.append(f"| `{tier['name']}` | {tier['status']} | {minutes} | {loads} | {tier.get('note', '')} |")
    lines += ["", "Each tier's own record is beside this file. A failure is triaged the day it's",
              "seen: a product bug gets an issue and a fix on the train; anything that passes",
              "on a re-run goes in docs/flakes.md.", ""]
    return "\n".join(lines)


def load_records(records_dir):
    """Every nightly.json under `records_dir`, oldest first."""
    found = []
    for path in sorted(Path(records_dir).glob(f"*/{RECORD}")):
        record = json.loads(path.read_text())
        if record.get("schema") == SCHEMA:
            found.append(record)
    return sorted(found, key=lambda record: record["finished"])


def freeze_problems(records, commit, tree, gate):
    """Why the candidate can't rest on the nightlies at code freeze, or an empty list.

    The latest nightly that's green for `gate` must have run the candidate's
    code: the same commit, or a commit with the same tree (the gate PR's
    merge into main has a new commit but, with nothing else on main, the
    train tip's tree). A red nightly on that code after the green one means
    the green one may have been luck, so that fails too, until the red night
    is triaged and a newer green night on the code follows it.
    """
    greens = [record for record in records if green(record, gate)]
    if not greens:
        return [f"no nightly is green for the {gate} gate"]
    latest = greens[-1]
    if latest["commit"] != commit and latest["tree"] != tree:
        return [f"the latest green nightly ({latest['date']}) ran {latest['commit']}, not the candidate "
                f"{commit}: wait for a nightly on the candidate, or run the {gate} gate's tiers on it"]
    later = [record for record in records
             if record["finished"] > latest["finished"] and record["tree"] == latest["tree"]
             and not green(record, gate)]
    return [f"the {record['date']} nightly on the same code was red: triage it before tagging"
            for record in later]


def required_gate(commit, previous=None, git=None):
    """(gate, reasons, previous, release): the gate `commit`'s release needs, by release_tier's rule.

    The release is the version in the commit's Cargo.toml; the previous
    release is `previous`, or the nearest `v*` tag behind the commit.
    Raises ValueError or CalledProcessError when git can't answer.
    """
    git = git or globals()["git"]
    if previous is None:
        previous = git("describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", commit).strip()
    release = release_tier.cargo_version(git("show", f"{commit}:Cargo.toml"))
    gate, reasons = release_tier.decide(
        previous, release,
        release_tier.parse_current(git("show", f"{previous}:{release_tier.COMPATIBILITY}")),
        release_tier.parse_current(git("show", f"{commit}:{release_tier.COMPATIBILITY}")),
        git("diff", "--no-renames", "--name-only", previous, commit).split(),
    )
    return gate, reasons, previous, release


def choose_gate(required, requested):
    """(gate, None) to run, or (None, why) when `requested` is below what the release needs."""
    order = list(GATE_TIERS)
    if requested is None:
        return required, None
    if order.index(requested) < order.index(required):
        return None, (f"this release needs the {required} gate (release_tier.py says why); "
                      f"--gate {requested} would skip its tiers")
    return requested, None


def resolve_gate(args, commit, git, label):
    """The gate to use, printing the rule's reasons, or None after printing why not."""
    try:
        required, reasons, previous, release = required_gate(commit, args.previous, git)
    except (ValueError, subprocess.CalledProcessError) as error:
        detail = getattr(error, "stderr", None) or error
        print(f"{label}: can't work out the gate: {str(detail).strip()}", file=sys.stderr)
        return None
    gate, problem = choose_gate(required, args.gate)
    if problem:
        print(f"{label}: {problem}", file=sys.stderr)
        for reason in reasons:
            print(f"  {reason}", file=sys.stderr)
        return None
    print(f"{label}: v{release} over {previous} needs the {required} gate; running the {gate} gate")
    for reason in reasons:
        print(f"  {reason}")
    return gate


def staged_problems(manifest, digest, commit, same_tree):
    """Why a staged build isn't the tip's, or an empty list. `same_tree(a, b)` compares commits' trees."""
    if hashlib.sha256(manifest).hexdigest() != digest:
        return ["candidate.json doesn't hash to --qualified-digest"]
    staged = json.loads(manifest).get("commit", "")
    if staged != commit and not same_tree(staged, commit):
        return [f"the staged build is of {staged}, not the tip {commit}"]
    return []


def signature_problems(manifest, guest_metadata, keys_source):
    """Why the staged build isn't signed with the key its channel names, or an empty list.

    A train's nightly build (`"channel": "nightly"` in candidate.json) must be
    signed with a key in `NIGHTLY_SIGNING_KEYS`, a staged release candidate
    with one in `EMBEDDED_RELEASE_KEYS` (`keys_source` is the text of
    src/upgrade/keys.rs). The guest images' signed statements are the check:
    a few kilobytes, where the signed binaries are hundreds of megabytes.
    Relish then checks every binary and image it installs against the keys
    compiled into it, which in a nightly build are the nightly keys.
    """
    record = json.loads(manifest)
    channel = record.get("channel", "release")
    keys = trusted_keys(keys_source, channel)
    if not keys:
        return [f"src/upgrade/keys.rs has no {channel} public key yet (docs/releasing.md, "
                "\"Nightly signing key\")"]
    assets = record.get("assets", {})
    if hashlib.sha256(guest_metadata).hexdigest() != assets.get(GUEST_METADATA, {}).get("sha256"):
        return [f"{GUEST_METADATA} doesn't match candidate.json"]
    images = json.loads(guest_metadata).get("images") or {}
    if not images:
        return [f"{GUEST_METADATA} signs no guest image"]
    problems = []
    for arch, image in sorted(images.items()):
        if image.get("sha256") != assets.get(image.get("asset"), {}).get("sha256"):
            problems.append(f"the {arch} guest image in {GUEST_METADATA} doesn't match candidate.json")
            continue
        statement = guest_image_statement(record.get("version", ""), arch, image["asset"], image["sha256"],
                                          image.get("source", {}).get("sha256", ""))
        if not any(verify_signature(key, statement, image.get("signature", "")) for key in keys):
            problems.append(f"the {arch} guest image isn't signed by the {channel} key")
    return problems


def tier_environment(environment, channel):
    """The environment a tier runs in: a nightly's soaks may install it through the website bootstrap.

    docs/website/install.sh refuses a `nightly-` mirror unless
    RELIABURGER_ACCEPT_NIGHTLY=1, so nobody installs one by accident. The
    nightly is the one place that asks for it, and only for a nightly build.
    """
    environment = {name: value for name, value in environment.items() if name != ACCEPT_NIGHTLY}
    if channel == "nightly":
        environment[ACCEPT_NIGHTLY] = "1"
    return environment


class NightlyError(Exception):
    """The evening's build or staging failed; the night goes on without a staged build."""


def armed_night(now, at, date=None):
    """When an armed night starts: `date` (default today) at `at` ("HH:MM"), which must be later than `now`."""
    match = re.fullmatch(r"([01]\d|2[0-3]):([0-5]\d)", at)
    if not match:
        raise ValueError(f"--at {at!r} isn't a time like 18:00")
    day = datetime.date.fromisoformat(date) if date else now.date()
    night = datetime.datetime.combine(day, datetime.time(int(match[1]), int(match[2])))
    if night <= now:
        raise ValueError(f"{night:%Y-%m-%d %H:%M} has passed: arm a later --at, or start the night now with "
                         "`scripts/release/nightly-qualify.sh --dispatch --train ...`")
    return night


def job_label(date):
    return f"{LABEL}.{date}"


def launchd_job(checkout, records, night, arguments):
    """The launchd job for one armed night: it fires once, at the cut-off on that date, and never repeats.

    The calendar entry has no year, so the night itself refuses another date
    (`--armed-for`) and removes the job before it starts. A login shell (-l)
    gives it the PATH that finds gh, limactl and python3.
    """
    date = night.date().isoformat()
    command = shlex.join(["scripts/release/nightly-qualify.sh", "--armed-for", date, *arguments])
    log = str(Path(records) / "launchd.log")
    return dict(Label=job_label(date), ProgramArguments=["/bin/bash", "-lc", f"exec {command}"],
                WorkingDirectory=str(checkout),
                StartCalendarInterval=dict(Month=night.month, Day=night.day, Hour=night.hour,
                                           Minute=night.minute),
                LaunchOnlyOnce=True, AbandonProcessGroup=True, StandardOutPath=log, StandardErrorPath=log)


def run_arguments(args, records):
    """The `run` options an armed night is started with: the operator's, and --dispatch without a staged build."""
    arguments = ["--train", args.train, "--records", str(records)]
    if args.remote != "origin":
        arguments += ["--remote", args.remote]
    if args.repository != GITHUB_REPOSITORY:
        arguments += ["--repository", args.repository]
    for option, value in (("--previous", args.previous), ("--gate", args.gate)):
        if value:
            arguments += [option, value]
    if args.soak_bun:
        arguments += ["--soak-bun", str(Path(args.soak_bun).expanduser().absolute())]
    if args.iterations != 10:
        arguments += ["--iterations", str(args.iterations)]
    if args.base_url and args.qualified_digest:
        arguments += ["--base-url", args.base_url, "--qualified-digest", args.qualified_digest]
    else:
        arguments.append("--dispatch")
    return arguments


def home_directory():
    return Path.home()


def local_now():
    return datetime.datetime.now()


def launch_agents():
    return home_directory() / "Library" / "LaunchAgents"


def launchd_domain():
    return f"gui/{os.getuid()}"


def launchctl(*arguments):
    return subprocess.run(["launchctl", *arguments], capture_output=True).returncode


def schedule_bootout(label):
    """Unload the armed night's job once this process has exited.

    Unloading a running job stops it, so a detached shell in its own session
    (which launchd doesn't stop with the job) does it a few seconds later.
    """
    subprocess.Popen(["/bin/sh", "-c", f"sleep 10; launchctl bootout {launchd_domain()}/{label} >/dev/null 2>&1"],
                     start_new_session=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                     stderr=subprocess.DEVNULL)


def quiet_problem():
    """Why the host isn't free for a night (power_cut/quiet.py's answer), or None."""
    lock = os.environ.get("RELIABURGER_QUIET_LOCK", quiet.DEFAULT_LOCK)
    chains = os.environ.get("RELIABURGER_GATE_CHAINS", quiet.DEFAULT_CHAINS)
    return quiet.busy_reason(lock, chains, 172800)


def quiet_lock_path():
    return Path(os.environ.get("RELIABURGER_QUIET_LOCK", quiet.DEFAULT_LOCK))


def take_quiet_lock(path):
    """Hold the quiet lock: a file descriptor to close when the night ends, or None when someone else holds it."""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(descriptor)
        return None
    return descriptor


def gh(*arguments):
    return subprocess.run(["gh", *arguments], check=True, capture_output=True, text=True).stdout


def run_ids(gh, repository, workflow, branch):
    listed = gh("run", "list", "--repo", repository, "--workflow", workflow, "--branch", branch,
                "--event", "workflow_dispatch", "--limit", "20", "--json", "databaseId")
    return [run["databaseId"] for run in json.loads(listed)]


def dispatch(gh, repository, workflow, branch, fields, sleep, attempts=30):
    """Dispatch `workflow` on `branch` and return the new run's ID: one that wasn't listed before."""
    before = set(run_ids(gh, repository, workflow, branch))
    gh("workflow", "run", workflow, "--repo", repository, "--ref", branch,
       *[part for field in fields for part in ("-f", field)])
    for _ in range(attempts):
        new = [run_id for run_id in run_ids(gh, repository, workflow, branch) if run_id not in before]
        if new:
            return max(new)
        sleep(10)
    raise NightlyError(f"no new {workflow} run appeared on {branch} after dispatching it")


def wait_for(gh, repository, workflow, run_id, timeout, sleep):
    """The finished run's view: {status, conclusion, headSha, attempt}."""
    for _ in range(max(1, timeout // POLL)):
        view = json.loads(gh("run", "view", str(run_id), "--repo", repository,
                             "--json", "status,conclusion,headSha,attempt"))
        if view["status"] == "completed":
            return view
        sleep(POLL)
    raise NightlyError(f"{workflow} run {run_id} didn't finish within {timeout // 60} minutes")


def local_staging_commands(repository, run_id, digest, directory, main_commit):
    """What stage.yml runs, run here instead: download and verify the build, then publish it."""
    script = str(HERE / "candidate.py")
    common = ["--directory", str(directory), "--repository", repository, "--run-id", str(run_id),
              "--qualified-digest", digest, "--channel", "nightly"]
    return ([sys.executable, script, "stage-fetch", *common],
            [sys.executable, script, "stage", *common, "--main-commit", main_commit])


def stage_locally(repository, run_id, digest, remote="origin", git=None):
    """Stage a nightly build with the operator's gh login, as stage.yml would, its tag on main too."""
    git = git or globals()["git"]
    main_commit = git("ls-remote", remote, "refs/heads/main").split()[0]
    with tempfile.TemporaryDirectory(prefix="reliaburger-nightly-stage-") as directory:
        for argv in local_staging_commands(repository, run_id, digest, Path(directory) / "dist", main_commit):
            if subprocess.run(argv).returncode:
                raise NightlyError(f"local staging failed: {shlex.join(argv)}")


def dispatch_and_stage(train, repository, gh, sleep=time.sleep, stage_locally=stage_locally):
    """Build the train's tip on the nightly channel and stage it: (commit, base URL, digest).

    The digest is the SHA-256 of the build's own candidate.json, from its
    manifest artefact; staging and the night both check every byte against it.
    """
    try:
        build = dispatch(gh, repository, "build.yml", train, [], sleep)
        print(f"nightly: build.yml run {build} is building {train}")
        view = wait_for(gh, repository, "build.yml", build, BUILD_TIMEOUT, sleep)
        if view["conclusion"] != "success":
            raise NightlyError(f"build.yml run {build} ended {view['conclusion']}: nothing to stage")
        commit, attempt = view["headSha"], view["attempt"]
        with tempfile.TemporaryDirectory(prefix="reliaburger-nightly-manifest-") as directory:
            gh("run", "download", str(build), "--repo", repository,
               "-n", f"{candidate.ARTEFACTS['nightly']}-manifest-{commit}-{attempt}", "-D", directory)
            manifest = (Path(directory) / candidate.RECORD).read_bytes()
        digest = hashlib.sha256(manifest).hexdigest()
        version = json.loads(manifest)["version"]
        tag = candidate.staging_tag(version, build, attempt, "nightly")
        stage = dispatch(gh, repository, "stage.yml", "main",
                         ["channel=nightly", f"candidate_run={build}", f"qualified_digest={digest}"], sleep)
        print(f"nightly: stage.yml run {stage} is staging it as {tag}")
        view = wait_for(gh, repository, "stage.yml", stage, STAGE_TIMEOUT, sleep)
        if view["conclusion"] != "success":
            print(f"nightly: stage.yml run {stage} ended {view['conclusion']}; staging locally with "
                  "candidate.py stage-fetch and stage, and this host's gh login "
                  "(docs/releasing.md, \"Arming a nightly\")", file=sys.stderr)
            stage_locally(repository, build, digest)
    except (subprocess.CalledProcessError, KeyError, ValueError) as error:
        detail = getattr(error, "stderr", None) or error
        raise NightlyError(f"building and staging {train}: {str(detail).strip()}") from error
    return commit, f"https://github.com/{repository}/releases/download/{tag}", digest


def git(*arguments, repository=REPOSITORY):
    return subprocess.run(["git", *arguments], cwd=repository, check=True, capture_output=True,
                          text=True).stdout.strip()


def tree_of(commit):
    return git("rev-parse", f"{commit}^{{tree}}")


def same_tree(first, second):
    try:
        return tree_of(first) == tree_of(second)
    except subprocess.CalledProcessError:
        return False


def compilers_running():
    return any(subprocess.run(["pgrep", "-x", name], capture_output=True).returncode == 0
               for name in ("rustc", "cargo"))


def now():
    return datetime.datetime.now().astimezone().isoformat(timespec="seconds")


def fetch(base_url, name):
    with urllib.request.urlopen(f"{base_url.rstrip('/')}/{name}", timeout=60) as response:
        return response.read()


def command_run(args, git=git, gh=gh):
    if not args.armed_for:
        return qualify(args, git, gh)
    # An armed night runs once: its job goes before anything else can fail,
    # and is unloaded after, whatever the result.
    label = job_label(args.armed_for)
    (launch_agents() / f"{label}.plist").unlink(missing_ok=True)
    try:
        today = local_now().date().isoformat()
        if args.armed_for != today:
            print(f"nightly: armed for {args.armed_for}, but it's {today}: not running, and disarmed",
                  file=sys.stderr)
            return 2
        return qualify(args, git, gh)
    finally:
        schedule_bootout(label)


def print_plan(args, commit, git):
    gate = resolve_gate(args, commit, git, "nightly")
    if gate is None:
        return 2
    run_dir = Path(args.records).expanduser() / f"{local_now().date().isoformat()}-{args.train}-{commit[:8]}"
    staged = (args.base_url, args.qualified_digest) if args.base_url and args.qualified_digest else None
    if args.dispatch and staged is None:
        print(f"nightly: first build.yml on {args.train}, then stage.yml on main with channel=nightly; "
              "the soaks install that staged build")
        staged = ("STAGED_BASE_URL", "ITS_DIGEST")
    checkout = Path(tempfile.gettempdir()) / f"reliaburger-nightly-{commit[:8]}"
    print(f"nightly {gate} gate on {args.train} at {commit}; records in {run_dir}")
    for name, argv, note in plan_steps(gate, commit, run_dir, checkout, args.iterations, staged, args.soak_bun):
        print(f"  {name}: {shlex.join(argv) if argv else 'not run: ' + note}")
    return 0


def qualify(args, git, gh):
    remote_tip = git("ls-remote", args.remote, f"refs/heads/{args.train}").split()
    if not remote_tip:
        print(f"nightly: {args.remote} has no branch {args.train}", file=sys.stderr)
        return 2
    commit = remote_tip[0]
    git("fetch", "--quiet", args.remote, f"refs/heads/{args.train}")
    if args.plan:
        return print_plan(args, commit, git)
    if compilers_running():
        print("nightly: rustc or cargo is running; the nightly needs a quiet host", file=sys.stderr)
        return 2
    problem = quiet_problem()
    if problem:
        print(f"nightly: the host isn't quiet: {problem}", file=sys.stderr)
        return 2
    staged = (args.base_url, args.qualified_digest) if args.base_url and args.qualified_digest else None
    if args.dispatch and staged is None:
        try:
            commit, base_url, digest = dispatch_and_stage(
                args.train, args.repository, gh,
                stage_locally=lambda *staging: stage_locally(*staging, remote=args.remote, git=git))
            staged = (base_url, digest)
            git("fetch", "--quiet", args.remote, f"refs/heads/{args.train}")
        except NightlyError as error:
            print(f"nightly: {error}; only the power-cut runs happen and the night isn't green",
                  file=sys.stderr)
    gate = resolve_gate(args, commit, git, "nightly")
    if gate is None:
        return 2
    date = local_now().date().isoformat()
    run_dir = Path(args.records).expanduser() / f"{date}-{args.train}-{commit[:8]}"
    checkout = Path(tempfile.gettempdir()) / f"reliaburger-nightly-{commit[:8]}"
    steps = plan_steps(gate, commit, run_dir, checkout, args.iterations, staged, args.soak_bun)
    if run_dir.exists():
        print(f"nightly: {run_dir} exists; records are never overwritten", file=sys.stderr)
        return 2
    channel = "release"
    if staged:
        manifest = fetch(staged[0], "candidate.json")
        problems = staged_problems(manifest, staged[1], commit, same_tree)
        if not problems:
            channel = json.loads(manifest).get("channel", "release")
            keys_source = (REPOSITORY / "src" / "upgrade" / "keys.rs").read_text()
            problems = signature_problems(manifest, fetch(staged[0], GUEST_METADATA), keys_source)
        if problems:
            print("nightly: " + "; ".join(problems), file=sys.stderr)
            return 2
    environment = tier_environment(os.environ, channel)
    # Another long run may have started while the build ran.
    problem = quiet_problem()
    lock = None if problem else take_quiet_lock(quiet_lock_path())
    if lock is None:
        print(f"nightly: the host isn't quiet: {problem or 'another run took the quiet lock'}", file=sys.stderr)
        return 2
    try:
        run_dir.mkdir(parents=True)
        git("worktree", "add", "--quiet", "--detach", str(checkout), commit)
        started = now()
        try:
            results = run_tiers(steps, lambda argv: subprocess.run(argv, cwd=checkout, env=environment).returncode)
        finally:
            git("worktree", "remove", "--force", str(checkout))
    finally:
        os.close(lock)
    record = dict(schema=SCHEMA, train=args.train, commit=commit, tree=tree_of(commit), date=date,
                  started=started, finished=now(), host=os.uname().nodename, gate=gate,
                  staged=dict(base_url=staged[0], qualified_digest=staged[1]) if staged else None,
                  tiers=results)
    (run_dir / RECORD).write_text(json.dumps(record, indent=2) + "\n")
    (run_dir / README).write_text(render_readme(record))
    print(f"nightly: {run_dir / README}")
    return 0 if green(record, gate) else 1


def command_arm(args):
    """Load a launchd job that runs the nightly once, at the cut-off on one night."""
    if PLATFORM != "darwin":
        print("nightly: arming needs launchd (macOS); elsewhere start "
              "`scripts/release/nightly-qualify.sh --dispatch --train ...` at the cut-off with at(1)",
              file=sys.stderr)
        return 2
    try:
        night = armed_night(local_now(), args.at, args.date)
    except ValueError as error:
        print(f"nightly: {error}", file=sys.stderr)
        return 2
    records = Path(args.records).expanduser().absolute()
    job = launchd_job(REPOSITORY, records, night, run_arguments(args, records))
    if args.dry_run:
        print(plistlib.dumps(job).decode(), end="")
        return 0
    problem = quiet_problem()
    if problem:
        print(f"nightly: the host is busy now ({problem}); the night checks again at the cut-off "
              "and doesn't run if it still is", file=sys.stderr)
    records.mkdir(parents=True, exist_ok=True)
    path = launch_agents() / f"{job['Label']}.plist"
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        launchctl("bootout", f"{launchd_domain()}/{job['Label']}")
    path.write_bytes(plistlib.dumps(job))
    if launchctl("bootstrap", launchd_domain(), str(path)):
        path.unlink()
        print(f"nightly: launchctl couldn't load {path}", file=sys.stderr)
        return 2
    print(f"nightly: armed for {night:%Y-%m-%d %H:%M}: {job['ProgramArguments'][-1].removeprefix('exec ')}")
    print(f"  it runs once and disarms itself; log in {job['StandardOutPath']}")
    print("  cancel with scripts/release/nightly-qualify.sh --disarm")
    return 0


def command_disarm(args):
    """Unload and remove every armed night that hasn't started; a running night is left alone."""
    armed = sorted(launch_agents().glob(f"{LABEL}.*.plist"))
    for path in armed:
        launchctl("bootout", f"{launchd_domain()}/{path.name.removesuffix('.plist')}")
        path.unlink(missing_ok=True)
        print(f"nightly: disarmed {path.name.removesuffix('.plist').removeprefix(LABEL + '.')}")
    if not armed:
        print("nightly: nothing armed")
    return 0


def command_freeze_check(args, git=git):
    try:
        commit = git("rev-parse", "--verify", f"{args.commit}^{{commit}}")
        tree = git("rev-parse", f"{commit}^{{tree}}")
    except subprocess.CalledProcessError:
        print(f"freeze-check: no commit {args.commit} here; fetch it first", file=sys.stderr)
        return 2
    gate = resolve_gate(args, commit, git, "freeze-check")
    if gate is None:
        return 2
    problems = freeze_problems(load_records(Path(args.records).expanduser()), commit, tree, gate)
    for problem in problems:
        print(f"freeze-check: {problem}", file=sys.stderr)
    if not problems:
        print(f"freeze-check: the latest green nightly ran {commit}'s code; "
              "the gate needs only the staged installs")
    return 1 if problems else 0


def add_gate_arguments(parser):
    parser.add_argument("--previous",
                        help="the previous release's tag (default: the nearest v* tag behind the commit)")
    parser.add_argument("--gate", choices=list(GATE_TIERS),
                        help="default: the gate release_tier.py says the release needs, which this may "
                             "only raise (patch: the fast tiers and the power-cut runs; full adds the "
                             "job final)")


def add_run_arguments(parser):
    parser.add_argument("--train", required=True, help="the merge train, such as release-0-2-4")
    parser.add_argument("--remote", default="origin")
    parser.add_argument("--repository", default=GITHUB_REPOSITORY, help="the GitHub repository to build in")
    add_gate_arguments(parser)
    parser.add_argument("--records", default="~/reliaburger-nightly")
    parser.add_argument("--base-url", help="a staged build of the tip, for the soaks")
    parser.add_argument("--qualified-digest", help="its candidate.json SHA-256")
    parser.add_argument("--soak-bun", help="the signed soak bun for the upgrade walks; the job final needs it")
    parser.add_argument("--iterations", type=int, default=10, help="power-cut and reboot runs per lane")


def main(argv=None, git=git, gh=gh):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="qualify the train's tip now")
    add_run_arguments(run)
    run.add_argument("--dispatch", action="store_true",
                     help="build the tip with build.yml and stage it with stage.yml first (channel nightly)")
    run.add_argument("--armed-for", metavar="DATE",
                     help="run as the night armed for DATE: remove its launchd job first, unload it after, "
                          "and refuse any other date")
    run.add_argument("--plan", "--dry-run", action="store_true", help="print the tiers, touch nothing")
    arm = commands.add_parser("arm", help="arm the nightly for one night (launchd)")
    add_run_arguments(arm)
    arm.add_argument("--at", default=DEFAULT_CUT_OFF, help="the evening cut-off, HH:MM (default 18:00)")
    arm.add_argument("--date", help="the night to arm, YYYY-MM-DD (default today)")
    arm.add_argument("--dry-run", action="store_true", help="print the launchd job, load nothing")
    commands.add_parser("disarm", help="remove every armed night that hasn't started")
    check = commands.add_parser("freeze-check", help="does the latest green nightly cover the candidate?")
    check.add_argument("--records", default="~/reliaburger-nightly")
    check.add_argument("--commit", required=True, help="the candidate commit")
    add_gate_arguments(check)
    args = parser.parse_args(argv)
    if args.command == "run":
        return command_run(args, git, gh)
    if args.command == "arm":
        return command_arm(args)
    if args.command == "disarm":
        return command_disarm(args)
    return command_freeze_check(args, git)


if __name__ == "__main__":
    raise SystemExit(main())
