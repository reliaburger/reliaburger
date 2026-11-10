"""A stand-in `relish` whose `mcp` serves one tool (stdlib only).

For the local-model solver's portable tests: `relish mcp` speaks
newline-delimited JSON-RPC and offers `diagnose`, which answers with the
`relish agent diagnose` envelope recorded in the crash-loop fixture's
`wtf -o json` call. With `--propose` it offers `propose` and `lease` too, as
`relish mcp --propose` does. It writes the arguments it started with to the
file `FAKE_MCP_ARGV` names, when that's set. Any other command goes to
fake_relish.py.
"""
import json
import os
from pathlib import Path
import runpy
import sys

TOOL = {
    "name": "diagnose",
    "description": "Ranked findings for the cluster",
    "inputSchema": {"type": "object", "properties": {"max_bytes": {"type": "integer"}}},
}
PROPOSING = [
    {"name": "propose", "description": "Propose a change for a person to approve",
     "inputSchema": {"type": "object", "properties": {"namespace": {"type": "string"}}}},
    {"name": "lease", "description": "Take, renew and release leases",
     "inputSchema": {"type": "object", "properties": {"namespace": {"type": "string"}}}},
]
ENVELOPE = {"ok": True, "data": {"findings": [{"id": "crashloop",
                                              "affected_resource": "app.web/default"}]}}


def serve(argv):
    tools = [TOOL, *PROPOSING] if "--propose" in argv else [TOOL]
    for line in sys.stdin:
        message = json.loads(line)
        method = message.get("method")
        if "id" not in message:
            continue
        if method == "initialize":
            result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                      "serverInfo": {"name": "relish", "version": "0.0.0"}}
        elif method == "tools/list":
            result = {"tools": tools}
        elif method == "tools/call":
            result = {"content": [{"type": "text", "text": json.dumps(ENVELOPE)}],
                      "isError": False}
        else:
            result = {}
        print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)


if __name__ == "__main__":
    if sys.argv[1:2] == ["mcp"]:
        if os.environ.get("FAKE_MCP_ARGV"):
            Path(os.environ["FAKE_MCP_ARGV"]).write_text(json.dumps(sys.argv[1:]))
        serve(sys.argv[1:])
    else:
        sys.argv = [sys.argv[0], *sys.argv[1:]]
        runpy.run_path(str(Path(__file__).parent / "fake_relish.py"), run_name="__main__")
