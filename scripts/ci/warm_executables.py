#!/usr/bin/env python3
"""Run each freshly built binary once, before any test times it (#722).

macOS assesses the first exec of every new executable file (syspolicyd)
before it runs, one file at a time across the whole host. Outside a
developer-tools context, such as a CI runner, that costs about 0.13 s for a
small file, about a second for a release `bun` and several seconds for a debug
build. A test that launches a freshly built binary under a deadline then waits
in that queue. Running each binary once here moves the cost to before the
suite. nextest already runs every test binary once to list its tests; this
covers the `[[bin]]` targets the tests launch.

    warm_executables.py [--manifest Cargo.toml] [--target-dir target] [--profile debug]

The target directory defaults to $CARGO_TARGET_DIR, then `target`. On other
platforms it does nothing. A binary that is missing, fails or hangs is
reported and skipped: warming never fails the build.

Fixtures: scripts/ci/test_warm_executables.py.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import time

# `name = "bun"` inside a `[[bin]]` table of Cargo.toml.
NAME = re.compile(r'^\s*name\s*=\s*"([^"]+)"\s*$')
TIMEOUT_SECONDS = 120.0


def binary_names(manifest):
    """The names of the manifest's `[[bin]]` targets, in order."""
    names = []
    in_bin = False
    for line in Path(manifest).read_text().splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_bin = stripped == "[[bin]]"
            continue
        match = NAME.match(line)
        if in_bin and match:
            names.append(match.group(1))
    return names


def warm(path, timeout, environment):
    """Run one binary with `--help`, which every binary here answers at once."""
    started = time.monotonic()
    try:
        subprocess.run([str(path), "--help"], stdin=subprocess.DEVNULL,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=timeout, env=environment, check=False)
    except subprocess.TimeoutExpired:
        return f"warm_executables: {path.name} did not exit within {timeout}s; skipped"
    except OSError as error:
        return f"warm_executables: cannot run {path.name}: {error}; skipped"
    return f"warm_executables: warmed {path.name} in {time.monotonic() - started:.2f}s"


def main(argv=None, *, platform=sys.platform, timeout=TIMEOUT_SECONDS, environment=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--manifest", default="Cargo.toml")
    parser.add_argument("--target-dir", default=os.environ.get("CARGO_TARGET_DIR") or "target")
    parser.add_argument("--profile", default="debug")
    arguments = parser.parse_args(argv)
    if platform != "darwin":
        print(f"warm_executables: nothing to warm on {platform}")
        return 0
    directory = Path(arguments.target_dir) / arguments.profile
    for name in binary_names(arguments.manifest):
        path = directory / name
        if not path.is_file():
            print(f"warm_executables: {name} not built; skipped")
            continue
        print(warm(path, timeout, environment), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
