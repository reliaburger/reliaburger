"""Tests for the website's agent recordings (scripts/record/).

    python3 -m unittest discover -s scripts/record -p 'test_*.py'

They check what can go wrong without a cluster or a model: what the screen
shows for each kind of event, the chapter markers, the session's tools, and
when a run is kept.
"""
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import recording  # noqa: E402
import stage  # noqa: E402
import claude_code  # noqa: E402


def plain(lines):
    """Lines with their terminal colours taken out."""
    import re
    return [re.sub("\x1b\\[[0-9;]*m", "", line) for line in lines]


def bash(id_, command):
    return {"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": id_, "name": "Bash", "input": {"command": command}}]}}


def result(id_, content, is_error=False):
    return {"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": id_, "content": content, "is_error": is_error}]}}


class Clock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now


class Screen(unittest.TestCase):
    def test_a_command_opens_its_chapter_once(self):
        renderer = stage.Renderer("debug")
        first = plain(renderer.lines(bash("1", "relish agent source grep 'fn log_lines'")))
        second = plain(renderer.lines(bash("2", "relish agent source show src/relish/client.rs:1-80")))
        self.assertEqual(first, ["", "» Read the source", "$ relish agent source grep 'fn log_lines'"])
        self.assertEqual(second, ["$ relish agent source show src/relish/client.rs:1-80"])

    def test_one_line_json_is_spread_out_and_cut(self):
        renderer = stage.Renderer("debug")
        renderer.lines(bash("1", "relish agent context"))
        answer = json.dumps({"ok": True, "data": {f"key{i}": i for i in range(20)}})
        shown = plain(renderer.lines(result("1", answer)))
        self.assertEqual(shown[0], "{")
        self.assertEqual(len(shown), stage.RESULT_LINES + 1)
        self.assertTrue(shown[-1].startswith("… ") and shown[-1].endswith(" more lines"))

    def test_a_slow_step_says_how_long_it_really_took(self):
        clock = Clock()
        renderer = stage.Renderer("debug", clock=clock)
        renderer.lines(bash("1", "relish agent patch build ./fix"))
        clock.now = 245
        shown = plain(renderer.lines(result("1", [{"type": "text", "text": "built"}])))
        self.assertEqual(shown[-1], "# that took 4 m 05 s; the recording cut the wait")

    def test_a_quick_step_says_nothing_about_time(self):
        clock = Clock()
        renderer = stage.Renderer("debug", clock=clock)
        renderer.lines(bash("1", "relish agent diagnose"))
        clock.now = 3
        self.assertEqual(plain(renderer.lines(result("1", "{}"))), ["{}"])

    def test_a_refused_call_says_so(self):
        renderer = stage.Renderer("act")
        renderer.lines(bash("1", "relish rollback web"))
        self.assertEqual(plain(renderer.lines(result("1", "forbidden", True)))[0], "(refused or failed)")

    def test_mcp_tools_and_file_edits_open_chapters_too(self):
        renderer = stage.Renderer("debug")
        mcp = plain(renderer.lines({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "1", "name": "mcp__plugin_reliaburger_reliaburger__source_grep",
             "input": {"pattern": "log_lines"}}]}}))
        self.assertEqual(mcp[1], "» Read the source")
        self.assertTrue(mcp[2].startswith("mcp source grep {"))
        edit = plain(renderer.lines({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "2", "name": "Edit", "input": {"file_path": "fix/src/relish/client.rs"}}]}}))
        self.assertEqual(edit, ["", "» Write the fix", "✎ edit fix/src/relish/client.rs"])

    def test_the_agents_words_and_the_session_are_labelled(self):
        renderer = stage.Renderer("act")
        self.assertEqual(plain(renderer.lines({"type": "system", "subtype": "init", "model": "m"})),
                         ["# Claude Code, model m"])
        said = plain(renderer.lines({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "web v2 exits after five seconds."}]}}))
        self.assertEqual(said, ["claude ▸ web v2 exits after five seconds."])
        ended = plain(renderer.lines({"type": "result", "num_turns": 9, "duration_ms": 75000}))
        self.assertEqual(ended[-1], "# The session ended after 9 turns and 1 m 15 s.")

    def test_relish_words_skip_flags_variables_and_pipes(self):
        self.assertEqual(stage.relish_words("relish agent patch build ./fix --bun"),
                         ["agent", "patch", "build", "./fix"])
        self.assertEqual(stage.relish_words("RELIABURGER_HOME=/x /tmp/bin/relish status | head"),
                         ["status"])
        self.assertIsNone(stage.relish_words("ls report"))

    def test_durations(self):
        self.assertEqual(stage.took(42.4), "42 s")
        self.assertEqual(stage.took(605), "10 m 05 s")


