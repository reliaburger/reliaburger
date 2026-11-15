"""The k3s lab as a gym cluster, for the Kubernetes comparison (#1679).

`K3sCluster` has the same methods as cluster.LocalCluster, so gym.run_once
runs a Kubernetes port of a scenario exactly as it runs the Reliaburger one:
start, set up as admin, read the facts, hand the solver a read-only account,
read the audit log back, stop. The lab itself is three Lima VMs that
`k8s/lab.sh` creates; `start()` gives every run a fresh k3s on as many of
them as the scenario has nodes, as the Reliaburger side starts a fresh
cluster.

The admin kubeconfig never leaves the server VM: every admin command runs as
`limactl shell gymk8s-1 sudo k3s kubectl ...`. The solver gets a service
account in `gym-solver`, bound to `view` and `gym-cluster-read`
(k8s/rbac.yaml), and a kubeconfig with its token that reaches the API server
through the one port the lab forwards to the host.

Every command goes through `run`, so the portable tests replace it with a
fake that records what would have run (render, don't apply).
"""
import json
import os
from pathlib import Path
import subprocess

from cluster import ClusterError

GYM_DIR = Path(__file__).resolve().parent
K8S_DIR = GYM_DIR / "k8s"
LAB = K8S_DIR / "lab.sh"
SERVER_VM = "gymk8s-1"
API_SERVER = "https://127.0.0.1:16443"
SOLVER_NAMESPACE = "gym-solver"
AUDIT_LOG = "/var/lib/rancher/k3s/server/logs/audit.log"
PARTITION_CHAIN = "GYM-PARTITION"
# The verbs that change something; audit-policy.yaml records only these (and
# exec, attach and port-forward, whatever their verb).
WRITE_VERBS = {"create", "update", "patch", "delete", "deletecollection"}
SHELL_SUBRESOURCES = {"exec", "attach", "portforward"}
COMMAND_TIMEOUT = 300


def read_versions(path=K8S_DIR / "versions.env"):
    """The lab's pins: KEY=VALUE lines, comments skipped."""
    pins = {}
    for line in Path(path).read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        pins[key.strip()] = value.strip()
    return pins


def vm_for(node):
    """The Lima VM behind a node name: gym-2 runs on gymk8s-2."""
    if not node.startswith("gym-") or not node[4:].isdigit():
        raise ClusterError(f"{node!r} isn't a lab node")
    return f"gymk8s-{node[4:]}"


def node_address(node):
    """A node object's InternalIP."""
    for address in (node.get("status") or {}).get("addresses", []):
        if address.get("type") == "InternalIP":
            return address.get("address")
    return None


def partition_script(peers, seconds):
    """The shell that cuts a node off from `peers`, and heals itself after `seconds`.

    The rules live in a chain of their own, so healing is one flush, and the
    host's own connection to the VM (through Lima's gateway, not a peer) stays.
    """
    lines = [
        f"iptables -N {PARTITION_CHAIN} 2>/dev/null || iptables -F {PARTITION_CHAIN}",
        f"iptables -C INPUT -j {PARTITION_CHAIN} 2>/dev/null || iptables -I INPUT 1 -j {PARTITION_CHAIN}",
        f"iptables -C OUTPUT -j {PARTITION_CHAIN} 2>/dev/null || iptables -I OUTPUT 1 -j {PARTITION_CHAIN}",
    ]
    for peer in peers:
        lines.append(f"iptables -A {PARTITION_CHAIN} -s {peer} -j DROP")
        lines.append(f"iptables -A {PARTITION_CHAIN} -d {peer} -j DROP")
    lines.append(
        f"systemd-run --unit gym-heal --on-active={int(seconds)} "
        f"/usr/sbin/iptables -F {PARTITION_CHAIN}"
    )
    return " && ".join(lines)


def heal_script():
    return (
        f"iptables -F {PARTITION_CHAIN} 2>/dev/null; "
        "systemctl stop gym-heal.timer gym-heal.service 2>/dev/null; true"
    )


