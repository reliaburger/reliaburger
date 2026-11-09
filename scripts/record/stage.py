#!/usr/bin/env python3
"""What a website recording shows. recording.py runs this inside `asciinema rec`.

    python3 scripts/record/stage.py debug|act

It narrates the setup in a few comment lines, runs the Claude Code session
recording.py built (print mode, stream-json) and shows each event as it
arrives: the agent's words, every command it runs and the start of each
answer. Nothing is replayed or typed for it. A tool call that takes a while
(a patch build, a throwaway cluster booting) leaves the screen still, which
the recording's idle limit cuts to two seconds, so its result line says how
long it really took.

Then the person's part, typed at a prompt the way the tour types it:

- `debug`: you read the report the agent drafted. Nothing is posted.
- `act`: you show, approve and undo the agent's proposal, with your own
  token, never the agent's.

Each part opens with a bold `» Title` line, which recording.py turns into a
chapter marker. recording.py passes everything through the environment:

  STAGE_ARGV           the session's command line, as a JSON list
  STAGE_PROMPT         the file holding the prompt
  STAGE_STREAM         where to keep the session's stream-json
  STAGE_TIMEOUT        seconds before the session is stopped
  STAGE_PERSON_TOKEN   (act) the person's token; the session never sees it
  STAGE_PERSON_PATH    (act) the person's PATH: the real relish, not the gym's shim
  STAGE_AGENT_NAME     (act) the agent token's name, to find its proposal
  STAGE_PERSON_LOG     (act) where to write what the person's commands answered
"""
import json
import os
from pathlib import Path
import random
import shlex
import subprocess
import sys
import textwrap
import threading
import time

GYM = Path(__file__).resolve().parent.parent / "gym"
sys.path.insert(0, str(GYM))

import cluster as cluster_module  # noqa: E402
import claude_code  # noqa: E402

WIDTH = 110
RESULT_LINES = 8
SLOW_SECONDS = 10
IDLE_LIMIT = 2

BOLD = "\033[1m"
DIM = "\033[2m"
GREEN = "\033[32m"
RESET = "\033[0m"

# A command's first words, flags left out, and the chapter it opens. The
# first matching prefix wins; a chapter is only opened when it changes.
PHASES = {
    "debug": [
        ("agent context", "Look at the cluster"),
        ("agent diagnose", "Diagnose"),
        ("agent explain", "Diagnose"),
        ("agent logs", "Diagnose"),
        ("logs", "Diagnose"),
        ("agent source", "Read the source"),
        ("agent patch init", "Start a fix"),
        ("agent patch apply", "Write the fix"),
        ("agent patch build", "Build it"),
        ("agent patch test", "Test it"),
        ("agent patch try", "Try it"),
        ("agent patch diff", "Try it"),
        ("agent report", "Draft the report"),
    ],
    "act": [
        ("agent context", "Look at the cluster"),
        ("agent diagnose", "Diagnose"),
        ("agent explain", "Diagnose"),
        ("agent logs", "Diagnose"),
        ("agent lease", "Take a lease"),
        ("agent propose", "Propose"),
        ("agent proposals", "Propose"),
    ],
}

INTRO = {
    "debug": [
        "A planted Reliaburger bug: this relish was built from its own source with a past fix",
        "(#1515) taken out. Claude Code gets the symptom, the Reliaburger plugin and a read-only",
        "token. Every line below is the real session as it ran. Idle time is cut to "
        f"{IDLE_LIMIT} s, and each slow step says how long it really took.",
    ],
    "act": [
        "The web app's new version crash-loops. Claude Code has the Reliaburger plugin and an",
        "agent token: it can read, take leases and propose changes, and the cluster refuses it",
        "everything else. Every line below is the real session as it ran. Idle time is cut to "
        f"{IDLE_LIMIT} s.",
    ],
}


def say(text):
    """A comment line, the way a person narrates at a shell prompt."""
    return f"{DIM}# {text}{RESET}"