class Markers(unittest.TestCase):
    def cast(self, version):
        header = {"version": version, "command": "/Users/me/stage.py", "env": {"SHELL": "zsh"}}
        return [json.dumps(header),
                json.dumps([0.5, "o", "hello\r\n"]),
                json.dumps([1.25, "o", "\x1b[1m» Build it\x1b[0m\r\n"])]

    def test_v3_markers_take_the_lines_gap(self):
        out = [json.loads(line) for line in recording.post_process(self.cast(3))]
        self.assertEqual(out[0], {"version": 3})
        self.assertEqual(out[2], [1.25, "m", "Build it"])
        self.assertEqual(out[3][0], 0.0)

    def test_v2_markers_keep_absolute_times(self):
        out = [json.loads(line) for line in recording.post_process(self.cast(2))]
        self.assertEqual(out[2], [1.25, "m", "Build it"])
        self.assertEqual(out[3][0], 1.25)


class Session(unittest.TestCase):
    def test_debug_may_build_and_write_only_in_its_directory(self):
        argv = recording.session_argv("debug", "claude", None, "20")
        self.assertFalse(set(claude_code.FORBIDDEN_FLAGS) & set(argv))
        self.assertEqual(argv[argv.index("--permission-mode") + 1], "dontAsk")
        self.assertNotIn("--disallowedTools", argv)
        start = argv.index("--allowedTools") + 1
        allowed = argv[start:argv.index("--permission-mode")]
        self.assertIn("Bash(relish *)", allowed)
        for tool in ("Read", "Edit", "Write"):
            self.assertIn(f"{tool}(./**)", allowed)
            self.assertNotIn(tool, allowed)

    def test_act_keeps_the_gyms_refusals(self):
        argv = recording.session_argv("act", "claude", None, "20")
        self.assertEqual(argv, claude_code.claude_argv("claude", None, "20"))

    def test_the_prompt_asks_for_every_step_and_keeps_the_answer_note(self):
        prompt = recording.recording_prompt("debug", "Find the bug.")
        for step in ("patch init", "patch build", "patch test", "patch try", "agent report"):
            self.assertIn(step, prompt)
        self.assertIn("Don't post it", prompt)
        self.assertTrue(prompt.endswith(recording.DEBUG_NOTE))
        self.assertNotIn("can't write files", prompt)
        self.assertTrue(recording.recording_prompt("act", "x").endswith(claude_code.NOTE))
        self.assertIn("relish agent lease take", recording.recording_prompt("act", "x"))

    def test_the_exact_prompt_is_the_scenarios_with_the_addendum(self):
        debug = recording.exact_prompt("debug")
        self.assertIn("`relish logs quiet --since 5s`", debug)
        self.assertIn("read-only token", debug)
        self.assertIn("relish agent patch try ./fix", debug)
        act = recording.exact_prompt("act")
        self.assertIn("propose the change", act)
        self.assertIn("relish agent lease take", act)

    def test_the_title_names_the_version(self):
        self.assertEqual(recording.title("debug", "0.2.8"), "Reliaburger 0.2.8: an agent debugs the platform")

    def test_the_person_never_runs_the_shim_or_the_agents_token(self):
        env = stage.person_env({"PATH": "/gym/bin:/usr/bin", "STAGE_PERSON_PATH": "/real:/usr/bin",
                                "GYM_CALL_LOG": "/gym/calls", "RELIABURGER_TOKEN": "agent",
                                "STAGE_PERSON_TOKEN": "person", "HOME": "/h"}, "person")
        self.assertEqual(env, {"PATH": "/real:/usr/bin", "RELIABURGER_TOKEN": "person", "HOME": "/h"})


def call(*words):
    return {"argv": ["relish", *words]}


class Kept(unittest.TestCase):
    def good_debug(self):
        return {"score": {"verdict": "correct"}, "patch_result": "pass", "timed_out": False}

    def debug_calls(self):
        return [call("agent", "source", "grep", "x"), call("agent", "patch", "build", "./fix"),
                call("agent", "patch", "test", "./fix"), call("agent", "patch", "try", "./fix"),
                call("agent", "report", "--out", "./report")]

    def test_a_debug_run_that_did_everything_is_kept(self):
        self.assertEqual(recording.debug_problems(self.good_debug(), self.debug_calls(), [{"dir": "r"}]), [])

    def test_a_debug_run_that_skipped_try_or_the_report_isnt(self):
        calls = [c for c in self.debug_calls() if "try" not in c["argv"]]
        problems = recording.debug_problems(self.good_debug(), calls, [])
        self.assertEqual(problems, ["the session never ran relish agent patch try", "there's no report draft"])

    def test_an_unsafe_or_unpatched_run_isnt(self):
        bad = {"score": {"verdict": "unsafe"}, "patch_result": "fail"}
        problems = recording.debug_problems(bad, self.debug_calls(), [{"dir": "r"}])
        self.assertIn("the gym scored the run unsafe, not correct", problems)
        self.assertIn("the answer's patch didn't pass the reverted tests (fail)", problems)

    def test_a_run_the_gym_couldnt_run_says_only_that(self):
        self.assertEqual(recording.debug_problems({"error": "no leader"}, [], []),
                         ["the gym couldn't run it: no leader"])

    def person(self, **changes):
        person = {"proposal": 3, "approve_exit": 0, "after_approve": "applied",
                  "undo_exit": 0, "after_undo": "undone"}
        person.update(changes)
        return person

    def test_an_act_run_with_a_lease_an_approval_and_an_undo_is_kept(self):
        calls = [call("agent", "lease", "take", "app", "web"), call("agent", "propose", "rollback", "web")]
        self.assertEqual(recording.act_problems({"score": {"verdict": "correct"}}, calls, self.person()), [])

    def test_an_act_run_without_a_lease_or_a_working_undo_isnt(self):
        calls = [call("agent", "propose", "rollback", "web")]
        problems = recording.act_problems({"score": {"verdict": "correct"}}, calls,
                                       self.person(after_undo="applied"))
        self.assertEqual(problems, ["the session never took a lease",
                                    "after_undo was 'applied', not 'undone'"])

    def test_an_act_run_without_a_proposal_isnt(self):
        problems = recording.act_problems({"score": {"verdict": "wrong"}}, [], {"proposal": None})
        self.assertIn("the session made no proposal", problems)


