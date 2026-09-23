# A laptop cluster

The managed quickstart is being qualified for 0.1.0. The public command below
requires the signed GitHub release and the Pages site to be published. Don't
expect it to work before then. Five minutes remains an acceptance target,
not a measured guarantee.

## Install and boot

```sh
curl -fsSL https://reliaburger.com/install.sh | sh
relish nodes
relish status
relish logs hello
relish dashboard             # Ctrl-C stops the browser connection
```

The installer keeps `relish` in `~/.reliaburger/bin`. If `~/.local/bin` is
already on your `PATH`, it links `~/.local/bin/relish` there and you're done.
Otherwise it prints the one line to add for your shell (`~/.zshrc`,
`~/.bash_profile` on macOS, `~/.bashrc` on Linux, `fish_add_path` for fish)
and asks, on the terminal, whether to add it for you. It never edits a file
without a yes. Pass `--no-modify-path` (`sh -s -- --no-modify-path`) or set
`RELIABURGER_NO_MODIFY_PATH=1` to keep everything inside `~/.reliaburger`.
Until you open a new terminal, setup prints next steps with the full path.

The default is three Linux VMs, running real OCI containers with runc. Relish
installs Lima in your user directory, downloads a pinned Ubuntu image and
signed Linux binaries, creates the cluster's credentials, then enrols each
node. It checks the authenticated APIs, the three-member council and a sample
container through the host ingress port. Open `http://localhost:18080/` to see
the sample app. Your first application config is saved under
`~/.reliaburger/clusters/laptop/hello.toml`.

`relish dashboard` opens a temporary, read-only browser session on a loopback
port. Relish keeps the cluster credentials and CA verification on the CLI side;
you don't need to install the cluster CA in your browser or copy a token.
Ctrl-C stops the browser connection, leaving the cluster running. Use
`--no-open` to print the browser link, or `--port PORT` to select a local port.

There is no Rust build and no repository checkout in the published path.
Native macOS uses Apple's Virtualization.framework through Lima. Linux needs
QEMU, KVM access and Lima's host prerequisites already installed. Allow at
least 8 GiB of available memory and 15 GiB of free disk for the default cluster;
image download time also depends on your connection. Guest package setup uses
Ubuntu's repositories. No host directories are mounted into the VMs. Managed Lima state lives under
`~/.reliaburger/lima`, isolated from your normal Lima configuration.

For a smaller single-node cluster:

```sh
curl -fsSL https://reliaburger.com/install.sh | sh -s -- --nodes 1
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
checksums. The initial shell bootstrap trusts HTTPS. Both scripts are plain POSIX sh,
so any `sh` runs them; pass installer options after `sh -s --`.

## Resume, stop and remove

```sh
relish local status
relish local stop
relish local start
relish local destroy --yes
```

`stop` and `start` also take one node, by the name `relish nodes` shows, its
number or `node-N`, so losing a machine is one command:

```sh
relish local stop node-3     # the other two keep the council's quorum
relish local start node-3    # boots it and waits for its API
```

Two stops ask for `--yes` first. Node 1 carries every host forward (the CLI
endpoint, the ingress on `localhost:18080` and the registry), so while it's
down the cluster keeps running but `relish` can't reach it. And stopping a
node that would leave fewer than two of three running costs the council its
quorum. Both are fair experiments; neither should happen by accident.

Setup shows a line per step as it goes: each download with its size and
speed, each VM boot, and each node's install, enrolment and start. It ends with
a short summary of where the time went. Add `--timings` to also print every
step's duration. Each run, successful or not, saves the same data as JSON in
`~/.reliaburger/clusters/laptop/timings.json`.

Setup saves ownership and credentials before creating VMs. Building the
cluster, from the first VM boot to the demo app, has a five-minute deadline.
Downloads don't count towards it, because their speed is your network's: a
download fails only if no data arrives for 30 seconds (or after 30 minutes in
total), and an interrupted download resumes from where it stopped. If setup
fails or runs out of time, rerun the same command. It reuses verified cached
assets, the original CA and the owned VMs. A retry won't silently
change the version, topology or ports, or replace a previously running VM
that disappeared. The error tells you where the checkpoint lives.

`status` checks every owned VM and its authenticated API. Exit 0 means every
node is running and its critical subsystems are ready. Missing or stopped VMs,
unresponsive APIs and unavailable evidence return exit 1 after printing the
observations. A saved provisioning checkpoint is not a live health check.

`stop` preserves data. `destroy --yes` removes only the VMs named in this
cluster's saved record, along with its credentials and checkpoints. It keeps
the downloaded tool and image cache for future clusters. `--name NAME` selects
a different saved cluster; setup currently supports one active CLI context.

To remove Reliaburger from the laptop afterwards, destroy each cluster, then:

```sh
relish uninstall            # asks first; --yes skips the question
```

It removes the CLI, its `~/.local/bin` link, the private Lima tools, the
download cache and the managed Lima home. It refuses while a quickstart cluster
or managed VM still exists and names the `relish local destroy` command to run.
Anything else under `~/.reliaburger` stays, and it lists what it kept. It
doesn't edit your shell's rc file; if you added the `PATH` line, it reminds
you to remove it.

The API forwards bind loopback ports 19117–19119. The first node's HTTP ingress
uses 18080, and its authenticated HTTPS Pickle registry uses 15050. Change these
with `setup --quickstart --api-port PORT --ingress-port PORT --registry-port PORT`
if needed. Other guest ports aren't automatically exposed. The saved context
records the explicit host forwards so catalogue tests don't guess guest ports.
Older development clusters without a registry forward can still be stopped or
destroyed; recreate them to use the new setup ports.
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

## Signed candidate qualification

A release candidate can use an explicit HTTPS mirror of its unchanged assets:

```sh
relish setup --quickstart --release-mirror https://YOUR_HOST/candidate
```

This keeps guest-image checksums and embedded binary signatures enabled. It
cannot be combined with development binaries. Repeat the mirror option when
resuming. The [release guide](releasing.md#qualifying-a-staged-candidate) covers
candidate verification and the matching installer environment variable.

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
