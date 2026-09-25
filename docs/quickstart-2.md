# A multi-node cluster on Linux VMs

This guide walks through deploying Reliaburger across three pre-existing Linux
virtual machines (or physical servers) running rootful `runc` and eBPF, forming
a three-node Raft council with mutual TLS (mTLS), and running the demo container
workload.

Many times, VMs are already deployed and you want to take advantage of existing configurations and runtimes.

If you want an automated, disposable local cluster on macOS or Linux using
managed Lima VMs instead, see the [quickstart guide](quickstart.md).

---

## 1. Prerequisites and network requirements

### Host requirements (each VM)

Every node must meet the following minimum specification:

- **OS / Architecture**: Linux x86_64 or aarch64 (e.g. Ubuntu 22.04+, Debian 12+, RHEL 9+).
- **Kernel**: Linux 5.8 or later with cgroup v2 enabled.
- **Privileges**: Root or `sudo` access on all three nodes.
- **BPF filesystem**: `bpffs` mounted at `/sys/fs/bpf`.
- **Resources**: At least 2 CPU cores, 2 GiB RAM, and 10 GiB available disk space per node.
- **Required packages**: `runc`, `uidmap` (or `shadow-utils`), `nftables`, `btrfs-progs`, and `curl`.

Install the required packages on all three nodes (Debian/Ubuntu example):

```sh
sudo apt-get update
sudo apt-get install -y runc uidmap btrfs-progs nftables curl
```

Ensure the BPF virtual filesystem is mounted:

```sh
mountpoint -q /sys/fs/bpf || sudo mount -t bpf bpf /sys/fs/bpf
```

### Network and firewall matrix

Assign hostnames or static IP addresses to your three VMs. For this guide, we use:

| Node ID | Role | Example IP |
|---------|------|------------|
| `node-01` | One-burger node, Council voter | `192.168.0.101` |
| `node-02` | Side-burger node, Council voter | `192.168.0.102` |
| `node-03` | Side-burger node, Council voter | `192.168.0.103` |

Ensure the following ports are open between the nodes:

| Port | Protocol | Purpose | Direction |
|------|----------|---------|-----------|
| `9117` | TCP | Bun API and Web Dashboard (Brioche) | Intersite / Operator |
| `9443` | TCP/UDP | SWIM Gossip (Mustard) | Node-to-node |
| `9444` | TCP | Raft consensus (Council) | Node-to-node |
| `9445` | TCP | State reporting tree (Mayo) | Node-to-node |
| `5050` | TCP | Pickle OCI image registry | Intersite / Node-to-node |
| `53` | UDP/TCP | Service discovery DNS (`.internal`) | Node-to-node |
| `80`, `443` | TCP | Ingress HTTP/HTTPS proxy (Wrapper) | External / Ingress |

---

## 2. Install binaries and prepare directories

Perform these steps on **all three nodes**:

### 2.1 Download and install the latest release from GitHub

Download the pre-built `bun` (node agent) and `relish` (CLI) binaries for your system architecture (`x86_64` or `aarch64`) from the GitHub repository release page:

```sh
# Detect host architecture
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64)   ARCH="x86_64" ;;
  aarch64|arm64)  ARCH="aarch64" ;;
  *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

# Download the latest Linux release binaries from GitHub
# Future URL
# BASE_URL="https://github.com/reliaburger/reliaburger/releases/latest/download"
BASE_URL="https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-36114757933-1"
curl -fsSL -o /tmp/bun "${BASE_URL}/bun-linux-${ARCH}"
curl -fsSL -o /tmp/relish "${BASE_URL}/relish-linux-${ARCH}"

# Install to /usr/local/bin
sudo install -m 0755 /tmp/bun /usr/local/bin/bun
sudo install -m 0755 /tmp/relish /usr/local/bin/relish
rm -f /tmp/bun /tmp/relish
```

Verify that the binaries are installed and executable:

```sh
bun --version
relish --version
```

*(Optional: If building from source instead of using pre-built releases, run `cargo build --release --bin bun --bin relish` from a repository checkout, then copy `target/release/bun` and `target/release/relish` into `/usr/local/bin`.)*

### 2.2 Create configuration and state directories

```sh
sudo install -d -m 0700 /etc/reliaburger /etc/reliaburger/identity
sudo install -d -m 0755 /var/lib/reliaburger
```

---

## 3. Bootstrap the cluster on the One-burger node (`node-01`)

