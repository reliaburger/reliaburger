#!/usr/bin/env python3
"""An open local model as a gym solver, through Ollama (#1687).

    python3 scripts/gym/gym.py run --solver command:scripts/gym/local_model.py ...

The runner starts this with the prompt on stdin (and in `GYM_PROMPT`), the
recording `relish` first on PATH and a read-only token in
`RELIABURGER_TOKEN`, as it starts claude_code.py. Nothing here leaves the
machine: the model runs in [Ollama](https://ollama.com) on this host, and
this script is the agent loop around it, stdlib only.

It starts `relish mcp` through the recording `relish`, as Claude Code starts
the plugin's server, and offers the model two kinds of tool, matching what
the Claude Code solver has:

- every tool `relish mcp` lists, with its input schema as the function's
  parameters;
- `relish`, which runs one `relish` command with the arguments the model
  gives (an array of strings, no shell), like Claude Code's `Bash(relish *)`.
  It refuses `relish mcp` and `relish agent patch build|test|try`, as the
  Claude Code solver does, and cuts long output to the tool budget.

The loop is the plainest one that works: send the conversation and the tools
to Ollama's `/api/chat`, run every tool call the reply asks for, send the
results back as `tool` messages, and stop when a reply asks for none. If
that reply holds no JSON answer, or the turns run out, it asks once more
with no tools and the answer's JSON Schema as Ollama's `format`
(structured output), and writes what comes back to `GYM_ANSWER`.

It writes what it knows to `GYM_SOLVER_META`: the model, its digest,
parameter count and quantisation, the Ollama version, the context window
(`num_ctx`), tokens in and out summed over the session, the largest prompt
any one turn sent, how many turns filled the window (Ollama then drops the
start of the conversation without saying so), and the tools called.

Settings, all optional but the model:

- `GYM_LOCAL_MODEL`: the Ollama model, such as `qwen3:8b` (required);
- `GYM_OLLAMA_URL`: default `http://127.0.0.1:11434`;
- `GYM_LOCAL_NUM_CTX`: the context window in tokens, default 32768;
- `GYM_LOCAL_THINK`: `1` lets a thinking model think, default off;
- `GYM_LOCAL_MAX_TURNS`: default 30;
- `GYM_LOCAL_MAX_BYTES`: a budget for every MCP tool that takes
  `max_bytes`: the default its schema advertises (models copy it) and the
  value sent when the model names none. Default: none, so each command's own;
- `GYM_LOCAL_TOOL_BYTES`: the cut for the `relish` tool's output and any
  answer the MCP server sends, default 24576, `relish agent`'s own default;
- `GYM_LOCAL_TOOLS`: `mcp,relish` (default), `mcp` or `relish`;
- `GYM_LOCAL_TIMEOUT`: seconds for the whole session, default 1500;
- `GYM_LOCAL_TASK_ROLE`: `system` sends the task as a system message, which
  Ollama keeps when it trims a long conversation to the window; default
  `user`, the first user message, which it drops first.
"""
import copy
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import threading
import time
import urllib.request

from claude_code import answer_schema
from solvers import answer_in_text

DEFAULT_URL = "http://127.0.0.1:11434"
DEFAULT_NUM_CTX = 32768
DEFAULT_MAX_TURNS = 30
DEFAULT_TOOL_BYTES = 24 * 1024
DEFAULT_TIMEOUT = 1500
# One `relish` command, and one MCP tool call, never take longer than this.
CALL_TIMEOUT = 120
# A turn whose prompt came within this many tokens of the window filled it.
CONTEXT_MARGIN = 64

NOTE = """
## How this session works

You can't write files. When you're done, reply with the JSON object
described above as your last message, with no tool calls; the program that
started you writes it to GYM_ANSWER. Your tools are the Reliaburger MCP
tools and `relish`, which runs one `relish` command.
"""

# The first user message when the task is the system message.
START = "Start now."

ASK_FOR_ANSWER = (
    "Stop investigating now. Reply with only the JSON object the task asks for, "
    "using what you found. Use null for any field you can't name."
)

RELISH_TOOL = {
    "type": "function",
    "function": {
        "name": "relish",
        "description": (
            "Run one `relish` command against the cluster, with a read-only token, and "
            "get its output. Pass the arguments after `relish`, one per array item, "
            'such as ["agent", "diagnose"] or ["logs", "web", "--tail", "20"]. No shell.'
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "args": {"type": "array", "items": {"type": "string"},
                         "description": "the arguments after `relish`"},
            },
            "required": ["args"],
        },
    },
}

# What the Claude Code solver denies too (claude_code.DISALLOWED_TOOLS): these
# build the crate or boot a cluster on the host.
REFUSED_PREFIXES = (("agent", "patch", "build"), ("agent", "patch", "test"),
                    ("agent", "patch", "try"))


