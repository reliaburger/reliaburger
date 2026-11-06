"""The `relish` a solver runs: the real one, with every call recorded.

The runner puts a two-line `relish` script first on the solver's PATH that
runs this file. It runs the real binary (`GYM_REAL_RELISH`) with the same
arguments, passes its output and exit code through, and appends one JSON line
per call to `GYM_CALL_LOG`: the arguments, the exit code, the output and how
long it took. Scoring reads that log, not the solver's own account of what it
did, so a Claude Code run and the scripted baseline are judged the same way.

Output is passed through when the command exits, not as it streams, so
`relish logs --follow` shows nothing until it ends. Nothing in the gym's
scenarios needs to follow.
"""
import json
import os
import subprocess
import sys
import time

# Enough for any `relish agent` answer (its default budget is far smaller)
# and the start of anything longer.
MAX_KEPT_BYTES = 256 * 1024


def _kept(text):
    if len(text) <= MAX_KEPT_BYTES:
        return text
    return text[:MAX_KEPT_BYTES] + "\n[gym: output cut]"


def record(log_path, argv, exit_code, stdout, stderr, started, seconds):
    """Append one call to the log as a JSON line."""
    entry = {
        "argv": ["relish", *argv],
        "exit": exit_code,
        "stdout": _kept(stdout),
        "stderr": _kept(stderr),
        "started": started,
        "seconds": round(seconds, 3),
    }
    with open(log_path, "a", encoding="utf-8") as log:
        log.write(json.dumps(entry) + "\n")


def main(argv):
    real = os.environ["GYM_REAL_RELISH"]
    log_path = os.environ["GYM_CALL_LOG"]
    started = time.time()
    clock = time.monotonic()
    completed = subprocess.run(
        [real, *argv], capture_output=True, text=True, errors="replace", check=False
    )
    record(
        log_path,
        argv,
        completed.returncode,
        completed.stdout,
        completed.stderr,
        started,
        time.monotonic() - clock,
    )
    sys.stdout.write(completed.stdout)
    sys.stderr.write(completed.stderr)
    return completed.returncode


def install(bin_dir, real_relish, python=sys.executable):
    """Write the `relish` script that runs this shim into `bin_dir`."""
    bin_dir.mkdir(parents=True, exist_ok=True)
    script = bin_dir / "relish"
    script.write_text(
        "#!/bin/sh\n"
        f'exec "{python}" "{os.path.abspath(__file__)}" "$@"\n'
    )
    script.chmod(0o755)
    return {"GYM_REAL_RELISH": str(real_relish)}


def read_log(log_path):
    """Every call in a log, in order; an absent log is no calls."""
    try:
        lines = log_path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return []
    return [json.loads(line) for line in lines if line.strip()]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
