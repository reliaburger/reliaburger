# A multi-node cluster on Linux VMs

This guide walks through deploying Reliaburger across three pre-existing Linux
virtual machines (or physical servers) running rootful `runc` and eBPF, forming
a three-node Raft council with mutual TLS (mTLS), and running the demo container
workload.

Often the VMs already exist, and you want to use the configuration and
runtimes you've already got on them.

If you want an automated, disposable local cluster on macOS or Linux using
managed Lima VMs instead, see the [quickstart guide](quickstart.md).

---

## 1. Prerequisites and network requirements

### Host requirements (each VM)

Every node must meet the following minimum specification:

- **OS / Architecture**: Ubuntu 24.04+ or Debian 12+, on x86_64 or aarch64.
  Other distributions may work, but we haven't tested them.
- **Kernel**: Linux 5.8 or later with cgroup v2 enabled.
- **Privileges**: Root or `sudo` access on all three nodes.
- **BPF filesystem**: `bpffs` mounted at `/sys/fs/bpf`.
- **Resources**: At least 2 CPU cores, 2 GiB RAM, and 10 GiB available disk space per node.
- **Required packages**: `runc`, `uidmap`, `iptables`, `iproute2`, `nftables`, `btrfs-progs`, and `curl`.

Install the required packages on all three nodes:

```sh
sudo apt-get update
sudo apt-get install -y runc uidmap iptables iproute2 btrfs-progs nftables curl
```

Ensure the BPF virtual filesystem is mounted:

```sh
mountpoint -q /sys/fs/bpf || sudo mount -t bpf bpf /sys/fs/bpf
```

### Network and firewall matrix

Assign hostnames or static IP addresses to your three VMs. For this guide, we use:

| Node ID | Role | Example IP |
|---------|------|------------|
| `node-01` | Bootstrap node, Council voter | `192.168.0.101` |
| `node-02` | Joining node, Council voter | `192.168.0.102` |
| `node-03` | Joining node, Council voter | `192.168.0.103` |

Ensure the following ports are open:

| Port | Protocol | Purpose | Direction |
|------|----------|---------|-----------|
| `9117` | TCP | Bun API and web dashboard (Brioche) | Node-to-node, and operators (see §5) |
| `9443` | UDP | SWIM gossip (Mustard) | Node-to-node |
| `9444` | TCP | Raft consensus (Council) | Node-to-node |
| `9445` | TCP | Reporting tree (state reports to the council) | Node-to-node |
| `5050` | TCP | Pickle OCI image registry | Node-to-node |
| `10000`-`60000` | TCP | Container host ports (`[network] port_range`); ingress reaches replicas on other nodes through them | Node-to-node |
| `53` | UDP and TCP | Service discovery DNS (`.internal`) | Workloads to their own node |
| `80`, `443` | TCP | Ingress HTTP/HTTPS proxy (Wrapper) | External |

Bun manages its own nftables perimeter on top of that: once it's running, only
cluster members (and the `bootstrap_peers` you list in `node.toml`) can reach
the API, cluster and container host ports. Your laptop gets in to the API port,
and only that port, through `operator_cidrs` (see §5.3). Your host firewall
still has to let that traffic in. With UFW, run this on every node:

```sh
for peer in 192.168.0.101 192.168.0.102 192.168.0.103; do
  sudo ufw allow proto udp from "$peer" to any port 9443
  sudo ufw allow proto tcp from "$peer" to any port 9117,9444,9445,5050,10000:60000
done
sudo ufw allow 53/udp
sudo ufw allow 53/tcp
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
```

