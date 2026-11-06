# Getting started

Reliaburger is a batteries-included container orchestrator in a single binary.
One agent (`bun`) runs on every node; one CLI (`relish`) drives it. No
add-ons to install, no YAML sprawl.

## A cluster on your laptop

The quickest way to see all of it is a three-node cluster in Linux VMs on your
laptop, running real OCI containers with runc:

```sh
relish setup --quickstart
```

Relish installs a private copy of Lima, downloads a signed Ubuntu 26.04 guest image
and signed Linux binaries, forms an mTLS cluster, deploys a sample app and
checks it answers on <http://localhost:18080/>. Rerun the same command to
resume if it stops. It needs a Mac with Apple silicon, or Linux with QEMU and
KVM, plus about 8 GiB of free memory and 15 GiB of disk. From 0.2.2 there's
no Intel Mac build: the installer refuses one and suggests a Linux host, or
0.2.1, the last release built for Intel Macs. `--nodes 1` builds a single-node
cluster instead. `curl -fsSL https://reliaburger.com/install.sh | sh -s -- --laptop`
installs the released `relish` and runs this for you. Without `--laptop` the
installer only explains the choices and installs nothing. A laptop cluster is
for development: until 1.0.0, Reliaburger is good enough for home labs and dev
clusters, not production.

On Linux machines you already have, 0.2.6 (not in 0.2.5) builds the cluster over
SSH instead:

```sh
relish setup --ssh ubuntu@10.0.0.1,ubuntu@10.0.0.2,ubuntu@10.0.0.3
```

The machines need Ubuntu or Debian, and passwordless `sudo` for that user. Every
machine is checked first, and if any fails nothing changes on any of them. Rerun
the same command to resume. Credentials stay on the machine you ran it from, so
back up the directory setup names when it finishes. `relish local` doesn't
manage these clusters. If a machine has several private addresses, give each
machine's address in the same order with `--advertise`. Afterwards `relish`
talks to port 9117 on the first machine, so a firewall in between has to let
you through; if it doesn't, setup and commands such as `relish status` print
the `ufw` rule to add, or the SSH tunnel to use instead. The
[Linux servers guide](https://github.com/reliaburger/reliaburger/blob/main/docs/linux-servers.md)
covers doing all this by hand.

There's no command to remove such a cluster yet. On each machine, stop Bun,
unmount what it left mounted for its workloads, delete its files, then
reboot: the firewall tables and eBPF programs it loaded stay in the kernel
until the machine restarts.

```sh
sudo systemctl disable --now reliaburger.service
findmnt -rn -o TARGET | grep '^/var/lib/reliaburger' | sort -r | xargs -r sudo umount
sudo rm -rf /etc/reliaburger /var/lib/reliaburger /etc/systemd/system/reliaburger.service \
  /usr/local/bin/bun /usr/local/bin/bun-* /usr/local/bin/relish
sudo reboot
```

Setup saves an admin credential for the cluster in `~/.reliaburger/context.json`
(readable only by you), so every `relish` command on this machine reaches the
cluster without flags. Then take the tour: `relish manual tour`.

Manage the laptop cluster with `relish local status`, `relish local stop`,
`relish local start` and `relish local destroy --yes`. `stop` and `start` also
take a single node, as in `relish local stop node-3`. `relish uninstall`
removes Relish itself once the cluster is gone.

## A server or a VM you already have

The guided path detects, installs and configures `bun` on this machine:

```sh
relish setup
```

It verifies `bun` against the release signatures, asks a few questions and
writes a starter `reliaburger.toml`. Pass `--yes` to accept every default.
Real containers need runc on Linux, and a cluster needs rootful runc with eBPF
(see `cluster-basics`). On macOS, use the quickstart above: it runs the same
runc path inside Linux VMs. Apple Container isn't supported in 0.2.5.

## From source, without containers

You'll need Rust 1.97 or later. Build every binary, including the `testapp`
workload the examples run:

```sh
git clone https://github.com/reliaburger/reliaburger
cd reliaburger && cargo build --locked --bins
```

The built-in process runtime supervises plain OS processes, so you don't
need a container runtime to try it. It runs host binaries the node allows,
not images. From the repository root, start Bun with the example node config,
which allows the `testapp` binary:

```sh
target/debug/bun --runtime process --config examples/node/process-runtime.toml
```

Leave it running. In a second terminal, also from the repository root:

```sh
target/debug/relish apply examples/phase-1/proc-first-run.toml
target/debug/relish status
```

You should see the `hello` workload in `Running` state. Open
<http://127.0.0.1:9117/> for the web dashboard, or run `relish` with no
arguments for the terminal one. The process runtime supervises real processes but
doesn't isolate them, so keep it for trying things out.

## Where to next

- The five-minute tour: `relish manual tour`
- Write and ship your own app: `deploy-an-app`
- Bring your Kubernetes manifests: `kubernetes`
- Form a real cluster on servers: `cluster-basics`
- Tokens, secrets and signed images: `security`
- Diagnose a broken cluster (`wtf`, `path`, `bench`): `diagnostics`
- Flags, environment variables, files and ports: `reference`
- Press `/` in this manual to search; `q` quits.
