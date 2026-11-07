#!/usr/bin/env python3
"""Write the MCP Registry entry for a published release, with each bundle's digest (#1695).

    mcp_registry.py --record candidate.json --server tagged-server.json \
        --qualified-digest SHA256 --output server.json

The committed `server.json` (written by `make agent-docs`) has no
`packages`: a package's `fileSha256` is the SHA-256 of a release's `.mcpb`
bundle, which only exists once the release is built. This adds one `mcpb`
package per relish platform, taking each digest from the release's
`candidate.json`: the same record promotion verified, so the entry names
exactly the bytes that were qualified and published. It never downloads a
bundle or hashes one itself. Run it locally against any candidate's record
as a dry run.

Publishing the entry needs the maintainer's GitHub login to the
`reliaburger` organisation: `mcp-publisher publish` in the directory that
holds the written `server.json` (docs/releasing.md).
"""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import re
import sys

from package import PLATFORMS, mcpb_name

# `relish mcp` talks over stdin and stdout.
PACKAGE_TRANSPORT = {"type": "stdio"}


def entry(record, server, qualified_digest=None):
    """`server` (the tagged server.json's text) with a package per bundle `record` (candidate.json's bytes) names."""
    if qualified_digest is not None and hashlib.sha256(record).hexdigest() != qualified_digest:
        raise ValueError("candidate record differs from the qualified digest")
    document = json.loads(record)
    if document.get("channel", "release") != "release":
        raise ValueError("a nightly build never reaches the MCP Registry")
    tag = document["version"]
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        raise ValueError(f"only a release tag reaches the MCP Registry, not {tag!r}")
    version = tag[1:]
    listing = json.loads(server)
    if listing.get("version") != version:
        raise ValueError(f"server.json is for {listing.get('version')!r}, not {version}; "
                         "run `make agent-docs` before tagging")
    if "packages" in listing:
        raise ValueError("server.json already lists packages; only promotion writes them")
    listing = copy.deepcopy(listing)
    packages = []
    for platform in PLATFORMS["relish"]:
        name = mcpb_name(platform)
        asset = document["assets"].get(name)
        if asset is None:
            raise ValueError(f"the record has no {name}")
        packages.append({
            "registryType": "mcpb",
            "identifier": f"https://github.com/{document['repository']}/releases/download/{tag}/{name}",
            "version": version,
            "fileSha256": asset["sha256"],
            "transport": dict(PACKAGE_TRANSPORT),
        })
    listing["packages"] = packages
    return json.dumps(listing, indent=2) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--record", type=Path, required=True, help="the published release's candidate.json")
    parser.add_argument("--server", type=Path, required=True, help="server.json as the release tag has it")
    parser.add_argument("--qualified-digest", help="refuse a record whose SHA-256 differs")
    parser.add_argument("--output", type=Path, help="default: standard output")
    args = parser.parse_args(argv)
    text = entry(args.record.read_bytes(), args.server.read_text(), args.qualified_digest)
    if args.output:
        args.output.write_text(text)
    else:
        sys.stdout.write(text)


if __name__ == "__main__":
    main()
