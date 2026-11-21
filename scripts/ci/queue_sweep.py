#!/usr/bin/env python3
"""Cancel the merge queue's CI runs that the queue no longer wants (#1723).

Each entry in a train's merge queue gets a temporary branch,
`gh-readonly-queue/<train>/pr-<number>-<base sha>`, and a `ci.yml` run on it.
When the queue re-forms (an entry fails, is removed or jumps the queue),
GitHub deletes the branches of the groups it drops and starts new ones, but
it doesn't cancel their runs. ci.yml's concurrency group for a merge group is
its own run id, so nothing else does either. On 20 November 2026 two such runs
held about twenty runners each for an hour while the live groups waited for
runners.

A run is superseded when its branch is gone: the queue deleted it, so nothing
will read the run's checks. A group that merged loses its branch too, but by
then its run has finished, so only runs still queued or in progress count.
Runs younger than a minute are left alone, in case the branch API lags the
push that created the run.

`queue-sweep.yml` runs this whenever the queue starts a group and whenever a
train is pushed, which covers every re-formation: dropping a group starts a
new one behind it, and a merge pushes the train. It never fails the workflow:
an API error leaves the run it was asking about alone.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timedelta, timezone
import json
import subprocess
import sys

QUEUE_PREFIX = "gh-readonly-queue/"
FINISHED = "completed"
GRACE = timedelta(minutes=1)


class NotFound(Exception):
    """The API answered 404."""


def gh_api(path, method="GET"):
    """Call the GitHub API through `gh`; raise NotFound on a 404."""
    result = subprocess.run(["gh", "api", "--method", method, path],
                            capture_output=True, text=True, timeout=60)
    if result.returncode != 0:
        if "HTTP 404" in result.stderr:
            raise NotFound(path)
        raise RuntimeError(f"gh api {method} {path}: {result.stderr.strip()}")
    return json.loads(result.stdout) if result.stdout.strip() else None


def parse_time(text):
    return datetime.fromisoformat(text.replace("Z", "+00:00"))


def superseded(runs, branch_exists, now):
    """The runs to cancel: unfinished merge-group runs whose queue branch is gone.

    `runs` are workflow-run rows from the API; `branch_exists(name)` returns
    True, False, or None when it couldn't tell, which keeps the run.
    """
    found = []
    for run in runs:
        branch = run.get("head_branch") or ""
        if run.get("event") != "merge_group" or not branch.startswith(QUEUE_PREFIX):
            continue
        if run.get("status") == FINISHED:
            continue
        if now - parse_time(run["created_at"]) < GRACE:
            continue
        if branch_exists(branch) is False:
            found.append(run)
    return found


def sweep(repository, api, now, own_run_id=None, log=print):
    """Cancel every superseded run of ci.yml; return the ids cancelled.

    The run this sweep belongs to isn't a ci.yml run, so it is never one of
    them; `own_run_id` is skipped anyway.
    """
    page = api(f"repos/{repository}/actions/workflows/ci.yml/runs?event=merge_group&per_page=100")
    runs = page.get("workflow_runs", [])

    def branch_exists(name):
        try:
            api(f"repos/{repository}/git/ref/heads/{name}")
            return True
        except NotFound:
            return False
        except Exception as error:  # an unknown answer keeps the run
            log(f"::warning::queue sweep: could not look up {name}: {error}")
            return None

    cancelled = []
    for run in superseded(runs, branch_exists, now):
        if str(run["id"]) == str(own_run_id):
            continue
        try:
            api(f"repos/{repository}/actions/runs/{run['id']}/cancel", method="POST")
        except Exception as error:
            log(f"::warning::queue sweep: could not cancel run {run['id']}: {error}")
            continue
        log(f"queue sweep: cancelled run {run['id']} on {run['head_branch']}: "
            "the queue dropped that group")
        cancelled.append(run["id"])
    if not cancelled:
        log("queue sweep: every unfinished merge-group run is still in the queue")
    return cancelled


def main(argv=None, api=gh_api, now=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repository", required=True)
    parser.add_argument("--own-run-id")
    args = parser.parse_args(argv)
    try:
        sweep(args.repository, api, now or datetime.now(timezone.utc), args.own_run_id)
    except Exception as error:  # never fail the workflow over a sweep
        print(f"::warning::queue sweep: {error}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