The first node generates the cluster PKI (Root CA and Node CA), the age encryption keypair,
the initial security bootstrap state, and its own node identity.

### 3.1 Initialize cluster credentials

On **Node 1 (`192.168.0.101`)**, run:

```sh
cd /etc/reliaburger
sudo relish init /etc/reliaburger --cluster-name prod --node-id node-01
```

This writes the following files under `/etc/reliaburger`:
- `prod-master.key`: Master secret key (used to encrypt CA and secrets).
- `prod-security-bootstrap.json`: Initial cluster security state.
- `identity/`: Node 1's mTLS certificates (`node.crt`, `node.key`, `root-ca.crt`, etc.).

Make a note of the **Root CA fingerprint** printed to stderr (e.g. `sha256:abcd...`). You can also inspect it later.

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
Restart=on-failure
RestartSec=2
LimitNOFILE=1048576
KillMode=process
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

> **Security Note (`--listen 127.0.0.1:9117`)**: During bootstrap, when no API tokens exist yet, Bun enforces a fail-closed policy (`AUTH3`) and strictly refuses binding non-loopback addresses (such as `0.0.0.0:9117`). Once the first admin token is minted below, you can change `--listen` to `0.0.0.0:9117` if remote access to this node's API is needed.

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

Because `/etc/reliaburger/` was created with restricted root-only permissions (`0700`), run `token create` with `sudo` (or copy `root-ca.crt` to your user directory first):

```sh
# Mint the admin token using sudo
ADMIN_TOKEN=$(sudo relish --ca-cert /etc/reliaburger/identity/root-ca.crt \
  --endpoint https://127.0.0.1:9117 \
  token create --name admin --role admin | tail -n 1)

# Copy the public Root CA certificate to your user directory for non-root CLI use
mkdir -p ~/.reliaburger
sudo cp /etc/reliaburger/identity/root-ca.crt ~/.reliaburger/root-ca.crt
sudo chown $(id -u):$(id -g) ~/.reliaburger/root-ca.crt

# Export connection settings for your regular user session
export RELIABURGER_CA_CERT="$HOME/.reliaburger/root-ca.crt"
export RELIABURGER_TOKEN="$ADMIN_TOKEN"
export RELIABURGER_ENDPOINT="https://127.0.0.1:9117"
```

Verify that the CLI can authenticate as a normal user:

```sh
relish status
```

### 3.5 (Optional) Open API listener to all interfaces

Now that the token store is populated, if you want Node 1's API to be accessible directly over the network (e.g. from your laptop at `https://192.168.0.101:9117`):

1. Edit `/etc/systemd/system/reliaburger.service` and change `--listen 127.0.0.1:9117` to `--listen 0.0.0.0:9117`.
2. Reload and restart:
   ```sh
   sudo systemctl daemon-reload
   sudo systemctl restart reliaburger.service
   ```
3. Update `RELIABURGER_ENDPOINT`:
   ```sh
   export RELIABURGER_ENDPOINT="https://192.168.0.101:9117"
   export RELIABURGER_CA_CERT="$HOME/.reliaburger/root-ca.crt"
   export RELIABURGER_TOKEN="<YOUR_ADMIN_TOKEN>"
   relish status
   ```

---

## 4. Enroll Node 2 and Node 3 (Side-burger nodes)

Nodes 2 and 3 require the cluster's master key to decrypt shared cluster secrets, plus a single-use join token to request signed mTLS node certificates from the cluster CA.

### 4.1 Copy the master key to Node 2 and Node 3

The cluster master key (`prod-master.key`) unlocks the cluster CA and shared secrets. Because `/etc/reliaburger/` is owned by `root:root` with `0700` permissions, transfer it via `/tmp` and set strict `0600` permissions.

#### From Node 1 (`node-01`):
Copy the key to Node 2 and Node 3:

```sh
# Copy to Node 2
sudo scp /etc/reliaburger/prod-master.key user@192.168.0.102:/tmp/prod-master.key

# Copy to Node 3
sudo scp /etc/reliaburger/prod-master.key user@192.168.0.103:/tmp/prod-master.key
```

> **Note**: `prod` is the name of reliaburger cluster initialized at the beginning of this procedure.

#### On Node 2 (`node-02`):
Move the key into place and restrict permissions:

```sh
sudo mv /tmp/prod-master.key /etc/reliaburger/prod-master.key
sudo chown root:root /etc/reliaburger/prod-master.key
sudo chmod 0600 /etc/reliaburger/prod-master.key
```