def heading(title):
    """A part's bold title line; recording.py puts a chapter marker on it."""
    return f"{BOLD}» {title}{RESET}"


def took(seconds):
    """A duration the way the recordings write it: `42 s`, `4 m 05 s`."""
    seconds = int(round(seconds))
    if seconds < 60:
        return f"{seconds} s"
    return f"{seconds // 60} m {seconds % 60:02d} s"


def relish_words(command):
    """The words after `relish` in a shell command, flags left out, or None.

    The command may set variables first or pipe into something; the first
    word naming a `relish` binary starts it.
    """
    try:
        words = shlex.split(command)
    except ValueError:
        words = command.split()
    for index, word in enumerate(words):
        if Path(word).name == "relish":
            rest = []
            for after in words[index + 1:]:
                if after in ("|", "&&", ";", "||"):
                    break
                if not after.startswith("-"):
                    rest.append(after)
            return rest
    return None


def mcp_words(tool_name):
    """An MCP tool's command words: `mcp__…__source_grep` is `agent source grep`."""
    return ["agent", *tool_name.rsplit("__", 1)[-1].split("_")]


def phase_of(mode, words):
    """The chapter a command opens, or None."""
    if not words:
        return None
    for prefix, title in PHASES[mode]:
        wanted = prefix.split()
        if words[: len(wanted)] == wanted:
            return title
    return None


def result_text(content):
    """A tool result's text, whether Claude Code sent a string or blocks."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(
            block.get("text", "") for block in content
            if isinstance(block, dict) and block.get("type") == "text"
        )
    return ""


def readable(text):
    """A result's text as lines: one-line JSON spread out, so it can be read."""
    stripped = text.strip()
    if stripped.startswith("{"):
        try:
            return json.dumps(json.loads(stripped), indent=2, ensure_ascii=False).splitlines()
        except json.JSONDecodeError:
            pass
    return text.splitlines()


def cut(line, width=WIDTH - 2):
    return line if len(line) <= width else line[: width - 1] + "…"


class Renderer:
    """Turns stream-json events into the lines the recording shows."""

    def __init__(self, mode, clock=time.monotonic):
        self.mode = mode
        self.clock = clock
        self.phase = None
        self.started = {}

    def _open(self, title):
        if title is None or title == self.phase:
            return []
        self.phase = title
        return ["", heading(title)]

    def _tool_use(self, block):
        self.started[block.get("id")] = self.clock()
        name = block.get("name", "")
        arguments = block.get("input") or {}
        if name == "Bash":
            command = arguments.get("command", "")
            lines = self._open(phase_of(self.mode, relish_words(command)))
            wrapped = textwrap.wrap(command, WIDTH - 2, break_on_hyphens=False) or [""]
            lines.append(f"{GREEN}{BOLD}${RESET} {wrapped[0]}")
            lines.extend(f"  {line}" for line in wrapped[1:])
            return lines
        if name.startswith("mcp__"):
            words = mcp_words(name)
            lines = self._open(phase_of(self.mode, words))
            shown = json.dumps(arguments, ensure_ascii=False, separators=(", ", ": "))
            lines.append(cut(f"{GREEN}{BOLD}mcp{RESET} {' '.join(words[1:])} {shown}", WIDTH + 12))
            return lines
        if name in ("Edit", "Write"):
            path = arguments.get("file_path", "")
            lines = self._open(phase_of(self.mode, ["agent", "patch", "apply"]))
            lines.append(f"{GREEN}{BOLD}✎{RESET} {name.lower()} {path}")
            return lines
        return [f"{DIM}({name}){RESET}"]

    def _tool_result(self, block):
        started = self.started.pop(block.get("tool_use_id"), None)
        text = result_text(block.get("content"))
        lines = readable(text)
        shown = [f"{DIM}{cut(line)}{RESET}" for line in lines[:RESULT_LINES]]
        if block.get("is_error"):
            shown.insert(0, f"{DIM}(refused or failed){RESET}")
        if len(lines) > RESULT_LINES:
            shown.append(f"{DIM}… {len(lines) - RESULT_LINES} more lines{RESET}")
        if started is not None and self.clock() - started >= SLOW_SECONDS:
            shown.append(say(f"that took {took(self.clock() - started)}; the recording cut the wait"))
        return shown

    def lines(self, event):
        """The lines one event adds to the screen."""
        kind = event.get("type")
        if kind == "system" and event.get("subtype") == "init":
            return [say(f"Claude Code, model {event.get('model')}")]
        if kind == "assistant":
            out = []
            for block in (event.get("message") or {}).get("content", []):
                if not isinstance(block, dict):
                    continue
                if block.get("type") == "text" and block.get("text", "").strip():
                    for paragraph in block["text"].strip().splitlines():
                        wrapped = textwrap.wrap(paragraph, WIDTH - 9) or [""]
                        out.append(f"{BOLD}claude ▸{RESET} {wrapped[0]}")
                        out.extend(f"         {line}" for line in wrapped[1:])
                elif block.get("type") == "tool_use":
                    out.extend(self._tool_use(block))
            return out
        if kind == "user":
            out = []
            for block in (event.get("message") or {}).get("content", []):
                if isinstance(block, dict) and block.get("type") == "tool_result":
                    out.extend(self._tool_result(block))
            return out
        if kind == "result":
            seconds = (event.get("duration_ms") or 0) / 1000
            return ["", say(f"The session ended after {event.get('num_turns')} turns and {took(seconds)}.")]
        return []


