#!/usr/bin/env python3
"""Local, digest-pinned OCI mirror for the gated Linux suites.

The privileged suites run real public images. Pulling them from the internet
in the middle of a timed test lets a slow CDN fail the build, so this script
fetches every image in tests/fixtures/pinned-images.txt once, with retries,
into a content-addressed cache, and serves that cache as a read-only OCI
registry on loopback. Tests point Bun at it through `[images] mirrors`
(see src/testkit/pinned_images.rs).

    mirror.py warm  [--cache DIR] [--arch ARCH]...
    mirror.py serve [--cache DIR] [--listen HOST:PORT]
    mirror.py run   [--cache DIR] [--listen HOST:PORT] [--arch ARCH]... -- COMMAND...

`run` warms, serves in the background, runs COMMAND with
RELIABURGER_TEST_IMAGE_MIRROR set to the listen address, and exits with
COMMAND's status. Every manifest and blob is checked against its digest when
fetched and again when served from disk; Bun verifies the chain once more, so
the mirror is never trusted.

Standard library only: it runs on a fresh CI runner or VM before any build.
"""

import argparse
import hashlib
import http.server
import json
import os
import platform
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DEFAULT_LIST = os.path.join(ROOT, "tests", "fixtures", "pinned-images.txt")
DEFAULT_CACHE = os.path.join(os.path.expanduser("~"), ".cache", "reliaburger", "test-images")
DEFAULT_LISTEN = "127.0.0.1:5099"
MIRROR_ENV = "RELIABURGER_TEST_IMAGE_MIRROR"

MANIFEST_TYPES = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.docker.distribution.manifest.v2+json",
]
INDEX_TYPES = MANIFEST_TYPES[0], MANIFEST_TYPES[2]
DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")
ATTEMPTS = 6
# A socket read that makes no progress for this long abandons the attempt.
READ_TIMEOUT = 60


def log(message):
    print(f"test-images: {message}", file=sys.stderr, flush=True)


def host_architecture():
    machine = platform.machine().lower()
    return {"x86_64": "amd64", "amd64": "amd64", "aarch64": "arm64", "arm64": "arm64"}[machine]


def parse_reference(reference):
    name, digest = reference.split("@", 1)
    host, repository = name.split("/", 1)
    if not DIGEST.match(digest):
        raise ValueError(f"{reference} is not pinned by a sha256 digest")
    return host, repository, digest


def read_list(path):
    with open(path) as handle:
        lines = [line.strip() for line in handle]
    return [line for line in lines if line and not line.startswith("#")]


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


OPENER = urllib.request.build_opener(NoRedirect)


class Upstream:
    """Anonymous pulls from one public registry, with bearer-token exchange."""

    def __init__(self):
        self.tokens = {}

    def token(self, host, repository, challenge):
        key = (host, repository)
        if key in self.tokens:
            return self.tokens[key]
        fields = dict(re.findall(r'(\w+)="([^"]*)"', challenge))
        query = {"scope": f"repository:{repository}:pull"}
        if "service" in fields:
            query["service"] = fields["service"]
        url = fields["realm"] + "?" + urllib.parse.urlencode(query)
        with OPENER.open(url, timeout=READ_TIMEOUT) as response:
            body = json.load(response)
        self.tokens[key] = body.get("token") or body.get("access_token")
        return self.tokens[key]

    def open(self, host, repository, path, accept=None):
        url = f"https://{host}/v2/{repository}/{path}"
        headers = {"Accept": ", ".join(accept)} if accept else {}
        for _ in range(3):
            request = urllib.request.Request(url, headers=headers)
            try:
                return OPENER.open(request, timeout=READ_TIMEOUT)
            except urllib.error.HTTPError as error:
                if error.code == 401 and "Authorization" not in headers:
                    challenge = error.headers.get("WWW-Authenticate", "")
                    headers["Authorization"] = "Bearer " + self.token(host, repository, challenge)
                    continue
                if error.code in (301, 302, 303, 307, 308):
                    # Blob CDNs reject the registry's bearer token: follow
                    # the redirect without it.
                    url = urllib.parse.urljoin(url, error.headers["Location"])
                    headers = {}
                    continue
                raise
        raise RuntimeError(f"too many redirects or challenges for {url}")