#### On Node 3 (`node-03`):
Move the key into place and restrict permissions:

```sh
sudo mv /tmp/prod-master.key /etc/reliaburger/prod-master.key
sudo chown root:root /etc/reliaburger/prod-master.key
sudo chmod 0600 /etc/reliaburger/prod-master.key
```

### 4.2 Create single-use join tokens and get the Root CA fingerprint

#### 1. Calculate the Root CA fingerprint

On **Node 1**, compute the SHA-256 fingerprint of `root-ca.crt` (or omit `--ca-fingerprint` during join):

```sh
# Calculate the DER certificate SHA-256 fingerprint
ROOT_CA_FINGERPRINT="sha256:$(sudo openssl x509 -in /etc/reliaburger/identity/root-ca.crt -outform DER | sha256sum | awk '{print $1}')"

echo "Root CA Fingerprint: $ROOT_CA_FINGERPRINT"
```

#### 2. Mint single-use join tokens
On **Node 1** (or from your operator terminal with `RELIABURGER_TOKEN` set), mint join tokens for Node 2 and Node 3:

```sh
relish join-token create --node-id node-02 --ttl 15m
relish join-token create --node-id node-03 --ttl 15m
```

Save each generated token string.

### 4.3 Enroll Node 2 (`node-02`)

On **Node 2 (`192.168.0.102`)**:

1. Enroll node identity using `relish join`:

   ```sh
   sudo relish join \
     --token "<TOKEN_FOR_NODE_02>" \
     --node-id node-02 \
     --identity-dir /etc/reliaburger/identity \
     --ca-fingerprint "$ROOT_CA_FINGERPRINT" \
     https://192.168.0.101:9117
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

3. Create `/etc/systemd/system/reliaburger.service` (same as Node 1) and start Bun:

   ```sh
   sudo systemctl daemon-reload
   sudo systemctl enable --now reliaburger.service
   ```

### 4.4 Enroll Node 3 (`node-03`)

On **Node 3 (`192.168.0.103`)**:

1. Enroll node identity using `relish join`:

   ```sh
   sudo relish join \
     --token "<TOKEN_FOR_NODE_03>" \
     --node-id node-03 \
     --identity-dir /etc/reliaburger/identity \
     --ca-fingerprint "<ROOT_CA_FINGERPRINT>" \
     https://192.168.0.101:9117
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

   ```sh
   sudo systemctl daemon-reload
   sudo systemctl enable --now reliaburger.service
   ```

---

## 5. Install and configure Relish on your laptop

You can manage (like a pro) the entire cluster remotely from your local workstation (whether you use a **MacBook** or a **Windows PC**) without needing an SSH shell into the VMs.

### 5.1 Install Relish on your laptop

#### On macOS (MacBook - Apple Silicon or Intel)

**Option A: Fast install via script**
```sh
curl -fsSL https://reliaburger.com/install.sh | sh -s -- --install-only
```

**Option B: Direct download from GitHub Releases**
```sh
# Detect Apple Silicon (arm64) vs Intel (x86_64)
ARCH="$(uname -m)"
case "$ARCH" in
  arm64|aarch64) ARCH="aarch64" ;;
  x86_64)        ARCH="x86_64" ;;
  *) echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

# Download and install relish for macOS
# Future URL
# BASE_URL="https://github.com/reliaburger/reliaburger/releases/latest/download"
BASE_URL="https://github.com/reliaburger/reliaburger/releases/download/staging-v0.1.0-36114757933-1"
curl -fsSL -o /tmp/relish "${BASE_URL}/relish-macos-${ARCH}"
sudo install -m 0755 /tmp/relish /usr/local/bin/relish
rm -f /tmp/relish

# Verify installation
relish --version
```

---

#### On Windows PC

##### Method 1: Using Windows Subsystem for Linux (WSL2 / Ubuntu) — Recommended
If you use WSL2, open your WSL terminal (e.g. Ubuntu) and install Relish using the Linux installer:
```sh
curl -fsSL https://reliaburger.com/install.sh | sh -s -- --install-only
```

##### Method 2: Native Windows (PowerShell & Git Bash)
On a Windows PC with PowerShell 5.1+ or PowerShell 7+:

1. Create a tools directory in your user profile:
   ```powershell
   New-Item -ItemType Directory -Force -Path "$HOME\.reliaburger\bin"
   ```

