"""The Claude Code solver and the shim's `relish mcp` pass-through, with fakes.

No model is called: `GYM_CLAUDE` points the wrapper at fixtures/fake_claude.py,
and the MCP test talks to a fake server. These run in `make test-ci-scripts`.
"""
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

import claude_code
import gym
import record
import score
import shim
import solvers
from test_gym import FIXTURES, SCENARIOS, FakeCluster, fake_relish

WRAPPER = Path(claude_code.__file__).resolve()
CRASH_ANSWER = {"app": "web", "namespace": "default", "node": "gym-1", "fault_kind": "crash_loop",
                "location": None, "patch": None, "summary": "v2 exits five seconds after start"}


def executable(directory, name, script):
    path = Path(directory) / name
    path.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{script}" "$@"\n')
    path.chmod(0o755)
    return path


class Wrapper(unittest.TestCase):
    def setUp(self):
        self.directory = Path(tempfile.mkdtemp())

    def run_claude(self, answer=CRASH_ANSWER, **env):
        fake = FakeCluster({"instance:web": "gym-1"}, FIXTURES / "crash-loop.json")
        claude = executable(self.directory, "fake-claude", FIXTURES / "fake_claude.py")
        relish = fake_relish(self.directory)
        settings = {"GYM_CLAUDE": str(claude), "GYM_FAKE_CLAUDE_ANSWER": json.dumps(answer),
                    "CLAUDECODE": "1", **env}
        solver = solvers.CommandSolver([sys.executable, str(WRAPPER)])
        with mock.patch.dict(os.environ, settings):
            return gym.run_once(
                SCENARIOS["crash-loop"], self.directory / "crash-loop" / "run-1",
                lambda nodes, work_dir: fake, solver, relish, None, timeout=60,
                sleep=lambda seconds: None,
            )

    def test_the_session_answers_through_the_wrapper_and_is_scored(self):
        result = self.run_claude()
        self.assertEqual(result["solver_exit"], 0, result)
        self.assertEqual(result["score"]["verdict"], "correct", json.dumps(result, indent=2))
        # The fake session's `relish status` went through the recording shim.
        self.assertEqual(result["score"]["calls"], 1)
        answer = json.loads((self.directory / "crash-loop" / "run-1" / "answer.json").read_text())
        self.assertEqual(answer["summary"], CRASH_ANSWER["summary"])

    def test_the_result_records_the_model_the_client_version_and_the_date(self):
        meta = self.run_claude()["solver_meta"]
        self.assertEqual(meta["model"], "fake-model")
        self.assertEqual(meta["client_version"], "9.9.9 (Claude Code)")
        self.assertRegex(meta["date"], r"^\d{4}-\d{2}-\d{2}$")
        self.assertEqual(meta["tool_uses"], ["Bash"])
        self.assertEqual(meta["cost_usd"], 0.01)
        self.assertFalse(meta["timed_out"])

    def test_the_record_names_the_model_and_the_client(self):
        result = self.run_claude()
        document = {
            "context": {"date": "2026-11-06", "solver": "claude_code.py", "solver_kind": "command",
                        "runs_per_scenario": 1, "host": "test", "relish_version": "relish 0.2.7"},
            "summary": score.summarise([result]),
            "runs": [result],
        }
        text = record.render(document, list(SCENARIOS.values()))
        self.assertIn("- Model: fake-model", text)
        self.assertIn("- Client: Claude Code 9.9.9 (Claude Code)", text)
        self.assertIn("- Reported cost: $0.01 over 1 session(s)", text)

    def test_a_session_past_its_timeout_is_stopped_and_gives_no_answer(self):
        result = self.run_claude(GYM_CLAUDE_TIMEOUT="1", GYM_FAKE_CLAUDE_SLEEP="30")
        self.assertEqual(result["solver_exit"], 124)
        self.assertTrue(result["solver_meta"]["timed_out"])
        self.assertEqual(result["score"]["verdict"], "no_answer")


class CommandLine(unittest.TestCase):
    def test_no_flag_skips_the_permission_check(self):
        argv = claude_code.claude_argv("claude")
        self.assertFalse(set(claude_code.FORBIDDEN_FLAGS) & set(argv))
        self.assertNotIn("bypassPermissions", argv)
        self.assertEqual(argv[argv.index("--permission-mode") + 1], "dontAsk")

    def test_only_relish_and_the_plugins_tools_are_allowed(self):
        argv = claude_code.claude_argv("claude")
        self.assertEqual(argv[argv.index("--tools") + 1], "Bash,Skill")
        start = argv.index("--allowedTools") + 1
        allowed = argv[start:argv.index("--disallowedTools")]
        self.assertEqual(allowed, ["Bash(relish *)", "mcp__reliaburger", "Skill"])
        self.assertIn("--strict-mcp-config", argv)
        self.assertEqual(argv[argv.index("--setting-sources") + 1], "")
        self.assertTrue((Path(argv[argv.index("--plugin-dir") + 1]) / ".mcp.json").is_file())

    def test_the_schema_offers_the_gyms_fault_kinds(self):
        schema = claude_code.answer_schema()
        self.assertIn("relish_bug", schema["properties"]["fault_kind"]["enum"])
        self.assertIn("location", schema["required"])

    def test_an_answer_in_the_result_text_is_taken_when_there_is_no_structured_output(self):
        event = {"type": "result", "result": 'Done.\n```json\n{"app": "web"}\n```'}
        self.assertEqual(claude_code.answer_from(event), {"app": "web"})
        self.assertIsNone(claude_code.answer_from({"type": "result", "result": "no idea"}))

    def test_the_outer_sessions_variables_are_left_out(self):
        env = claude_code.session_env(
            {"CLAUDECODE": "1", "CLAUDE_CODE_SESSION_ID": "x", "PATH": "/bin", "GYM_ANSWER": "a"}
        )
        self.assertEqual(env, {"PATH": "/bin", "GYM_ANSWER": "a"})


