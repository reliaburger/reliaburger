"""A throwaway local cluster for one gym run (stdlib only).

`LocalCluster` starts one to five real `bun --cluster` processes on the
process runtime, so it needs no container runtime and runs on macOS and Linux
alike. Every file it writes, the nodes' data, logs and `RELIABURGER_HOME`
included, goes under one work directory, and every port comes from one block
the caller names. `stop()` stops the nodes and every workload process whose
command line names that directory, and the caller removes it.

The nodes run plaintext on loopback (`require_mtls = false`) with a shared
master key, so the API still checks every token: the solver's read-only token
is refused anything else, and the refusal lands in the audit log.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import time
import urllib.error
import urllib.request

# Ports per node: the API, gossip, Raft and reporting.
PORTS_PER_NODE = 4
# Ports after the nodes' for the scenario apps (@APP_PORT@, @SECOND_PORT@).
APP_PORTS = 2
START_TIMEOUT = 180
COMMAND_TIMEOUT = 60

# The port blocks the maintainer's other clusters use on the Studio: refuse
# a base that would land on them.
RESERVED_PORTS = {9117, 8080, 15050, 29117, 29118, 29119, 28080, 25050}


class ClusterError(RuntimeError):
    """The cluster didn't start, or a command the runner needed failed."""


def ports_needed(nodes):
    return nodes * PORTS_PER_NODE + APP_PORTS


def check_port_block(base, nodes):
    """Refuse a block that overlaps a reserved port or runs past 65535."""
    block = set(range(base, base + ports_needed(nodes)))
    clash = sorted(block & RESERVED_PORTS)
    if clash:
        raise ClusterError(f"port block {base}+{len(block)} overlaps reserved ports {clash}")
    if base < 1024 or base + len(block) > 65535:
        raise ClusterError(f"port block {base}+{len(block)} is out of range")


def node_config(name, root, ports, join, master_key, testapp):
    """The node config for one gym node."""
    api, gossip, raft, reporting = ports
    join_line = f'join = ["127.0.0.1:{join}"]' if join else "join = []"
    return f"""
[node]
name = "{name}"

[cluster]
{join_line}
gossip_port = {gossip}
raft_port = {raft}
reporting_port = {reporting}

[network]
advertise_address = "127.0.0.1"

[security]
master_key_path = "{master_key}"
# A loopback throwaway cluster with no node certificates: plaintext on purpose.
require_mtls = false

[storage]
data = "{root}/data"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"

[images]
registry_port = 0

[process_workloads]
allowed_binaries = ["{testapp}", "/bin/sh"]
mount_isolation = false

[testing]
safety_class = "development"
allowed_operations = ["inject_workload_faults", "alter_node_state"]
"""


