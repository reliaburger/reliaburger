#!/usr/bin/env python3
"""Fail the final CI gate when any job it waited for did not succeed.

"ignored-test evidence" needs every other job in ci.yml, so a merge queue
can require that one check. A job may be skipped only because select jobs
left it out: that job must itself have succeeded first. Only a pull request
may skip the code jobs altogether (a documentation-only change). The
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
    code = needs['changes'].get('outputs', {}).get('code')
    if not found and code != 'true' and event != 'pull_request':
        found.append(f'a {event} run must select the code jobs')
    return found


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--needs', required=True, help='the toJSON(needs) of the gate job')
    parser.add_argument('--event', required=True, help='the workflow event name')
    args = parser.parse_args(argv)
    found = problems(json.loads(Path(args.needs).read_text()), args.event)
    for problem in found:
        print('CI gate: ' + problem, file=sys.stderr)
    return 1 if found else 0


if __name__ == '__main__':
    raise SystemExit(main())