FAKE_MCP_SERVER = '''
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if message.get("method") == "tools/call":
        text = json.dumps({"ok": True, "data": {"untrusted": {"line": "GYM-CANARY-7F3A"}}})
        reply = {"jsonrpc": "2.0", "id": message["id"],
                 "result": {"content": [{"type": "text", "text": text}], "isError": False}}
    else:
        reply = {"jsonrpc": "2.0", "id": message.get("id"), "result": {}}
    print(json.dumps(reply), flush=True)
'''


class McpPassThrough(unittest.TestCase):
    def test_each_tool_call_is_recorded_as_its_agent_command(self):
        directory = Path(tempfile.mkdtemp())
        server = directory / "server.py"
        server.write_text(FAKE_MCP_SERVER)
        real = executable(directory, "relish", server)
        log = directory / "calls.jsonl"
        requests = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
             "params": {"name": "logs", "arguments": {"app": "chatty", "tail": 20}}},
            {"jsonrpc": "2.0", "id": 3, "method": "tools/call",
             "params": {"name": "source_grep", "arguments": {"pattern": "fn log_lines"}}},
        ]
        stdin = io.BytesIO("".join(json.dumps(r) + "\n" for r in requests).encode())
        stdout = io.BytesIO()
        self.assertEqual(shim.serve_mcp(str(real), ["mcp"], log, stdin, stdout), 0)
        # Every reply reached the client, in order.
        replies = [json.loads(line) for line in stdout.getvalue().splitlines()]
        self.assertEqual([reply["id"] for reply in replies], [1, 2, 3])
        calls = shim.read_log(log)
        self.assertEqual(
            [call["argv"] for call in calls],
            [["relish", "agent", "logs", "--app=chatty", "--tail=20"],
             ["relish", "agent", "source", "grep", "--pattern=fn log_lines"]],
        )
        self.assertEqual({call["via"] for call in calls}, {"mcp"})
        self.assertIn("GYM-CANARY-7F3A", calls[0]["stdout"])
        self.assertEqual(calls[0]["exit"], 0)

    def test_a_tool_error_is_recorded_with_the_exit_code_the_command_gives(self):
        directory = Path(tempfile.mkdtemp())
        server = directory / "server.py"
        server.write_text(FAKE_FAILING_MCP_SERVER)
        real = executable(directory, "relish", server)
        log = directory / "calls.jsonl"
        codes = ["guardrail", "lease_held", "stale", "usage", "unreachable", "not_found", None,
                 "something_new"]
        requests = [
            {"jsonrpc": "2.0", "id": index, "method": "tools/call",
             "params": {"name": "propose", "arguments": {"kind": "scale", "code": code}}}
            for index, code in enumerate(codes)
        ]
        requests.append({"jsonrpc": "2.0", "id": 99, "method": "tools/call",
                         "params": {"name": "no_such_tool", "arguments": {}}})
        stdin = io.BytesIO("".join(json.dumps(r) + "\n" for r in requests).encode())
        self.assertEqual(shim.serve_mcp(str(real), ["mcp"], log, stdin, io.BytesIO()), 0)
        # As `relish agent`: refused 4, failed 5, usage 2, unreachable 3; an
        # error with no envelope code (a protocol error) stays 1.
        self.assertEqual([call["exit"] for call in shim.read_log(log)],
                         [4, 4, 5, 2, 3, 5, 0, 5, 1])


FAKE_FAILING_MCP_SERVER = '''
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    params = message.get("params") or {}
    if params.get("name") == "no_such_tool":
        reply = {"jsonrpc": "2.0", "id": message["id"],
                 "error": {"code": -32602, "message": "relish mcp has no tool called no_such_tool"}}
    else:
        code = params["arguments"]["code"]
        envelope = {"ok": True, "data": {}} if code is None else {
            "ok": False, "error": {"code": code, "message": "refused"}}
        reply = {"jsonrpc": "2.0", "id": message["id"],
                 "result": {"content": [{"type": "text", "text": json.dumps(envelope)}],
                            "isError": code is not None}}
    print(json.dumps(reply), flush=True)
'''


if __name__ == "__main__":
    unittest.main()
