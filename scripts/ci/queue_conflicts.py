#!/usr/bin/env python3
"""Say whether a pull request will merge cleanly behind a train's queue (#2264).

GitHub's merge queue builds each entry on top of every entry ahead of it, not
on the train. An entry whose branch conflicts with one of those is never
built: the queue marks it `UNMERGEABLE` and leaves it waiting, silently, until
it reaches the head, then drops it for `merge_conflict`. On 24 November 2026
four 0.3.3 PRs sat like that for 1.6 to 2.6 hours each, and every one merged
cleanly onto the train itself: the conflict was with a PR still in the queue,
editing the same rows of `docs/flakes.md` or `CHANGELOG.md`.

So checking against the train isn't enough. This checks the PR against the
queue's tip, the head commit of the last entry the queue has built, and when
that conflicts it walks back to the first entry that introduced the
conflict, so the author knows which PR to merge in:

    python3 scripts/ci/queue_conflicts.py 2268

Run it before `gh pr merge --merge`, and again after a queue event. It exits
0 when the PR merges cleanly onto the tip, 1 when it conflicts (or the queue
already marks it `UNMERGEABLE`), and 2 when it can't tell. It only reads:
it never queues, dequeues or pushes anything.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys

QUEUE_QUERY = """
query($owner: String!, $name: String!, $branch: String!) {
  repository(owner: $owner, name: $name) {
    mergeQueue(branch: $branch) {
      entries(first: 100) {
        nodes { position state pullRequest { number } headCommit { oid } }
      }
    }
  }
}
"""


def git(*arguments, cwd=None):
    return subprocess.run(["git", *arguments], cwd=cwd, capture_output=True,
                          text=True, timeout=300)


def conflicting_paths(base, head, cwd=None):
    """The paths that conflict when `head` merges into `base`; [] when clean.

    `git merge-tree --write-tree --name-only` prints the merged tree's id,
    then on a conflict one conflicted path per line, then a blank line and
    its messages. It exits 1 on a conflict, but also on some errors, such as
    a commit it doesn't have, which print no tree: so a conflict is exit 1
    with a tree id on the first line, and anything else is an error.
    """
    result = git("merge-tree", "--write-tree", "--name-only", base, head, cwd=cwd)
    lines = result.stdout.splitlines()
    wrote_tree = bool(lines) and len(lines[0]) in (40, 64) and all(c in "0123456789abcdef" for c in lines[0])
    if result.returncode == 0 and wrote_tree:
        return []
    if result.returncode != 1 or not wrote_tree:
        raise RuntimeError(f"git merge-tree {base} {head}: {result.stderr.strip() or 'no tree written'}")
    lines = lines[1:]
    paths = []
    for line in lines:
        if not line:
            break
        if line not in paths:
            paths.append(line)
    return paths


def queue_ahead(entries, number):
    """The entries a PR would merge behind, in queue order.

    A PR that is already queued is checked against the entries ahead of it;
    one that isn't goes to the back, behind them all.
    """
    ordered = sorted(entries, key=lambda entry: entry["position"])
    for index, entry in enumerate(ordered):
        if entry["pullRequest"]["number"] == number:
            return ordered[:index]
    return ordered


def find_blocker(head, train_tip, ahead, conflicts):
    """The first thing `head` conflicts with, as (PR number or None, paths).

    The candidates are the train's tip and then the head commit of each
    entry ahead that the queue has built, in order; an entry the queue marks
    `UNMERGEABLE` has no head commit and adds nothing. None for the number
    means the train itself conflicts. Returns None when `head` merges
    cleanly onto the queue's tip. `conflicts(base, head)` returns the
    conflicting paths.
    """
    bases = [(None, train_tip)]
    bases += [(entry["pullRequest"]["number"], entry["headCommit"]["oid"])
              for entry in ahead if entry.get("headCommit")]
    tip_paths = conflicts(bases[-1][1], head)
    if not tip_paths:
        return None
    for number, base in bases[:-1]:
        paths = conflicts(base, head)
        if paths:
            return number, paths
    return bases[-1][0], tip_paths


def gh_json(*arguments):
    result = subprocess.run(["gh", *arguments], capture_output=True, text=True, timeout=120)
    if result.returncode != 0:
        raise RuntimeError(f"gh {' '.join(arguments[:2])}: {result.stderr.strip()}")
    return json.loads(result.stdout)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("pr", type=int, help="the pull request's number")
    parser.add_argument("--repository", default="reliaburger/reliaburger")
    parser.add_argument("--remote", default="origin", help="the git remote to fetch commits from")
    options = parser.parse_args(argv)
    owner, name = options.repository.split("/", 1)
    try:
        pull = gh_json("api", f"repos/{options.repository}/pulls/{options.pr}")
        train = pull["base"]["ref"]
        head = pull["head"]["sha"]
        train_tip = gh_json("api", f"repos/{options.repository}/branches/{train}")["commit"]["sha"]
        queue = gh_json("api", "graphql", "-f", f"query={QUEUE_QUERY}", "-f", f"owner={owner}",
                        "-f", f"name={name}", "-f", f"branch={train}")
        entries = ((queue["data"]["repository"]["mergeQueue"] or {}).get("entries") or {}).get("nodes") or []
        ahead = queue_ahead(entries, options.pr)
        wanted = [head, train_tip] + [entry["headCommit"]["oid"] for entry in ahead if entry.get("headCommit")]
        fetched = git("fetch", "--quiet", "--no-tags", options.remote, *wanted)
        if fetched.returncode != 0:
            raise RuntimeError(f"git fetch: {fetched.stderr.strip()}")
        blocker = find_blocker(head, train_tip, ahead, conflicting_paths)
    except (RuntimeError, KeyError, ValueError, subprocess.TimeoutExpired) as error:
        print(f"#{options.pr}: can't tell: {error}", file=sys.stderr)
        return 2
    own = [entry for entry in entries if entry["pullRequest"]["number"] == options.pr]
    marked = bool(own) and own[0]["state"] == "UNMERGEABLE"
    built = sum(1 for entry in ahead if entry.get("headCommit"))
    if blocker is None and not marked:
        print(f"#{options.pr} merges cleanly onto {train} and the {built} built entries ahead of it")
        return 0
    if blocker is None:
        print(f"#{options.pr}: the queue marks it UNMERGEABLE, though it merges cleanly onto the "
              "tip now; remove it and queue it again")
        return 1
    number, paths = blocker
    against = f"{train} itself" if number is None else f"#{number}, queued ahead of it"
    print(f"#{options.pr} conflicts with {against}: {', '.join(paths)}")
    if number is None:
        print(f"  merge {train} into the branch first")
    else:
        print(f"  the queue would hold it unbuilt until it reached the head, then drop it; "
              f"merge {train} into the branch after #{number} lands, or rebase on #{number}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
