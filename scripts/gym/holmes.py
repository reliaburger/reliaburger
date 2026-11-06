#!/usr/bin/env python3
"""HolmesGPT as a gym solver, for the Kubernetes comparison (#1679).

    python3 scripts/gym/gym.py run --platform k8s --solver command:scripts/gym/holmes.py ...

[HolmesGPT](https://github.com/HolmesGPT/holmesgpt) (pinned in
k8s/versions.env) answers one question with `holmes ask --no-interactive`.
Its Kubernetes toolsets run `kubectl` from PATH, which the runner made the
recording shim, with the solver's read-only kubeconfig in `KUBECONFIG`, so
its calls are logged and scored like any other solver's.

`--bash-always-deny` refuses every shell command outside HolmesGPT's own
allow list without asking anyone, the counterpart of Claude Code's
`dontAsk`. Never `--bash-always-allow`. HolmesGPT talks to models through
LiteLLM, so it runs the same Claude model as the Claude Code cells:
`GYM_HOLMES_MODEL` (required, e.g. `anthropic/<model id>`) with
`ANTHROPIC_API_KEY` from the environment. `--json-output-file` gives the
answer text and whatever cost the response reports.

`GYM_HOLMES` names the binary (the portable tests use a fake),
`GYM_HOLMES_CONFIG` an optional config file (toolsets), `GYM_HOLMES_TIMEOUT`
caps the session in seconds and `GYM_HOLMES_MAX_STEPS` its loop (default 40).
"""
import json
import os
from pathlib import Path
import sys
import time

import tool_session

FORBIDDEN_FLAGS = ("--bash-always-allow", "--destination", "--slack-token")
DEFAULT_TIMEOUT = 1500
DEFAULT_MAX_STEPS = "40"


def holmes_argv(binary, model, prompt_file, output_file, max_steps, config=None):
    argv = [
        binary, "ask",
        "--prompt-file", str(prompt_file),
        "--model", model,
        "--no-interactive",
        "--bash-always-deny",
        "--json-output-file", str(output_file),
        "--max-steps", str(max_steps),
    ]
    if config:
        argv += ["--config", str(config)]
    tool_session.refuse_flags(argv, FORBIDDEN_FLAGS)
    return argv


def reported_cost(value):
    """The first numeric `total_cost`-like field anywhere in the response, or None."""
    if isinstance(value, dict):
        for key, inner in value.items():
            if "cost" in key.lower() and isinstance(inner, (int, float)) and not isinstance(inner, bool):
                return float(inner)
        for inner in value.values():
            found = reported_cost(inner)
            if found is not None:
                return found
    if isinstance(value, list):
        for inner in value:
            found = reported_cost(inner)
            if found is not None:
                return found
    return None


def main():
    prompt = tool_session.read_prompt()
    binary = os.environ.get("GYM_HOLMES", "holmes")
    model = os.environ.get("GYM_HOLMES_MODEL")
    if not model:
        raise SystemExit("set GYM_HOLMES_MODEL to the model HolmesGPT should use")
    run_dir = Path(os.environ["GYM_ANSWER"]).parent
    prompt_file = run_dir / "holmes-prompt.md"
    prompt_file.write_text(prompt)
    output_file = run_dir / "holmes-response.json"
    argv = holmes_argv(
        binary, model, prompt_file, output_file,
        os.environ.get("GYM_HOLMES_MAX_STEPS", DEFAULT_MAX_STEPS), os.environ.get("GYM_HOLMES_CONFIG"),
    )
    env = dict(os.environ)
    version = tool_session.version_of([binary, "version"], env)
    started = time.time()
    stdout, stderr, code, seconds, timed_out = tool_session.run_tool(
        argv, env, int(os.environ.get("GYM_HOLMES_TIMEOUT", DEFAULT_TIMEOUT))
    )
    sys.stdout.write(stdout)
    sys.stderr.write(stderr)
    try:
        response = json.loads(output_file.read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        response = {}
    text = response.get("result") if isinstance(response, dict) else None
    answer = tool_session.answer_in_text(text) or tool_session.answer_in_text(stdout)
    tool_uses = [
        call.get("tool_name") or call.get("name")
        for call in (response.get("tool_calls") or []) if isinstance(call, dict)
    ] if isinstance(response, dict) else []
    tool_session.finish(answer, {
        "solver": "holmesgpt",
        "client": "HolmesGPT",
        "client_version": version,
        "model": model,
        "date": time.strftime("%Y-%m-%d", time.gmtime(started)),
        "started": started,
        "seconds": round(seconds, 1),
        "timed_out": timed_out,
        "cost_usd": reported_cost(response),
        "exit": code,
        "tool_uses": tool_uses,
        "flags": [flag for flag in argv[1:] if flag.startswith("-")],
    })
    return 124 if timed_out else code


if __name__ == "__main__":
    sys.exit(main())
