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

Anything else, an API error included, means "run the suite here": the build
fails safe, never open. The decision goes to `$GITHUB_OUTPUT` as
`reuse=true|false`, `ci_run=<id>` and `ci_run_attempt=1`, with the reason on
stderr.
`test_ci_reuse.py` keeps `REQUIRED_JOBS` equal to ci.yml's own job list.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys

WORKFLOW_PATH = ".github/workflows/ci.yml"

# Events whose ci.yml run selects every job (scripts/ci/select-jobs.sh gives
# everything to anything but a pull request) and checks out the commit itself.
FULL_EVENTS = ("push", "merge_group", "schedule")

# The job ci.yml starts only for a closed pull request; any real run skips it.
SKIPPED_JOB = "closed pull request"

# The only attempt whose green counts: anything later is a re-run.
FIRST_ATTEMPT = 1

# Every check name a full ci.yml run reports, as the API names them: the
# job's `name:`, with the macOS matrix expanded. test_ci_reuse.py fails when
# ci.yml gains, loses or renames a job and this list doesn't follow.
REQUIRED_JOBS = (
    "select jobs",
    "CI policy",
    "dependency advisory audit",
    "lint",
    "demo app",
    "minimum Rust 1.97.0",
    "portable Linux",
    "privileged Linux",
    "build tests",
    "multi-node cluster",
    "acceptance",
    "optimized contract boundaries",
    "ignored-test evidence",
    "portable macOS (1/3)",
    "portable macOS (2/3)",
    "portable macOS (3/3)",
    "benchmarks",
    "task-array benchmarks",
)


def run_problems(run, jobs, commit, repository):
    """Why this ci.yml run can't stand in for the candidate's own, or an empty list."""
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
    conclusions = {}
    for job in jobs:
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


def decide(commit, repository, runs, jobs_for):
    """Return (run id or None, reasons): the first run on `commit` that passed everything.

    `jobs_for(run_id)` returns that run's jobs; it's only called for runs on
    the right commit from a full event, so a build doesn't page through every
    pull request run.
    """
    reasons = []
    for run in runs:
        if run.get("head_sha") != commit or run.get("event") not in FULL_EVENTS:
            continue
        problems = run_problems(run, jobs_for(run["id"]), commit, repository)
        if not problems:
            return run["id"], []
        reasons.append(f"run {run.get('id')}: " + "; ".join(problems))
    return None, reasons or [f"no completed {'/'.join(FULL_EVENTS)} run of ci.yml on {commit}"]


def gh_json(path):
    result = subprocess.run(["gh", "api", path], check=True, capture_output=True, text=True,
                            timeout=120)
    return json.loads(result.stdout)


def fetch_runs(repository, commit):
    page = gh_json(f"repos/{repository}/actions/workflows/ci.yml/runs"
                   f"?head_sha={commit}&status=completed&per_page=100")
    return page.get("workflow_runs", [])


def fetch_jobs(repository, run_id):
    page = gh_json(f"repos/{repository}/actions/runs/{run_id}/jobs?filter=latest&per_page=100")
    jobs = page.get("jobs", [])
    if page.get("total_count", len(jobs)) != len(jobs):
        raise ValueError(f"run {run_id} has {page.get('total_count')} jobs; only {len(jobs)} were read")
    return jobs


def main(argv=None, runs_for=fetch_runs, jobs_for=fetch_jobs):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT"))
    args = parser.parse_args(argv)
    run_id = None
    if not re.fullmatch(r"[0-9a-f]{40}", args.commit):
        reasons = [f"{args.commit!r} is not a full commit SHA"]
    else:
        try:
            run_id, reasons = decide(args.commit, args.repository,
                                     runs_for(args.repository, args.commit),
                                     lambda run: jobs_for(args.repository, run))
        except Exception as error:  # fail safe: any doubt runs the suite
            run_id, reasons = None, [f"GitHub API error: {error}"]
    if run_id is None:
        print("CI reuse: running the suite in this build", file=sys.stderr)
        for reason in reasons:
            print("CI reuse: " + reason, file=sys.stderr)
    else:
        print(f"CI reuse: ci.yml run {run_id} passed every job on {args.commit} at attempt {FIRST_ATTEMPT}",
              file=sys.stderr)
    reused = run_id is not None
    lines = (f"reuse={'true' if reused else 'false'}\nci_run={run_id or ''}\n"
             f"ci_run_attempt={FIRST_ATTEMPT if reused else ''}\n")
    if args.github_output:
        with open(args.github_output, "a", encoding="utf-8") as output:
            output.write(lines)
    else:
        sys.stdout.write(lines)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