FAKE_CLAUDE = r"""
import json, sys
if "--version" in sys.argv:
    print("9.9.9 (Claude Code)")
    sys.exit(0)
sys.stdin.read()
events = [
    {"type": "system", "subtype": "init", "model": "fake-model", "tools": ["Bash"]},
    {"type": "assistant", "message": {"content": [
        {"type": "text", "text": "Looking at the cluster first."},
        {"type": "tool_use", "id": "1", "name": "Bash", "input": {"command": "relish agent context"}}]}},
    {"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": "1", "content": "{\"ok\": true}"}]}},
    {"type": "result", "num_turns": 2, "duration_ms": 4000, "total_cost_usd": 0.01,
     "structured_output": {"app": "quiet", "fault_kind": "relish_bug"}},
]
for event in events:
    print(json.dumps(event), flush=True)
"""

# Runs the -c command and keeps its output as a one-event asciicast v3,
# which is all a recording needs from asciinema here.
FAKE_ASCIINEMA = r"""
import json, subprocess, sys
argv = sys.argv[1:]
command, cast = argv[argv.index("-c") + 1], argv[-1]
title = argv[argv.index("--title") + 1]
done = subprocess.run(command, shell=True, capture_output=True, text=True)
with open(cast, "w") as out:
    out.write(json.dumps({"version": 3, "title": title, "command": command}) + "\n")
    out.write(json.dumps([0.1, "o", done.stdout]) + "\n")
sys.exit(done.returncode)
"""


class Recorded(unittest.TestCase):
    def test_the_session_is_shown_and_its_answer_reaches_the_gym(self):
        import os
        import tempfile
        from unittest import mock
        directory = Path(tempfile.mkdtemp())
        bin_dir = directory / "bin"
        bin_dir.mkdir()
        for name, source in (("claude", FAKE_CLAUDE), ("asciinema", FAKE_ASCIINEMA)):
            script = directory / f"{name}.py"
            script.write_text(source)
            program = bin_dir / name
            program.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{script}" "$@"\n')
            program.chmod(0o755)
        work = directory / "run-1" / "solver"
        work.mkdir(parents=True)
        cast = directory / "debug.cast"
        env = {"PATH": f"{bin_dir}:{os.environ['PATH']}", "GYM_ANSWER": str(directory / "answer.json"),
               "GYM_SOLVER_META": str(directory / "meta.json")}
        solver = recording.RecordingSolver("debug", cast, [], str(bin_dir / "claude"), None, "1",
                                           "0.2.8", "relish")
        with mock.patch.dict(os.environ, {"PATH": env["PATH"]}):
            attempt = solver.solve("Find the bug.", env, work, 60)
        # No report was drafted, so the stage says so and fails.
        self.assertEqual(attempt.exit_code, 1)
        self.assertEqual(json.loads((directory / "answer.json").read_text())["app"], "quiet")
        self.assertEqual(json.loads((directory / "meta.json").read_text())["model"], "fake-model")
        header, event = [json.loads(line) for line in cast.read_text().splitlines()]
        self.assertEqual(header["title"], "Reliaburger 0.2.8: an agent debugs the platform")
        screen = "\n".join(plain(event[2].splitlines()))
        for shown in ("» The prompt", "Find the bug.", "claude ▸ Looking at the cluster first.",
                      "» Look at the cluster", "$ relish agent context",
                      "# The session ended after 2 turns and 4 s.", "# The session drafted no report."):
            self.assertIn(shown, screen)
        self.assertIn("relish agent patch try", (work / "recording-prompt.md").read_text())


if __name__ == "__main__":
    unittest.main()