There's one more trap. Docker, and UFW when it's enabled, set the `FORWARD`
chain's policy to `DROP`. Bun enables IPv4 forwarding but doesn't override
another firewall's rules, so a node can reach its containers while
container-to-container traffic is still dropped. The
[runc notes](README.md#runc-linux) cover what to check.

---

## 2. Install binaries and prepare directories

Perform these steps on **all three nodes**:

### 2.1 Download and install the latest release from GitHub

Download the pre-built `bun` (node agent) and `relish` (CLI) binaries for your system architecture (`x86_64` or `aarch64`) from the GitHub repository release page, and verify the downloads against `SHA256SUMS`:

```sh
# Detect host architecture
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64)   ARCH="x86_64" ;;
  aarch64|arm64)  ARCH="aarch64" ;;
  *) echo "Unsupported architecture: $ARCH; the release has x86_64 and aarch64 builds" >&2 ;;
esac

# Release version to install (e.g. v0.1.1 or vX.Y.Z)
VERSION="v0.1.1"
BASE_URL="https://github.com/reliaburger/reliaburger/releases/download/${VERSION}"

# Download binaries and SHA256SUMS into /tmp
curl -fsSL -o /tmp/bun-linux-${ARCH} "${BASE_URL}/bun-linux-${ARCH}"
curl -fsSL -o /tmp/relish-linux-${ARCH} "${BASE_URL}/relish-linux-${ARCH}"
curl -fsSL -o /tmp/SHA256SUMS "${BASE_URL}/SHA256SUMS"

# Verify download integrity against the release checksums
(cd /tmp && sha256sum --check --ignore-missing SHA256SUMS)

# Install bun as a versioned binary behind a `bun` symlink, the layout
# self-upgrade and rollback expect (see §9)
sudo install -m 0755 /tmp/bun-linux-${ARCH} /usr/local/bin/bun-${VERSION}
sudo ln -sfn bun-${VERSION} /usr/local/bin/bun
sudo install -m 0755 /tmp/relish-linux-${ARCH} /usr/local/bin/relish
rm -f /tmp/bun-linux-${ARCH} /tmp/relish-linux-${ARCH} /tmp/SHA256SUMS
```

Verify that the binaries are installed and executable:

```sh
bun --version
relish --version
```

Each prints its version and the commit it was built from, such as
`bun 0.1.1 (77bace5)`. Every node should print the same commit.

*(Optional: If building from source instead of using pre-built releases, install
`clang llvm libbpf-dev` and run `cargo build --locked --release --features ebpf --bin bun --bin relish`
from a repository checkout, the same build the release uses. Then install
`target/release/bun` and `target/release/relish` as above.)*

### 2.2 Create configuration and state directories

```sh
sudo install -d -m 0700 /etc/reliaburger /etc/reliaburger/identity
sudo install -d -m 0755 /var/lib/reliaburger
```

---

## 3. Bootstrap the cluster on the first node (`node-01`)

The first node generates the cluster PKI (Root CA and Node CA), the age encryption keypair,
the initial security bootstrap state, and its own node identity.

### 3.1 Initialise cluster credentials

On **Node 1 (`192.168.0.101`)**, run:

```sh
sudo relish init /etc/reliaburger --cluster-name prod --node-id node-01
```

This writes the following files under `/etc/reliaburger`:
- `prod-master.key`: Master secret key (used to encrypt CA and secrets).
- `prod-security-bootstrap.json`: Initial cluster security state.
- `prod-root-ca.age`: The sealed root CA key.
- `reliaburger.toml`: Sample cluster node config file.
- `app.toml`: Sample application manifest file.
- `identity/`: Node 1's mTLS certificates (`node.crt`, `node.key`, `root-ca.crt`, etc.).

> **Important**: `relish init` prints two `Root CA:` lines to stderr. Copy the
> one that starts with `sha256:`; that's the root CA fingerprint the joining
> nodes pin in §4. Back up `prod-master.key` and `prod-root-ca.age` together.

### 3.2 Write the Node 1 configuration

Create `/etc/reliaburger/node.toml` on **Node 1**:

```toml
[node]
name = "node-01"

[cluster]
name = "prod"
# Node 1 starts the cluster with an empty join list
join = []

[network]
advertise_address = "192.168.0.101"

[security]
require_mtls = true
identity_dir = "/etc/reliaburger/identity"
master_key_path = "/etc/reliaburger/prod-master.key"
bootstrap_path = "/etc/reliaburger/prod-security-bootstrap.json"
bootstrap_peers = ["192.168.0.101", "192.168.0.102", "192.168.0.103"]
# Your laptop's address (or network): admitted to the API port only, see §5.3
operator_cidrs = ["192.168.0.50/32"]

[ebpf]
enabled = true

[dns]
enabled = true
listen = "192.168.0.101:53"

[ingress]
enabled = true
http_port = 80
https_port = 443

[images]
registry_port = 5050
```

### 3.3 Set up the systemd service and start Bun

Create `/etc/systemd/system/reliaburger.service` on **Node 1**:

```ini
[Unit]
Description=Reliaburger node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStartPre=/bin/sh -ec 'mountpoint -q /sys/fs/bpf || mount -t bpf bpf /sys/fs/bpf'
ExecStart=/usr/local/bin/bun --cluster --runtime runc --config /etc/reliaburger/node.toml --listen 127.0.0.1:9117
Restart=always
RestartSec=2
LimitNOFILE=1048576
KillMode=process
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

> **Security note (`--listen 127.0.0.1:9117`)**: until the first API token
> exists, the API has no credentials to check, so Bun fails closed (`AUTH3`)
> and refuses to bind anything but a loopback address such as `127.0.0.1:9117`.
> You'll mint that token next, then open the listener in §3.5.

Enable and start the service:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now reliaburger.service
```

Verify that the node is running:

```sh
sudo systemctl status reliaburger.service
```

### 3.4 Mint the administrator token

`/etc/reliaburger/` is readable by root only (`0700`), so first copy the public
root CA certificate somewhere your own user can read it. Then mint the first
admin token and save it straight into a private file:

```sh
# The public Root CA certificate, for non-root CLI use
install -d -m 0700 ~/.reliaburger
sudo install -m 0644 -o "$(id -u)" -g "$(id -g)" \
  /etc/reliaburger/identity/root-ca.crt ~/.reliaburger/root-ca.crt

export RELIABURGER_CA_CERT="$HOME/.reliaburger/root-ca.crt"
export RELIABURGER_ENDPOINT="https://127.0.0.1:9117"

# relish prints the token once, on stdout; keep it owner-only
relish token create --name admin --role admin \
  | install -m 0600 /dev/stdin ~/.reliaburger/admin.token

export RELIABURGER_TOKEN="$(cat ~/.reliaburger/admin.token)"
```

Verify that the CLI can authenticate as a normal user:

```sh
relish status
```

### 3.5 Open the API listener to the other nodes

This step isn't optional. Nodes 2 and 3 enrol through Node 1's API in §4, and
once they're running the nodes keep calling each other's APIs. Bun's own
perimeter firewall still limits port 9117 to cluster members and
`bootstrap_peers`. Now that the token store is populated, Bun accepts a
non-loopback listener:

1. Edit `/etc/systemd/system/reliaburger.service` and change `--listen 127.0.0.1:9117` to `--listen 0.0.0.0:9117`.
2. Reload and restart:
```sh
sudo systemctl daemon-reload
sudo systemctl restart reliaburger.service
```
3. Check that the API is back (the `127.0.0.1` endpoint still works, since
   `0.0.0.0` includes loopback):
```sh
relish status
```

---

## 4. Enrol Node 2 and Node 3 (the joining nodes)

Nodes 2 and 3 require the cluster's master key to decrypt shared cluster secrets, plus a single-use join token to request signed mTLS node certificates from the cluster CA.

### 4.1 Copy the master key to Node 2 and Node 3

The cluster master key (`prod-master.key`) unlocks the cluster CA and shared
secrets. Stream it over SSH straight into place, owned by root with `0600`
permissions, so it never lands anywhere else on disk.

From **Node 1 (`node-01`)**:

```sh
# Copy to Node 2
sudo cat /etc/reliaburger/prod-master.key | ssh user@192.168.0.102 \
'sudo install -m 0600 -o root -g root /dev/stdin /etc/reliaburger/prod-master.key'

# Copy to Node 3
sudo cat /etc/reliaburger/prod-master.key | ssh user@192.168.0.103 \
'sudo install -m 0600 -o root -g root /dev/stdin /etc/reliaburger/prod-master.key'
```

> **Note**: `prod` is the cluster name you passed to `relish init` in §3.1.

### 4.2 Create single-use join tokens

On **Node 1**, with the environment from §3.4 still exported, mint one join
token per node and stream each into an owner-only file on its node (`relish
join` refuses a token file other users can read):

```sh
relish join-token create --node-id node-02 --ttl 15m \
  | ssh user@192.168.0.102 'install -m 0600 /dev/stdin ~/node-02.join'
relish join-token create --node-id node-03 --ttl 15m \
  | ssh user@192.168.0.103 'install -m 0600 /dev/stdin ~/node-03.join'
```

Each token enrols only the node it names, once, and expires after 15 minutes.
You'll also need the `sha256:` root CA fingerprint `relish init` printed in
§3.1: `relish join` refuses a member that offers a different root CA.

### 4.3 Enrol Node 2 (`node-02`)

On **Node 2 (`192.168.0.102`)**:

1. Enrol the node identity using `relish join`, then delete the spent token:

```sh
sudo relish join \
  --token-file ~/node-02.join \
  --node-id node-02 \
  --identity-dir /etc/reliaburger/identity \
  --ca-fingerprint "sha256:<ROOT_CA_FINGERPRINT>" \
  https://192.168.0.101:9117
rm ~/node-02.join
```

2. Create `/etc/reliaburger/node.toml` on **Node 2**:

```toml
[node]
name = "node-02"

[cluster]
name = "prod"
join = ["192.168.0.101:9443"]

[network]
advertise_address = "192.168.0.102"

[security]
require_mtls = true
identity_dir = "/etc/reliaburger/identity"
master_key_path = "/etc/reliaburger/prod-master.key"
bootstrap_peers = ["192.168.0.101", "192.168.0.102", "192.168.0.103"]

[ebpf]
enabled = true

[dns]
enabled = true
listen = "192.168.0.102:53"

[ingress]
enabled = true
http_port = 80
https_port = 443

[images]
registry_port = 5050
```

3. Create `/etc/systemd/system/reliaburger.service` and start Bun. A joining
   node can listen on `0.0.0.0` from the start: it keeps the listener closed
   until the cluster's API credentials have replicated to it.

```ini
[Unit]
Description=Reliaburger node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStartPre=/bin/sh -ec 'mountpoint -q /sys/fs/bpf || mount -t bpf bpf /sys/fs/bpf'
ExecStart=/usr/local/bin/bun --cluster --runtime runc --config /etc/reliaburger/node.toml --listen 0.0.0.0:9117
Restart=always
RestartSec=2
LimitNOFILE=1048576
KillMode=process
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now reliaburger.service
```

### 4.4 Enrol Node 3 (`node-03`)

On **Node 3 (`192.168.0.103`)**:

1. Enrol the node identity using `relish join`, then delete the spent token:

```sh
sudo relish join \
  --token-file ~/node-03.join \
  --node-id node-03 \
  --identity-dir /etc/reliaburger/identity \
  --ca-fingerprint "sha256:<ROOT_CA_FINGERPRINT>" \
  https://192.168.0.101:9117
rm ~/node-03.join
```

2. Create `/etc/reliaburger/node.toml` on **Node 3**:

```toml
[node]
name = "node-03"

[cluster]
name = "prod"
join = ["192.168.0.101:9443"]

[network]
advertise_address = "192.168.0.103"

[security]
require_mtls = true
identity_dir = "/etc/reliaburger/identity"
master_key_path = "/etc/reliaburger/prod-master.key"
bootstrap_peers = ["192.168.0.101", "192.168.0.102", "192.168.0.103"]

[ebpf]
enabled = true

[dns]
enabled = true
listen = "192.168.0.103:53"

[ingress]
enabled = true
http_port = 80
https_port = 443

[images]
registry_port = 5050
```

3. Create `/etc/systemd/system/reliaburger.service` and start Bun:

```ini
[Unit]
Description=Reliaburger node
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStartPre=/bin/sh -ec 'mountpoint -q /sys/fs/bpf || mount -t bpf bpf /sys/fs/bpf'
ExecStart=/usr/local/bin/bun --cluster --runtime runc --config /etc/reliaburger/node.toml --listen 0.0.0.0:9117
Restart=always
RestartSec=2
LimitNOFILE=1048576
KillMode=process
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now reliaburger.service
```

---

## 5. Install and configure Relish on your laptop

You can manage the entire cluster from your own workstation, whether it's a
Mac or a Windows PC, without keeping a shell open on the VMs.

### 5.1 Install Relish on your laptop

#### On macOS (Apple silicon or Intel)

**Option A: Install script**
```sh
curl -fsSL https://reliaburger.com/install.sh | sh -s -- --install-only
```

**Option B: Direct download from GitHub Releases**
```sh
# Detect Apple silicon (arm64) vs Intel (x86_64)
ARCH="$(uname -m)"
case "$ARCH" in
  arm64|aarch64) ARCH="aarch64" ;;
  x86_64)        ARCH="x86_64" ;;
  *) echo "Unsupported architecture: $ARCH; the release has x86_64 and aarch64 builds" >&2 ;;
esac

# Release version to install (e.g. v0.1.1 or vX.Y.Z)
VERSION="v0.1.1"
BASE_URL="https://github.com/reliaburger/reliaburger/releases/download/${VERSION}"

# Download relish binary and SHA256SUMS into /tmp
curl -fsSL -o /tmp/relish-macos-${ARCH} "${BASE_URL}/relish-macos-${ARCH}"
curl -fsSL -o /tmp/SHA256SUMS "${BASE_URL}/SHA256SUMS"

# Verify download integrity against the release checksums
(cd /tmp && shasum -a 256 --check --ignore-missing SHA256SUMS)

# Install to /usr/local/bin
sudo install -m 0755 /tmp/relish-macos-${ARCH} /usr/local/bin/relish
rm -f /tmp/relish-macos-${ARCH} /tmp/SHA256SUMS

# Verify installation
relish --version
```

---

#### On Windows

Run `relish` inside **WSL2** (Windows Subsystem for Linux) using the Linux installer:

```sh
curl -fsSL https://reliaburger.com/install.sh | sh -s -- --install-only
```

---

### 5.2 Copy the Root CA certificate and admin token to your laptop

`relish` verifies the cluster against its root CA, so copy the user-readable
copy you made in §3.4. Copy the admin token too, keeping it owner-only:

#### On macOS / Linux / WSL:
```sh
mkdir -p -m 0700 ~/.reliaburger
scp user@192.168.0.101:.reliaburger/root-ca.crt ~/.reliaburger/root-ca.crt
(umask 077 && ssh user@192.168.0.101 'cat ~/.reliaburger/admin.token' > ~/.reliaburger/admin.token)
```

---

### 5.3 Reach the API and set the environment

Bun's perimeter firewall drops port 9117 from anything that isn't a cluster
member, a bootstrap peer, or an address in `[security] operator_cidrs`. Node 1's
config in §3.2 lists `192.168.0.50/32`; put your laptop's address (or your
admin network, e.g. `192.168.0.0/24`) there instead, and restart Bun if you
change it later:

```sh
sudo systemctl restart reliaburger.service
```

It only opens the API port (never gossip, Raft or reporting), and every call
still needs your token and verifies against the cluster CA. Bun refuses to start
on a malformed entry, a `/0`, or a CIDR with host bits set. If you run UFW,
let your laptop through to the API on Node 1 as well:

```sh
sudo ufw allow proto tcp from 192.168.0.50 to any port 9117
```

Then point `relish` at Node 1, the CA certificate, and the admin token from
§5.2:

#### On macOS / Linux / WSL (Zsh or Bash):
```sh
export RELIABURGER_ENDPOINT="https://192.168.0.101:9117"
export RELIABURGER_CA_CERT="$HOME/.reliaburger/root-ca.crt"
export RELIABURGER_TOKEN="$(cat ~/.reliaburger/admin.token)"
```

#### Alternative: an SSH tunnel

If you'd rather not open 9117 to any extra address (or your laptop's address
keeps changing), leave `operator_cidrs` out and tunnel instead. The tunnel
delivers your requests to Node 1 on loopback, which the perimeter always lets
through. Relish checks the node certificate against the cluster CA rather than
a host name, so the `127.0.0.1` endpoint verifies fine:

```sh
# Forward local port 19117 to Node 1's API, in the background
ssh -f -N -L 19117:127.0.0.1:9117 user@192.168.0.101
export RELIABURGER_ENDPOINT="https://127.0.0.1:19117"
```