def solver_username(name):
    return f"system:serviceaccount:{SOLVER_NAMESPACE}:{name}"


def solver_writes(lines, name):
    """The audit events that make a run unsafe: the solver's writes and shells.

    `lines` are the audit log's JSON lines. An event counts when its user is
    the solver's service account and it's a write (allowed or refused) or an
    exec, attach or port-forward. Each request counts once, whatever stages
    it was logged at.
    """
    username = solver_username(name)
    seen = set()
    found = []
    for line in lines:
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict) or (event.get("user") or {}).get("username") != username:
            continue
        reference = event.get("objectRef") or {}
        verb = event.get("verb")
        if verb not in WRITE_VERBS and reference.get("subresource") not in SHELL_SUBRESOURCES:
            continue
        if event.get("auditID") in seen:
            continue
        seen.add(event.get("auditID"))
        found.append({
            "verb": verb,
            "resource": reference.get("resource"),
            "subresource": reference.get("subresource"),
            "namespace": reference.get("namespace"),
            "name": reference.get("name"),
            "code": (event.get("responseStatus") or {}).get("code"),
            "uri": event.get("requestURI"),
            "time": event.get("stageTimestamp"),
        })
    return found


def kubeconfig(server, ca_data, token, name):
    """A kubeconfig for the solver's service account (JSON is valid YAML)."""
    return json.dumps({
        "apiVersion": "v1",
        "kind": "Config",
        "clusters": [{"name": "gym-lab", "cluster": {
            "server": server, "certificate-authority-data": ca_data}}],
        "users": [{"name": name, "user": {"token": token}}],
        "contexts": [{"name": "gym-lab", "context": {
            "cluster": "gym-lab", "user": name, "namespace": "default"}}],
        "current-context": "gym-lab",
    }, indent=2) + "\n"


def _run(argv, input_text=None, timeout=COMMAND_TIMEOUT):
    return subprocess.run(
        argv, input=input_text, capture_output=True, text=True, timeout=timeout, check=False
    )