class LocalCluster:
    """Real Bun nodes for one run; see the module docstring."""

    def __init__(self, work_dir, nodes, port_base, binaries):
        check_port_block(port_base, nodes)
        self.work_dir = Path(work_dir)
        self.nodes = nodes
        self.port_base = port_base
        self.binaries = binaries
        self.names = [f"gym-{index + 1}" for index in range(nodes)]
        self.processes = {}
        self.admin_token = None
        app_base = port_base + nodes * PORTS_PER_NODE
        self.substitutions = {
            "@TESTAPP@": str(binaries["testapp"]),
            "@APP_PORT@": str(app_base),
            "@SECOND_PORT@": str(app_base + 1),
        }

    def _ports(self, index):
        base = self.port_base + index * PORTS_PER_NODE
        return (base, base + 1, base + 2, base + 3)

    def api(self, name):
        return f"http://127.0.0.1:{self._ports(self.names.index(name))[0]}"

    def base_env(self):
        """The environment every `relish` here runs with, minus a token."""
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("RELIABURGER_")
        }
        env["RELIABURGER_HOME"] = str(self.work_dir / "home")
        env["RELIABURGER_ENDPOINT"] = self.api(self.names[0])
        return env

    def relish(self, args, token=None, binary=None, timeout=COMMAND_TIMEOUT):
        """Run `relish` against this cluster as admin (or with `token`)."""
        env = self.base_env()
        chosen = token if token is not None else self.admin_token
        if chosen:
            env["RELIABURGER_TOKEN"] = chosen
        return subprocess.run(
            [str(binary or self.binaries["relish"]), *args],
            capture_output=True, text=True, env=env, timeout=timeout, check=False,
        )

    def relish_json(self, args):
        completed = self.relish([*args, "-o", "json"])
        if completed.returncode != 0:
            raise ClusterError(f"relish {' '.join(args)} failed: {completed.stderr.strip()}")
        return json.loads(completed.stdout)

    def start(self):
        (self.work_dir / "home").mkdir(parents=True, exist_ok=True)
        master_key = self.work_dir / "master.key"
        master_key.write_text(secrets.token_hex(32))
        master_key.chmod(0o600)
        seed = None
        for index, name in enumerate(self.names):
            root = self.work_dir / name
            root.mkdir(parents=True, exist_ok=True)
            ports = self._ports(index)
            config = root / "node.toml"
            config.write_text(
                node_config(name, root, ports, seed, master_key, self.binaries["testapp"])
            )
            if seed is None:
                seed = ports[1]
            log = open(root / "bun.log", "w")
            self.processes[name] = subprocess.Popen(
                [
                    str(self.binaries["bun"]), "--config", str(config),
                    "--listen", f"127.0.0.1:{ports[0]}", "--runtime", "process", "--cluster",
                ],
                stdout=log, stderr=subprocess.STDOUT, env=self.base_env(),
                start_new_session=True,
            )
        self._wait("every node to answer", lambda: all(self._healthy(n) for n in self.names))
        self._wait("the first admin token", self._create_admin_token)
        self._wait("a leader and the full council", self._council_complete)

    def _create_admin_token(self):
        """Create the admin token, once the council can commit it.

        A fresh cluster has no tokens yet, so the first one needs none.
        """
        created = self.relish(["token", "create", "--name", "gym-admin", "--role", "admin"], token="")
        if created.returncode != 0:
            return False
        self.admin_token = created.stdout.strip().splitlines()[-1].strip()
        return True

    def _healthy(self, name):
        try:
            with urllib.request.urlopen(f"{self.api(name)}/v1/health", timeout=2) as reply:
                return reply.status == 200
        except (urllib.error.URLError, OSError):
            return False

    def council(self):
        """`relish council status -o json`, or None while it can't answer."""
        completed = self.relish(["council", "status", "-o", "json"])
        if completed.returncode != 0:
            return None
        try:
            return json.loads(completed.stdout)
        except json.JSONDecodeError:
            return None

    def _council_complete(self):
        council = self.council()
        if not council:
            return False
        voters = council_voters(council)
        expected = self.nodes if self.nodes % 2 == 1 else self.nodes - 1
        return council_leader(council) is not None and len(voters) >= expected

    def _wait(self, what, condition, timeout=START_TIMEOUT):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for name, process in self.processes.items():
                if process.poll() is not None:
                    raise ClusterError(f"bun on {name} exited while waiting for {what}")
            if condition():
                return
            time.sleep(1)
        raise ClusterError(f"timed out after {timeout}s waiting for {what}")

    def manifest(self, relative):
        """A scenario app manifest with this cluster's binary and ports in it."""
        from scenario import GYM_DIR

        text = (GYM_DIR / relative).read_text()
        for placeholder, value in self.substitutions.items():
            text = text.replace(placeholder, value)
        return text

    def write_manifest(self, relative):
        """Write a scenario manifest out for this cluster; its path."""
        path = self.work_dir / Path(relative).name
        path.write_text(self.manifest(relative))
        return path

    def apply(self, relative):
        path = self.write_manifest(relative)
        completed = self.relish(["apply", str(path)])
        if completed.returncode != 0:
            raise ClusterError(f"relish apply {relative} failed: {completed.stderr.strip()}")

    def instances(self):
        return status_instances(self.relish_json(["apps", "--instances"]))

    def wait_running(self, app):
        def running():
            ours = [i for i in self.instances() if i["app"] == app]
            return bool(ours) and all(i["state"] == "running" for i in ours)

        self._wait(f"{app} to run", running, timeout=120)

    def facts(self, needed):
        """The live values of the `@` facts a scenario's truth names."""
        found = {}
        for fact in needed:
            if fact.startswith("instance:"):
                app = fact.split(":", 1)[1]
                nodes = sorted(i["node"] for i in self.instances() if i["app"] == app)
                if not nodes:
                    raise ClusterError(f"no instance of {app} to name a node for")
                found[fact] = nodes[0]
            elif fact == "leader":
                found[fact] = council_leader(self.council())
            elif fact == "follower":
                found[fact] = self.follower()
        return found

    def follower(self):
        if not hasattr(self, "_follower"):
            leader = council_leader(self.council())
            self._follower = sorted(n for n in self.names if n != leader)[-1]
        return self._follower

    def inject(self, fault):
        """Inject a setup step's Smoker fault as admin, through the API."""
        from scenario import parse_duration

        target = self.follower()
        peers = [name for name in self.names if name != target]
        seconds = parse_duration(fault["duration"])
        body = {
            "fault_type": {"type": "CouncilPartition", "peers": peers},
            "target_service": "",
            "namespace": None,
            "target_instance": None,
            "target_node": target,
            "duration": {"secs": int(seconds), "nanos": 0},
            "injected_by": "gym",
            "reason": "agent gym scenario",
            "include_leader": False,
            "override_safety": False,
            "acknowledged": True,
        }
        request = urllib.request.Request(
            f"{self.api(target)}/v1/fault",
            data=json.dumps(body).encode(),
            headers={
                "Authorization": f"Bearer {self.admin_token}",
                "Content-Type": "application/json",
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as reply:
                return json.loads(reply.read() or b"{}")
        except urllib.error.HTTPError as error:
            raise ClusterError(
                f"the partition of {target} was refused: {error.code} {error.read().decode()}"
            ) from error

    def write_release(self, directory, kind):
        """Write the release files a scenario hands the solver; see scenario.RELEASES.

        The metadata names the version and platform the first node reports,
        so `relish agent verify` finds the release, and a SHA-256 of other
        bytes, so the node's binary doesn't match it. Returns what it wrote.
        """
        with urllib.request.urlopen(f"{self.api(self.names[0])}/v1/version", timeout=10) as reply:
            running = json.loads(reply.read())
        return write_release(directory, kind, running["version"], running["platform"])

    def create_token(self, name):
        """A read-only token for the solver, and its plaintext."""
        created = self.relish(["token", "create", "--name", name, "--role", "read-only"])
        if created.returncode != 0:
            raise ClusterError(f"couldn't create the solver's token: {created.stderr.strip()}")
        return created.stdout.strip().splitlines()[-1].strip()

    def create_agent_token(self, name):
        """A deployer agent token (#1682): it proposes and takes leases, never writes."""
        created = self.relish(["token", "create", "--name", name, "--agent", "--role", "deployer"])
        if created.returncode != 0:
            raise ClusterError(f"couldn't create the agent token {name}: {created.stderr.strip()}")
        return created.stdout.strip().splitlines()[-1].strip()

    def newest_proposal(self, author):
        """The number of the newest proposal `author` made, or None."""
        return newest_proposal(self.relish_json(["proposal", "list"]), author)

    def audit(self, principal):
        """Every audit record, on every node, whose principal is `principal`."""
        records = self.relish_json(["audit", "--principal", principal, "--cluster", "--limit", "10000"])
        return audit_records(records)

    def solver_env(self, token):
        env = self.base_env()
        env["RELIABURGER_TOKEN"] = token
        return env

    def stop(self):
        for process in self.processes.values():
            if process.poll() is None:
                process.terminate()
        deadline = time.monotonic() + 30
        for process in self.processes.values():
            remaining = max(0.1, deadline - time.monotonic())
            try:
                process.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        self._kill_workloads()

    def _kill_workloads(self):
        """Kill workload processes left behind: the process runtime's owners
        outlive Bun, and their command lines name the node's data directory."""
        listing = subprocess.run(
            ["ps", "-axo", "pid=,command="], capture_output=True, text=True, check=False
        )
        marker = str(self.work_dir)
        for line in listing.stdout.splitlines():
            pid, _, command = line.strip().partition(" ")
            if marker in command and pid.isdigit() and int(pid) != os.getpid():
                try:
                    os.kill(int(pid), signal.SIGKILL)
                except ProcessLookupError:
                    pass


def status_instances(document):
    """`relish apps --instances -o json` as [{"app", "namespace", "node", "state"}]."""
    rows = document if isinstance(document, list) else document.get("instances", [])
    instances = []
    for row in rows:
        instances.append(
            {
                "app": row.get("app") or row.get("app_name"),
                "namespace": row.get("namespace", "default"),
                "node": row.get("node") or row.get("node_id"),
                "state": str(row.get("state", "")).lower(),
            }
        )
    return instances


def council_leader(council):
    """The one leader `relish council status -o json` reports, or None."""
    leaders = ((council or {}).get("summary") or {}).get("leaders") or []
    return leaders[0] if len(leaders) == 1 else None


def council_voters(council):
    """The voters that answered `relish council status`."""
    return ((council or {}).get("summary") or {}).get("voters_answering") or []


def newest_proposal(listing, author):
    """The highest proposal id by `author` in `relish proposal list -o json`."""
    rows = listing if isinstance(listing, list) else listing.get("proposals", [])
    ids = [row["id"] for row in rows if row.get("author") == author]
    return max(ids) if ids else None


def audit_records(document):
    if isinstance(document, list):
        return document
    return document.get("records", [])


def write_release(directory, kind, version, platform):
    """`metadata.json` for a release of `version` whose bun for `platform`
    isn't the bytes the cluster runs (kind `drifted`). Returns the published
    SHA-256 and the file written."""
    if kind != "drifted":
        raise ClusterError(f"unknown release kind {kind!r}")
    published = hashlib.sha256(f"the published bun {version} for {platform}".encode()).hexdigest()
    document = {
        "schema": 1,
        "latest": version,
        "releases": [{
            "version": version,
            "platforms": {platform: {
                "url": f"https://github.com/reliaburger/reliaburger/releases/download/{version}/bun-{platform}",
                "sha256": published,
                "embedded_signature": base64.b64encode(bytes(64)).decode(),
                "external_signature": None,
            }},
        }],
    }
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / "metadata.json"
    path.write_text(json.dumps(document, indent=2) + "\n")
    return {"kind": kind, "version": version, "platform": platform, "published_sha256": published,
            "metadata": str(path)}
