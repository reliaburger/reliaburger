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

Relish installs a private copy of Lima, downloads a signed Ubuntu guest image
and signed Linux binaries, forms an mTLS cluster, deploys a sample app and
checks it answers on <http://localhost:18080/>. Rerun the same command to
resume if it stops. It needs macOS, or Linux with QEMU and KVM, plus about
8 GiB of free memory and 15 GiB of disk. `--nodes 1` builds a single-node
cluster instead. From 0.1.0, `curl -fsSL https://reliaburger.com/install.sh | sh`
installs `relish` and runs this for you.

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
runc path inside Linux VMs. Apple Container isn't supported in 0.1.0.

## From source, without containers

You'll need Rust 1.97 or later. Build every binary, including the `testapp`
workload the examples run:

```sh
git clone https://github.com/reliaburger/reliaburger
cd reliaburger && cargo build --locked --bins
```

The built-in ProcessGrill runtime supervises plain OS processes, so you don't
need a container runtime to try it. From the repository root:

```sh
target/debug/bun --runtime process
```

Leave it running. In a second terminal, also from the repository root:

```sh
target/debug/relish apply examples/phase-1/proc-first-run.toml
target/debug/relish status
```

You should see the `hello` workload in `Running` state. Open
<http://127.0.0.1:9117/> for the web dashboard, or run `relish` with no
arguments for the terminal one. ProcessGrill supervises real processes but
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
