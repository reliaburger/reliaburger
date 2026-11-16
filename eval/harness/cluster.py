"""Cluster lifecycle following docs/linux-servers.md, and the host-side relish client.

The procedure is the one a target user follows on pre-existing Linux VMs:
install packages, install the release binaries, `relish init` on node-01,
write node.toml and the systemd unit, mint the admin token, open the API
listener, copy the master key, mint join tokens, `relish join` on the other
nodes, start them. Each documented step is counted as an operator action.

Deviations from the document are recorded in DEVIATIONS and in the evidence.
"""

from __future__ import annotations

import json
import re
import shlex
import textwrap
import time
from typing import Any

from .core import Blocked, CmdResult, CommandFailed, Config, RunLog, now_iso, wait_until
from .lab import Lab, Node
from . import product

DEVIATIONS = [
    "The master key and join tokens are streamed node-to-node through the operator workstation "
    "(ssh node-01 ... | ssh node-0N ...) instead of from node-01 over SSH; the bytes never touch the workstation's disk, as in the docs.",
    "apt-get gets -o DPkg::Lock::Timeout=300 because a fresh cloud image runs unattended-upgrades at first boot.",
    "The [testing] section that §8.3 adds later (with a restart) is written at creation, so fault scenarios need no rolling restart.",
    "buildah (optional in §1) is installed on node-01 only.",
    "Cluster name is `rbeval`, not `prod`; addresses are the lab's 10.77.0.0/24.",
    "Operator CIDR is the hypervisor bridge address 10.77.0.1/32, where the harness runs.",
    "operator_cidrs goes in every node's config, not only Node 1's as §3.2 shows. With Node 1 alone the "
    "workstation reaches no other node's API, so relish cannot fail over and the survivors of a Node 1 "
    "failure cannot be observed from outside.",
]

PACKAGES = "runc uidmap iptables iproute2 btrfs-progs nftables curl"


def node_toml(cfg: Config, node: Node, first: Node, all_nodes: list[Node]) -> str:
    cluster = cfg.lab["cluster_name"]
    peers = ", ".join(f'"{n.ip}"' for n in all_nodes)
    join = "[]" if node == first else f'["{first.ip}:9443"]'
    bootstrap = (f'bootstrap_path = "/etc/reliaburger/{cluster}-security-bootstrap.json"\n'
                 if node == first else "")
    operator = f'operator_cidrs = ["{cfg.lab["network"]["gateway"]}/32"]\n'
    return textwrap.dedent(f"""\
        [node]
        name = "{node.name}"

        [cluster]
        name = "{cluster}"
        join = {join}

        [network]
        advertise_address = "{node.ip}"

        [security]
        require_mtls = true
        identity_dir = "/etc/reliaburger/identity"
        master_key_path = "/etc/reliaburger/{cluster}-master.key"
        """) + bootstrap + f"bootstrap_peers = [{peers}]\n" + operator + textwrap.dedent(f"""\

        [ebpf]
        enabled = true

        [dns]
        enabled = true
        listen = "{node.ip}:53"

        [ingress]
        enabled = true
        http_port = 80
        https_port = 443

        [images]
        registry_port = 5050

        [testing]
        safety_class = "development"
        allowed_operations = ["inject_workload_faults", "alter_node_state"]
        """)


def systemd_unit(listen: str) -> str:
    return textwrap.dedent(f"""\
        [Unit]
        Description=Reliaburger node
        After=network-online.target
        Wants=network-online.target

        [Service]
        Type=simple
        ExecStartPre=/bin/sh -ec 'mountpoint -q /sys/fs/bpf || mount -t bpf bpf /sys/fs/bpf'
        ExecStart=/usr/local/bin/bun --cluster --runtime runc --config /etc/reliaburger/node.toml --listen {listen}
        Restart=always
        RestartSec=2
        OOMScoreAdjust=-900
        LimitNOFILE=1048576
        KillMode=process
        TimeoutStopSec=30

        [Install]
        WantedBy=multi-user.target
        """)


