"""Disposable libvirt/KVM lab: network, storage pool, three Ubuntu VMs.

Only resources named by config/lab.yaml and allowed by config/safety.yaml are
ever created or destroyed.
"""

from __future__ import annotations

import hashlib
import tempfile
import textwrap
import time
from dataclasses import dataclass
from pathlib import Path

from .core import Blocked, CmdResult, Config, RunLog, wait_until


@dataclass(frozen=True)
class Node:
    name: str
    ip: str
    mac: str

    @property
    def domain(self) -> str:
        return f"rbeval-{self.name}"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


class Lab:
    def __init__(self, cfg: Config, log: RunLog):
        self.cfg = cfg
        self.log = log
        self.uri = cfg.lab["libvirt_uri"]
        self.nodes = [Node(**n) for n in cfg.lab["nodes"]]
        self.state_dir = cfg.state_dir
        self.state_dir.mkdir(parents=True, exist_ok=True)
        self.ssh_dir = self.state_dir / "ssh"
        self.key = self.ssh_dir / "id_ed25519"
        self.user = cfg.lab["ssh_user"]
        self.touched = False  # set when a run creates or uses the VMs; gates teardown

    # ------------------------------------------------------------------ safety

    def _check_domain(self, domain: str) -> None:
        prefix = self.cfg.safety["libvirt"]["allowed_domain_prefix"]
        if not domain.startswith(prefix):
            raise RuntimeError(f"safety: refusing to act on domain {domain!r} (prefix {prefix!r})")

    def _check_network(self, name: str) -> None:
        s = self.cfg.safety["libvirt"]
        if name != s["allowed_network"] or name in s["forbidden_networks"]:
            raise RuntimeError(f"safety: refusing to act on network {name!r}")

    # ------------------------------------------------------------------ helpers

    def virsh(self, *args: str, check: bool = True, timeout: float = 120, quiet=False) -> CmdResult:
        return self.log.run(["virsh", "-c", self.uri, *args], check=check, timeout=timeout,
                            host="hypervisor", quiet=quiet)

    def node(self, name: str) -> Node:
        for n in self.nodes:
            if n.name == name or n.domain == name:
                return n
        raise KeyError(name)

    def domain_state(self, node: Node) -> str:
        r = self.virsh("domstate", node.domain, check=False, quiet=True)
        return r.stdout.strip() if r.ok else "absent"

    # ------------------------------------------------------------------ ssh

    def ssh_argv(self, node: Node, connect_timeout: int = 5) -> list[str]:
        return [
            "ssh", "-i", str(self.key),
            "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR", "-o", f"ConnectTimeout={connect_timeout}",
            "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3",
            "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none",
            f"{self.user}@{node.ip}",
        ]

    def ssh(self, node: Node | str, command: str, *, check: bool = True, timeout: float = 300,
            input: str | bytes | None = None, operator: str | None = None, quiet=False) -> CmdResult:
        if isinstance(node, str):
            node = self.node(node)
        return self.log.run(self.ssh_argv(node) + [command], check=check, timeout=timeout,
                            input=input, host=node.name, display=f"[{node.name}] {command}",
                            operator=operator, quiet=quiet)

    def scp_to(self, node: Node, local: Path, remote: str) -> CmdResult:
        argv = ["scp", "-q", "-i", str(self.key), "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR",
                "-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none",
                str(local), f"{self.user}@{node.ip}:{remote}"]
        return self.log.run(argv, check=True, timeout=600, host=node.name)

    def wait_ssh(self, node: Node, timeout: float = 300) -> float:
        def ready():
            r = self.log.run(self.ssh_argv(node, 3) + ["true"], timeout=10, quiet=True)
            return r.ok
        _, secs = wait_until(ready, timeout, 2, f"ssh on {node.name}")
        return secs

    # ------------------------------------------------------------------ assets

    def ensure_ssh_key(self) -> None:
        if self.key.exists():
            return
        self.ssh_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.log.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "rbeval-lab",
                      "-f", str(self.key)], check=True)

    def cached_image(self) -> Path:
        img = self.cfg.lab["image"]
        path = self.cfg.cache_dir / "images" / img["cache_name"]
        if not path.exists():
            path.parent.mkdir(parents=True, exist_ok=True)
            part = path.with_suffix(".part")
            self.log.run(["curl", "-fsSL", "--retry", "5", "-o", str(part), img["url"]],
                         check=True, timeout=3600)
            part.rename(path)
        digest = sha256_file(path)
        if digest != img["sha256"]:
            raise Blocked(f"image checksum mismatch for {path}: {digest} != {img['sha256']}")
        return path

    # ------------------------------------------------------------------ pool, network

    def ensure_pool(self) -> None:
        pool = self.cfg.lab["storage_pool"]
        if pool["name"] != self.cfg.safety["libvirt"]["allowed_pool"]:
            raise RuntimeError("safety: pool name not allowed")
        r = self.virsh("pool-info", pool["name"], check=False, quiet=True)
        if not r.ok:
            self.virsh("pool-define-as", pool["name"], "dir", "--target", pool["path"])
            self.virsh("pool-build", pool["name"])
        if "running" not in self.virsh("pool-info", pool["name"], quiet=True).stdout:
            self.virsh("pool-start", pool["name"])

    def base_volume(self) -> str:
        return "rbeval-base-" + self.cfg.lab["image"]["sha256"][:12] + ".qcow2"

    def ensure_base_volume(self) -> None:
        pool = self.cfg.lab["storage_pool"]["name"]
        vol = self.base_volume()
        if self.virsh("vol-info", vol, "--pool", pool, check=False, quiet=True).ok:
            return
        img = self.cached_image()
        size = img.stat().st_size
        self.virsh("vol-create-as", pool, vol, str(size), "--format", "qcow2")
        self.virsh("vol-upload", vol, str(img), "--pool", pool, timeout=1800)

    def network_xml(self) -> str:
        net = self.cfg.lab["network"]
        hosts = "\n".join(
            f"      <host mac='{n.mac}' name='{n.name}' ip='{n.ip}'/>" for n in self.nodes
        )
        return textwrap.dedent(f"""\
            <network>
              <name>{net['name']}</name>
              <forward mode='nat'/>
              <bridge name='{net['bridge']}' stp='on' delay='0'/>
              <ip address='{net['gateway']}' netmask='{net['netmask']}'>
                <dhcp>
                  <range start='10.77.0.100' end='10.77.0.200'/>
            {hosts}
                </dhcp>
              </ip>
            </network>
            """)

    def ensure_network(self) -> None:
        name = self.cfg.lab["network"]["name"]
        self._check_network(name)
        if not self.virsh("net-info", name, check=False, quiet=True).ok:
            with tempfile.NamedTemporaryFile("w", suffix=".xml", delete=False) as fh:
                fh.write(self.network_xml())
            self.virsh("net-define", fh.name)
        if "Active:         yes" not in self.virsh("net-info", name, quiet=True).stdout:
            self.virsh("net-start", name)

    # ------------------------------------------------------------------ VMs

    def cloud_init(self, node: Node) -> tuple[str, str]:
        pub = (self.key.with_suffix(".pub")).read_text().strip()
        user_data = textwrap.dedent(f"""\
            #cloud-config
            hostname: {node.name}
            manage_etc_hosts: true
            users:
              - name: {self.user}
                sudo: ALL=(ALL) NOPASSWD:ALL
                shell: /bin/bash
                ssh_authorized_keys:
                  - {pub}
            ssh_pwauth: false
            package_update: false
            package_upgrade: false
            """)
        meta_data = f"instance-id: {node.domain}-{int(time.time())}\nlocal-hostname: {node.name}\n"
        return user_data, meta_data

    def create_node(self, node: Node) -> None:
        self._check_domain(node.domain)
        pool = self.cfg.lab["storage_pool"]["name"]
        spec = self.cfg.lab["node_spec"]
        disk = f"{node.domain}.qcow2"
        seed = f"{node.domain}-seed.iso"
        for vol in (disk, seed):
            if self.virsh("vol-info", vol, "--pool", pool, check=False, quiet=True).ok:
                self.virsh("vol-delete", vol, "--pool", pool)
        self.virsh("vol-create-as", pool, disk, f"{spec['disk_gib']}G", "--format", "qcow2",
                   "--backing-vol", self.base_volume(), "--backing-vol-format", "qcow2")
        user_data, meta_data = self.cloud_init(node)
        with tempfile.TemporaryDirectory() as tmp:
            t = Path(tmp)
            (t / "user-data").write_text(user_data)
            (t / "meta-data").write_text(meta_data)
            iso = t / "seed.iso"
            self.log.run(["xorriso", "-as", "mkisofs", "-quiet", "-output", str(iso), "-volid", "cidata",
                          "-joliet", "-rock", str(t / "user-data"), str(t / "meta-data")], check=True)
            self.virsh("vol-create-as", pool, seed, str(iso.stat().st_size), "--format", "raw")
            self.virsh("vol-upload", seed, str(iso), "--pool", pool)
        self.log.run([
            "virt-install", "--connect", self.uri, "--name", node.domain,
            "--memory", str(spec["memory_mib"]), "--vcpus", str(spec["vcpus"]),
            "--cpu", "host-passthrough",
            "--disk", f"vol={pool}/{disk},bus=virtio",
            "--disk", f"vol={pool}/{seed},device=cdrom",
            "--network", f"network={self.cfg.lab['network']['name']},mac={node.mac},model=virtio",
            "--osinfo", "detect=on,require=off", "--import", "--graphics", "none",
            "--console", "pty,target_type=serial", "--noautoconsole", "--boot", "hd",
        ], check=True, timeout=300, host="hypervisor")

    def destroy_node(self, node: Node) -> None:
        self._check_domain(node.domain)
        state = self.domain_state(node)
        if state == "absent":
            return
        if state != "shut off":
            self.virsh("destroy", node.domain, check=False)
        self.virsh("undefine", node.domain, "--remove-all-storage", "--nvram", check=False)
        if self.domain_state(node) != "absent":
            self.virsh("undefine", node.domain, "--remove-all-storage")

    def up(self) -> dict:
        """Create network, pool and fresh VMs; return timings."""
        self.touched = True
        t0 = time.monotonic()
        self.ensure_ssh_key()
        self.ensure_pool()
        self.ensure_base_volume()
        self.ensure_network()
        for n in self.nodes:
            self.destroy_node(n)
            self.create_node(n)
        ssh_secs = {}
        for n in self.nodes:
            ssh_secs[n.name] = round(self.wait_ssh(n), 1)
        for n in self.nodes:
            self.ssh(n, "cloud-init status --wait >/dev/null; cloud-init status", timeout=600)
        return {"vm_boot_s": round(time.monotonic() - t0, 1), "ssh_ready_s": ssh_secs}

    def down(self) -> None:
        for n in self.nodes:
            self.destroy_node(n)
        name = self.cfg.lab["network"]["name"]
        self._check_network(name)
        if self.virsh("net-info", name, check=False, quiet=True).ok:
            self.virsh("net-destroy", name, check=False)
            self.virsh("net-undefine", name, check=False)

    def purge(self) -> None:
        """Also remove the storage pool and base image volume."""
        self.down()
        pool = self.cfg.lab["storage_pool"]["name"]
        if self.virsh("pool-info", pool, check=False, quiet=True).ok:
            vols = self.virsh("vol-list", pool, "--name", check=False, quiet=True).stdout.split()
            for v in vols:
                if v.startswith("rbeval-"):
                    self.virsh("vol-delete", v, "--pool", pool, check=False)
            self.virsh("pool-destroy", pool, check=False)
            self.virsh("pool-undefine", pool, check=False)

    # ------------------------------------------------------------------ faults

    def power_off(self, node: Node) -> CmdResult:
        """Immediate power cut (virsh destroy), not a guest shutdown."""
        self._check_domain(node.domain)
        return self.virsh("destroy", node.domain)

    def power_on(self, node: Node) -> CmdResult:
        self._check_domain(node.domain)
        return self.virsh("start", node.domain)

    def set_link(self, node: Node, up: bool) -> CmdResult:
        """Cut or restore the VM's only NIC at the hypervisor (full isolation)."""
        self._check_domain(node.domain)
        return self.virsh("domif-setlink", node.domain, node.mac, "up" if up else "down")

    def domstats(self) -> dict:
        r = self.virsh("domstats", "--cpu-total", "--balloon", *[n.domain for n in self.nodes],
                       check=False, quiet=True)
        stats: dict = {}
        current = None
        for line in r.stdout.splitlines():
            line = line.strip()
            if line.startswith("Domain:"):
                current = line.split("'")[1]
                stats[current] = {}
            elif "=" in line and current:
                k, v = line.split("=", 1)
                stats[current][k] = v
        return stats
