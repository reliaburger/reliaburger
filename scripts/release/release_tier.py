#!/usr/bin/env python3
"""Say which release gate a release needs: the patch gate, or the full gate with the 8-hour job final.

The gate scales with the release (maintainer decision, 26 October 2026,
#1363). Every release runs the staged installs, the fast soak tiers (V02 and
jobs) and the power-cut and reboot runs: the patch gate. The full gate adds
the job final (`qualify-jobs-sustained.sh --tier final`), and a release needs
it when any of these hold:

- it's a minor or major release (`vX.Y.0`);
- `CURRENT` in `src/compatibility.rs` changed since the previous release, so
  the durable state or the wire format moved;
- it touches the agent loop (`src/bun/agent.rs` or `src/bun/agent/`), whose
  turn budget and ordering the final's eight hours of faults exercise.

Usage:

    scripts/release/release_tier.py --previous v0.2.3 --release v0.2.4 [--commit REF]

prints `patch` or `full` on the first line and the reasons after it.
`--commit` is the candidate (default `HEAD`); the comparison is
`--previous..--commit`.
"""
from __future__ import annotations

import argparse
import re
import subprocess
import sys

PATCH = "patch"
FULL = "full"
AGENT_LOOP = ("src/bun/agent.rs", "src/bun/agent/")
COMPATIBILITY = "src/compatibility.rs"


def parse_version(text):
    """`v0.2.4` or `0.2.4-dev` as (0, 2, 4)."""
    match = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)(?:-[0-9A-Za-z.-]+)?", text.strip())
    if match is None:
        raise ValueError(f"{text!r} is not a release version")
    return tuple(int(part) for part in match.groups())


def parse_current(source):
    """The (protocol, state) pair `CURRENT` declares in compatibility.rs's source."""
    match = re.search(r"pub const CURRENT: Compatibility = Compatibility \{\s*protocol:\s*(\d+),\s*state:\s*(\d+),?\s*\}",
                      source)
    if match is None:
        raise ValueError("src/compatibility.rs declares no CURRENT protocol and state")
    return int(match.group(1)), int(match.group(2))


def decide(previous, release, previous_current, release_current, changed):
    """Return (PATCH or FULL, reasons) for a release over its previous one."""
    previous, release = parse_version(previous), parse_version(release)
    if release <= previous:
        raise ValueError(f"release {release} is not newer than {previous}")
    reasons = []
    if release[2] == 0 or release[:2] != previous[:2]:
        reasons.append("a minor or major release")
    if previous_current[0] != release_current[0]:
        reasons.append(f"the wire format moved: protocol {previous_current[0]} -> {release_current[0]}")
    if previous_current[1] != release_current[1]:
        reasons.append(f"the durable state moved: state {previous_current[1]} -> {release_current[1]}")
    loop = sorted(path for path in changed if path == AGENT_LOOP[0] or path.startswith(AGENT_LOOP[1]))
    if loop:
        shown = ", ".join(loop[:5]) + (f" and {len(loop) - 5} more" if len(loop) > 5 else "")
        reasons.append(f"the agent loop changed: {shown}")
    if reasons:
        return FULL, reasons
    return PATCH, ["a patch release with the same formats and an unchanged agent loop"]


def git(*arguments):
    return subprocess.run(["git", *arguments], check=True, capture_output=True, text=True).stdout


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--previous", required=True, help="the previous release's tag, such as v0.2.3")
    parser.add_argument("--release", required=True, help="the version being released, such as v0.2.4")
    parser.add_argument("--commit", default="HEAD", help="the candidate commit (default HEAD)")
    args = parser.parse_args(argv)
    try:
        tier, reasons = decide(
            args.previous, args.release,
            parse_current(git("show", f"{args.previous}:{COMPATIBILITY}")),
            parse_current(git("show", f"{args.commit}:{COMPATIBILITY}")),
            git("diff", "--no-renames", "--name-only", args.previous, args.commit).split(),
        )
    except (ValueError, subprocess.CalledProcessError) as error:
        detail = getattr(error, "stderr", None) or error
        print(f"release_tier: {str(detail).strip()}", file=sys.stderr)
        return 2
    print(tier)
    for reason in reasons:
        print(f"  {reason}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