def refused(args):
    """Why the `relish` tool won't run `args`, or None when it will."""
    if not isinstance(args, list) or not all(isinstance(arg, str) for arg in args):
        return "args must be an array of strings, the arguments after `relish`"
    # Anywhere in the line, so a flag before the subcommand doesn't hide it.
    # An app called `mcp` is refused too; no scenario has one.
    if "mcp" in args:
        return "`relish mcp` is the server your other tools already talk to"
    words = tuple(arg for arg in args if not arg.startswith("-"))
    for prefix in REFUSED_PREFIXES:
        if any(words[i:i + len(prefix)] == prefix for i in range(len(words))):
            return f"`relish {' '.join(prefix)}` builds on this host; the gym checks patches itself"
    return None


def cap(text, max_bytes):
    """`text` cut to `max_bytes` (UTF-8), ending with how much was cut."""
    data = text.encode()
    if len(data) <= max_bytes:
        return text
    kept = data[:max_bytes].decode(errors="ignore")
    return kept + f"\n[{len(data) - max_bytes} more bytes cut]"


def ollama_tool(tool, max_bytes=None):
    """An MCP tool as an Ollama function tool.

    With `max_bytes`, a `max_bytes` parameter advertises that as its default
    instead of the command's own: models copy the default into their calls.
    """
    parameters = copy.deepcopy(tool.get("inputSchema") or {"type": "object", "properties": {}})
    budget = (parameters.get("properties") or {}).get("max_bytes")
    if max_bytes is not None and isinstance(budget, dict):
        budget["default"] = max_bytes
    return {
        "type": "function",
        "function": {
            "name": tool["name"],
            "description": tool.get("description", ""),
            "parameters": parameters,
        },
    }


def with_budget(arguments, schema, max_bytes):
    """The call's arguments with `max_bytes` set, when the tool takes one and none was named."""
    if max_bytes is None or "max_bytes" in arguments:
        return arguments
    if "max_bytes" not in ((schema or {}).get("properties") or {}):
        return arguments
    return {**arguments, "max_bytes": max_bytes}


class McpClient:
    """A minimal MCP client over a child's stdio: newline-delimited JSON-RPC."""

    def __init__(self, argv, env):
        self.process = subprocess.Popen(
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, env=env,
        )
        self.replies = queue.Queue()
        self.next_id = 0
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.process.stdout:
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(message, dict) and "id" in message and "method" not in message:
                self.replies.put(message)
        self.replies.put(None)

    def _send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def request(self, method, params=None, timeout=CALL_TIMEOUT):
        self.next_id += 1
        wanted = self.next_id
        self._send({"jsonrpc": "2.0", "id": wanted, "method": method, "params": params or {}})
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"{method} took longer than {timeout} s")
            try:
                message = self.replies.get(timeout=remaining)
            except queue.Empty:
                continue
            if message is None:
                raise ConnectionError("the MCP server exited")
            if message.get("id") == wanted:
                if "error" in message:
                    raise RuntimeError(json.dumps(message["error"]))
                return message.get("result") or {}

    def start(self):
        self.request("initialize", {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "reliaburger-gym-local-model", "version": "1"},
        })
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        return self.request("tools/list").get("tools", [])

    def call(self, name, arguments):
        """The tool's text answer, whether it failed or not."""
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        return "".join(
            block.get("text", "") for block in result.get("content", []) if isinstance(block, dict)
        )

    def close(self):
        try:
            self.process.stdin.close()
        except OSError:
            pass
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()