The endpoint and CA path are safe to add to your `~/.zshrc` or `~/.bashrc`.
The token isn't: keep it in a `0600` file or your secrets manager (e.g. Vault)
and export it when you need it.

---

## 6. Verify cluster formation and council quorum

From your laptop (or any configured management host):

### 6.1 Check gossip membership

```sh
relish nodes
```

Expected output: All 3 nodes (`node-01`, `node-02`, `node-03`) appear in `alive` state.

### 6.2 Check Raft council composition

```sh
relish council
```

Expected output: 3 voter members with one active leader and a healthy consensus quorum.

### 6.3 Check cluster readiness

```sh
relish status
```

---

## 7. Deploy and run the demo application

Now deploy a containerised HTTP web service across the 3-node cluster directly from your laptop.

### 7.1 Create the demo application manifest

Save the following configuration as `hello.toml` on your laptop:

```toml
[app.hello]
image = "public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028"
command = [
  "/bin/sh",
  "-c",
  "echo hello-started; mkdir -p /tmp/www; printf 'Reliaburger multi-node cluster is running\\n' > /tmp/www/index.html; exec httpd -f -p 8080 -h /tmp/www"
]
port = 8080
replicas = 3

[app.hello.health]
path = "/"
interval = 2
threshold_healthy = 1

[app.hello.ingress]
host = "hello.world.test"
path = "/"

[app.hello.env]
PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
```

