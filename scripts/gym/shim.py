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

`relish mcp` is the exception. It's a server that talks JSON-RPC over stdin
and stdout for as long as its client runs, so the shim streams it line by line
and records each `tools/call` as the `relish agent` command the tool is, with
the envelope it answered as the output and the exit code that command would
have given. A Claude Code solver that uses the
plugin's MCP server is then scored from the same log as one that types the
commands into a shell. The server process itself isn't a call.

The Kubernetes side of the comparison (#1679) records the same way. The
runner installs this shim as `kubectl`, and as `kubernetes-mcp-server`, the
MCP server its Claude Code solver uses: a kubectl call is logged as its argv,
and an MCP tool call as `kubernetes-mcp-server tool NAME --key=value...`. The
installed script names the program and the real binary (`GYM_SHIM_AS`,
`GYM_SHIM_REAL`); without them it's `relish` and `GYM_REAL_RELISH`, as before.
"""
import json
import os
import subprocess
import sys
import threading
import time

# Enough for any `relish agent` answer (its default budget is far smaller)
# and the start of anything longer.
MAX_KEPT_BYTES = 256 * 1024


def _kept(text):
    if len(text) <= MAX_KEPT_BYTES:
        return text
    return text[:MAX_KEPT_BYTES] + "\n[gym: output cut]"


# The MCP servers the shim knows how to record, by the program it stands in for.
KUBERNETES_MCP = "kubernetes-mcp-server"


def record(log_path, argv, exit_code, stdout, stderr, started, seconds, via=None, program="relish"):
    """Append one call to the log as a JSON line."""
    entry = {
        "argv": [program, *argv],
        "exit": exit_code,
        "stdout": _kept(stdout),
        "stderr": _kept(stderr),
        "started": started,
        "seconds": round(seconds, 3),
    }
    if via:
        entry["via"] = via
    with open(log_path, "a", encoding="utf-8") as log:
        log.write(json.dumps(entry) + "\n")


def tool_argv(name, arguments):
    """The `relish` argv a `relish mcp` tool call stands for.

    A tool is named after its `relish agent` command with spaces as
    underscores (`source_grep`), and its arguments become `--name=value`, so
    the scorer reads the command and subcommand the same way it reads a typed
    one.
    """
    argv = ["agent", *name.split("_")]
    for key, value in sorted((arguments or {}).items()):
        if value is None or value is False:
            continue
        if value is True:
            argv.append(f"--{key}")
        else:
            argv.append(f"--{key}={value if isinstance(value, str) else json.dumps(value)}")
    return argv


def k8s_tool_argv(name, arguments):
    """The argv a Kubernetes MCP tool call is recorded as: `tool NAME --key=value...`."""
    argv = ["tool", name]
    for key, value in sorted((arguments or {}).items()):
        if value is None:
            continue
        argv.append(f"--{key}={value if isinstance(value, str) else json.dumps(value)}")
    return argv


# The exit code `relish agent` ends with for each envelope error code
# (`ErrorCode::exit_status` in src/relish/agent/envelope.rs): usage 2,
# unreachable 3, refused 4, anything else 5. A tool error is recorded with the
# code the typed command would have given, so a scenario's outcome (a
# guardrail refusal is exit 4, a stale undo exit 5) reads the same either way.
USAGE_CODES = {"usage"}
UNREACHABLE_CODES = {
    "no_endpoint", "unreachable", "dns", "tls_untrusted", "timeout", "unauthenticated",
}
REFUSED_CODES = {"forbidden", "lease_held", "guardrail"}


def exit_for_error_code(code):
    """The exit code `relish agent` gives a failure with envelope error `code`."""
    if code in USAGE_CODES:
        return 2
    if code in UNREACHABLE_CODES:
        return 3
    if code in REFUSED_CODES:
        return 4
    return 5


def _envelope_error_code(text):
    try:
        envelope = json.loads(text)
    except json.JSONDecodeError:
        return None
    error = envelope.get("error") if isinstance(envelope, dict) else None
    code = error.get("code") if isinstance(error, dict) else None
    return code if isinstance(code, str) else None


def _tool_output(message):
    """The text a tool answered with, and the exit code it stands for.

    A protocol error (an unknown tool, arguments that don't fit its schema)
    has no envelope and no command behind it, so it's recorded as exit 1.
    """
    if "error" in message:
        return json.dumps(message["error"]), 1
    result = message.get("result") or {}
    text = "".join(
        block.get("text", "") for block in result.get("content", []) if isinstance(block, dict)
    )
    if not result.get("isError"):
        return text, 0
    code = _envelope_error_code(text)
    return text, 1 if code is None else exit_for_error_code(code)


def with_mcp_exit(call):
    """`call` with a `relish mcp` tool error's exit read from its envelope.

    Logs written before the shim read the envelope's code recorded every tool
    error as exit 1; scoring such a log again reads the code it answered. A
    call that isn't a `relish mcp` tool error comes back unchanged.
    """
    if call.get("via") != "mcp" or call.get("argv", ["relish"])[:1] != ["relish"]:
        return call
    if call.get("exit") in (0, None):
        return call
    code = _envelope_error_code(call.get("stdout", ""))
    if code is None:
        return call
    return {**call, "exit": exit_for_error_code(code)}


def _parse(line):
    try:
        message = json.loads(line)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    return message if isinstance(message, dict) else None


def serve_mcp(real, argv, log_path, stdin=None, stdout=None, program="relish", env=None):
    """Run an MCP server with its stdio passed through, recording tool calls.

    `relish mcp` tool calls are recorded as the `relish agent` command each
    tool is; any other server's as `tool NAME --key=value...`.
    """
    stdin = stdin or sys.stdin.buffer
    stdout = stdout or sys.stdout.buffer
    to_argv = tool_argv if program == "relish" else k8s_tool_argv
    server = subprocess.Popen(
        [real, *argv], stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env
    )
    pending = {}
    lock = threading.Lock()

    def forward_requests():
        for line in stdin:
            message = _parse(line)
            if message and message.get("method") == "tools/call" and "id" in message:
                params = message.get("params") or {}
                with lock:
                    pending[json.dumps(message["id"])] = (
                        params.get("name", ""), params.get("arguments") or {},
                        time.time(), time.monotonic(),
                    )
            try:
                server.stdin.write(line)
                server.stdin.flush()
            except BrokenPipeError:
                return
        server.stdin.close()

    threading.Thread(target=forward_requests, daemon=True).start()
    for line in server.stdout:
        stdout.write(line)
        stdout.flush()
        message = _parse(line)
        if not message or "id" not in message or "method" in message:
            continue
        with lock:
            request = pending.pop(json.dumps(message["id"]), None)
        if request is None:
            continue
        name, arguments, started, clock = request
        text, exit_code = _tool_output(message)
        record(log_path, to_argv(name, arguments), exit_code, text, "", started,
               time.monotonic() - clock, via="mcp", program=program)
    server.stdout.close()
    return server.wait()


def main(argv):
    program = os.environ.get("GYM_SHIM_AS", "relish")
    real = os.environ.get("GYM_SHIM_REAL") or os.environ["GYM_REAL_RELISH"]
    log_path = os.environ["GYM_CALL_LOG"]
    # The real program doesn't need to know it was shimmed.
    env = {key: value for key, value in os.environ.items() if not key.startswith("GYM_SHIM_")}
    if program == "relish" and argv[:1] == ["mcp"] and not {"--help", "-h", "--list"} & set(argv):
        return serve_mcp(real, argv, log_path, env=env)
    if program == KUBERNETES_MCP and not {"--help", "-h", "--version"} & set(argv):
        return serve_mcp(real, argv, log_path, program=program, env=env)
    started = time.time()
    clock = time.monotonic()
    completed = subprocess.run(
        [real, *argv], capture_output=True, text=True, errors="replace", check=False, env=env
    )
    record(
        log_path,
        argv,
        completed.returncode,
        completed.stdout,
        completed.stderr,
        started,
        time.monotonic() - clock,
        program=program,
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


def install_program(bin_dir, program, real, python=sys.executable):
    """Write a recording `program` (e.g. `kubectl`) that runs `real` into `bin_dir`."""
    bin_dir.mkdir(parents=True, exist_ok=True)
    script = bin_dir / program
    script.write_text(
        "#!/bin/sh\n"
        f'exec env GYM_SHIM_AS="{program}" GYM_SHIM_REAL="{real}" '
        f'"{python}" "{os.path.abspath(__file__)}" "$@"\n'
    )
    script.chmod(0o755)
    return script


def read_log(log_path):
    """Every call in a log, in order; an absent log is no calls."""
    try:
        lines = log_path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return []
    return [json.loads(line) for line in lines if line.strip()]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
