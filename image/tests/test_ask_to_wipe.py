"""Tests for the netboot installer's ask-to-wipe, which reports a used disk to
relish netboot and waits for the operator's answer. They run the real script
against a small HTTP server playing relish, with a fake lsblk on PATH:

    python3 -m unittest discover -s image/tests
"""

import os
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SCRIPT = (
    Path(__file__).resolve().parents[1]
    / "mkosi.images/installer/mkosi.extra/usr/lib/reliaburger/ask-to-wipe"
)

LSBLK = (
    'NAME="/dev/vda" TYPE="disk" SIZE="8589934592" PTTYPE="gpt" FSTYPE="" LABEL="" PARTLABEL=""\n'
    'NAME="/dev/vda1" TYPE="part" SIZE="1048576" PTTYPE="gpt" FSTYPE="ext4" LABEL="ThinOS" PARTLABEL=""\n'
)


class Relish(BaseHTTPRequestHandler):
    """POST /disk takes the report and answers a ticket; GET /disk/<ticket>
    answers each of `answers` in turn, then the last one forever."""

    def do_POST(self):
        server = self.server
        length = int(self.headers.get("Content-Length", 0))
        server.reports.append((self.path, self.rfile.read(length).decode()))
        if server.refuse_reports:
            self.send_response(403)
            self.end_headers()
            return
        self.reply("0123456789abcdef\n", 202)

    def do_GET(self):
        server = self.server
        server.polls.append(self.path)
        if self.path != "/disk/0123456789abcdef":
            self.reply("no such ticket\n", 404)
            return
        answer = server.answers[min(len(server.polls), len(server.answers)) - 1]
        self.reply(answer + "\n")

    def reply(self, text, status=200):
        body = text.encode()
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class AskToWipe(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        bin_dir = Path(self.tmp.name) / "bin"
        bin_dir.mkdir()
        lsblk = bin_dir / "lsblk"
        lsblk.write_text('#!/bin/sh\necho "$*" > "$LSBLK_ARGS"\nprintf %s "$LSBLK_OUT"\n')
        lsblk.chmod(0o755)
        self.path = f"{bin_dir}:{os.environ['PATH']}"
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Relish)
        self.server.reports = []
        self.server.polls = []
        self.server.answers = ["wait"]
        self.server.refuse_reports = False
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        port = self.server.server_address[1]
        self.ask = f"http://127.0.0.1:{port}/disk?mac=52:54:00:42:00:21&uuid=4c4c4544-0042"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.tmp.cleanup()

    def run_ask(self, timeout="10"):
        env = dict(
            os.environ,
            PATH=self.path,
            LSBLK_OUT=LSBLK,
            LSBLK_ARGS=str(Path(self.tmp.name) / "lsblk-args"),
            RELIABURGER_ASK_POLL="0.05",
            RELIABURGER_ASK_TIMEOUT=timeout,
        )
        return subprocess.run(
            ["bash", str(SCRIPT), self.ask, "/dev/vda"],
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
        )

    def test_reports_the_disk_and_wipes_after_a_yes(self):
        self.server.answers = ["wait", "wait", "wipe"]
        done = self.run_ask()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(len(self.server.reports), 1)
        path, body = self.server.reports[0]
        self.assertEqual(path, "/disk?mac=52:54:00:42:00:21&uuid=4c4c4544-0042")
        self.assertEqual(body.strip(), LSBLK.strip())
        args = (Path(self.tmp.name) / "lsblk-args").read_text()
        self.assertIn("NAME,TYPE,SIZE,PTTYPE,FSTYPE,LABEL,PARTLABEL", args)
        self.assertIn("/dev/vda", args)
        self.assertGreaterEqual(len(self.server.polls), 3)
        self.assertIn("wipe? [y/N]", done.stdout)

    def test_a_no_leaves_the_disk(self):
        self.server.answers = ["wait", "decline"]
        done = self.run_ask()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("leave /dev/vda alone", done.stdout)

    def test_install_for_a_disk_the_installer_saw_used_is_not_a_yes(self):
        # Only "wipe" lets the installer write over a used disk: an
        # "install" means relish saw a blank report, which isn't this disk's.
        self.server.answers = ["install"]
        done = self.run_ask()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)

    def test_no_answer_within_the_timeout_leaves_the_disk(self):
        self.server.answers = ["wait"]
        done = self.run_ask(timeout="1")
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn("no answer", done.stdout)

    def test_unknown_words_keep_waiting_rather_than_wiping(self):
        self.server.answers = ["yes", "WIPE", "wipe please"]
        done = self.run_ask(timeout="1")
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)

    def test_a_refused_report_leaves_the_disk(self):
        self.server.refuse_reports = True
        done = self.run_ask()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertEqual(self.server.polls, [])

    def test_an_unreachable_server_leaves_the_disk(self):
        self.ask = "http://127.0.0.1:9/disk?mac=52:54:00:42:00:21"
        done = self.run_ask(timeout="1")
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)


if __name__ == "__main__":
    unittest.main()