2. Download the Windows binary (or use the binary compiled from source via `cargo build --release --bin relish`):
   ```powershell
   # If downloading from GitHub Releases or copying from your build host:
   Invoke-WebRequest -Uri "https://github.com/reliaburger/reliaburger/releases/latest/download/relish-windows-x86_64.exe" -OutFile "$HOME\.reliaburger\bin\relish.exe"
   ```

3. Add `~/.reliaburger/bin` to your user `PATH` if it is not already present:
   ```powershell
   [Environment]::SetEnvironmentVariable("Path", $env:Path + ";$HOME\.reliaburger\bin", "User")
   ```

4. Verify in a new PowerShell window:
   ```powershell
   relish --version
   ```

---

### 5.2 Copy the Root CA certificate to your laptop

Copy the cluster Root CA certificate generated on Node 1 to your laptop so `relish` can securely verify the cluster over TLS.

#### On macOS / Linux / WSL:
```sh
mkdir -p ~/.reliaburger
scp user@192.168.0.101:/etc/reliaburger/identity/root-ca.crt ~/.reliaburger/root-ca.crt
```

#### On Windows (PowerShell):
OpenSSH client is built into Windows 10/11:
```powershell
New-Item -ItemType Directory -Force -Path "$HOME\.reliaburger"
scp user@192.168.0.101:/etc/reliaburger/identity/root-ca.crt "$HOME\.reliaburger\root-ca.crt"
```

---

### 5.3 Configure connection environment variables

Set the cluster endpoint, CA certificate path, and administrator token (minted in step 3.4) on your laptop:

#### On macOS / Linux / WSL (Zsh or Bash):
```sh
export RELIABURGER_ENDPOINT="https://192.168.0.101:9117"
export RELIABURGER_CA_CERT="$HOME/.reliaburger/root-ca.crt"
export RELIABURGER_TOKEN="<ADMIN_TOKEN>"
```
*(Tip: Add these exports to your `~/.zshrc` or `~/.bashrc` to make them persistent across terminal sessions.)*

#### On Windows (PowerShell):
For the current PowerShell session:
```powershell
$env:RELIABURGER_ENDPOINT = "https://192.168.0.101:9117"
$env:RELIABURGER_CA_CERT = "$HOME\.reliaburger\root-ca.crt"
$env:RELIABURGER_TOKEN = "<ADMIN_TOKEN>"
```

To set them permanently for your Windows user profile:
```powershell
[Environment]::SetEnvironmentVariable("RELIABURGER_ENDPOINT", "https://192.168.0.101:9117", "User")
[Environment]::SetEnvironmentVariable("RELIABURGER_CA_CERT", "$HOME\.reliaburger\root-ca.crt", "User")
[Environment]::SetEnvironmentVariable("RELIABURGER_TOKEN", "<ADMIN_TOKEN>", "User")
```

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

Now deploy a containerized HTTP web service across the 3-node cluster directly from your laptop.

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
host = "hello.internal"
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
curl -H "Host: hello.internal" http://192.168.0.101/
curl -H "Host: hello.internal" http://192.168.0.102/
curl -H "Host: hello.internal" http://192.168.0.103/
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

Relish terminates the mTLS connection and opens the Brioche dashboard locally in your default web browser (press `Ctrl-C` when done).

---

## 8. Fault tolerance and day-two operations

### 8.1 Scaling the application

Reliaburger is declarative: to scale the application to 6 replicas, edit `replicas = 6` in `hello.toml` on your laptop and run:

```sh
relish apply hello.toml
```

Then check `relish status` to observe the new replicas being placed across the nodes.

### 8.2 Testing node failure and self-healing

Simulate the loss of `node-03`:

```sh
# On Node 3:
sudo systemctl stop reliaburger.service
```

From your laptop, observe the cluster behaviour:

1. **Council quorum**: `relish council` confirms that `node-01` and `node-02` maintain quorum (2 out of 3 votes).
2. **Workload rescheduling**: `relish status` shows the scheduler automatically moving workloads from `node-03` to the surviving nodes.
3. **Ingress continuity**: `curl -H "Host: hello.internal" http://192.168.0.101/` continues serving traffic seamlessly.

Restart Node 3:

```sh
# On Node 3:
sudo systemctl start reliaburger.service
```

Node 3 rejoins gossip, catches up with the Raft log, and resumes serving as an active council member and workload node.

---

## 9. Summary of essential CLI commands

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
