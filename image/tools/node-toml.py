#!/usr/bin/env python3
"""node.toml for one appliance node of a fleet seeded by hand (preview).

    node-toml.py --cluster NAME --addresses IP1,IP2,... --index N
                 --operator IP [--operator-key ed25519:...]

Node N (1-based) gets the N-th address. Node 1 bootstraps the cluster;
the others join it. Mirrors src/relish/quickstart/provision.rs::node_config
(mTLS, eBPF, DNS, ingress, the laptop fault policy), plus operator_cidrs so
relish on the operator's machine can reach the API, and optionally the
operator key cluster bun upgrades need (docs/manual/12_operations.md).
"""
import argparse

parser = argparse.ArgumentParser()
parser.add_argument("--cluster", required=True)
parser.add_argument("--addresses", required=True, help="comma-separated, node 1 first")
parser.add_argument("--index", type=int, required=True)
parser.add_argument("--operator", required=True, help="address relish connects from")
parser.add_argument("--operator-key", default="")
args = parser.parse_args()

addresses = [a.strip() for a in args.addresses.split(",") if a.strip()]
if not 1 <= args.index <= len(addresses):
    parser.error(f"--index must be between 1 and {len(addresses)}")
me = addresses[args.index - 1]
peers = ", ".join(f'"{a}"' for a in addresses)
join = "[]" if args.index == 1 else f'["{addresses[0]}:9443"]'
bootstrap = 'bootstrap_path = "/etc/reliaburger/security-bootstrap.json"\n' if args.index == 1 else ""

print(f'''[node]
name = "node-{args.index:02d}"

[cluster]
name = "{args.cluster}"
join = {join}

[network]
advertise_address = "{me}"

[security]
require_mtls = true
allow_insecure_cluster = false
identity_dir = "/etc/reliaburger/identity"
master_key_path = "/etc/reliaburger/master.key"
{bootstrap}bootstrap_peers = [{peers}]
operator_cidrs = ["{args.operator}"]

[ebpf]
enabled = true

[dns]
enabled = true
listen = "{me}:53"

[ingress]
enabled = true

[testing]
safety_class = "development"
allowed_operations = ["inject_workload_faults", "alter_node_state"]
''', end="")
if args.operator_key:
    print(f'''
[upgrades]
external_signing_key = "{args.operator_key}"
''', end="")