def emit(lines):
    for line in lines:
        print(line, flush=True)


def type_command(text):
    """Type a command at a prompt, a character at a time, as the tour does."""
    sys.stdout.write(f"{GREEN}{BOLD}${RESET} ")
    sys.stdout.flush()
    time.sleep(0.3)
    for character in text:
        sys.stdout.write(character)
        sys.stdout.flush()
        time.sleep(random.choice((0.01, 0.02, 0.03)))
    time.sleep(0.3)
    print(flush=True)


def show(command, env, cwd):
    """Type a command, then run exactly what was typed; its exit code."""
    type_command(command)
    return subprocess.run(command, shell=True, env=env, cwd=cwd, check=False).returncode


def run_session(mode, argv, prompt, env, cwd, stream_path, timeout):
    """Run the session, showing each event as it arrives; its exit code."""
    renderer = Renderer(mode)
    session = subprocess.Popen(
        argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        text=True, env=env, cwd=cwd,
    )
    timer = threading.Timer(timeout, session.kill)
    timer.start()
    try:
        session.stdin.write(prompt)
        session.stdin.close()
        with open(stream_path, "w", encoding="utf-8") as stream:
            for line in session.stdout:
                stream.write(line)
                stream.flush()
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(event, dict):
                    emit(renderer.lines(event))
        return session.wait()
    finally:
        timer.cancel()


def report_dir(root):
    """The newest directory under `root` holding a drafted report, or None."""
    found = [path.parent for path in Path(root).rglob("MANIFEST.json")
             if (path.parent / "issue.md").is_file()]
    return max(found, key=lambda path: path.stat().st_mtime) if found else None


def review_report(cwd):
    """The person reads the draft. Returns 0 when there was one."""
    draft = report_dir(cwd)
    if draft is None:
        emit(["", say("The session drafted no report.")])
        return 1
    where = shlex.quote(os.path.relpath(draft, cwd))
    emit(["", heading("You read the draft"),
          say("The report is files on this machine. Nothing has been posted.")])
    env = dict(os.environ)
    show(f"ls {where}", env, cwd)
    show(f"head -n 30 {where}/issue.md", env, cwd)
    show(f"head -n 24 {where}/MANIFEST.json", env, cwd)
    emit([say("Posting it is your call: issue.md says what to check first, and the report's answer"),
          say("carries the `gh issue create` command for you to run.")])
    return 0


