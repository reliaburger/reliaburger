#!/usr/bin/env python3
"""Write the Homebrew formula for a published release, and push it to the tap (#1583).

    homebrew.py formula --record candidate.json --qualified-digest SHA256 --output relish.rb
    homebrew.py publish --formula relish.rb --version v0.2.7 --tap reliaburger/homebrew-tap

`formula` fills `relish.rb.in` from a release's `candidate.json`: the same
record promotion verified, so the formula pins exactly the bytes that were
qualified and published. It never downloads a binary or hashes one itself.
Run it locally against any candidate's record as a dry run.

`publish` commits the formula to the tap as `Formula/relish.rb` and pushes.
It authenticates with `HOMEBREW_TAP_TOKEN`; without one it says so and
succeeds, so promotion works before the maintainer has made the tap.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

from package import PLATFORMS

TEMPLATE = Path(__file__).with_name("relish.rb.in")
FORMULA_PATH = "Formula/relish.rb"
TOKEN = "HOMEBREW_TAP_TOKEN"


def placeholder(platform):
    """`macos-aarch64` becomes `@MACOS_AARCH64@`."""
    return "@" + platform.upper().replace("-", "_") + "@"


def formula(record, qualified_digest=None):
    """The formula text for the release `record` (candidate.json's bytes) describes."""
    if qualified_digest is not None and hashlib.sha256(record).hexdigest() != qualified_digest:
        raise ValueError("candidate record differs from the qualified digest")
    document = json.loads(record)
    if document.get("channel", "release") != "release":
        raise ValueError("a nightly build never reaches the tap")
    tag = document["version"]
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError(f"only a release tag reaches the tap, not {tag!r}")
    text = TEMPLATE.read_text()
    text = (text.replace("@TAG@", tag).replace("@VERSION@", tag[1:])
            .replace("@REPOSITORY@", document["repository"]))
    for platform in PLATFORMS["relish"]:
        asset = document["assets"].get(f"relish-{platform}")
        if asset is None:
            raise ValueError(f"the record has no relish-{platform}")
        if placeholder(platform) not in text:
            raise ValueError(f"relish.rb.in has no {placeholder(platform)}; add relish-{platform} to it")
        text = text.replace(placeholder(platform), asset["sha256"])
    leftover = re.findall(r"@[A-Z0-9_]+@", text)
    if leftover:
        raise ValueError(f"relish.rb.in names platforms no release builds: {sorted(set(leftover))}")
    return text


def git(*args, cwd=None, token=None):
    """Run git; with a token, send it as a header so it never lands in .git/config."""
    command = ["git"]
    if token:
        basic = base64.b64encode(f"x-access-token:{token}".encode()).decode()
        command += ["-c", f"http.https://github.com/.extraheader=AUTHORIZATION: basic {basic}"]
    subprocess.run(command + list(args), cwd=cwd, check=True)


def publish(text, version, remote, token=None):
    """Commit `text` as the tap's formula and push it. Returns False when it was already there."""
    with tempfile.TemporaryDirectory(prefix="reliaburger-tap-") as scratch:
        checkout = Path(scratch) / "tap"
        git("clone", "--quiet", "--depth", "1", remote, str(checkout), token=token)
        path = checkout / FORMULA_PATH
        if path.exists() and path.read_text() == text:
            print(f"{FORMULA_PATH} already describes relish {version}")
            return False
        path.parent.mkdir(exist_ok=True)
        path.write_text(text)
        git("add", FORMULA_PATH, cwd=checkout)
        git("-c", "user.name=github-actions[bot]",
            "-c", "user.email=41898282+github-actions[bot]@users.noreply.github.com",
            "commit", "--quiet", "-m", f"relish {version.removeprefix('v')}", cwd=checkout)
        git("push", "--quiet", "origin", "HEAD", cwd=checkout, token=token)
        print(f"pushed relish {version} to {remote}")
        return True


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    operations = parser.add_subparsers(dest="operation", required=True)
    write = operations.add_parser("formula", help="write the formula from a release's candidate.json")
    write.add_argument("--record", type=Path, required=True)
    write.add_argument("--qualified-digest", help="refuse a record whose SHA-256 differs")
    write.add_argument("--output", type=Path, help="default: standard output")
    push = operations.add_parser("publish", help="commit the formula to the tap and push it")
    push.add_argument("--formula", type=Path, required=True)
    push.add_argument("--version", required=True)
    push.add_argument("--tap", default="reliaburger/homebrew-tap", help="the tap's GitHub repository")
    push.add_argument("--remote", help="clone and push here instead of the tap on GitHub")
    args = parser.parse_args(argv)

    if args.operation == "formula":
        text = formula(args.record.read_bytes(), args.qualified_digest)
        if args.output:
            args.output.write_text(text)
        else:
            sys.stdout.write(text)
        return

    token = os.environ.get(TOKEN)
    remote = args.remote or f"https://github.com/{args.tap}.git"
    if not token and not args.remote:
        print(f"::notice title=Homebrew tap not updated::{TOKEN} is not set, so {args.tap} keeps "
              f"its formula. See docs/releasing.md#the-homebrew-tap.")
        return
    publish(args.formula.read_text(), args.version, remote, token)


if __name__ == "__main__":
    main()
