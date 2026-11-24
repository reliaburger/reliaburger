#!/usr/bin/env python3
"""Fail the final CI gate when any job it waited for did not succeed.

"ignored-test evidence" needs every other job in ci.yml, so a merge queue
can require that one check. A job may be skipped only because select jobs
left it out: that job must itself have succeeded first. Three kinds of run may
skip the code jobs altogether: a pull request or a merge group that changes
only documentation, and a push to a train whose commit the merge queue already
passed, which names that run in select jobs' `tested_by` (#1723). The
evidence owners' own checks are in workflow_adapter.py aggregate.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys


def problems(needs, event):
    """Why this run must not pass, or an empty list."""
    if 'changes' not in needs:
        return ['select jobs did not run']
    found = [f'{name} finished as {job.get("result")!r}' for name, job in sorted(needs.items())
             if job.get('result') not in ('success', 'skipped')]
    if needs['changes'].get('result') == 'skipped':
        found.append("changes finished as 'skipped'")
    outputs = needs['changes'].get('outputs', {})
    if not found and outputs.get('code') != 'true' and event not in ('pull_request', 'merge_group') \
            and not (event == 'push' and tested_by(needs)):
        found.append(f'a {event} run must select the code jobs')
    return found


def tested_by(needs):
    """The merge-queue run select jobs says already tested this push's commit, or None."""
    run = needs.get('changes', {}).get('outputs', {}).get('tested_by') or ''
    return run if run.isdigit() else None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--needs', required=True, help='the toJSON(needs) of the gate job')
    parser.add_argument('--event', required=True, help='the workflow event name')
    args = parser.parse_args(argv)
    needs = json.loads(Path(args.needs).read_text())
    found = problems(needs, args.event)
    run = tested_by(needs) if args.event == 'push' else None
    if run and not found:
        print(f'::notice::CI gate: merge-queue run {run} already passed every job on this commit, '
              'so this push ran none of them')
    for problem in found:
        print('CI gate: ' + problem, file=sys.stderr)
    return 1 if found else 0


if __name__ == '__main__':
    raise SystemExit(main())
