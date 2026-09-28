#!/usr/bin/env python3
# node-toml.py <n> <count>: node.toml for lab node n of count (spike S3).
# Mirrors src/relish/quickstart/provision.rs::node_config, plus
# operator_cidrs for relish on the lab server (192.168.105.1).
import sys
n, count = int(sys.argv[1]), int(sys.argv[2])
ip = lambda i: f"192.168.105.{100 + i}"
peers = ", ".join(f'"{ip(i)}"' for i in range(1, count + 1))
join = "[]" if n == 1 else f'["{ip(1)}:9443"]'
bootstrap = 'bootstrap_path = "/etc/reliaburger/security-bootstrap.json"\n' if n == 1 else ""
print(f'''[node]
name = "node-{n:02d}"

[cluster]
name = "lab"
join = {join}

[network]
advertise_address = "{ip(n)}"

[security]
require_mtls = true
allow_insecure_cluster = false
identity_dir = "/etc/reliaburger/identity"
master_key_path = "/etc/reliaburger/master.key"
{bootstrap}bootstrap_peers = [{peers}]
operator_cidrs = ["192.168.105.1"]

[ebpf]
enabled = true

[dns]
enabled = true
listen = "{ip(n)}:53"

[ingress]
enabled = true

[testing]
safety_class = "development"
allowed_operations = ["inject_workload_faults", "alter_node_state"]
''', end="")