class Relish:
    """Host-side relish CLI, authenticated like an operator laptop (§5)."""

    CONNECT_ERRORS = ("error sending request", "connection refused", "timed out", "operation timed out",
                      "No route to host", "tcp connect", "Connection reset")

    def __init__(self, cluster: "Cluster"):
        self.cluster = cluster
        self.log = cluster.log
        self.binary = str(product.host_relish(cluster.cfg, cluster.log))
        self.preferred: str | None = None

    def env(self, endpoint: str) -> dict:
        c = self.cluster
        return {
            "RELIABURGER_ENDPOINT": endpoint,
            "RELIABURGER_CA_CERT": str(c.ca_path),
            "RELIABURGER_TOKEN": c.token(),
            "NO_COLOR": "1",
        }

    def endpoints(self) -> list[str]:
        nodes = [n for n in self.cluster.lab.nodes if n.name not in self.cluster.down_nodes]
        eps = [f"https://{n.ip}:9117" for n in nodes]
        if self.preferred in eps:
            eps.remove(self.preferred)
            eps.insert(0, self.preferred)
        return eps

    def run(self, *args: str, output: str | None = None, endpoint: str | None = None,
            check: bool = True, timeout: float = 120, input: str | None = None,
            operator: str | None = None, failover: bool = True, quiet: bool = False) -> CmdResult:
        argv = [self.binary] + (["--output", output] if output else []) + list(args)
        candidates = [endpoint] if endpoint else self.endpoints()
        if not failover:
            candidates = candidates[:1]
        result: CmdResult | None = None
        for ep in candidates:
            result = self.log.run(argv, env=self.env(ep), timeout=timeout, input=input,
                                  display=f"relish {shlex.join(([] if not output else ['--output', output]) + list(args))}  # {ep}",
                                  operator=operator, quiet=quiet)
            if result.ok or not any(e.lower() in result.stderr.lower() for e in self.CONNECT_ERRORS):
                if result.ok:
                    self.preferred = ep
                break
        assert result is not None
        if check and not result.ok:
            raise CommandFailed(result)
        return result

    def json(self, *args: str, **kw) -> Any:
        r = self.run(*args, output="json", **kw)
        try:
            return json.loads(r.stdout)
        except json.JSONDecodeError as exc:
            raise CommandFailed(r, f"relish {' '.join(args)} --output json did not print JSON: {exc}")