def proposal_state(proposal, env, cwd):
    shown = subprocess.run(["relish", "proposal", "show", str(proposal), "-o", "json"],
                           capture_output=True, text=True, env=env, cwd=cwd, check=False)
    try:
        return json.loads(shown.stdout).get("state")
    except (json.JSONDecodeError, AttributeError):
        return None


def person_env(environ, person_token):
    """The person's environment: their token, the real `relish`, and none of the gym's files.

    The session's PATH starts with the gym's recording shim, which logs
    every call as the solver's. The person's commands aren't the solver's,
    so they run the `relish` in STAGE_PERSON_PATH and write no call log.
    """
    env = {
        key: value for key, value in environ.items()
        if not key.startswith("STAGE_") and not key.startswith("GYM_")
    }
    env["PATH"] = environ["STAGE_PERSON_PATH"]
    env["RELIABURGER_TOKEN"] = person_token
    return env


def decide(person_token, agent_name, log_path, cwd):
    """The person shows, approves and undoes the proposal. Returns 0 when it all ran."""
    env = person_env(os.environ, person_token)
    listed = subprocess.run(["relish", "proposal", "list", "-o", "json"],
                            capture_output=True, text=True, env=env, cwd=cwd, check=False)
    try:
        proposal = cluster_module.newest_proposal(json.loads(listed.stdout), agent_name)
    except json.JSONDecodeError:
        proposal = None
    record = {"proposal": proposal}
    if proposal is None:
        emit(["", say("The session made no proposal.")])
        Path(log_path).write_text(json.dumps(record, indent=2) + "\n")
        return 1
    emit(["", heading("You review it"),
          say("You now, with your own token. The agent's can't approve anything.")])
    show("relish proposal list", env, cwd)
    show(f"relish proposal show {proposal}", env, cwd)
    emit(["", heading("You approve it"),
          say("--yes answers the question approve asks, which a recording can't type.")])
    record["approve_exit"] = show(f"relish proposal approve {proposal} --yes", env, cwd)
    record["after_approve"] = proposal_state(proposal, env, cwd)
    show("relish apps web", env, cwd)
    emit(["", heading("Undo"), say("Changed your mind? The undo was in the proposal all along.")])
    record["undo_exit"] = show(f"relish undo {proposal} --yes", env, cwd)
    record["after_undo"] = proposal_state(proposal, env, cwd)
    show(f"relish proposal show {proposal}", env, cwd)
    Path(log_path).write_text(json.dumps(record, indent=2) + "\n")
    return 0


def main(argv):
    mode = argv[0]
    if mode not in PHASES:
        raise SystemExit(f"usage: stage.py {'|'.join(PHASES)}")
    cwd = os.getcwd()
    prompt = Path(os.environ["STAGE_PROMPT"]).read_text()
    session_argv = json.loads(os.environ["STAGE_ARGV"])
    timeout = int(os.environ.get("STAGE_TIMEOUT", "5400"))
    session_env = claude_code.session_env(
        {key: value for key, value in os.environ.items() if not key.startswith("STAGE_")}
    )
    emit([say(line) for line in INTRO[mode]])
    emit(["", heading("The prompt")])
    emit(f"{DIM}{cut(line)}{RESET}" for line in prompt.strip().splitlines()[:14])
    status = run_session(mode, session_argv, prompt, session_env, cwd,
                         os.environ["STAGE_STREAM"], timeout)
    if status != 0:
        emit([say(f"Claude Code exited with {status}.")])
    if mode == "debug":
        return review_report(cwd) or status
    return decide(os.environ["STAGE_PERSON_TOKEN"], os.environ["STAGE_AGENT_NAME"],
                  os.environ["STAGE_PERSON_LOG"], cwd) or status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
