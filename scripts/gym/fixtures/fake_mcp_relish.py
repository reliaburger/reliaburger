"""A stand-in `relish` whose `mcp` serves one tool (stdlib only).

For the local-model solver's portable tests: `relish mcp` speaks
newline-delimited JSON-RPC and offers `diagnose`, which answers with the
`relish agent diagnose` envelope recorded in the crash-loop fixture's
`wtf -o json` call. Any other command goes to fake_relish.py.
"""
import json
from pathlib import Path
import runpy
import sys

TOOL = {
    "name": "diagnose",
    "description": "Ranked findings for the cluster",
    "inputSchema": {"type": "object", "properties": {"max_bytes": {"type": "integer"}}},
}
ENVELOPE = {"ok": True, "data": {"findings": [{"id": "crashloop",
                                              "affected_resource": "app.web/default"}]}}


def serve():
    for line in sys.stdin:
        message = json.loads(line)
        method = message.get("method")
        if "id" not in message:
            continue
        if method == "initialize":
            result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                      "serverInfo": {"name": "relish", "version": "0.0.0"}}
        elif method == "tools/list":
            result = {"tools": [TOOL]}
        elif method == "tools/call":
            result = {"content": [{"type": "text", "text": json.dumps(ENVELOPE)}],
                      "isError": False}
        else:
            result = {}
        print(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}), flush=True)


if __name__ == "__main__":
    if sys.argv[1:2] == ["mcp"]:
        serve()
    else:
        sys.argv = [sys.argv[0], *sys.argv[1:]]
        runpy.run_path(str(Path(__file__).parent / "fake_relish.py"), run_name="__main__")
