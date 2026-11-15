#!/usr/bin/env python3
"""Publish a promoted release's MCP Registry entry, after checking it (#1695).

    make mcp-registry-publish TAG=v0.2.8

which runs

    mcp_registry_publish.py --tag v0.2.8

It finds the successful Promote run for the tag (`promote.yml` names its runs
`Promote <tag>`), downloads that run's `mcp-registry-server-json` artefact into
a temporary directory and checks the entry: it names `relish mcp`, its version
is the tag's, and it has exactly one `mcpb` package per relish platform, each
pointing at the tag's bundle with the SHA-256 GitHub stores for the published
asset. Only then does it run `mcp-publisher publish` in that directory, after
`mcp-publisher login github` when there is no unexpired registry token.

It needs `gh`, signed in, and `mcp-publisher`. Only the maintainer runs it.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time

from mcp_registry import PACKAGE_TRANSPORT
from package import PLATFORMS, mcpb_name

REPOSITORY = "reliaburger/reliaburger"
ARTEFACT = "mcp-registry-server-json"
WORKFLOW = "promote.yml"
NAME = "io.github.reliaburger/relish"
# Where `mcp-publisher login` keeps the registry's token, relative to the
# home directory (cmd/publisher/commands/login.go in modelcontextprotocol/registry).
TOKEN_FILE = Path(".config/mcp-publisher/token.json")
# Log in again when the token has less than this left, so it can't expire
# between the check and the publish.
TOKEN_MARGIN_SECONDS = 60


def run_title(tag):
    """The name `promote.yml`'s `run-name` gives a run that promotes `tag`."""
    return f"Promote {tag}"


def gh_json(*arguments):
    result = subprocess.run(["gh", *arguments], check=True, capture_output=True, text=True)
    return json.loads(result.stdout)


def check_tag(tag):
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError(f"only a release tag reaches the MCP Registry, not {tag!r}")


def promote_run(repository, tag):
    """The newest successful Promote run on main for `tag`."""
    runs = gh_json("run", "list", "--repo", repository, "--workflow", WORKFLOW, "--branch", "main",
                   "--status", "success", "--limit", "100",
                   "--json", "databaseId,displayTitle,createdAt")
    matching = [run for run in runs if run.get("displayTitle") == run_title(tag)]
    if not matching:
        raise ValueError(f"no successful {WORKFLOW} run named {run_title(tag)!r}; pass --run with its ID")
    return max(matching, key=lambda run: run["createdAt"])["databaseId"]


def download_entry(repository, run_id, directory):
    """Download the run's entry into `directory` and return its text."""
    subprocess.run(["gh", "run", "download", str(run_id), "--repo", repository,
                    "--name", ARTEFACT, "--dir", str(directory)], check=True, stdout=sys.stderr)
    path = directory / "server.json"
    if not path.is_file():
        raise ValueError(f"run {run_id}'s {ARTEFACT} artefact has no server.json")
    return path.read_text()


def published_digests(repository, tag):
    """`{asset name: sha256}` for the tag's public release, as GitHub stores them."""
    release = gh_json("api", f"repos/{repository}/releases/tags/{tag}")
    if release.get("draft") is not False or release.get("prerelease") is not False:
        raise ValueError(f"{tag} is not a published release")
    digests = {}
    for asset in release.get("assets", []):
        digest = asset.get("digest") or ""
        if digest.startswith("sha256:"):
            digests[asset["name"]] = digest.removeprefix("sha256:")
    return digests


def check_entry(text, tag, repository, digests):
    """Refuse an entry that isn't this tag's, or whose bundles aren't the published ones."""
    check_tag(tag)
    document = json.loads(text)
    version = tag[1:]
    if document.get("name") != NAME:
        raise ValueError(f"the entry is for {document.get('name')!r}, not {NAME}")
    if document.get("version") != version:
        raise ValueError(f"the entry is version {document.get('version')!r}, not {version}")
    packages = document.get("packages") or []
    expected = {f"https://github.com/{repository}/releases/download/{tag}/{mcpb_name(platform)}":
                mcpb_name(platform) for platform in PLATFORMS["relish"]}
    found = [package.get("identifier") for package in packages]
    if sorted(found) != sorted(expected):
        raise ValueError(f"the entry's packages are {found}, not one bundle per platform of {tag}")
    for package in packages:
        name = expected[package["identifier"]]
        if package.get("registryType") != "mcpb" or package.get("version") != version \
                or package.get("transport") != PACKAGE_TRANSPORT:
            raise ValueError(f"the package for {name} isn't an mcpb {version} stdio package")
        if name not in digests:
            raise ValueError(f"the {tag} release has no {name}")
        if package.get("fileSha256") != digests[name]:
            raise ValueError(f"{name}: the entry says {package.get('fileSha256')}, "
                             f"the release has {digests[name]}")


def logged_in(home, now=None):
    """Whether `mcp-publisher` holds a registry token that hasn't expired.

    The token is a JWT; its payload's `exp` says when it stops working. A
    token whose expiry can't be read counts as no token: logging in again is
    harmless, publishing with a dead token isn't.
    """
    now = time.time() if now is None else now
    try:
        token = json.loads((home / TOKEN_FILE).read_text())["token"]
        payload = token.split(".")[1]
        claims = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
        return float(claims["exp"]) > now + TOKEN_MARGIN_SECONDS
    except (OSError, ValueError, KeyError, IndexError, TypeError):
        return False


def publish(directory, home):
    """Log in when needed, then publish the entry in `directory`."""
    if shutil.which("mcp-publisher") is None:
        raise ValueError("mcp-publisher isn't on PATH: brew install mcp-publisher")
    if not logged_in(home):
        subprocess.run(["mcp-publisher", "login", "github"], check=True)
    subprocess.run(["mcp-publisher", "publish"], cwd=directory, check=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tag", required=True, help="the promoted release, such as v0.2.8")
    parser.add_argument("--repository", default=REPOSITORY)
    parser.add_argument("--run", type=int, help="the Promote run's ID, when its name doesn't carry the tag")
    args = parser.parse_args(argv)
    try:
        check_tag(args.tag)
        run_id = args.run or promote_run(args.repository, args.tag)
        with tempfile.TemporaryDirectory(prefix="mcp-registry-") as temporary:
            directory = Path(temporary)
            text = download_entry(args.repository, run_id, directory)
            check_entry(text, args.tag, args.repository, published_digests(args.repository, args.tag))
            print(f"run {run_id}'s entry matches the {args.tag} release; publishing", file=sys.stderr)
            publish(directory, Path.home())
    except (ValueError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(f"published {NAME} {args.tag[1:]}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
