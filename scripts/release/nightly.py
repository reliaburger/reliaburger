#!/usr/bin/env python3
"""Nightly qualification of a merge train's tip, and the code-freeze check against it (#1363).

Every night the quiet host runs the long tiers on the current tip of a train,
one after another: the power-cut and reboot runs, the V02 and jobs fast soaks
and, for a release that needs the full gate, the 8-hour job final. Each night
leaves a dated directory under `--records` with `nightly.json` and a
README naming the exact commit. At code freeze, `freeze-check` asks whether the
latest green nightly ran the candidate's code; if it did, the gate only adds
the staged installs.

    nightly.py run --train release-0-2-4 [--gate patch|full] [options]
    nightly.py freeze-check --records DIR --commit SHA [--gate patch|full]

The soaks install a staged build of the tip: normally the train's nightly
build, signed with the nightly key and staged under a `nightly-` tag
(docs/releasing.md). Before any tier runs, `run` checks that its
candidate.json hashes to the digest, names the tip's code, and that its guest
images are signed with the key its channel names.

`scripts/release/nightly-qualify.sh` wraps `run` and keeps the Mac awake.
`run --plan` (or `--dry-run`) resolves the tip and prints the tiers it would
run, touching nothing. Only the pure parts are tested here
(`test_nightly.py`); the tiers themselves need Lima and hours.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import urllib.request

from package import GUEST_METADATA, guest_image_statement, trusted_keys, verify_signature

HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[1]
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


def command_run(args):
    remote_tip = git("ls-remote", args.remote, f"refs/heads/{args.train}").split()
    if not remote_tip:
        print(f"nightly: {args.remote} has no branch {args.train}", file=sys.stderr)
        return 2
    commit = remote_tip[0]
    date = datetime.date.today().isoformat()
    run_dir = Path(args.records).expanduser() / f"{date}-{args.train}-{commit[:8]}"
    staged = (args.base_url, args.qualified_digest) if args.base_url and args.qualified_digest else None
    checkout = Path(tempfile.gettempdir()) / f"reliaburger-nightly-{commit[:8]}"
    steps = plan_steps(args.gate, commit, run_dir, checkout, args.iterations, staged, args.soak_bun)
    if args.plan:
        print(f"nightly {args.gate} gate on {args.train} at {commit}; records in {run_dir}")
        for name, argv, note in steps:
            print(f"  {name}: {shlex.join(argv) if argv else 'not run: ' + note}")
        return 0
    if compilers_running():
        print("nightly: rustc or cargo is running; the nightly needs a quiet host", file=sys.stderr)
        return 2
    if run_dir.exists():
        print(f"nightly: {run_dir} exists; records are never overwritten", file=sys.stderr)
        return 2
    git("fetch", "--quiet", args.remote, f"refs/heads/{args.train}")
    channel = "release"
    if staged:
        manifest = fetch(args.base_url, "candidate.json")
        problems = staged_problems(manifest, args.qualified_digest, commit, same_tree)
        if not problems:
            channel = json.loads(manifest).get("channel", "release")
            keys_source = (REPOSITORY / "src" / "upgrade" / "keys.rs").read_text()
            problems = signature_problems(manifest, fetch(args.base_url, GUEST_METADATA), keys_source)
        if problems:
            print("nightly: " + "; ".join(problems), file=sys.stderr)
            return 2
    environment = tier_environment(os.environ, channel)
    run_dir.mkdir(parents=True)
    git("worktree", "add", "--quiet", "--detach", str(checkout), commit)
    started = now()
    try:
        results = run_tiers(steps, lambda argv: subprocess.run(argv, cwd=checkout, env=environment).returncode)
    finally:
        git("worktree", "remove", "--force", str(checkout))
    record = dict(schema=SCHEMA, train=args.train, commit=commit, tree=tree_of(commit), date=date,
                  started=started, finished=now(), host=os.uname().nodename, gate=args.gate,
                  staged=dict(base_url=staged[0], qualified_digest=staged[1]) if staged else None,
                  tiers=results)
    (run_dir / RECORD).write_text(json.dumps(record, indent=2) + "\n")
    (run_dir / README).write_text(render_readme(record))
    print(f"nightly: {run_dir / README}")
    return 0 if green(record, args.gate) else 1


def command_freeze_check(args):
    try:
        commit = git("rev-parse", "--verify", f"{args.commit}^{{commit}}")
        tree = tree_of(commit)
    except subprocess.CalledProcessError:
        print(f"freeze-check: no commit {args.commit} here; fetch it first", file=sys.stderr)
        return 2
    problems = freeze_problems(load_records(Path(args.records).expanduser()), commit, tree, args.gate)
    for problem in problems:
        print(f"freeze-check: {problem}", file=sys.stderr)
    if not problems:
        print(f"freeze-check: the latest green nightly ran {commit}'s code; "
              "the gate needs only the staged installs")
    return 1 if problems else 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="qualify the train's tip tonight")
    run.add_argument("--train", required=True, help="the merge train, such as release-0-2-4")
    run.add_argument("--remote", default="origin")
    run.add_argument("--gate", choices=sorted(GATE_TIERS), default="patch",
                     help="patch: the fast tiers and the power-cut runs; full adds the job final "
                          "(release_tier.py says which a release needs)")
    run.add_argument("--records", default="~/reliaburger-nightly")
    run.add_argument("--base-url", help="a staged build of the tip, for the soaks")
    run.add_argument("--qualified-digest", help="its candidate.json SHA-256")
    run.add_argument("--soak-bun", help="the signed soak bun for the upgrade walks; the job final needs it")
    run.add_argument("--iterations", type=int, default=10, help="power-cut and reboot runs per lane")
    run.add_argument("--plan", "--dry-run", action="store_true", help="print the tiers, touch nothing")
    check = commands.add_parser("freeze-check", help="does the latest green nightly cover the candidate?")
    check.add_argument("--records", default="~/reliaburger-nightly")
    check.add_argument("--commit", required=True, help="the candidate commit")
    check.add_argument("--gate", choices=sorted(GATE_TIERS), default="patch")
    args = parser.parse_args(argv)
    return command_run(args) if args.command == "run" else command_freeze_check(args)


if __name__ == "__main__":
    raise SystemExit(main())