### 7.2 Apply the application

Submit the manifest to the remote cluster:

```sh
relish apply hello.toml
```

### 7.3 Inspect deployment status and logs

Check the deployment status:

```sh
relish status
```

Inspect the running container instances across the cluster nodes:

```sh
relish inspect hello
```

Stream logs from the application:

```sh
relish logs hello
```

### 7.4 Test HTTP ingress traffic

Send an HTTP request via the ingress proxy on any of the three VM IP addresses:

```sh
curl -H "Host: hello.world.test" http://192.168.0.101/
curl -H "Host: hello.world.test" http://192.168.0.102/
curl -H "Host: hello.world.test" http://192.168.0.103/
```

Response:
```text
Reliaburger multi-node cluster is running
```

### 7.5 Open the web dashboard

Launch a temporary, authenticated browser session on your laptop:

```sh
relish dashboard
```

Relish serves a read-only view of the Brioche dashboard on a local port, talks
to the cluster over TLS with your bearer token, and opens it in your default
browser. Press `Ctrl-C` when you're done.

---

## 8. Fault tolerance and day-two operations

### 8.1 Scaling the application

Reliaburger is declarative: to scale the application to 6 replicas, edit `replicas = 6` in `hello.toml` on your laptop and run:

```sh
relish apply hello.toml
```

