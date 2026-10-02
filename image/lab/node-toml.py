#!/usr/bin/env python3
# node-toml.py <n> <count>: node.toml for lab node n of count, through
# image/tools/node-toml.py with the lab's addresses (.101 up, one per
# reservation in conf/router.conf) and relish on the lab server (.1).
# OPERATOR_KEY="ed25519:..." adds the key cluster bun upgrades need.
import os
import subprocess
import sys

n, count = int(sys.argv[1]), int(sys.argv[2])
tool = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tools", "node-toml.py")
addresses = ",".join(f"192.168.105.{100 + i}" for i in range(1, count + 1))
command = [sys.executable, tool, "--cluster", "lab", "--addresses", addresses,
           "--index", str(n), "--operator", "192.168.105.1"]
if os.environ.get("OPERATOR_KEY"):
    command += ["--operator-key", os.environ["OPERATOR_KEY"]]
sys.exit(subprocess.run(command).returncode)
