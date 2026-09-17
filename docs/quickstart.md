# A laptop cluster

The managed quickstart is being qualified for 0.1.0. The public command below
requires the signed GitHub release and the Pages site to be published. Don't
expect it to work before then. Five minutes remains an acceptance target,
not a measured guarantee.

## Install and boot

```sh
curl -fsSL https://reliaburger.com/install.sh | bash
export PATH="$HOME/.reliaburger/bin:$PATH"
relish nodes
relish status
relish logs hello
```

The default is three Linux VMs, running real OCI containers with runc. Relish
installs Lima in your user directory, downloads a pinned Ubuntu image and
signed Linux binaries, creates the cluster's credentials, then enrols each
node. It checks the authenticated APIs, the three-member council and a sample
container through the host ingress port. Open `http://localhost:18080/` to see
the sample app. Your first application config is saved under
`~/.reliaburger/clusters/laptop/hello.toml`.

There is no Rust build and no repository checkout in the published path.
Native macOS uses Apple's Virtualization.framework through Lima. Linux needs
QEMU, KVM access and Lima's host prerequisites already installed. Allow at
least 8 GiB of available memory and 15 GiB of free disk for the default cluster;
image download time also depends on your connection. Guest package setup uses
Ubuntu's repositories. No host directories are mounted into the VMs. Managed Lima state lives under
`~/.reliaburger/lima`, isolated from your normal Lima configuration.

For a smaller single-node cluster:

```sh
curl -fsSL https://reliaburger.com/install.sh | bash -s -- --nodes 1
```

To install only the CLI, pass `--install-only`. Then start it separately:

```sh
relish setup --quickstart --nodes 3
```

Use `/install.sh`: GitHub Pages serves the same static page to browsers and
curl, so the site's root is HTML. The bootstrap downloads a complete,
version-specific installer over HTTPS before running it. That installer pins
the native CLI's SHA-256; Relish separately requires the compiled-in release
signing key for guest binaries. Guest images and Lima archives have fixed
checksums. The initial shell bootstrap trusts HTTPS.

## Resume, stop and remove

```sh
relish local status
relish local stop
relish local start
relish local destroy --yes
```

Setup saves ownership and credentials before creating VMs. If it fails or
hits its five-minute deadline, rerun the same command. It reuses verified
cached assets, the original CA and the owned VMs. A retry won't silently
change the version, topology or ports, or replace a previously running VM
that disappeared. The error tells you where the checkpoint lives.

`stop` preserves data. `destroy --yes` removes only the VMs named in this
cluster's saved record, along with its credentials and checkpoints. It keeps
the downloaded tool and image cache for future clusters. `--name NAME` selects
a different saved cluster; setup currently supports one active CLI context.

The API forwards bind loopback ports 19117–19119. The first node's HTTP ingress
uses 18080. Change these with `setup --quickstart --api-port PORT
--ingress-port PORT` if needed. Other guest ports aren't automatically exposed.
The context contains an administrator credential and is stored with mode 0600;
normal CLI commands read it without requiring a VM shell. The node APIs use
TLS and pinned cluster CAs. Explicit `--endpoint` settings don't inherit this
context's credentials.

For guest diagnostics, use the VM name printed by `relish local status`:

```sh
LIMA_HOME="$HOME/.reliaburger/lima" \
  ~/.reliaburger/tools/lima-2.1.0/bin/limactl shell VM_NAME \
  sudo journalctl -u reliaburger.service --no-pager -n 100
```

## Development qualification

Before a release exists, build Linux `bun` and `relish` with `--features ebpf`
and supply their directory explicitly:

```sh
RELIABURGER_HOME=/absolute/path/to/isolated-state \
  target/debug/relish setup --quickstart \
  --development-binaries /absolute/path/to/linux-binaries
```

This bypasses release downloads for those two operator-supplied binaries. It
still verifies the pinned upstream guest image and Lima archive. It prints a
notice and does not count as qualification of a signed, downloadable release.
Use the same `RELIABURGER_HOME` for subsequent CLI and lifecycle commands.

The outstanding release gates are tracked in the
[0.1.0 plan](plans/2026-09-16-v0.1.0-release-plan.md), including clean-host timing,
interrupted setup, real container networking, restart, recovery and cleanup.

The demo uses Docker's public ECR BusyBox repository, pinned to the same image
index digest as the test workload. It does not need a Docker Hub login. Public
registry availability and quotas still apply; setup only succeeds after the
container answers through ingress.
