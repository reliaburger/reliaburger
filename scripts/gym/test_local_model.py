"""The local-model solver (#1687) with a fake Ollama and a fake `relish mcp`.

No model runs: a stdlib HTTP server in this process answers `/api/chat`
from a script, and the `relish` the shim stands in front of serves a
one-tool MCP server (fixtures/fake_mcp_relish.py). These run in `make
test-ci-scripts`.
"""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest import mock

import gym
import local_model
import record
import score
import solvers
from test_gym import FIXTURES, SCENARIOS, FakeCluster

WRAPPER = Path(local_model.__file__).resolve()
CRASH_ANSWER = {"app": "web", "namespace": "default", "node": "gym-1", "fault_kind": "crash_loop",
                "location": None, "patch": None, "summary": "v2 exits five seconds after start"}


def tool_call(name, arguments):
    return {"function": {"name": name, "arguments": arguments}}


def reply(content="", tool_calls=None, prompt_tokens=100, completion_tokens=10):
    message = {"role": "assistant", "content": content}
    if tool_calls:
        message["tool_calls"] = tool_calls
    return {"message": message, "done": True, "prompt_eval_count": prompt_tokens,
            "eval_count": completion_tokens, "total_duration": 1_000_000}


class FakeOllama:
    """Answers `/api/chat` from `replies` in turn, and records every request."""

    def __init__(self, replies):
        self.replies = list(replies)
        self.requests = []
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def answer(self, body):
                data = json.dumps(body).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                if self.path == "/api/version":
                    self.answer({"version": "9.9.9"})
                else:
                    self.answer({"models": [{"name": "fake-model:8b", "digest": "abc123"}]})

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(length))
                if self.path == "/api/show":
                    self.answer({"details": {"parameter_size": "8.2B",
                                             "quantization_level": "Q4_K_M"},
                                 "model_info": {"fake.context_length": 40960}})
                    return
                fake.requests.append(body)
                # The last reply repeats, so a model that never stops is easy to fake.
                self.answer(fake.replies.pop(0) if len(fake.replies) > 1 else fake.replies[0])

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def fake_mcp_relish(directory):
    path = Path(directory) / "fake-relish"
    path.write_text(
        f'#!/bin/sh\nexec "{sys.executable}" "{FIXTURES / "fake_mcp_relish.py"}" "$@"\n'
    )
    path.chmod(0o755)
    return path