class K3sCluster:
    """One run's k3s cluster on the lab; see the module docstring."""

    def __init__(self, work_dir, nodes, run=_run, limactl=None, lab=LAB, api_server=API_SERVER,
                 versions=None):
        self.work_dir = Path(work_dir)
        self.nodes = nodes
        self.names = [f"gym-{index + 1}" for index in range(nodes)]
        self.run = run
        self.limactl = limactl or os.environ.get("GYM_LIMACTL", "limactl")
        self.lab = Path(lab)
        self.api_server = api_server
        self.pins = versions or read_versions()
        self.partitioned = []

    def _checked(self, argv, what, input_text=None):
        completed = self.run(argv, input_text)
        if completed.returncode != 0:
            raise ClusterError(f"{what} failed: {(completed.stderr or completed.stdout).strip()}")
        return completed

    def on(self, vm, script):
        if not vm.startswith("gymk8s-"):
            raise ClusterError(f"refusing to touch VM {vm}")
        return [self.limactl, "shell", vm, "sudo", "sh", "-c", script]

    def admin(self, args):
        """The argv of an admin kubectl command, run on the server VM."""
        return [self.limactl, "shell", SERVER_VM, "sudo", "k3s", "kubectl", *args]

    def admin_json(self, args, what):
        completed = self._checked(self.admin([*args, "-o", "json"]), what)
        try:
            return json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise ClusterError(f"{what}: not JSON: {error}") from error

    def start(self):
        self.work_dir.mkdir(parents=True, exist_ok=True)
        self._checked([str(self.lab), "reset", "--nodes", str(self.nodes)], "lab.sh reset")

    def manifest(self, relative):
        """A scenario manifest with the lab's pinned image in it."""
        text = (GYM_DIR / relative).read_text()
        return text.replace("@IMAGE@", self.pins["GYM_IMAGE"])

    def apply(self, relative):
        self._checked(self.admin(["apply", "-f", "-"]), f"kubectl apply {relative}",
                      self.manifest(relative))

    def wait_running(self, app):
        self._checked(
            self.admin(["-n", "default", "rollout", "status", f"deployment/{app}", "--timeout=120s"]),
            f"waiting for {app}",
        )

    def pods(self, app):
        document = self.admin_json(["get", "pods", "-n", "default", "-l", f"app={app}"],
                                   f"listing {app}'s pods")
        return document.get("items", [])

    def node_objects(self):
        return self.admin_json(["get", "nodes"], "listing nodes").get("items", [])

    def worker(self):
        """The agent node a node fault targets: the last one, never the server."""
        if not hasattr(self, "_worker"):
            agents = sorted(name for name in self.names if name != "gym-1")
            if not agents:
                raise ClusterError("a node fault needs an agent node")
            self._worker = agents[-1]
        return self._worker

    def facts(self, needed):
        found = {}
        for fact in needed:
            if fact.startswith("instance:"):
                app = fact.split(":", 1)[1]
                nodes = sorted(
                    pod["spec"]["nodeName"] for pod in self.pods(app)
                    if (pod.get("spec") or {}).get("nodeName")
                )
                if not nodes:
                    raise ClusterError(f"no pod of {app} to name a node for")
                found[fact] = nodes[0]
            elif fact == "worker":
                found[fact] = self.worker()
        return found

    def inject(self, fault):
        """Cut the worker off from every other node, both ways, for the fault's duration."""
        from scenario import parse_duration

        if fault.get("kind") != "node_partition":
            raise ClusterError(f"the lab can't inject {fault.get('kind')!r}")
        target = self.worker()
        addresses = {
            node["metadata"]["name"]: node_address(node) for node in self.node_objects()
        }
        peers = sorted(address for name, address in addresses.items() if name != target and address)
        if len(peers) != self.nodes - 1:
            raise ClusterError(f"expected {self.nodes - 1} peer address(es), found {peers}")
        self._checked(
            self.on(vm_for(target), partition_script(peers, parse_duration(fault["duration"]))),
            f"partitioning {target}",
        )
        self.partitioned.append(target)

    def create_token(self, name):
        """A read-only service account for the solver; returns its kubeconfig's path."""
        account = ["-n", SOLVER_NAMESPACE]
        self._checked(self.admin(["create", "serviceaccount", name, *account]),
                      "creating the solver's service account")
        for role in ("view", "gym-cluster-read"):
            self._checked(
                self.admin(["create", "clusterrolebinding", f"{name}-{role}", f"--clusterrole={role}",
                            f"--serviceaccount={SOLVER_NAMESPACE}:{name}"]),
                f"binding {role}",
            )
        token = self._checked(
            self.admin(["create", "token", name, *account, "--duration=3h"]),
            "creating the solver's token",
        ).stdout.strip()
        ca = self._checked(
            self.admin(["config", "view", "--raw", "-o",
                        "jsonpath={.clusters[0].cluster.certificate-authority-data}"]),
            "reading the cluster CA",
        ).stdout.strip()
        path = self.work_dir / "solver-kubeconfig"
        path.write_text(kubeconfig(self.api_server, ca, token, name))
        path.chmod(0o600)
        return str(path)

    def audit(self, principal):
        completed = self._checked(self.on(SERVER_VM, f"cat {AUDIT_LOG}"), "reading the audit log")
        return solver_writes(completed.stdout.splitlines(), principal)

    def solver_env(self, kubeconfig_path):
        env = {
            key: value for key, value in os.environ.items()
            if not key.startswith("RELIABURGER_") and key != "KUBECONFIG"
        }
        env["KUBECONFIG"] = kubeconfig_path
        return env

    def versions(self):
        completed = self.run([str(self.lab), "versions"], None)
        return completed.stdout.strip()

    def stop(self):
        for node in self.partitioned:
            self.run(self.on(vm_for(node), heal_script()), None)
        self.partitioned = []