def with_retries(what, action):
    delay = 2
    for attempt in range(1, ATTEMPTS + 1):
        try:
            return action()
        except urllib.error.HTTPError as error:
            if error.code not in (408, 429, 500, 502, 503, 504) or attempt == ATTEMPTS:
                raise RuntimeError(f"{what}: HTTP {error.code}") from error
            reason = f"HTTP {error.code}"
        except (OSError, urllib.error.URLError, ValueError) as error:
            if attempt == ATTEMPTS:
                raise RuntimeError(f"{what}: {error}") from error
            reason = str(error)
        log(f"{what}: attempt {attempt} failed ({reason}); retrying in {delay}s")
        time.sleep(delay)
        delay *= 2


class Cache:
    """Content-addressed files: blobs/sha256/<hex>, plus <hex>.type for manifests."""

    def __init__(self, root):
        self.root = root
        os.makedirs(os.path.join(root, "blobs", "sha256"), exist_ok=True)

    def path(self, digest):
        return os.path.join(self.root, "blobs", "sha256", digest.split(":", 1)[1])

    def verified(self, digest, size=None):
        path = self.path(digest)
        if not os.path.isfile(path) or (size is not None and os.path.getsize(path) != size):
            return False
        return file_digest(path) == digest

    def media_type(self, digest):
        try:
            with open(self.path(digest) + ".type") as handle:
                return handle.read().strip()
        except FileNotFoundError:
            return None

    def store(self, digest, size, stream, media_type=None):
        """Stream into a temporary file, check digest and size, then publish."""
        path = self.path(digest)
        temporary = f"{path}.partial.{os.getpid()}"
        hasher = hashlib.sha256()
        written = 0
        try:
            with open(temporary, "wb") as handle:
                while chunk := stream.read(1 << 20):
                    hasher.update(chunk)
                    handle.write(chunk)
                    written += len(chunk)
            actual = "sha256:" + hasher.hexdigest()
            if actual != digest or (size is not None and written != size):
                raise ValueError(f"{digest}: received {actual} ({written} bytes)")
            if media_type:
                with open(path + ".type", "w") as handle:
                    handle.write(media_type)
            os.replace(temporary, path)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)


def file_digest(path):
    hasher = hashlib.sha256()
    with open(path, "rb") as handle:
        while chunk := handle.read(1 << 20):
            hasher.update(chunk)
    return "sha256:" + hasher.hexdigest()


def fetch(upstream, cache, host, repository, digest, size=None, manifest=False):
    if cache.verified(digest, size):
        return
    kind = "manifests" if manifest else "blobs"

    def attempt():
        with upstream.open(host, repository, f"{kind}/{digest}", MANIFEST_TYPES if manifest else None) as response:
            media_type = response.headers.get("Content-Type", "").split(";")[0].strip()
            cache.store(digest, size, response, media_type if manifest else None)

    with_retries(f"{host}/{repository} {kind} {digest[:19]}", attempt)


def warm_image(upstream, cache, reference, architectures):
    host, repository, digest = parse_reference(reference)
    fetch(upstream, cache, host, repository, digest, manifest=True)
    with open(cache.path(digest), "rb") as handle:
        root = json.load(handle)
    manifests = [(digest, root)]
    if root.get("mediaType") in INDEX_TYPES or "manifests" in root:
        manifests = []
        for architecture in architectures:
            child = next(
                (
                    entry
                    for entry in root["manifests"]
                    if entry.get("platform", {}).get("os") == "linux"
                    and entry.get("platform", {}).get("architecture") == architecture
                ),
                None,
            )
            if child is None:
                raise RuntimeError(f"{reference} has no linux/{architecture} manifest")
            fetch(upstream, cache, host, repository, child["digest"], child.get("size"), manifest=True)
            with open(cache.path(child["digest"]), "rb") as handle:
                manifests.append((child["digest"], json.load(handle)))
    for _, image in manifests:
        for blob in [image["config"], *image["layers"]]:
            fetch(upstream, cache, host, repository, blob["digest"], blob.get("size"))
    log(f"warm: {reference} ({', '.join(architectures)})")