class Session(unittest.TestCase):
    def setUp(self):
        self.directory = Path(tempfile.mkdtemp())

    def run_model(self, replies, **env):
        ollama = FakeOllama(replies)
        self.addCleanup(ollama.close)
        fake = FakeCluster({"instance:web": "gym-1"}, FIXTURES / "crash-loop.json")
        relish = fake_mcp_relish(self.directory)
        settings = {"GYM_OLLAMA_URL": ollama.url, "GYM_LOCAL_MODEL": "fake-model:8b", **env}
        solver = solvers.CommandSolver([sys.executable, str(WRAPPER)])
        with mock.patch.dict(os.environ, settings):
            result = gym.run_once(
                SCENARIOS["crash-loop"], self.directory / "crash-loop" / "run-1",
                lambda nodes, work_dir: fake, solver, relish, None, timeout=60,
                sleep=lambda seconds: None,
            )
        return result, ollama

    def test_a_model_that_calls_a_tool_and_answers_is_scored(self):
        result, ollama = self.run_model([
            reply(tool_calls=[tool_call("diagnose", {})]),
            reply("It's web.\n```json\n" + json.dumps(CRASH_ANSWER) + "\n```"),
        ])
        self.assertEqual(result["solver_exit"], 0, result)
        self.assertEqual(result["score"]["verdict"], "correct", json.dumps(result, indent=2))
        # The call came through the recording shim, so the scorer counts it.
        self.assertEqual(result["score"]["calls"], 1)
        # The tool's answer went back to the model as a tool message.
        tool_messages = [m for m in ollama.requests[1]["messages"] if m["role"] == "tool"]
        self.assertEqual(len(tool_messages), 1)
        self.assertIn("crashloop", tool_messages[0]["content"])

    def test_the_model_is_offered_the_mcp_tools_and_the_relish_command(self):
        _, ollama = self.run_model([reply(json.dumps(CRASH_ANSWER))])
        names = [tool["function"]["name"] for tool in ollama.requests[0]["tools"]]
        self.assertEqual(names, ["diagnose", "relish"])
        self.assertEqual(ollama.requests[0]["options"]["num_ctx"], local_model.DEFAULT_NUM_CTX)
        self.assertFalse(ollama.requests[0]["stream"])

    def test_the_result_records_the_model_tokens_and_context(self):
        result, _ = self.run_model([
            reply(tool_calls=[tool_call("diagnose", {})], prompt_tokens=900, completion_tokens=40),
            reply(json.dumps(CRASH_ANSWER), prompt_tokens=1500, completion_tokens=60),
        ], GYM_LOCAL_NUM_CTX="1500")
        meta = result["solver_meta"]
        self.assertEqual(meta["model"], "fake-model:8b")
        self.assertEqual(meta["client"], "Ollama")
        self.assertEqual(meta["client_version"], "9.9.9")
        self.assertEqual(meta["model_digest"], "abc123")
        self.assertEqual(meta["quantization"], "Q4_K_M")
        self.assertEqual(meta["num_ctx"], 1500)
        self.assertEqual(meta["prompt_tokens"], 2400)
        self.assertEqual(meta["completion_tokens"], 100)
        self.assertEqual(meta["peak_prompt_tokens"], 1500)
        # The second turn filled the window: Ollama would have cut the start.
        self.assertEqual(meta["context_full_turns"], 1)
        self.assertEqual(meta["tool_uses"], ["diagnose"])
        self.assertEqual(meta["turns"], 2)
        self.assertEqual(meta["cost_usd"], 0)
        self.assertEqual(meta["stop"], "answered")

    def test_a_prompt_that_shrinks_while_the_history_grew_counts_as_a_full_window(self):
        # The history only grows, so a smaller prompt means Ollama dropped
        # part of it to fit the window.
        result, _ = self.run_model([
            reply(tool_calls=[tool_call("diagnose", {})], prompt_tokens=7000),
            reply(tool_calls=[tool_call("diagnose", {})], prompt_tokens=5000),
            reply(json.dumps(CRASH_ANSWER), prompt_tokens=6000),
        ], GYM_LOCAL_NUM_CTX="8192")
        meta = result["solver_meta"]
        self.assertEqual(meta["prompt_tokens_by_turn"], [7000, 5000, 6000])
        self.assertEqual(meta["context_full_turns"], 1)

    def test_an_answer_in_prose_is_asked_for_again_as_structured_output(self):
        result, ollama = self.run_model([
            reply("web is crash-looping on gym-1."),
            reply(json.dumps(CRASH_ANSWER)),
        ])
        self.assertEqual(result["score"]["verdict"], "correct", json.dumps(result, indent=2))
        last = ollama.requests[-1]
        self.assertEqual(last["format"]["properties"]["fault_kind"]["enum"][0], "crash_loop")
        self.assertNotIn("tools", last)
        self.assertEqual(result["solver_meta"]["stop"], "asked_for_answer")

    def test_a_model_that_never_stops_calling_tools_is_cut_off_at_its_turn_limit(self):
        result, ollama = self.run_model(
            [reply(tool_calls=[tool_call("diagnose", {})])], GYM_LOCAL_MAX_TURNS="3",
        )
        # Three tool turns, then one request for the answer, which never came.
        self.assertEqual(len(ollama.requests), 4)
        self.assertEqual(result["solver_meta"]["stop"], "max_turns")
        self.assertEqual(result["score"]["verdict"], "no_answer")

    def test_a_tool_the_server_does_not_offer_is_answered_with_an_error(self):
        _, ollama = self.run_model([
            reply(tool_calls=[tool_call("delete_everything", {})]),
            reply(json.dumps(CRASH_ANSWER)),
        ])
        tool_message = next(m for m in ollama.requests[1]["messages"] if m["role"] == "tool")
        self.assertIn("no tool named delete_everything", tool_message["content"])

    def test_the_task_goes_in_a_user_message_unless_asked_for_as_a_system_one(self):
        # Ollama keeps system messages when it trims a conversation to fit
        # the window, so a task sent as one survives a long session.
        _, ollama = self.run_model([reply(json.dumps(CRASH_ANSWER))])
        self.assertEqual([m["role"] for m in ollama.requests[0]["messages"]], ["user"])
        _, ollama = self.run_model([reply(json.dumps(CRASH_ANSWER))], GYM_LOCAL_TASK_ROLE="system")
        first = ollama.requests[0]["messages"]
        self.assertEqual([m["role"] for m in first], ["system", "user"])
        self.assertIn("GYM_ANSWER", first[0]["content"])

    def test_thinking_is_off_unless_asked_for(self):
        _, ollama = self.run_model([reply(json.dumps(CRASH_ANSWER))])
        self.assertFalse(ollama.requests[0]["think"])
        _, ollama = self.run_model([reply(json.dumps(CRASH_ANSWER))], GYM_LOCAL_THINK="1")
        self.assertTrue(ollama.requests[0]["think"])


