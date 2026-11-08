#!/usr/bin/env python3
"""Decide whether a candidate build can reuse a CI run that already passed on its commit.

`build.yml`'s `validate-release` job re-ran the whole of `ci.yml` on a commit
that had already passed it on the gate PR and on the push to `main`: an hour
of the candidate's two, most of it waiting for hosted macOS runners (#1363).
This script asks the GitHub API for a `ci.yml` run on exactly the commit being
built, and says it can be reused only when that run, and every job in it,
passed:

- the run is on this commit, in this repository, from `ci.yml`, and from an
  event that selects every job (`push`, `merge_group` or `schedule`; a pull
  request's run tests a merge commit, not its head, and may skip jobs);
- the run completed with `success` on its first attempt: a run re-run until
  it passed has a flaky failure behind it, and retries are 0 (#1436). The
  API reports a run's latest attempt, and its jobs with `filter=latest`, so
  a re-run's green says nothing about the attempts before it;
- every job in `REQUIRED_JOBS` is there and succeeded, `ignored-test
  evidence` included, and every other job in the run succeeded too, apart
  from the closed-PR no-op, which a real run always skips.

A candidate is normally dispatched minutes after the push to `main` that it
builds, while that push's CI is still running. So while a run from one of
those events is queued or in progress on the commit, and none has passed yet,
the script waits for it (#1722), polling every minute up to `--wait-minutes`,
then applies the same rules. The build jobs run meanwhile, and they take about
40 minutes anyway, so waiting costs nothing until the wait outlasts them. It
waits a few minutes, too, when there's no run at all yet: GitHub may not have
created the push's run when a build is dispatched the moment a merge lands.

Anything else, an API error included, means "run the suite here": the build
fails safe, never open. The decision goes to `$GITHUB_OUTPUT` as
`reuse=true|false`, `ci_run=<id>` and `ci_run_attempt=1`, with the reason on
stderr and in the job summary, which links the reused run.
`test_ci_reuse.py` keeps `REQUIRED_JOBS` equal to ci.yml's own job list.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time

WORKFLOW_PATH = ".github/workflows/ci.yml"

# Events whose ci.yml run selects every job (scripts/ci/select-jobs.sh gives
# everything to anything but a pull request) and checks out the commit itself.
FULL_EVENTS = ("push", "merge_group", "schedule")

# The job ci.yml starts only for a closed pull request; any real run skips it.
SKIPPED_JOB = "closed pull request"

# The only attempt whose green counts: anything later is a re-run.
FIRST_ATTEMPT = 1

# How often to ask again while a run on the commit is still going, and how
# long to wait for a push's run that GitHub hasn't created yet.
POLL_SECONDS = 60
GRACE_SECONDS = 180

# Every check name a full ci.yml run reports, as the API names them: the
# job's `name:`. test_ci_reuse.py fails when
# ci.yml gains, loses or renames a job and this list doesn't follow.
REQUIRED_JOBS = (
    "select jobs",
    "CI policy",
    "dependency advisory audit",
    "lint",
    "demo app",
    "minimum Rust 1.97.0",
    "source bundle builds",
    "portable Linux",
    "privileged Linux",
    "privileged Linux (Ubuntu 26.04)",
    "build tests",
    "multi-node cluster",
    "acceptance",
    "optimized contract boundaries",
    "ignored-test evidence",
    "portable macOS",
    "benchmarks",
    "task-array benchmarks",
    "S3 round trip",
)


def run_problems(run, jobs, commit, repository):
    """Why this ci.yml run can't stand in for the candidate's own, or an empty list.

    `jobs` is a callable returning the run's jobs, called only when the run
    itself passed.
    """
    found = []
    if run.get("head_sha") != commit:
        found.append(f"run is on {run.get('head_sha')}, not {commit}")
    if run.get("path") != WORKFLOW_PATH:
        found.append(f"run is from {run.get('path')!r}, not {WORKFLOW_PATH}")
    for key in ("repository", "head_repository"):
        if (run.get(key) or {}).get("full_name") != repository:
            found.append(f"run's {key} is not {repository}")
    if run.get("event") not in FULL_EVENTS:
        found.append(f"a {run.get('event')} run doesn't run every job on this commit")
    if run.get("status") != "completed" or run.get("conclusion") != "success":
        found.append(f"run finished as {run.get('status')}/{run.get('conclusion')}")
    if run.get("run_attempt") != FIRST_ATTEMPT:
        found.append(f"run's result is from attempt {run.get('run_attempt')}: a re-run, not a first-time pass")
    if found:
        # Its jobs can't rescue it, so don't spend an API call reading them.
        return found
    conclusions = {}
    for job in jobs():
        conclusions.setdefault(job.get("name"), []).append(job.get("conclusion"))
    for name in REQUIRED_JOBS:
        seen = conclusions.get(name)
        if not seen:
            found.append(f"required job {name!r} is missing")
        elif any(conclusion != "success" for conclusion in seen):
            found.append(f"required job {name!r} finished as {', '.join(map(str, seen))}")
    for name, seen in sorted(conclusions.items(), key=lambda item: str(item[0])):
        if name in REQUIRED_JOBS:
            continue
        allowed = ("skipped",) if name == SKIPPED_JOB else ("success",)
        if any(conclusion not in allowed for conclusion in seen):
            found.append(f"job {name!r} finished as {', '.join(map(str, seen))}")
    return found


def candidates(runs, commit):
    """The runs on `commit` from an event whose run could be reused."""
    return [run for run in runs if run.get("head_sha") == commit and run.get("event") in FULL_EVENTS]


def still_going(runs, commit):
    """The runs worth waiting for: on `commit`, from a full event, and not finished."""
    return [run for run in candidates(runs, commit) if run.get("status") != "completed"]


def decide(commit, repository, runs, jobs_for):
    """Return (run id or None, reasons): the first finished run on `commit` that passed everything.

    `jobs_for(run_id)` returns that run's jobs; it's only called for a run on
    the right commit, from a full event, that succeeded, so a build doesn't
    page through every pull request run. Runs still going are left to
    `wait_for_evidence`.
    """
    reasons = []
    for run in candidates(runs, commit):
        if run.get("status") != "completed":
            continue
        problems = run_problems(run, lambda: jobs_for(run["id"]), commit, repository)
        if not problems:
            return run["id"], []
        reasons.append(f"run {run.get('id')}: " + "; ".join(problems))
    return None, reasons or [f"no completed {'/'.join(FULL_EVENTS)} run of ci.yml on {commit}"]


def wait_for_evidence(commit, repository, fetch_runs, jobs_for, *, wait_seconds,
                      poll_seconds=POLL_SECONDS, grace_seconds=GRACE_SECONDS,
                      now=time.monotonic, sleep=time.sleep):
    """Return (run id or None, reasons, seconds waited), waiting for a run still going.

    Polls `fetch_runs()` until a finished run passes, or nothing is left to
    wait for, or `wait_seconds` have gone by. With no run on the commit at
    all, it keeps asking for `grace_seconds`. An exception from either
    callable propagates: the caller treats it as "run the suite".
    """
    start = now()
    while True:
        runs = fetch_runs()
        run_id, reasons = decide(commit, repository, runs, jobs_for)
        waited = now() - start
        if run_id is not None:
            return run_id, [], waited
        going = still_going(runs, commit)
        if going:
            if waited >= wait_seconds:
                state = ", ".join(f"run {run.get('id')} still {run.get('status')}" for run in going)
                return None, reasons + [f"{state} after {round(waited / 60)} minutes"], waited
        elif candidates(runs, commit) or waited >= grace_seconds:
            return None, reasons, waited
        sleep(poll_seconds)


def gh_json(path):
    result = subprocess.run(["gh", "api", path], check=True, capture_output=True, text=True,
                            timeout=120)
    return json.loads(result.stdout)


def fetch_runs(repository, commit):
    """Every ci.yml run on `commit`, finished or not."""
    page = gh_json(f"repos/{repository}/actions/workflows/ci.yml/runs?head_sha={commit}&per_page=100")
    runs = page.get("workflow_runs", [])
    if page.get("total_count", len(runs)) != len(runs):
        raise ValueError(f"{commit} has {page.get('total_count')} ci.yml runs; only {len(runs)} were read")
    return runs


def fetch_jobs(repository, run_id):
    page = gh_json(f"repos/{repository}/actions/runs/{run_id}/jobs?filter=latest&per_page=100")
    jobs = page.get("jobs", [])
    if page.get("total_count", len(jobs)) != len(jobs):
        raise ValueError(f"run {run_id} has {page.get('total_count')} jobs; only {len(jobs)} were read")
    return jobs


def run_url(repository, run_id):
    return f"https://github.com/{repository}/actions/runs/{run_id}"


def main(argv=None, runs_for=fetch_runs, jobs_for=fetch_jobs, now=time.monotonic, sleep=time.sleep):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--wait-minutes", type=int, default=120,
                        help="how long to wait for a run on the commit that's still going")
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT"))
    parser.add_argument("--step-summary", default=os.environ.get("GITHUB_STEP_SUMMARY"))
    args = parser.parse_args(argv)
    run_id, waited = None, 0
    if not re.fullmatch(r"[0-9a-f]{40}", args.commit):
        reasons = [f"{args.commit!r} is not a full commit SHA"]
    else:
        try:
            run_id, reasons, waited = wait_for_evidence(
                args.commit, args.repository, lambda: runs_for(args.repository, args.commit),
                lambda run: jobs_for(args.repository, run), wait_seconds=args.wait_minutes * 60,
                now=now, sleep=sleep)
        except Exception as error:  # fail safe: any doubt runs the suite
            run_id, reasons = None, [f"GitHub API error: {error}"]
    minutes = round(waited / 60)
    if run_id is None:
        summary = "Source CI: ran in this build, because:\n\n" + "".join(f"- {r}\n" for r in reasons)
        print("CI reuse: running the suite in this build", file=sys.stderr)
        for reason in reasons:
            print("CI reuse: " + reason, file=sys.stderr)
    else:
        after = f", after waiting {minutes} minutes for it to finish" if minutes else ""
        summary = (f"Source CI: reused ci.yml run [{run_id}]({run_url(args.repository, run_id)}), "
                   f"attempt {FIRST_ATTEMPT}, which passed every job on `{args.commit}`{after}\n")
        print(f"CI reuse: ci.yml run {run_id} passed every job on {args.commit} at attempt {FIRST_ATTEMPT}"
              f"{after}", file=sys.stderr)
    reused = run_id is not None
    lines = (f"reuse={'true' if reused else 'false'}\nci_run={run_id or ''}\n"
             f"ci_run_attempt={FIRST_ATTEMPT if reused else ''}\n")
    if args.github_output:
        with open(args.github_output, "a", encoding="utf-8") as output:
            output.write(lines)
    else:
        sys.stdout.write(lines)
    if args.step_summary:
        with open(args.step_summary, "a", encoding="utf-8") as output:
            output.write(summary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
