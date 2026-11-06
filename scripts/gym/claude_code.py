#!/usr/bin/env python3
"""Claude Code as a gym solver, for the 0.2.7 exit test (#1093).

    python3 scripts/gym/gym.py run --solver command:scripts/gym/claude_code.py ...

The runner starts this with the prompt on stdin (and in `GYM_PROMPT`), the
recording `relish` first on PATH and a read-only token in
`RELIABURGER_TOKEN`. It runs `claude -p` (print mode, no human) with:

- the Reliaburger plugin (`plugins/reliaburger/`): its skill through
  `--plugin-dir`, and its `.mcp.json` through `--mcp-config`, which starts
  `relish mcp` through the same recording `relish`. `--strict-mcp-config`
  leaves out every other MCP server (and, with it, the plugin's own copy of
  the server, hence `--mcp-config`), and `--setting-sources ""` every plugin,
  hook and setting on this machine;
- two built-in tools, Bash and Skill (to read the plugin's skill), with
  permission for nothing in Bash but `relish` (`--allowedTools
  'Bash(relish *)'`), and the MCP server's tools (`mcp__reliaburger`).
  `--permission-mode dontAsk` refuses every other tool call without asking
  anyone. No skip-permissions flag, no "always allow";
- `--json-schema`, so the answer comes back as structured output, which this
  script writes to `GYM_ANSWER`: the session has no tool that writes files.

It writes what it knows about the session to `GYM_SOLVER_META`: the model,
the Claude Code version, the date, cost, turns and the tools the session had.
The transcript (stream-json) goes to stdout, which the runner keeps as
`solver.stdout`.

`GYM_CLAUDE` names the `claude` to run (the portable tests use a fake),
`GYM_CLAUDE_MODEL` picks a model (default: whatever Claude Code picks), and
`GYM_CLAUDE_TIMEOUT` caps the session in seconds, below the runner's own
timeout so the session is stopped here rather than orphaned.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

from scenario import FAULT_KINDS

REPOSITORY = Path(__file__).resolve().parent.parent.parent
PLUGIN = REPOSITORY / "plugins" / "reliaburger"
# Claude Code names a plugin's MCP server `plugin:<plugin>:<server>` and its
# tools `mcp__plugin_<plugin>_<server>__<tool>`; this allows every tool on it.
MCP_TOOLS = "mcp__reliaburger"
ALLOWED_TOOLS = ["Bash(relish *)", MCP_TOOLS, "Skill"]
# These build the whole crate or boot a cluster on the host, which a shared
# machine running a soak can't take, and the session has no tool to edit
# the workspace they'd build anyway. The gym checks a patch itself
# (`--check-patch`).
DISALLOWED_TOOLS = [
    "Bash(relish agent patch build *)",
    "Bash(relish agent patch test *)",
    "Bash(relish agent patch try *)",
]
# Flags this script must never pass: they skip the permission check.
FORBIDDEN_FLAGS = ("--dangerously-skip-permissions", "--allow-dangerously-skip-permissions")
DEFAULT_TIMEOUT = 1500
DEFAULT_BUDGET_USD = "20"

NOTE = """
## How this session works

This session can't write files. Instead of writing GYM_ANSWER yourself, end
with the JSON object as your structured output; the wrapper that started you
writes it to GYM_ANSWER. The `relish` CLI (through Bash) and the Reliaburger
MCP tools are the only tools you have.
"""


def answer_schema():
    """The JSON Schema of the gym's answer (solvers.ANSWER_CONTRACT)."""
    text = {"type": ["string", "null"]}
    return {
        "type": "object",
        "properties": {
            "app": text,
            "namespace": text,
            "node": text,
            "fault_kind": {"type": ["string", "null"], "enum": [*FAULT_KINDS, None]},
            "location": {
                "type": ["object", "null"],
                "properties": {"file": text, "function": text},
            },
            "patch": text,
            "summary": text,
        },
        "required": ["app", "namespace", "node", "fault_kind", "location", "patch", "summary"],
    }


def claude_argv(claude, model=None, budget=DEFAULT_BUDGET_USD, plugin=PLUGIN):
    """The `claude` command line for one gym session."""
    argv = [
        claude, "-p",
        "--output-format", "stream-json", "--verbose",
        "--no-session-persistence",
        "--setting-sources", "",
        "--strict-mcp-config",
        "--mcp-config", str(plugin / ".mcp.json"),
        "--plugin-dir", str(plugin),
        "--tools", "Bash,Skill",
        "--allowedTools", *ALLOWED_TOOLS,
        "--disallowedTools", *DISALLOWED_TOOLS,
        "--permission-mode", "dontAsk",
        "--json-schema", json.dumps(answer_schema()),
        "--max-budget-usd", budget,
    ]
    if model:
        argv += ["--model", model]
    return argv


