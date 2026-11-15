#!/usr/bin/env python3
"""Check the release binaries the profile step measures against their candidate.json.

  profile_fetch.py verify --record candidate.json [--digest SHA256]
      --asset bun-linux-aarch64=PATH --asset relish-linux-aarch64=PATH

A staged candidate and a published release both carry candidate.json, the
manifest build.yml wrote: version, commit, build run and every asset's
SHA-256. The profile measures the release binaries unchanged, so every
downloaded asset must match it, and with --digest the manifest itself must
hash to the qualified digest. Prints the identity as shell assignments
(version=..., commit=..., run_id=..., run_attempt=...) for qualify-profile.sh.
"""
import argparse
import hashlib
import json
import re
import sys


class VerifyError(Exception):
    """The manifest or an asset doesn't match."""


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def verify(record_path, assets, digest=None):
    """Return the manifest's identity once the manifest and every asset check out."""
    if digest and sha256_file(record_path) != digest.lower():
        raise VerifyError(f"{record_path} doesn't hash to the qualified digest {digest}")
    with open(record_path) as source:
        try:
            document = json.load(source)
        except ValueError as error:
            raise VerifyError(f"{record_path} isn't JSON: {error}") from error
    listed = document.get("assets", {})
    for name, path in assets.items():
        expected = listed.get(name, {}).get("sha256")
        if not expected:
            raise VerifyError(f"candidate.json lists no {name}")
        actual = sha256_file(path)
        if actual != expected:
            raise VerifyError(f"{name} has SHA-256 {actual}, candidate.json says {expected}")
    identity = {key: document.get(key) for key in ("version", "commit", "run_id", "run_attempt", "repository")}
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", str(identity["version"] or "")):
        raise VerifyError("candidate.json names no release version")
    if not re.fullmatch(r"[0-9a-f]{40}", str(identity["commit"] or "")):
        raise VerifyError("candidate.json names no full commit")
    return identity


def shell_assignments(identity):
    lines = []
    for key, value in identity.items():
        text = str(value if value is not None else "")
        if not re.fullmatch(r"[A-Za-z0-9._/-]*", text):
            raise VerifyError(f"candidate.json {key} has unexpected characters")
        lines.append(f"{key}={text}")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    check = sub.add_parser("verify")
    check.add_argument("--record", required=True)
    check.add_argument("--digest")
    check.add_argument("--asset", action="append", default=[], metavar="NAME=PATH")
    args = parser.parse_args()
    assets = {}
    for item in args.asset:
        name, _, path = item.partition("=")
        if not name or not path:
            parser.error(f"--asset needs NAME=PATH, not {item!r}")
        assets[name] = path
    try:
        print(shell_assignments(verify(args.record, assets, args.digest)))
    except (VerifyError, OSError) as error:
        print(f"profile_fetch: {error}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
