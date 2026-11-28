#!/usr/bin/env python3
"""Does a Lima VM keep the test suites' ports to itself?

A loop run with --parallel (qualify-v02-loops-local.sh) runs the macOS lanes
beside the VM's. Both sides lease test ports from the same ranges, each in
its own /tmp, so neither sees the other's leases. Lima's built-in fallback
rule forwards every port a guest process binds on loopback to the same port
on the Mac's 127.0.0.1, TCP and UDP. On 28 November a guest test's port,
forwarded that way, held a Mac node's Raft port and another's API port
through their upgrade restarts, and both upgrades reverted.

The ranges that must stay in the guest are the leased fixture ports
(TEST_PORTS, 27000-29999) and the ports test Buns hand workloads
(TEST_WORKLOAD_PORTS, 10000-14999), both in tests/support/bun_process.rs.

  limactl list --json VM | lima_forwards.py check

Exits 0 when the VM forwards none of them, 1 with the ports it forwards and
the rule to add on stderr.
"""
import json
import sys

# (first, last) inclusive, as Lima's guestPortRange has them.
TEST_RANGES = [(10000, 14999), (27000, 29999)]
PROTOCOLS = ("tcp", "udp")

# One rule covers both ranges and the in-process suites' fixed ports between
# them (15000-26999), which the two hosts would share the same way. Not
# `guestIP: "0.0.0.0"`: Lima 2.2 stores that with guestIPMustBeZero true, so
# it covers wildcard binds only and the tests' 127.0.0.1 binds still forward.
RULE_YAML = """portForwards:
- guestIP: "127.0.0.1"
  guestPortRange: [10000, 29999]
  proto: any
  ignore: true"""

# Test Buns bind 127.0.0.1. A rule's default guestIP, 127.0.0.1, matches
# that bind, and so does 0.0.0.0 unless guestIPMustBeZero limits it to
# wildcard binds.
LOOPBACK_MATCHES = {"127.0.0.1", "0.0.0.0"}


def rule_ports(rule):
    """The guest ports a rule matches, or None for a socket rule."""
    if "guestSocket" in rule:
        return None
    if "guestPort" in rule:
        port = int(rule["guestPort"])
        return port, port
    first, last = rule.get("guestPortRange", [1, 65535])
    return int(first), int(last)


def rule_matches(rule, protocol, port):
    """Does Lima apply `rule` to a guest process binding 127.0.0.1:`port`?"""
    ports = rule_ports(rule)
    if ports is None or not ports[0] <= port <= ports[1]:
        return False
    proto = str(rule.get("proto", "tcp")).lower()
    if proto not in ("any", protocol):
        return False
    guest_ip = str(rule.get("guestIP", "127.0.0.1"))
    if guest_ip not in LOOPBACK_MATCHES:
        return False
    return not (guest_ip == "0.0.0.0" and rule.get("guestIPMustBeZero", False))


def forwarded(rules, protocol, port):
    """Lima applies the first matching rule, then its fallback, which forwards."""
    for rule in rules:
        if rule_matches(rule, protocol, port):
            return not rule.get("ignore", False)
    return True


def leaks(rules):
    """Every test port the VM forwards to the Mac, as (protocol, first, last)."""
    rules = rules or []
    found = []
    for protocol in PROTOCOLS:
        for first, last in TEST_RANGES:
            start = None
            for port in range(first, last + 2):
                leaking = port <= last and forwarded(rules, protocol, port)
                if leaking and start is None:
                    start = port
                elif not leaking and start is not None:
                    found.append((protocol, start, port - 1))
                    start = None
    return found


def main(argv, stdin=sys.stdin):
    if argv != ["check"]:
        print("usage: limactl list --json VM | lima_forwards.py check", file=sys.stderr)
        return 2
    try:
        listing = json.loads(stdin.readline())
        name = listing["name"]
        rules = (listing.get("config") or {}).get("portForwards")
    except (ValueError, KeyError, TypeError) as error:
        print(f"cannot read the limactl listing: {error}", file=sys.stderr)
        return 1
    found = leaks(rules)
    if not found:
        return 0
    ports = ", ".join(f"{protocol} {first}-{last}" for protocol, first, last in found)
    print(
        f"{name} forwards the test suites' ports to the Mac ({ports}), so its lanes "
        "and the Mac's collide on them. Add this to the VM's lima.yaml "
        f"(`limactl edit {name}`, or edit ~/.lima/{name}/lima.yaml and restart it):\n"
        f"{RULE_YAML}",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