def session_env(env):
    """The session's environment: the gym's, without this machine's Claude Code session.

    A solver started from inside a Claude Code session would otherwise
    inherit its session variables and be treated as a nested session.
    """
    return {
        key: value
        for key, value in env.items()
        if key != "CLAUDECODE" and not key.startswith("CLAUDE_CODE_") and key != "CLAUDE_PID"
    }


def events(stdout):
    """The stream-json events Claude Code printed, skipping anything else."""
    parsed = []
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict):
            parsed.append(event)
    return parsed


def answer_from(result_event):
    """The answer in the session's result: its structured output, else JSON in its text."""
    if result_event is None:
        return None
    structured = result_event.get("structured_output")
    if isinstance(structured, dict):
        return structured
    text = result_event.get("result")
    if not isinstance(text, str) or "{" not in text:
        return None
    try:
        found = json.loads(text[text.index("{"):text.rindex("}") + 1])
    except json.JSONDecodeError:
        return None
    return found if isinstance(found, dict) else None


def meta_from(stream, version, started, seconds, argv, timed_out):
    """What the record says about the session."""
    init = next((e for e in stream if e.get("type") == "system" and e.get("subtype") == "init"), {})
    result = next((e for e in reversed(stream) if e.get("type") == "result"), {})
    tool_uses = [
        block.get("name")
        for event in stream
        if event.get("type") == "assistant"
        for block in (event.get("message") or {}).get("content", [])
        if isinstance(block, dict) and block.get("type") == "tool_use"
    ]
    denials = result.get("permission_denials") or []
    return {
        "solver": "claude-code",
        "client": "Claude Code",
        "client_version": version,
        "model": init.get("model"),
        "models_used": sorted((result.get("modelUsage") or {}).keys()),
        "date": time.strftime("%Y-%m-%d", time.gmtime(started)),
        "started": started,
        "seconds": round(seconds, 1),
        "timed_out": timed_out,
        "turns": result.get("num_turns"),
        "cost_usd": result.get("total_cost_usd"),
        "result_subtype": result.get("subtype"),
        "tools": init.get("tools"),
        "mcp_servers": init.get("mcp_servers"),
        "permission_mode": init.get("permissionMode"),
        "tool_uses": tool_uses,
        "permission_denials": len(denials),
        "flags": [flag for flag in argv[1:] if flag.startswith("-")],
        "allowed_tools": ALLOWED_TOOLS,
        "disallowed_tools": DISALLOWED_TOOLS,
    }


def main():
    prompt = sys.stdin.read()
    if not prompt.strip() and os.environ.get("GYM_PROMPT"):
        prompt = Path(os.environ["GYM_PROMPT"]).read_text()
    claude = os.environ.get("GYM_CLAUDE", "claude")
    timeout = int(os.environ.get("GYM_CLAUDE_TIMEOUT", DEFAULT_TIMEOUT))
    env = session_env(os.environ)
    version = subprocess.run(
        [claude, "--version"], capture_output=True, text=True, env=env, check=False
    ).stdout.strip()
    argv = claude_argv(claude, os.environ.get("GYM_CLAUDE_MODEL"),
                       os.environ.get("GYM_CLAUDE_BUDGET_USD", DEFAULT_BUDGET_USD))
    started = time.time()
    clock = time.monotonic()
    timed_out = False
    session = subprocess.Popen(
        argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, env=env,
    )
    try:
        stdout, stderr = session.communicate(prompt.rstrip() + "\n" + NOTE, timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        session.kill()
        stdout, stderr = session.communicate()
    seconds = time.monotonic() - clock
    sys.stdout.write(stdout)
    sys.stderr.write(stderr)

    stream = events(stdout)
    result = next((e for e in reversed(stream) if e.get("type") == "result"), None)
    answer = answer_from(result)
    if answer is not None and os.environ.get("GYM_ANSWER"):
        Path(os.environ["GYM_ANSWER"]).write_text(json.dumps(answer, indent=2) + "\n")
    if os.environ.get("GYM_SOLVER_META"):
        meta = meta_from(stream, version, started, seconds, argv, timed_out)
        Path(os.environ["GYM_SOLVER_META"]).write_text(json.dumps(meta, indent=2) + "\n")
    if timed_out:
        return 124
    return session.returncode


if __name__ == "__main__":
    sys.exit(main())
