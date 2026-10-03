#!/usr/bin/env python3
"""The nodes fleet-measure.sh samples, one `<name> <address>` per line.

    fleet-nodes.py <dir>                 # a claim or seed-fleet.sh directory
    fleet-nodes.py --relish-json <file>  # `relish nodes --output json`; - for stdin

A claim directory (`relish machines claim`, `relish cluster create
--bare-metal`) has fleet.json, whose nodes carry a name and an address. A
seed-fleet.sh directory has a `fleet` file of `<n> <mac> <ip>` lines, and
its nodes are node-01, node-02 and so on, as they always were. With both,
fleet.json wins.

`relish nodes --output json` lists every node gossip knows, dead ones too
(they get an empty row, so the gap shows). Its `address` is the gossip
endpoint, so the port goes.
"""

import json
import sys
from pathlib import Path


def from_directory(directory):
    directory = Path(directory)
    claimed = directory / "fleet.json"
    if claimed.is_file():
        fleet = json.loads(claimed.read_text())
        return [(node["name"], node["address"]) for node in fleet["nodes"]]
    seeded = directory / "fleet"
    if seeded.is_file():
        nodes = []
        for line in seeded.read_text().splitlines():
            if line.strip():
                index, _mac, address = line.split()
                nodes.append((f"node-{int(index):02d}", address))
        return nodes
    raise ValueError(f"{directory} has no fleet.json or fleet file")


def host(endpoint):
    """`10.77.0.11:9443` → `10.77.0.11`; `[fd00::7]:9443` → `fd00::7`."""
    if endpoint.startswith("["):
        return endpoint[1:endpoint.index("]")]
    if endpoint.count(":") == 1:
        return endpoint.rsplit(":", 1)[0]
    return endpoint


def from_relish_nodes(text):
    try:
        nodes = json.loads(text)
    except json.JSONDecodeError:
        nodes = []
    if not nodes:
        raise ValueError("relish nodes listed no nodes: is relish pointed at the cluster?")
    return [(node["node_id"], host(node["address"])) for node in nodes]


def main(argv):
    try:
        if len(argv) == 2 and argv[0] == "--relish-json":
            text = sys.stdin.read() if argv[1] == "-" else Path(argv[1]).read_text()
            nodes = from_relish_nodes(text)
        elif len(argv) == 1 and not argv[0].startswith("-"):
            nodes = from_directory(argv[0])
        else:
            print("usage: fleet-nodes.py <dir> | --relish-json <file|->", file=sys.stderr)
            return 2
    except (ValueError, KeyError, OSError) as error:
        print(f"fleet-nodes.py: {error}", file=sys.stderr)
        return 1
    for name, address in nodes:
        print(name, address)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
