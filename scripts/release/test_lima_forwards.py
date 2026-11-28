"""Tests for lima_forwards.py: does a Lima VM keep the test ports to itself?

A loop run with --parallel runs the macOS lanes beside the VM's. Both lease
test ports from the same ranges, each in its own /tmp, and Lima's default
rule forwards every guest loopback port onto the Mac's 127.0.0.1. On
28 November a guest test's port, forwarded that way, held a Mac node's Raft
port and another's API port through their upgrade restarts, and both
upgrades reverted.
"""
import io
import json
import sys
import unittest
from contextlib import redirect_stderr
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import lima_forwards  # noqa: E402

RECOMMENDED = [
    {"guestIP": "127.0.0.1", "guestPortRange": [10000, 29999], "proto": "any", "ignore": True},
]


def instance(rules):
    config = {"vmType": "vz"}
    if rules is not None:
        config["portForwards"] = rules
    return {"name": "reliaburger-test", "status": "Running", "config": config}


class LeakTests(unittest.TestCase):
    def test_a_vm_with_no_rules_forwards_every_test_port_over_both_protocols(self):
        self.assertEqual(
            lima_forwards.leaks(None),
            [
                ("tcp", 10000, 14999),
                ("tcp", 27000, 29999),
                ("udp", 10000, 14999),
                ("udp", 27000, 29999),
            ],
        )

    def test_the_recommended_rule_keeps_every_test_port_in_the_guest(self):
        self.assertEqual(lima_forwards.leaks(RECOMMENDED), [])

    def test_the_recommended_rule_is_what_the_refusal_prints(self):
        text = lima_forwards.RULE_YAML
        self.assertIn('guestIP: "127.0.0.1"', text)
        self.assertIn("guestPortRange: [10000, 29999]", text)
        self.assertIn("proto: any", text)
        self.assertIn("ignore: true", text)

    def test_an_ignore_rule_without_a_protocol_covers_tcp_only(self):
        rules = [{"guestPortRange": [10000, 29999], "ignore": True}]
        self.assertEqual(
            lima_forwards.leaks(rules),
            [("udp", 10000, 14999), ("udp", 27000, 29999)],
        )

    def test_the_first_matching_rule_wins(self):
        rules = [{"guestPort": 28000, "proto": "any"}] + RECOMMENDED
        self.assertEqual(
            lima_forwards.leaks(rules),
            [("tcp", 28000, 28000), ("udp", 28000, 28000)],
        )

    def test_a_rule_for_wildcard_binds_only_does_not_match_loopback_binds(self):
        rules = [dict(RECOMMENDED[0], guestIP="0.0.0.0", guestIPMustBeZero=True)]
        self.assertEqual(len(lima_forwards.leaks(rules)), 4)

    def test_a_wildcard_rule_matches_loopback_binds_only_when_told_to(self):
        rules = [dict(RECOMMENDED[0], guestIP="0.0.0.0", guestIPMustBeZero=False)]
        self.assertEqual(lima_forwards.leaks(rules), [])

    def test_a_rule_for_another_guest_address_does_not_match_loopback_binds(self):
        rules = [dict(RECOMMENDED[0], guestIP="127.0.0.2")]
        self.assertEqual(len(lima_forwards.leaks(rules)), 4)

    def test_a_socket_rule_matches_no_port(self):
        rules = [{"guestSocket": "/run/x.sock", "hostSocket": "x", "ignore": True}]
        self.assertEqual(len(lima_forwards.leaks(rules)), 4)

    def test_two_narrower_rules_cover_the_ranges_too(self):
        rules = [
            {"guestPortRange": [10000, 15000], "proto": "any", "ignore": True},
            {"guestIP": "127.0.0.1", "guestPortRange": [27000, 29999], "proto": "any", "ignore": True},
        ]
        self.assertEqual(lima_forwards.leaks(rules), [])


class CommandTests(unittest.TestCase):
    def run_check(self, listing):
        stderr = io.StringIO()
        with redirect_stderr(stderr):
            code = lima_forwards.main(["check"], stdin=io.StringIO(listing))
        return code, stderr.getvalue()

    def test_check_passes_a_vm_that_ignores_the_test_ports(self):
        code, message = self.run_check(json.dumps(instance(RECOMMENDED)) + "\n")
        self.assertEqual(code, 0, message)

    def test_check_refuses_a_vm_that_forwards_them_and_prints_the_rule(self):
        code, message = self.run_check(json.dumps(instance(None)) + "\n")
        self.assertEqual(code, 1)
        self.assertIn("reliaburger-test forwards", message)
        self.assertIn("tcp 27000-29999", message)
        self.assertIn(lima_forwards.RULE_YAML, message)
        self.assertIn("limactl edit", message)

    def test_check_refuses_a_wildcard_rule_as_lima_2_fills_it_in(self):
        # Lima 2.2 stores `guestIP: "0.0.0.0"` with guestIPMustBeZero true,
        # so the rule covers wildcard binds only, and the test Buns' 127.0.0.1
        # binds still fall through to the forwarding fallback (seen on
        # reliaburger-test, 28 Nov).
        rule = {
            "guestIPMustBeZero": True,
            "guestIP": "0.0.0.0",
            "guestPortRange": [10000, 29999],
            "hostIP": "127.0.0.1",
            "hostPortRange": [10000, 29999],
            "proto": "any",
            "ignore": True,
        }
        code, message = self.run_check(json.dumps(instance([rule])) + "\n")
        self.assertEqual(code, 1)
        self.assertIn("tcp 27000-29999", message)

    def test_check_refuses_a_listing_it_cannot_read(self):
        code, message = self.run_check("not json\n")
        self.assertEqual(code, 1)
        self.assertIn("cannot read", message)


class LoopScriptTests(unittest.TestCase):
    def test_the_loop_script_checks_every_vm_before_a_parallel_run(self):
        script = (Path(__file__).resolve().parent / "qualify-v02-loops-local.sh").read_text()
        check = script.index('python3 "$here/lima_forwards.py" check')
        self.assertIn('if [ "$parallel" = true ]; then', script[check - 200 : check])
        self.assertIn('limactl list --json "$vm" |', script[check - 100 : check])
        self.assertIn('for vm in "${vms[@]}"; do', script[check - 800 : check])


if __name__ == "__main__":
    unittest.main()
