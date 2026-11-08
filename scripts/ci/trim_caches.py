#!/usr/bin/env python3
"""Delete the GitHub Actions caches nothing can restore any more (#1724).

GitHub keeps 10 GB of caches per repository and evicts the least recently
used beyond that, so a stale cache pushes out one a job still needs. A cache
is stale when

- its ref is a pull request's merge ref and the pull request is closed or
  merged: only runs on that ref could restore it;
- its ref is a branch that no longer exists (a merge-queue group's
  `gh-readonly-queue/...` branch, say), for the same reason; or
- a newer cache of the same family was saved on the same ref. A family is
  the key without its trailing hashes and run ids: rust-cache's
  `v0-rust-portable-Linux-x64-<env>-<lock>` after a lock file change, or
  `task-array-bench-<run id>`, which restores by prefix and so only ever
  takes the newest.

Everything else stays, `main`'s current caches and open pull requests'
included. Without `--apply` it only prints what it would delete. The
scheduled `cache-trim.yml` runs it with `--apply`.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys

# A trailing `-<hash>` (rust-cache's environment and lock-file hashes, a
# digest) or `-<digits>` (a run id). Lower-case hex only: `X64` and `arm64`
# are part of the family.
TRAILING_ID = re.compile(r"-(?:[0-9a-f]{8,}|[0-9]+)$")
PULL_REF = re.compile(r"^refs/pull/(\d+)/merge$")
BRANCH_REF = re.compile(r"^refs/heads/(.+)$")


def family(key):
    """The key without its trailing hashes and run ids."""
    while True:
        shorter = TRAILING_ID.sub("", key)
        if shorter == key:
            return key
        key = shorter


def stale(caches, open_pulls, branches):
    """[(cache, reason)] for every cache nothing will restore again.

    `caches` are `gh cache list --json id,key,ref,sizeInBytes,createdAt`
    rows; `open_pulls` the open pull request numbers; `branches` the names
    of the branches that exist.
    """
    found = []
    newest = {}
    for cache in caches:
        ref = cache["ref"]
        pull = PULL_REF.match(ref)
        branch = BRANCH_REF.match(ref)
        if pull and int(pull.group(1)) not in open_pulls:
            found.append((cache, f"pull request #{pull.group(1)} is closed"))
            continue
        if branch and branch.group(1) not in branches:
            found.append((cache, f"branch {branch.group(1)} is gone"))
            continue
        slot = (ref, family(cache["key"]))
        kept = newest.get(slot)
        if kept is None:
            newest[slot] = cache
            continue
        older, newer = sorted((kept, cache), key=lambda row: row["createdAt"])
        newest[slot] = newer
        found.append((older, f"superseded by {newer['key']}"))
    return found


def gh_json(arguments):
    """Run one `gh` command and parse its JSON output."""
    return json.loads(subprocess.run(["gh", *arguments], check=True, capture_output=True, text=True).stdout)


def branch_names(repository):
    """Every branch the repository has now."""
    listed = subprocess.run(
        ["gh", "api", "--paginate", f"repos/{repository}/branches?per_page=100", "--jq", ".[].name"],
        check=True, capture_output=True, text=True).stdout
    return set(listed.split())


def delete(repository, cache_id):
    """Delete one cache by its id."""
    subprocess.run(["gh", "cache", "delete", str(cache_id), "--repo", repository], check=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--repository", required=True, help="owner/name")
    parser.add_argument("--apply", action="store_true", help="delete the stale caches; without it, only list them")
    args = parser.parse_args(argv)
    repository = args.repository
    caches = gh_json(["cache", "list", "--repo", repository, "--limit", "1000",
                      "--json", "id,key,ref,sizeInBytes,createdAt"])
    open_pulls = {row["number"] for row in gh_json(
        ["pr", "list", "--repo", repository, "--state", "open", "--limit", "1000", "--json", "number"])}
    found = stale(caches, open_pulls, branch_names(repository))
    for cache, reason in found:
        print(f"{'deleting' if args.apply else 'would delete'} {cache['key']} on {cache['ref']} "
              f"({cache['sizeInBytes'] / 1e6:.0f} MB): {reason}")
        if args.apply:
            delete(repository, cache["id"])
    freed = sum(cache["sizeInBytes"] for cache, _ in found)
    total = sum(cache["sizeInBytes"] for cache in caches)
    print(f"{len(found)} of {len(caches)} caches stale, {freed / 1e9:.2f} of {total / 1e9:.2f} GB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
