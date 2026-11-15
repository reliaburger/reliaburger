"""A stand-in `claude` for the portable tests of claude_code.py (stdlib only).

It checks the command line the wrapper built (print mode, `dontAsk`, the
plugin, no skip-permissions flag), runs one `relish` the way a session's
Bash tool would, and prints a short stream-json transcript whose result
carries `GYM_FAKE_CLAUDE_ANSWER` as structured output. No model is called.

With `GYM_PLATFORM=k8s` it checks the Kubernetes session instead (Bash
only, no plugin, an MCP config whose server is read-only) and runs one
`kubectl get pods` instead.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

FORBIDDEN = ("--dangerously-skip-permissions", "--allow-dangerously-skip-permissions")


def value(argv, flag):
    return argv[argv.index(flag) + 1] if flag in argv else None


def k8s_problems(argv):
    found = []
    if value(argv, "--tools") != "Bash":
        found.append("built-in tools other than Bash")
    if "--plugin-dir" in argv:
        found.append("a plugin on Kubernetes")
    config = value(argv, "--mcp-config")
    if config is None or not Path(config).is_file():
        found.append("no MCP config")
    else:
        server = json.loads(Path(config).read_text())["mcpServers"]["kubernetes"]
        settings = Path(server["args"][server["args"].index("--config") + 1]).read_text()
        if "read_only = true" not in settings:
            found.append("the MCP server isn't read-only")
    return found


def problems(argv):
    found = []
    if "-p" not in argv:
        found.append("not in print mode")
    if value(argv, "--permission-mode") != "dontAsk":
        found.append("permission mode isn't dontAsk")
    if set(FORBIDDEN) & set(argv):
        found.append("a skip-permissions flag")
    if os.environ.get("GYM_PLATFORM") == "k8s":
        found += k8s_problems(argv)
    else:
        if value(argv, "--tools") != "Bash,Skill":
            found.append("built-in tools other than Bash and Skill")
        plugin = value(argv, "--plugin-dir")
        if plugin is None or not (Path(plugin) / ".mcp.json").is_file():
            found.append("no plugin with an .mcp.json")
    if "--json-schema" not in argv:
        found.append("no answer schema")
    return found


def main(argv):
    if argv == ["--version"]:
        print("9.9.9 (Claude Code)")
        return 0
    wrong = problems(argv)
    if wrong:
        sys.stderr.write(f"fake claude: {', '.join(wrong)}\n")
        return 3
    prompt = sys.stdin.read()
    if "GYM_ANSWER" not in prompt or "CLAUDECODE" in os.environ:
        return 4
    time.sleep(float(os.environ.get("GYM_FAKE_CLAUDE_SLEEP", "0")))
    command = (["kubectl", "get", "pods", "-A", "-o", "json"]
               if os.environ.get("GYM_PLATFORM") == "k8s" else ["relish", "status", "-o", "json"])
    shown = subprocess.run(command, capture_output=True, text=True)
    stream = [
        {"type": "system", "subtype": "init", "model": "fake-model", "tools": ["Bash"],
         "mcp_servers": [{"name": "plugin:reliaburger:reliaburger", "status": "connected"}],
         "permissionMode": "dontAsk"},
        {"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "Bash", "input": {"command": " ".join(command)}}]}},
        {"type": "user", "message": {"content": [{"type": "tool_result", "content": shown.stdout}]}},
        {"type": "result", "subtype": "success", "num_turns": 2, "total_cost_usd": 0.01,
         "modelUsage": {"fake-model": {}}, "result": "done",
         "structured_output": json.loads(os.environ["GYM_FAKE_CLAUDE_ANSWER"])},
    ]
    for event in stream:
        print(json.dumps(event))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