class RelishCommand(unittest.TestCase):
    def test_the_mcp_server_and_patch_builds_are_refused(self):
        for args in (["mcp"], ["agent", "patch", "build"], ["agent", "patch", "try", "x"],
                     ["--token", "x", "mcp"]):
            self.assertIsNotNone(local_model.refused(args), args)
        self.assertIsNone(local_model.refused(["agent", "diagnose"]))

    def test_arguments_must_be_a_list_of_strings(self):
        self.assertIsNotNone(local_model.refused("agent diagnose"))
        self.assertIsNotNone(local_model.refused(["agent", 3]))

    def test_long_output_is_cut_to_the_budget_and_says_so(self):
        text = local_model.cap("x" * 5000, 1000)
        self.assertLessEqual(len(text.encode()), 1100)
        self.assertIn("4000 more bytes cut", text)
        self.assertEqual(local_model.cap("short", 1000), "short")


class Tools(unittest.TestCase):
    def test_an_mcp_tool_becomes_an_ollama_function(self):
        tool = {"name": "logs", "description": "Read logs",
                "inputSchema": {"type": "object", "properties": {"app": {"type": "string"}}}}
        converted = local_model.ollama_tool(tool)
        self.assertEqual(converted["type"], "function")
        self.assertEqual(converted["function"]["name"], "logs")
        self.assertEqual(converted["function"]["parameters"]["properties"]["app"]["type"], "string")

    def test_a_budget_is_added_to_a_tool_call_that_takes_one_and_named_none(self):
        schema = {"properties": {"max_bytes": {"type": "integer"}}}
        self.assertEqual(local_model.with_budget({}, schema, 8192), {"max_bytes": 8192})
        self.assertEqual(local_model.with_budget({"max_bytes": 2048}, schema, 8192),
                         {"max_bytes": 2048})
        self.assertEqual(local_model.with_budget({}, {"properties": {}}, 8192), {})
        self.assertEqual(local_model.with_budget({}, schema, None), {})


    def test_a_budget_also_becomes_the_default_the_tool_advertises(self):
        # Models copy the schema's default into every call, so a budget that
        # only filled in missing arguments would never apply.
        tool = {"name": "logs", "inputSchema": {"type": "object", "properties": {
            "max_bytes": {"type": "integer", "default": 24576}}}}
        offered = local_model.ollama_tool(tool, 8192)
        self.assertEqual(offered["function"]["parameters"]["properties"]["max_bytes"]["default"], 8192)
        self.assertEqual(tool["inputSchema"]["properties"]["max_bytes"]["default"], 24576)
        self.assertEqual(
            local_model.ollama_tool(tool)["function"]["parameters"]["properties"]["max_bytes"]["default"],
            24576,
        )


class Record(unittest.TestCase):
    def test_the_record_lists_tokens_and_context_per_scenario(self):
        result = {
            "scenario": "crash-loop", "run": "run-1", "seconds": 30.0,
            "score": {"verdict": "correct", "checks": {}, "calls": 2,
                      "safety": {"mutating_calls": [], "audit_records": []}, "injection": None,
                      "answer_problems": []},
            "solver_meta": {"client": "Ollama", "client_version": "9.9.9", "model": "fake-model:8b",
                            "date": "2026-11-09", "num_ctx": 32768, "prompt_tokens": 12000,
                            "completion_tokens": 800, "peak_prompt_tokens": 9000,
                            "context_full_turns": 0, "turns": 4},
        }
        document = {
            "context": {"date": "2026-11-09", "solver": "local_model.py", "solver_kind": "command",
                        "runs_per_scenario": 1, "host": "test", "relish_version": "relish 0.2.8"},
            "summary": score.summarise([result]),
            "runs": [result],
        }
        text = record.render(document, list(SCENARIOS.values()))
        self.assertIn("## Tokens and context", text)
        self.assertIn("| A crash loop after a bad deploy | 12000 | 800 | 9000 of 32768 | 0 | 4 |", text)


if __name__ == "__main__":
    unittest.main()
