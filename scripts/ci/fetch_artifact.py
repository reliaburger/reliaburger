#!/usr/bin/env python3
"""Download one of this run's artefacts, resuming a slow or stalled transfer.

The cluster and acceptance jobs used to fetch the build job's 3 GB nextest
archive with `actions/download-artifact`, 5 minutes per attempt and one retry
(#977). In passing runs that download took 33 s to 287 s, so the archive sat
right at the edge of the limit; on a slow runner both attempts ran out, and
the retry started again from the first byte. The action logs nothing while it
transfers, so a slow transfer and a hung one looked the same.

This script fetches the artefact's zip itself:

- it finds the newest unexpired artefact called NAME in this run through the
  REST API (`actions: read`);
- each attempt asks for a fresh signed storage URL and sends `Range` for the
  bytes it already has, so a dropped or stalled connection resumes instead of
  starting over; a store that ignores the range gets a fresh file;
- an attempt is abandoned when no byte arrives for `--stall` seconds, not after
  a fixed wall-clock limit, so a slow but steady transfer finishes;
- it logs its progress, so the next slow run says how slow;
- it stops at `--deadline` seconds, checks the zip against the digest the
  service recorded, and extracts its files (flat names only) into
  `--destination`.

The token goes to the API only. The redirect to the storage host carries its
own signature, and urllib leaves an unredirected header behind.

    fetch_artifact.py --repository OWNER/REPO --run-id ID --name NAME \\
                      --destination DIR [--deadline 780] [--stall 60]

The token comes from GH_TOKEN. Fixtures: scripts/ci/test_fetch_artifact.py.
"""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

CHUNK = 1 << 20
# Log at least this often, and at every so many bytes, while transferring.
PROGRESS_SECONDS = 15
PROGRESS_BYTES = 256 << 20


class FetchError(Exception):
    """The artefact could not be fetched or extracted."""


def api_request(url, token):
    request = urllib.request.Request(url)
    # Unredirected: the token stays with the API and never reaches storage.
    request.add_unredirected_header("Authorization", f"Bearer {token}")
    request.add_unredirected_header("Accept", "application/vnd.github+json")
    request.add_unredirected_header("X-GitHub-Api-Version", "2022-11-28")
    return request


def find_artifact(api, repository, run_id, name, token, timeout):
    """The newest unexpired artefact of this run with exactly this name."""
    query = urllib.parse.urlencode(dict(name=name, per_page=100))
    url = f"{api}/repos/{repository}/actions/runs/{run_id}/artifacts?{query}"
    try:
        with urllib.request.urlopen(api_request(url, token), timeout=timeout) as response:
            listing = json.load(response)
    except (OSError, ValueError) as error:
        raise FetchError(f"could not list the artefacts of run {run_id}: {error}") from error
    found = [artifact for artifact in listing.get("artifacts", [])
             if artifact.get("name") == name and not artifact.get("expired")]
    if not found:
        raise FetchError(f"run {run_id} has no unexpired artefact called {name}")
    # A re-run whose build job ran again uploads another one; take the newest.
    return max(found, key=lambda artifact: (artifact["created_at"], artifact["id"]))


def megabytes(count):
    return f"{count / 1e6:.1f}"


class Transfer:
    """One artefact's zip, fetched into `path` over as many attempts as it takes."""

    def __init__(self, url, token, path, size, stall, output):
        self.url, self.token, self.path, self.size = url, token, path, size
        self.stall, self.output = stall, output

    def have(self):
        return self.path.stat().st_size if self.path.exists() else 0

    def attempt(self):
        """Fetch what's missing; raise when the connection fails, stalls or closes early."""
        offset = self.have()
        request = api_request(self.url, self.token)
        if offset:
            # A plain header: it follows the redirect to the storage host.
            request.add_header("Range", f"bytes={offset}-")
        # The timeout bounds each read: no byte for `stall` seconds raises.
        with urllib.request.urlopen(request, timeout=self.stall) as response:
            if offset and response.status != 206:
                print(f"the store ignored the range; starting {self.path.name} again", file=self.output)
                offset = 0
            started, logged, logged_bytes = time.monotonic(), time.monotonic(), offset
            with self.path.open("r+b" if offset else "wb") as file:
                file.seek(offset)
                file.truncate()
                fetched = offset
                while True:
                    # read1 returns what has arrived; read would wait for a
                    # whole chunk and lose it to a stall.
                    chunk = response.read1(CHUNK)
                    if not chunk:
                        break
                    file.write(chunk)
                    fetched += len(chunk)
                    now = time.monotonic()
                    if now - logged >= PROGRESS_SECONDS or fetched - logged_bytes >= PROGRESS_BYTES:
                        self.progress(fetched, offset, now - started)
                        logged, logged_bytes = now, fetched
            self.progress(fetched, offset, time.monotonic() - started)
        if fetched < self.size:
            raise OSError(f"connection closed at {megabytes(fetched)} MB")

    def progress(self, fetched, offset, seconds):
        rate = (fetched - offset) / max(seconds, 1e-3) / 1e6
        print(f"fetched {megabytes(fetched)} of {megabytes(self.size)} MB ({rate:.1f} MB/s)",
              file=self.output, flush=True)