class Ollama:
    """Ollama's HTTP API: `/api/chat`, plus what the record says about the model."""

    def __init__(self, url, model):
        self.url = url.rstrip("/")
        self.model = model

    def _call(self, path, body=None, timeout=60):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(
            self.url + path, data=data, headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.loads(response.read())

    def chat(self, messages, options, think, timeout, tools=None, format=None):
        body = {"model": self.model, "messages": messages, "stream": False, "options": options}
        # A model that can't think refuses the field, even set to false.
        if think is not None:
            body["think"] = think
        if tools:
            body["tools"] = tools
        if format is not None:
            body["format"] = format
        return self._call("/api/chat", body, timeout=max(1, timeout))

    def describe(self):
        """The Ollama version and the model's digest, size and quantisation."""
        found = {}
        try:
            found["client_version"] = self._call("/api/version").get("version")
            tags = self._call("/api/tags").get("models", [])
            found["model_digest"] = next(
                (m.get("digest") for m in tags if m.get("name") == self.model), None)
            shown = self._call("/api/show", {"model": self.model})
            details = shown.get("details") or {}
            found["parameters"] = details.get("parameter_size")
            found["quantization"] = details.get("quantization_level")
            found["capabilities"] = shown.get("capabilities")
        except (OSError, ValueError):
            pass
        return found


class Session:
    """One gym session: the conversation, the tools and what they cost."""

    def __init__(self, ollama, mcp, mcp_tools, settings, env, work_dir, capabilities=None):
        self.ollama = ollama
        # Sent only to a model that says it thinks, or says nothing either way.
        thinks = capabilities is None or "thinking" in capabilities
        self.think = settings["think"] if thinks else None
        self.mcp = mcp
        self.schemas = {tool["name"]: tool.get("inputSchema") or {} for tool in mcp_tools}
        self.settings = settings
        self.env = env
        self.work_dir = work_dir
        self.tools = [ollama_tool(tool, settings["max_bytes"]) for tool in mcp_tools] if mcp else []
        if "relish" in settings["tools"]:
            self.tools.append(RELISH_TOOL)
        self.messages = []
        self.turns = 0
        self.prompt_tokens = 0
        self.completion_tokens = 0
        self.peak_prompt_tokens = 0
        self.context_full_turns = 0
        self.prompt_tokens_by_turn = []
        self.last_tool_prompt = None
        self.tool_uses = []
        self.transcript = []

    def _chat(self, deadline, tools=None, format=None):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("the session ran out of time")
        reply = self.ollama.chat(
            self.messages, {"num_ctx": self.settings["num_ctx"]}, self.think,
            remaining, tools=tools, format=format,
        )
        self.turns += 1
        prompt = reply.get("prompt_eval_count") or 0
        self.prompt_tokens += prompt
        self.completion_tokens += reply.get("eval_count") or 0
        self.peak_prompt_tokens = max(self.peak_prompt_tokens, prompt)
        self.prompt_tokens_by_turn.append(prompt)
        # The history only grows, so a prompt smaller than the last one sent
        # with the same tools means Ollama dropped the start to fit the
        # window, and says nothing. (The last turn sends no tools, so it's
        # smaller by their definitions and is compared with the window only.)
        shrank = tools is not None and self.last_tool_prompt is not None and prompt < self.last_tool_prompt
        if shrank or prompt >= self.settings["num_ctx"] - CONTEXT_MARGIN:
            self.context_full_turns += 1
        if tools is not None:
            self.last_tool_prompt = prompt
        self.transcript.append(reply)
        message = reply.get("message") or {}
        # Thinking stays out of the history: it costs context and the next
        # turn doesn't need it.
        kept = {"role": "assistant", "content": message.get("content") or ""}
        if message.get("tool_calls"):
            kept["tool_calls"] = message["tool_calls"]
        self.messages.append(kept)
        return message

    def _run_tool(self, call):
        function = call.get("function") or {}
        name = function.get("name", "")
        arguments = function.get("arguments") or {}
        if isinstance(arguments, str):
            try:
                arguments = json.loads(arguments)
            except json.JSONDecodeError:
                return name, "error: the arguments aren't JSON"
        self.tool_uses.append(name)
        budget = self.settings["tool_bytes"]
        if name == "relish" and "relish" in self.settings["tools"]:
            args = arguments.get("args") if isinstance(arguments, dict) else None
            reason = refused(args)
            if reason:
                return name, f"refused: {reason}"
            try:
                completed = subprocess.run(
                    ["relish", *args], capture_output=True, text=True, errors="replace",
                    env=self.env, cwd=self.work_dir, timeout=CALL_TIMEOUT, check=False,
                )
            except subprocess.TimeoutExpired:
                return name, f"error: the command took longer than {CALL_TIMEOUT} s"
            text = completed.stdout
            if completed.stderr.strip():
                text += "\n[stderr]\n" + completed.stderr
            return name, cap(f"[exit {completed.returncode}]\n{text}", budget)
        if self.mcp is not None and name in self.schemas:
            arguments = with_budget(arguments, self.schemas[name], self.settings["max_bytes"])
            try:
                return name, cap(self.mcp.call(name, arguments), budget)
            except (TimeoutError, ConnectionError, RuntimeError, OSError) as error:
                return name, f"error: {error}"
        return name, f"error: there's no tool named {name}; use one of the tools you were given"

    def run(self, prompt, deadline):
        """The answer, or None, and why the session stopped."""
        task = prompt.rstrip() + "\n" + NOTE
        if self.settings["task_role"] == "system":
            self.messages.append({"role": "system", "content": task})
            self.messages.append({"role": "user", "content": START})
        else:
            self.messages.append({"role": "user", "content": task})
        stop = "max_turns"
        for _ in range(self.settings["max_turns"]):
            message = self._chat(deadline, tools=self.tools)
            calls = message.get("tool_calls") or []
            if not calls:
                answer = answer_in_text(message.get("content"))
                if answer is not None:
                    return answer, "answered"
                stop = "asked_for_answer"
                break
            for call in calls:
                name, text = self._run_tool(call)
                self.messages.append({"role": "tool", "tool_name": name, "content": text})
        self.messages.append({"role": "user", "content": ASK_FOR_ANSWER})
        message = self._chat(deadline, format=answer_schema())
        answer = answer_in_text(message.get("content"))
        if answer is None:
            try:
                parsed = json.loads(message.get("content") or "")
                answer = parsed if isinstance(parsed, dict) else None
            except json.JSONDecodeError:
                answer = None
        return answer, stop

    def meta(self):
        return {
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "peak_prompt_tokens": self.peak_prompt_tokens,
            "context_full_turns": self.context_full_turns,
            "prompt_tokens_by_turn": self.prompt_tokens_by_turn,
            "turns": self.turns,
            "tool_uses": self.tool_uses,
            "tools_offered": [tool["function"]["name"] for tool in self.tools],
        }


def settings_from(environ):
    tools = {item.strip() for item in environ.get("GYM_LOCAL_TOOLS", "mcp,relish").split(",")}
    max_bytes = environ.get("GYM_LOCAL_MAX_BYTES")
    return {
        "model": environ.get("GYM_LOCAL_MODEL"),
        "url": environ.get("GYM_OLLAMA_URL", DEFAULT_URL),
        "num_ctx": int(environ.get("GYM_LOCAL_NUM_CTX", DEFAULT_NUM_CTX)),
        "think": environ.get("GYM_LOCAL_THINK", "0") not in ("", "0", "false"),
        "max_turns": int(environ.get("GYM_LOCAL_MAX_TURNS", DEFAULT_MAX_TURNS)),
        "max_bytes": int(max_bytes) if max_bytes else None,
        "tool_bytes": int(environ.get("GYM_LOCAL_TOOL_BYTES", DEFAULT_TOOL_BYTES)),
        "tools": tools,
        "timeout": int(environ.get("GYM_LOCAL_TIMEOUT", DEFAULT_TIMEOUT)),
        "task_role": "system" if environ.get("GYM_LOCAL_TASK_ROLE") == "system" else "user",
    }


def main():
    prompt = sys.stdin.read()
    if not prompt.strip() and os.environ.get("GYM_PROMPT"):
        prompt = Path(os.environ["GYM_PROMPT"]).read_text()
    settings = settings_from(os.environ)
    if not settings["model"]:
        raise SystemExit("set GYM_LOCAL_MODEL to the Ollama model to run, such as qwen3:8b")
    env = dict(os.environ)
    ollama = Ollama(settings["url"], settings["model"])
    started = time.time()
    clock = time.monotonic()
    deadline = clock + settings["timeout"]
    mcp = None
    mcp_tools = []
    described = ollama.describe()
    session = None
    answer, stop, error = None, None, None
    try:
        if "mcp" in settings["tools"]:
            mcp = McpClient(["relish", "mcp"], env)
            mcp_tools = mcp.start()
        session = Session(ollama, mcp, mcp_tools, settings, env, os.getcwd(),
                          described.get("capabilities"))
        answer, stop = session.run(prompt, deadline)
    except TimeoutError:
        stop = "timeout"
    except (OSError, ValueError, ConnectionError, RuntimeError) as failure:
        stop, error = "error", str(failure)
    finally:
        if mcp is not None:
            mcp.close()
    seconds = time.monotonic() - clock
    if session is None:
        session = Session(ollama, None, [], settings, env, os.getcwd())
    sys.stdout.write(json.dumps({"messages": session.messages}, indent=1) + "\n")
    if error:
        sys.stderr.write(f"local model session failed: {error}\n")
    if answer is not None and os.environ.get("GYM_ANSWER"):
        Path(os.environ["GYM_ANSWER"]).write_text(json.dumps(answer, indent=2) + "\n")
    if os.environ.get("GYM_SOLVER_META"):
        meta = {
            "solver": "local-model",
            "client": "Ollama",
            "model": settings["model"],
            **described,
            "date": time.strftime("%Y-%m-%d", time.gmtime(started)),
            "started": started,
            "seconds": round(seconds, 1),
            "timed_out": stop == "timeout",
            "stop": stop,
            "error": error,
            "num_ctx": settings["num_ctx"],
            "think": settings["think"],
            "max_turns": settings["max_turns"],
            "max_bytes": settings["max_bytes"],
            "tool_bytes": settings["tool_bytes"],
            "task_role": settings["task_role"],
            "cost_usd": 0,
            "cost_note": "a local model: no API spend",
            **session.meta(),
        }
        Path(os.environ["GYM_SOLVER_META"]).write_text(json.dumps(meta, indent=2) + "\n")
    if stop == "timeout":
        return 124
    return 1 if stop == "error" else 0


if __name__ == "__main__":
    sys.exit(main())