def warm(cache_root, list_path, architectures):
    cache = Cache(cache_root)
    upstream = Upstream()
    for reference in read_list(list_path):
        warm_image(upstream, cache, reference, architectures)


def handler_for(cache):
    route = re.compile(r"^/v2/(?P<repository>.+)/(?P<kind>manifests|blobs)/(?P<digest>[^/]+)$")

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, format, *args):
            log("serve: " + format % args)

        def reply(self, status, body, headers):
            self.send_response(status)
            for name, value in headers.items():
                self.send_header(name, value)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)

        def not_found(self, code):
            error = {"errors": [{"code": code, "message": "not in the local test mirror"}]}
            self.reply(404, json.dumps(error).encode(), {"Content-Type": "application/json"})

        def do_GET(self):
            path = urllib.parse.urlparse(self.path).path
            if path in ("/v2", "/v2/"):
                return self.reply(200, b"{}", {"Content-Type": "application/json"})
            match = route.match(path)
            manifest = bool(match) and match["kind"] == "manifests"
            unknown = "MANIFEST_UNKNOWN" if manifest else "BLOB_UNKNOWN"
            if not match or not DIGEST.match(match["digest"]):
                return self.not_found(unknown)
            digest = match["digest"]
            if not cache.verified(digest):
                return self.not_found(unknown)
            media_type = cache.media_type(digest) if manifest else None
            if manifest and not media_type:
                return self.not_found(unknown)
            with open(cache.path(digest), "rb") as handle:
                body = handle.read()
            self.reply(
                200,
                body,
                {
                    "Content-Type": media_type or "application/octet-stream",
                    "Docker-Content-Digest": digest,
                },
            )

        do_HEAD = do_GET

    return Handler


class Server(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def handle_error(self, request, client_address):
        # Clients drop idle keep-alive connections; that isn't a server fault.
        if not isinstance(sys.exc_info()[1], ConnectionError):
            super().handle_error(request, client_address)


def start_server(cache_root, listen):
    host, port = listen.rsplit(":", 1)
    server = Server((host, int(port)), handler_for(Cache(cache_root)))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("action", choices=["warm", "serve", "run"])
    parser.add_argument("--cache", default=os.environ.get("TEST_IMAGE_CACHE") or DEFAULT_CACHE)
    parser.add_argument("--list", default=DEFAULT_LIST)
    parser.add_argument("--listen", default=DEFAULT_LISTEN)
    parser.add_argument("--arch", action="append", help="linux architecture to warm (default: this host's)")
    arguments = sys.argv[1:]
    command = []
    if "--" in arguments:
        split = arguments.index("--")
        arguments, command = arguments[:split], arguments[split + 1 :]
    options = parser.parse_args(arguments)
    architectures = options.arch or [host_architecture()]

    if options.action in ("warm", "run"):
        warm(options.cache, options.list, architectures)
    if options.action == "warm":
        return 0
    server = start_server(options.cache, options.listen)
    log(f"serving {options.cache} on {options.listen}")
    if options.action == "serve":
        try:
            threading.Event().wait()
        except KeyboardInterrupt:
            return 0
    if not command:
        parser.error("run needs a command after --")
    environment = dict(os.environ, **{MIRROR_ENV: options.listen})
    try:
        return subprocess.call(command, env=environment)
    finally:
        server.shutdown()


if __name__ == "__main__":
    sys.exit(main())