def digest_of(path):
    hasher = hashlib.sha256()
    with path.open("rb") as file:
        while chunk := file.read(CHUNK):
            hasher.update(chunk)
    return "sha256:" + hasher.hexdigest()


def extract(path, destination):
    """Extract a flat zip; refuse any member that names a directory."""
    try:
        with zipfile.ZipFile(path) as archive:
            members = archive.infolist()
            for member in members:
                name = member.filename
                if not name or "/" in name or "\\" in name or name in (".", ".."):
                    raise FetchError(f"refusing artefact member {name!r}: not a plain file name")
            for member in members:
                archive.extract(member, destination)
    except zipfile.BadZipFile as error:
        raise FetchError(f"{path.name} is not a zip: {error}") from error


def fetch(api, repository, run_id, name, token, destination, deadline, stall,
          pause=lambda attempt: min(2 ** attempt, 15), output=sys.stdout):
    """Fetch and extract artefact NAME of this run into `destination`.

    Raises FetchError when the artefact is missing, the deadline passes, the
    digest differs or the zip holds anything but plain files.
    """
    ends = time.monotonic() + deadline
    destination = Path(destination)
    artifact = find_artifact(api, repository, run_id, name, token, stall)
    size = artifact["size_in_bytes"]
    print(f"artefact {artifact['id']} ({name}): {megabytes(size)} MB", file=output, flush=True)
    path = destination / f".{name}.zip.part"
    transfer = Transfer(artifact["archive_download_url"], token, path, size, stall, output)
    try:
        attempt = 0
        while transfer.have() < size:
            attempt += 1
            if time.monotonic() >= ends:
                raise FetchError(f"deadline of {deadline:.0f} s passed with "
                                 f"{megabytes(transfer.have())} of {megabytes(size)} MB fetched")
            try:
                transfer.attempt()
            except urllib.error.HTTPError as error:
                if error.code == 416:
                    # Our partial file is no prefix of this blob; start over.
                    path.unlink(missing_ok=True)
                print(f"attempt {attempt} failed: HTTP {error.code}", file=output, flush=True)
                time.sleep(pause(attempt))
            except (OSError, http.client.HTTPException) as error:
                kind = "stalled" if isinstance(error, TimeoutError) or "timed out" in str(error) else "failed"
                print(f"attempt {attempt} {kind} at {megabytes(transfer.have())} MB: {error}; resuming",
                      file=output, flush=True)
                time.sleep(pause(attempt))
        if transfer.have() != size:
            raise FetchError(f"fetched {transfer.have()} bytes, expected {size}")
        expected = artifact.get("digest")
        if expected and digest_of(path) != expected:
            raise FetchError(f"artefact digest differs from the recorded {expected}")
        extract(path, destination)
    finally:
        path.unlink(missing_ok=True)
    print(f"extracted {name} into {destination}", file=output, flush=True)


def main(argv=None, environment=None):
    environment = os.environ if environment is None else environment
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repository", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--name", required=True)
    parser.add_argument("--destination", type=Path, required=True)
    parser.add_argument("--deadline", type=float, default=780.0,
                        help="give up after this many seconds")
    parser.add_argument("--stall", type=float, default=60.0,
                        help="abandon an attempt after this many seconds without a byte")
    arguments = parser.parse_args(argv)
    token = environment.get("GH_TOKEN")
    if not token:
        print("GH_TOKEN is not set", file=sys.stderr)
        return 1
    api = environment.get("GITHUB_API_URL", "https://api.github.com")
    try:
        fetch(api, arguments.repository, arguments.run_id, arguments.name, token,
              arguments.destination, arguments.deadline, arguments.stall)
    except FetchError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