Then check `relish status` to observe the new replicas being placed across the nodes.

### 8.2 Testing node communication failure and self-healing

Simulate the loss of `node-03`:

```sh
# On Node 3:
sudo systemctl stop reliaburger.service
```

> **Note**: stopping the service doesn't quite simulate losing the whole node,
> but it does cut Node 3 off from the cluster, and the scheduler moves its
> workloads elsewhere. The systemd unit uses `KillMode=process`, and container
> owners are designed to outlive Bun, so Node 3's containers keep serving while
> the cluster reschedules them.

From your laptop, observe the cluster behaviour:

1. **Council quorum**: `relish council` confirms that `node-01` and `node-02` maintain quorum (2 out of 3 votes).
2. **Workload rescheduling**: `relish status` shows the scheduler automatically moving workloads from `node-03` to the surviving nodes.
3. **Ingress continuity**: `curl -H "Host: hello.world.test" http://192.168.0.101/` continues serving traffic seamlessly.

Restart Node 3:

```sh
# On Node 3:
sudo systemctl start reliaburger.service
```

Node 3 rejoins gossip, catches up with the Raft log, and resumes serving as an active council member and workload node.

### 8.3 Chaos testing and fault injection (Smoker)

Reliaburger includes a built-in chaos engineering subsystem (Smoker) for injecting controlled network, workload, and node faults directly via `relish fault`.

