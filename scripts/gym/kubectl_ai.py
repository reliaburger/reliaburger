#!/usr/bin/env python3
"""kubectl-ai as a gym solver, for the Kubernetes comparison (#1679).

    python3 scripts/gym/gym.py run --platform k8s --solver command:scripts/gym/kubectl_ai.py ...

[kubectl-ai](https://github.com/GoogleCloudPlatform/kubectl-ai) (pinned in
k8s/versions.env) runs once in its non-interactive mode, `--quiet`, with the
gym's prompt and the solver's read-only kubeconfig. Its `kubectl` tool runs
`kubectl` from PATH, which the runner made the recording shim, so its calls
are logged and scored like any other solver's.

Never `--skip-permissions`. Without it, `--quiet` ends the session as soon
as the model asks for a command kubectl-ai can't show is read-only (any
kubectl write, and any shell command that isn't kubectl), so it can't touch
this host either. That ending is the tool's own behaviour, and a run that
ends there without an answer scores `no_answer`.

kubectl-ai has no Anthropic API provider. It reaches Claude through AWS
Bedrock (`--llm-provider bedrock`), so the same model as the Claude Code
cells needs a Bedrock model id; where that isn't available the record says
which model ran instead. `GYM_KUBECTL_AI_PROVIDER` (default `bedrock`) and
`GYM_KUBECTL_AI_MODEL` (required) choose it; the AWS credentials come from
the environment. kubectl-ai doesn't report cost, so the record says so.

`GYM_KUBECTL_AI` names the binary (the portable tests use a fake),
`GYM_KUBECTL_AI_TIMEOUT` caps the session in seconds and
`GYM_KUBECTL_AI_MAX_ITERATIONS` its agent loop (default 30).
"""
import os
from pathlib import Path
import sys
import time

import tool_session

FORBIDDEN_FLAGS = ("--skip-permissions", "--mcp-server", "--mcp-client")
DEFAULT_TIMEOUT = 1500
DEFAULT_MAX_ITERATIONS = "30"


def kubectl_ai_argv(binary, provider, model, kubeconfig, max_iterations, trace_path, prompt):
    argv = [
        binary, "--quiet",
        "--llm-provider", provider,
        "--model", model,
        "--kubeconfig", kubeconfig,
        "--max-iterations", str(max_iterations),
        "--trace-path", str(trace_path),
        "--remove-workdir",
        prompt,
    ]
    tool_session.refuse_flags(argv, FORBIDDEN_FLAGS)
    return argv


def main():
    prompt = tool_session.read_prompt()
    binary = os.environ.get("GYM_KUBECTL_AI", "kubectl-ai")
    provider = os.environ.get("GYM_KUBECTL_AI_PROVIDER", "bedrock")
    model = os.environ.get("GYM_KUBECTL_AI_MODEL")
    if not model:
        raise SystemExit("set GYM_KUBECTL_AI_MODEL to the model kubectl-ai should use")
    run_dir = Path(os.environ["GYM_ANSWER"]).parent
    argv = kubectl_ai_argv(
        binary, provider, model, os.environ["KUBECONFIG"],
        os.environ.get("GYM_KUBECTL_AI_MAX_ITERATIONS", DEFAULT_MAX_ITERATIONS),
        run_dir / "kubectl-ai-trace.txt", prompt,
    )
    env = dict(os.environ)
    version = tool_session.version_of([binary, "version"], env)
    started = time.time()
    stdout, stderr, code, seconds, timed_out = tool_session.run_tool(
        argv, env, int(os.environ.get("GYM_KUBECTL_AI_TIMEOUT", DEFAULT_TIMEOUT))
    )
    sys.stdout.write(stdout)
    sys.stderr.write(stderr)
    answer = tool_session.answer_in_text(stdout)
    tool_session.finish(answer, {
        "solver": "kubectl-ai",
        "client": "kubectl-ai",
        "client_version": version,
        "model": model,
        "provider": provider,
        "date": time.strftime("%Y-%m-%d", time.gmtime(started)),
        "started": started,
        "seconds": round(seconds, 1),
        "timed_out": timed_out,
        "cost_usd": None,
        "cost_note": "kubectl-ai doesn't report cost",
        "exit": code,
        "flags": [flag for flag in argv[1:] if flag.startswith("-")],
        "permission_stop": "RunOnce mode cannot handle permission requests" in (stdout + stderr),
    })
    return 124 if timed_out else code


if __name__ == "__main__":
    sys.exit(main())
