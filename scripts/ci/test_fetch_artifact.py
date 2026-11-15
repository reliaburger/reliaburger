"""The test archive download resumes a slow or stalled transfer instead of restarting it (#977)."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import hashlib
import io
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
import zipfile

import fetch_artifact

TOKEN = "ghs_test-token"
REPOSITORY = "owner/repo"
RUN_ID = "41"


def zipped(members):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_STORED) as archive:
        for name, data in members.items():
            archive.writestr(name, data)
    return buffer.getvalue()


class Storage:
    """What the fake API and blob store serve, and what they saw."""

    def __init__(self, blob):
        self.blob = blob
        self.digest = "sha256:" + hashlib.sha256(blob).hexdigest()
        # Per blob request, in order: None serves normally, ("drop", n) sends
        # n bytes then closes, ("stall", n) sends n bytes then goes silent,
        # "ignore-range" answers 200 with the whole blob.
        self.faults = []
        self.blob_requests = []
        self.artifacts = None

    def listing(self, base):
        if self.artifacts is not None:
            return self.artifacts
        return [dict(id=7, name="nextest-archive", size_in_bytes=len(self.blob), expired=False,
                     created_at="2026-10-27T10:00:00Z", digest=self.digest,
                     archive_download_url=f"{base}/repos/{REPOSITORY}/actions/artifacts/7/zip")]


def handler_for(storage):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def base(self):
            return f"http://127.0.0.1:{self.server.server_port}"

        def do_GET(self):
            if self.path.startswith(f"/repos/{REPOSITORY}/actions/runs/{RUN_ID}/artifacts"):
                if self.headers.get("Authorization") != f"Bearer {TOKEN}":
                    self.send_error(401)
                    return
                body = json.dumps(dict(artifacts=storage.listing(self.base()))).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            elif self.path.endswith("/zip"):
                if self.headers.get("Authorization") != f"Bearer {TOKEN}":
                    self.send_error(401)
                    return
                self.send_response(302)
                self.send_header("Location", f"{self.base()}/blob?sig=signed")
                self.send_header("Content-Length", "0")
                self.end_headers()
            elif self.path.startswith("/blob"):
                self.serve_blob()
            else:
                self.send_error(404)

        def serve_blob(self):
            fault = storage.faults.pop(0) if storage.faults else None
            storage.blob_requests.append(dict(range=self.headers.get("Range"),
                                              authorization=self.headers.get("Authorization")))
            start = 0
            requested = self.headers.get("Range")
            if requested and fault != "ignore-range":
                start = int(requested.removeprefix("bytes=").rstrip("-"))
                self.send_response(206)
                self.send_header("Content-Range", f"bytes {start}-{len(storage.blob) - 1}/{len(storage.blob)}")
            else:
                self.send_response(200)
            body = storage.blob[start:]
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if isinstance(fault, tuple):
                kind, count = fault
                self.wfile.write(body[:count])
                self.wfile.flush()
                if kind == "stall":
                    time.sleep(1.5)
                self.close_connection = True
                return
            self.wfile.write(body)

    return Handler


class FetchTests(unittest.TestCase):
    MEMBERS = {"tests.tar.zst": b"x" * 300_000, "archive-origin.json": b"{}\n",
               "archive-runtime.json": b"{\"runtime\": 1}\n"}

    def setUp(self):
        self.storage = Storage(zipped(self.MEMBERS))
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(self.storage))
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, kwargs=dict(poll_interval=0.05),
                         daemon=True).start()
        self.api = f"http://127.0.0.1:{self.server.server_port}"
        self.directory = tempfile.TemporaryDirectory()
        self.destination = Path(self.directory.name)
        self.output = io.StringIO()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.directory.cleanup()

    def fetch(self, deadline=30.0, stall=0.5):
        return fetch_artifact.fetch(self.api, REPOSITORY, RUN_ID, "nextest-archive", TOKEN,
                                    self.destination, deadline=deadline, stall=stall,
                                    pause=lambda attempt: 0.0, output=self.output)

    def assert_extracted(self):
        for name, data in self.MEMBERS.items():
            self.assertEqual((self.destination / name).read_bytes(), data)
        self.assertEqual(sorted(path.name for path in self.destination.iterdir()), sorted(self.MEMBERS))

    def test_fetches_and_extracts_the_named_artefact_of_this_run(self):
        self.fetch()
        self.assert_extracted()
        self.assertEqual(len(self.storage.blob_requests), 1)
        self.assertIsNone(self.storage.blob_requests[0]["range"])

    def test_a_dropped_transfer_resumes_from_the_bytes_already_fetched(self):
        self.storage.faults = [("drop", 100_000)]
        self.fetch()
        self.assert_extracted()
        self.assertEqual([request["range"] for request in self.storage.blob_requests],
                         [None, "bytes=100000-"])

    def test_a_stalled_transfer_is_abandoned_and_resumed_rather_than_waited_out(self):
        self.storage.faults = [("stall", 50_000), ("stall", 120_000)]
        started = time.monotonic()
        self.fetch(stall=0.3)
        self.assert_extracted()
        self.assertEqual([request["range"] for request in self.storage.blob_requests],
                         [None, "bytes=50000-", "bytes=170000-"])
        self.assertLess(time.monotonic() - started, 5)
        self.assertIn("stalled", self.output.getvalue())

    def test_the_token_never_reaches_the_storage_host(self):
        self.storage.faults = [("drop", 10_000)]
        self.fetch()
        self.assertEqual([request["authorization"] for request in self.storage.blob_requests], [None, None])

    def test_a_store_that_ignores_the_range_starts_the_file_again(self):
        self.storage.faults = [("drop", 100_000), "ignore-range"]
        self.fetch()
        self.assert_extracted()

    def test_progress_is_logged(self):
        self.fetch()
        self.assertRegex(self.output.getvalue(), r"fetched [0-9.]+ of [0-9.]+ MB")

    def test_gives_up_at_the_deadline(self):
        self.storage.faults = [("drop", 0)] * 100_000
        with self.assertRaises(fetch_artifact.FetchError) as raised:
            self.fetch(deadline=1.0)
        self.assertIn("deadline", str(raised.exception))
        self.assertFalse((self.destination / "tests.tar.zst").exists())

    def test_a_digest_mismatch_is_refused(self):
        self.storage.artifacts = self.storage.listing(self.api)
        self.storage.artifacts[0]["digest"] = "sha256:" + "0" * 64
        with self.assertRaises(fetch_artifact.FetchError) as raised:
            self.fetch()
        self.assertIn("digest", str(raised.exception))
        self.assertFalse((self.destination / "tests.tar.zst").exists())

    def test_a_missing_artefact_is_an_error(self):
        self.storage.artifacts = []
        with self.assertRaises(fetch_artifact.FetchError):
            self.fetch()

    def test_an_expired_artefact_is_not_chosen(self):
        listing = self.storage.listing(self.api)
        listing[0]["expired"] = True
        self.storage.artifacts = listing
        with self.assertRaises(fetch_artifact.FetchError):
            self.fetch()

    def test_the_newest_artefact_of_that_name_is_chosen(self):
        current = self.storage.listing(self.api)[0]
        older = dict(current, id=3, created_at="2026-10-27T09:00:00Z", digest="sha256:" + "1" * 64,
                     archive_download_url=current["archive_download_url"].replace("/7/", "/3/"))
        other = dict(current, id=9, name="coverage", created_at="2026-10-27T11:00:00Z")
        self.storage.artifacts = [older, current, other]
        self.fetch()
        self.assert_extracted()

    def test_a_member_outside_the_destination_is_refused(self):
        for name in ("../escape", "/absolute", "nested/file"):
            with self.subTest(name=name):
                self.storage.blob = zipped({name: b"payload"})
                self.storage.digest = "sha256:" + hashlib.sha256(self.storage.blob).hexdigest()
                with self.assertRaises(fetch_artifact.FetchError):
                    self.fetch()
                self.assertFalse((self.destination.parent / "escape").exists())


class CommandLineTests(unittest.TestCase):
    def test_the_command_fails_without_a_token(self):
        with tempfile.TemporaryDirectory() as directory:
            status = fetch_artifact.main(["--repository", REPOSITORY, "--run-id", RUN_ID,
                                          "--name", "nextest-archive", "--destination", directory],
                                         environment={})
        self.assertEqual(status, 1)


if __name__ == "__main__":
    unittest.main()