class Cluster:
    def __init__(self, cfg: Config, log: RunLog, lab: Lab):
        self.cfg = cfg
        self.log = log
        self.lab = lab
        self.state_path = cfg.state_dir / "state.json"
        self.secrets = cfg.state_dir / "secrets"
        self.ca_path = self.secrets / "root-ca.crt"
        self.token_path = self.secrets / "admin.token"
        self.down_nodes: set[str] = set()
        self._relish: Relish | None = None

    # ------------------------------------------------------------------ state

    @property
    def relish(self) -> Relish:
        if self._relish is None:
            self._relish = Relish(self)
        return self._relish

    def token(self) -> str:
        tok = self.token_path.read_text().strip()
        self.log.redact.add(tok)
        return tok

    def state(self) -> dict:
        if self.state_path.exists():
            return json.loads(self.state_path.read_text())
        return {}

    def save_state(self, data: dict) -> None:
        self.state_path.parent.mkdir(parents=True, exist_ok=True)
        self.state_path.write_text(json.dumps(data, indent=2))

    def forget(self) -> None:
        for p in (self.state_path, self.token_path, self.ca_path):
            if p.exists():
                p.unlink()

    # ------------------------------------------------------------------ health

    def health(self) -> dict:
        """One snapshot of cluster health as an operator would read it."""
        h: dict[str, Any] = {"ts": now_iso()}
        try:
            nodes = self.relish.json("nodes", timeout=30, quiet=True)
            council = self.relish.json("council", timeout=30, quiet=True)
        except (CommandFailed, FileNotFoundError) as exc:
            h["ok"] = False
            h["error"] = str(exc)[:500]
            return h
        h["nodes"] = {n["node_id"]: n["state"] for n in nodes}
        s = council.get("summary", {})
        h["leaders"] = s.get("leaders", [])
        h["voters"] = s.get("voters", [])
        h["voters_answering"] = s.get("voters_answering", [])
        h["unanswered"] = s.get("unanswered", [])
        h["fenced"] = s.get("fenced", [])
        expected = {n.name for n in self.lab.nodes}
        h["ok"] = (
            {k for k, v in h["nodes"].items() if v == "alive"} >= expected
            and len(h["leaders"]) == 1
            and set(h["voters_answering"]) >= expected
        )
        return h

    def wait_healthy(self, timeout: float) -> tuple[dict, float]:
        last: dict = {}

        def check():
            nonlocal last
            last = self.health()
            return last if last.get("ok") else None

        try:
            return wait_until(check, timeout, 2, "cluster healthy")
        except TimeoutError as exc:
            raise TimeoutError(f"{exc}; last health: {json.dumps(last)[:800]}")

    def is_reusable(self) -> bool:
        st = self.state()
        if st.get("product_key") != self.cfg.product_key or not self.token_path.exists():
            return False
        if any(self.lab.domain_state(n) != "running" for n in self.lab.nodes):
            return False
        return bool(self.health().get("ok"))

    # ------------------------------------------------------------------ provisioning

    def _op(self, node: Node | str, command: str, step: str, **kw) -> CmdResult:
        return self.lab.ssh(node, command, operator=step, **kw)

    def install_node(self, node: Node, with_buildah: bool) -> None:
        p = self.cfg.product
        pkgs = PACKAGES + (" buildah" if with_buildah else "")
        apt = "sudo DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=300"
        self._op(node, f"{apt} update -qq && {apt} install -y -qq {pkgs}",
                 f"§1 install required packages on {node.name}", timeout=900, check=True)
        self._op(node, "mountpoint -q /sys/fs/bpf || sudo mount -t bpf bpf /sys/fs/bpf",
                 f"§1 mount bpffs on {node.name}", check=True)
        ver = p["release_tag"] if p["channel"] == "stable" else f"v{p['version']}"
        script = textwrap.dedent(f"""\
            set -e
            ARCH=x86_64
            VERSION={shlex.quote(ver)}
            BASE_URL={shlex.quote(p['base_url'])}
            curl -fsSL -o /tmp/bun-linux-${{ARCH}} "${{BASE_URL}}/bun-linux-${{ARCH}}"
            curl -fsSL -o /tmp/relish-linux-${{ARCH}} "${{BASE_URL}}/relish-linux-${{ARCH}}"
            curl -fsSL -o /tmp/SHA256SUMS "${{BASE_URL}}/SHA256SUMS"
            (cd /tmp && sha256sum --check --ignore-missing SHA256SUMS)
            sudo install -m 0755 /tmp/bun-linux-${{ARCH}} /usr/local/bin/bun-${{VERSION}}
            sudo ln -sfn bun-${{VERSION}} /usr/local/bin/bun
            sudo install -m 0755 /tmp/relish-linux-${{ARCH}} /usr/local/bin/relish
            rm -f /tmp/bun-linux-${{ARCH}} /tmp/relish-linux-${{ARCH}} /tmp/SHA256SUMS
            bun --version
            relish --version
            sha256sum "$(readlink -f /usr/local/bin/bun)" /usr/local/bin/relish
            """)
        r = self._op(node, f"bash -c {shlex.quote(script)}",
                     f"§2.1 download, verify and install bun and relish on {node.name}", timeout=900, check=True)
        if self.cfg.product["sha256"]["bun-linux-x86_64"] not in r.stdout:
            raise Blocked(f"installed bun on {node.name} does not match the pinned digest")
        self._op(node, "sudo install -d -m 0700 /etc/reliaburger /etc/reliaburger/identity && "
                       "sudo install -d -m 0755 /var/lib/reliaburger",
                 f"§2.2 create config and state directories on {node.name}", check=True)

    def write_file(self, node: Node, path: str, content: str, step: str, mode: str = "0644") -> None:
        self._op(node, f"sudo install -m {mode} /dev/stdin {shlex.quote(path)}", step,
                 input=content, check=True)

    def provision(self) -> dict:
        """Run the documented procedure on fresh VMs. Returns timings and evidence."""
        cfg = self.cfg
        nodes = self.lab.nodes
        first, joiners = nodes[0], nodes[1:]
        cluster = cfg.lab["cluster_name"]
        timings: dict[str, Any] = {}
        t0 = time.monotonic()
        self.forget()
        product.fetch(cfg, self.log)

        # §1-2 on every node
        for i, n in enumerate(nodes):
            self.install_node(n, with_buildah=(i == 0))
        timings["install_s"] = round(time.monotonic() - t0, 1)

        # §3.1 relish init
        r = self._op(first, f"sudo relish init /etc/reliaburger --cluster-name {cluster} --node-id {first.name}",
                     "§3.1 relish init on node-01", check=True)
        m = re.search(r"Root CA:\s+(sha256:[0-9a-f]{64})", r.stdout + r.stderr)
        if not m:
            raise Blocked("relish init printed no sha256 root CA fingerprint")
        fingerprint = m.group(1)

        # §3.2-3.3 node.toml, unit on loopback, start
        self.write_file(first, "/etc/reliaburger/node.toml", node_toml(cfg, first, first, nodes),
                        "§3.2 write node-01 node.toml")
        self.write_file(first, "/etc/systemd/system/reliaburger.service", systemd_unit("127.0.0.1:9117"),
                        "§3.3 write node-01 systemd unit")
        self._op(first, "sudo systemctl daemon-reload && sudo systemctl enable --now reliaburger.service",
                 "§3.3 enable and start bun on node-01", check=True)

        # §3.4 mint admin token (as the unprivileged user, from the node)
        env = 'export RELIABURGER_CA_CERT="$HOME/.reliaburger/root-ca.crt" RELIABURGER_ENDPOINT="https://127.0.0.1:9117"'
        self._op(first, 'install -d -m 0700 ~/.reliaburger && sudo install -m 0644 -o "$(id -u)" -g "$(id -g)" '
                        '/etc/reliaburger/identity/root-ca.crt ~/.reliaburger/root-ca.crt',
                 "§3.4 copy root CA certificate for the CLI", check=True)

        def mint():
            r = self.lab.ssh(first, f'{env}; relish token create --name admin --role admin '
                                    '| install -m 0600 /dev/stdin ~/.reliaburger/admin.token && test -s ~/.reliaburger/admin.token',
                             check=False)
            return r.ok
        _, secs = wait_until(mint, 60, 2, "admin token")
        self.log.note_operator_action("§3.4 mint the admin token")
        timings["token_after_start_s"] = round(secs, 1)

        # Bring CA and token to the operator workstation (§5.2)
        self.secrets.mkdir(parents=True, exist_ok=True, mode=0o700)
        ca = self.lab.ssh(first, "cat ~/.reliaburger/root-ca.crt", check=True).stdout
        self.ca_path.write_text(ca)
        tok = self.lab.ssh(first, "cat ~/.reliaburger/admin.token", check=True, quiet=True).stdout.strip()
        self.log.redact.add(tok)
        self.token_path.touch(mode=0o600)
        self.token_path.write_text(tok + "\n")
        self.log.note_operator_action("§5.2 copy root CA and admin token to the workstation")

        # §3.5 open the API listener
        self.write_file(first, "/etc/systemd/system/reliaburger.service", systemd_unit("0.0.0.0:9117"),
                        "§3.5 change --listen to 0.0.0.0:9117")
        self._op(first, "sudo systemctl daemon-reload && sudo systemctl restart reliaburger.service",
                 "§3.5 reload and restart bun on node-01", check=True)
        wait_until(lambda: self.relish.run("status", endpoint=f"https://{first.ip}:9117",
                                           check=False, quiet=True).ok, 90, 2, "node-01 API from workstation")

        # §4 joiners
        for n in joiners:
            self.log.run(["bash", "-c",
                          shlex.join(self.lab.ssh_argv(first) + ["sudo cat /etc/reliaburger/" + cluster + "-master.key"])
                          + " | " + shlex.join(self.lab.ssh_argv(n) + [
                              f"sudo install -m 0600 -o root -g root /dev/stdin /etc/reliaburger/{cluster}-master.key"])],
                         check=True, display=f"[{first.name}->{n.name}] stream master key", operator=f"§4.1 copy master key to {n.name}")
            self.log.run(["bash", "-c",
                          shlex.join(self.lab.ssh_argv(first) + [f"{env}; export RELIABURGER_TOKEN=$(cat ~/.reliaburger/admin.token); "
                                                                f"relish join-token create --node-id {n.name} --ttl 15m"])
                          + " | " + shlex.join(self.lab.ssh_argv(n) + [f"install -m 0600 /dev/stdin ~/{n.name}.join"])],
                         check=True, display=f"[{first.name}->{n.name}] mint and stream join token",
                         operator=f"§4.2 create single-use join token for {n.name}")
            self._op(n, f"sudo relish join --token-file ~/{n.name}.join --node-id {n.name} "
                        f"--identity-dir /etc/reliaburger/identity --ca-fingerprint {shlex.quote(fingerprint)} "
                        f"https://{first.ip}:9117 && rm ~/{n.name}.join",
                     f"§4.3 relish join on {n.name}", check=True)
            self.write_file(n, "/etc/reliaburger/node.toml", node_toml(cfg, n, first, nodes),
                            f"§4.3 write {n.name} node.toml")
            self.write_file(n, "/etc/systemd/system/reliaburger.service", systemd_unit("0.0.0.0:9117"),
                            f"§4.3 write {n.name} systemd unit")
        last_start = time.monotonic()
        for n in joiners:
            self._op(n, "sudo systemctl daemon-reload && sudo systemctl enable --now reliaburger.service",
                     f"§4.3 enable and start bun on {n.name}", check=True)
        timings["procedure_s"] = round(time.monotonic() - t0, 1)

        # §6 verify from the workstation
        health, secs = self.wait_healthy(cfg.threshold("bootstrap.cluster_formation_s") + 60)
        timings["formation_after_last_start_s"] = round(time.monotonic() - last_start, 1)
        for c in ("nodes", "council", "status"):
            self.relish.run(c, operator=f"§6 relish {c}")
        timings["total_s"] = round(time.monotonic() - t0, 1)

        state = {
            "product_key": cfg.product_key,
            "product_commit": cfg.product["commit"],
            "created": now_iso(),
            "ca_fingerprint": fingerprint,
            "timings": timings,
            "health": health,
        }
        self.save_state(state)
        return state

    def node_versions(self) -> dict:
        out = {}
        for n in self.lab.nodes:
            r = self.lab.ssh(n, "bun --version; uname -r; nproc; free -m | awk '/Mem:/{print $2}'", check=False, quiet=True)
            out[n.name] = r.stdout.strip().splitlines() if r.ok else ["unreachable"]
        return out
