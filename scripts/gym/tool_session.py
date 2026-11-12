"""What the kubectl-ai and HolmesGPT wrappers share (stdlib only).

Both tools answer in prose, so each wrapper runs its tool once, non
interactively, with the gym's prompt and a note asking for the answer
contract's JSON object at the end. This module runs the tool under a
deadline, finds the JSON in what it said, writes `GYM_ANSWER` and
`GYM_SOLVER_META`, and refuses any flag that would let the tool act without
its own permission check.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

from solvers import answer_in_text

PROSE_NOTE = """
## How this session works

You can't write files. End your final reply with the JSON object described
above, on its own, as the last thing you write; the wrapper that started you
writes it to GYM_ANSWER.
"""


def read_prompt():
    prompt = sys.stdin.read()
    if not prompt.strip() and os.environ.get("GYM_PROMPT"):
        prompt = Path(os.environ["GYM_PROMPT"]).read_text()
    return prompt.rstrip() + "\n" + PROSE_NOTE


def refuse_flags(argv, forbidden):
    found = sorted(set(forbidden) & set(argv))
    if found:
        raise SystemExit(f"refusing to run with {', '.join(found)}")


def run_tool(argv, env, timeout):
    """Run the tool; its stdout, stderr, exit code, seconds and whether it timed out."""
    clock = time.monotonic()
    try:
        completed = subprocess.run(
            argv, capture_output=True, text=True, env=env, timeout=timeout, check=False,
            stdin=subprocess.DEVNULL,
        )
        return completed.stdout, completed.stderr, completed.returncode, time.monotonic() - clock, False
    except subprocess.TimeoutExpired as expired:
        out = expired.stdout.decode(errors="replace") if isinstance(expired.stdout, bytes) else (expired.stdout or "")
        err = expired.stderr.decode(errors="replace") if isinstance(expired.stderr, bytes) else (expired.stderr or "")
        return out, err, 124, time.monotonic() - clock, True


def version_of(argv, env):
    """The first line a tool prints about its version, or None."""
    try:
        completed = subprocess.run(argv, capture_output=True, text=True, env=env, check=False,
                                   timeout=60)
    except (OSError, subprocess.TimeoutExpired):
        return None
    lines = (completed.stdout or completed.stderr).strip().splitlines()
    return lines[0] if lines else None


def finish(answer, meta):
    """Write the answer (if any) and the meta where the runner asked."""
    if answer is not None and os.environ.get("GYM_ANSWER"):
        Path(os.environ["GYM_ANSWER"]).write_text(json.dumps(answer, indent=2) + "\n")
    if os.environ.get("GYM_SOLVER_META"):
        Path(os.environ["GYM_SOLVER_META"]).write_text(json.dumps(meta, indent=2) + "\n")


__all__ = ["answer_in_text", "finish", "read_prompt", "refuse_flags", "run_tool", "version_of"]
