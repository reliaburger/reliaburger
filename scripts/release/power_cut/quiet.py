#!/usr/bin/env python3
"""Is a release gate using this host? The power-cut scripts ask before every VM step.

A release gate's soaks need a quiet host (docs/releasing.md): VMs booting
every few minutes beside one fake slow turns. Until the quiet lock of step 5
of docs/plans/2026-11-18-plan-power-cut-endurance.md exists, a gate shows
itself two ways, and either one means "stop":

- the quiet lock file (default `$HOME/.reliaburger/quiet.lock`) is locked
  with flock by another process;
- a gate chain's log (default `$HOME/github/rb-wt/gate-*/chain.log`) was
  written in the last two days and its last line isn't `chain done`.

  quiet.py check [--lock FILE] [--chains GLOB] [--max-age SECONDS]

Exits 0 when the host is free, 1 with the reason on stdout when a gate holds
it.
"""
import argparse
import fcntl
import glob
import os
import sys
import time

DEFAULT_LOCK = os.path.join(os.path.expanduser("~"), ".reliaburger", "quiet.lock")
DEFAULT_CHAINS = os.path.join(os.path.expanduser("~"), "github", "rb-wt", "gate-*", "chain.log")


def lock_held(path):
    """True when another process holds an flock on PATH, shared or exclusive. A missing file isn't held.

    It asks for an exclusive lock: a shared request would be granted beside
    a shared holder and miss it.
    """
    try:
        descriptor = os.open(path, os.O_RDONLY)
    except FileNotFoundError:
        return False
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        return True
    finally:
        os.close(descriptor)
    return False


def last_line(path):
    with open(path, "rb") as file:
        lines = file.read().splitlines()
    return lines[-1].decode(errors="replace").strip() if lines else ""


def running_chains(pattern, max_age, now):
    """Gate chain logs that look mid-run: recent, and not ended with `chain done`."""
    running = []
    for path in sorted(glob.glob(pattern)):
        try:
            if now - os.path.getmtime(path) > max_age:
                continue
            if not last_line(path).endswith("chain done"):
                running.append(path)
        except OSError:
            continue
    return running


def busy_reason(lock, chains, max_age, now=None):
    """Why the host isn't free, or None when it is."""
    now = time.time() if now is None else now
    if lock_held(lock):
        return f"the quiet lock {lock} is held"
    running = running_chains(chains, max_age, now)
    if running:
        return f"a gate chain is running: {running[0]}"
    return None


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    command = commands.add_parser("check")
    command.add_argument("--lock", default=os.environ.get("RELIABURGER_QUIET_LOCK", DEFAULT_LOCK))
    command.add_argument("--chains", default=os.environ.get("RELIABURGER_GATE_CHAINS", DEFAULT_CHAINS))
    command.add_argument("--max-age", type=float, default=172800)
    arguments = parser.parse_args(argv)
    reason = busy_reason(arguments.lock, arguments.chains, arguments.max_age)
    if reason:
        print(reason)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