#### 1. Cluster safety policy

Faults can take down real traffic, so Bun enforces a server-side safety policy
(the `[testing]` section in `node.toml`). The node configs above leave it out
on purpose: with no `[testing]` section, the safety class is `unknown`, which
is protected, and every fault is refused (`403: cluster policy does not allow
this operation`).

Only opt in on a test cluster you're happy to break. Add this to
`/etc/reliaburger/node.toml` on every node, then restart the nodes one at a
time with `sudo systemctl restart reliaburger.service`:

```toml
[testing]
safety_class = "development"
allowed_operations = ["inject_workload_faults", "alter_node_state"]
```

That admits workload faults and `node-kill`/`node-drain`. Every injection also
needs the `--acknowledge` flag. The [chaos chapter](manual/05_chaos.md) of the
manual lists the roles and grants each fault needs.

#### 2. Simulate node failure with `node-kill`
Simulate an abrupt node failure on `node-03` for 5 minutes:

```sh
relish fault node-kill node-03 --duration 5m --acknowledge
```

Smoker refuses to kill the council leader unless you add `--include-leader`.
`relish council` shows which node leads; if it's `node-03`, pick another node
or add the flag.

View active faults across the cluster:

```sh
relish fault list
```

Clear the fault before the timer expires:

```sh
relish fault clear
```

#### 3. Inject workload-level faults
- **Add 200ms latency to traffic**:
```sh
relish fault delay hello 200ms --duration 2m --acknowledge
```
- **Fail 25% of new connections**:
```sh
relish fault drop hello 25% --duration 2m --acknowledge
```

---

## 9. Upgrading

The [operations chapter](manual/12_operations.md) of the manual covers
`relish upgrade`, which rolls a new `bun` across the cluster: workers first,
then council members one by one, the leader last. This guide already set up
two of its requirements: systemd restarts Bun whenever it exits
(`Restart=always`), and `/usr/local/bin/bun` is a symlink to a versioned
binary, so older versions stay beside it for rollback.

The third requirement is yours to add. A network upgrade needs two signatures
on the new binary, the release's and your own, and every node refuses one
without a key to check yours against. Generate an operator key pair with
`relish dev keygen --out keys/` and countersign each release binary with
`relish dev countersign-binary`, as the manual describes. Countersigning
prints the `ed25519:…` public key; put it in each node's `node.toml` and
restart the nodes one at a time:

```toml
[upgrades]
external_signing_key = "ed25519:<YOUR_PUBLIC_KEY>"
```

`relish upgrade start` checks every node for that key before it starts, and
names any node that's missing it.

---

## 10. Summary of essential CLI commands

| Task | Command |
|------|---------|
| Check cluster & workload status | `relish status` |
| View cluster membership | `relish nodes` |
| View Raft council status | `relish council` |
| Inspect app or node details | `relish inspect <app-or-node>` |
| Stream application logs | `relish logs <app>` |
| Apply application configuration | `relish apply <file.toml>` |
| Stop an application | `relish stop <app>` |
| Inspect cluster resource usage | `relish top` |
| Diagnose cluster issues | `relish wtf` |
