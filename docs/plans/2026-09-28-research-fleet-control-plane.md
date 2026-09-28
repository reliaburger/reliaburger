# Research: a fleet control plane for Reliaburger appliances

*Research note, 28 September 2026. Docs only, no product code. It builds on the appliance OS research ([`2026-09-26-research-appliance-os.md`](2026-09-26-research-appliance-os.md), draft PR #218) and the S1 image work (`feat/appliance-image`, draft PR #259). Repo facts come from `main` at `087d882f`. External facts come from primary sources fetched today; the URLs are in the Sources section. **[unverified]** marks a claim nobody has confirmed from a primary source. **[inference]** marks my own reading of code or docs.*

> **Status: complete, awaiting maintainer review.** Branch `research/fleet-control-plane`, based on `research/appliance-os`. Nobody should start building from this until the maintainer answers §15.

## Progress checklist (for whoever resumes this)

- [x] Read the appliance research, the spike plan, the S1 image plan and the current join and decommission code
- [x] Prior art: Omni (machines, classes, templates, SideroLink, Image Factory) (§2.1)
- [x] Prior art: Metal³/Ironic, MAAS, Tinkerbell, Foreman, Incus OS, AuroraBoot (§2.2–§2.4)
- [x] Machine lifecycle state machine (§4)
- [x] Identity and trust model (§5)
- [x] Discovery (§6)
- [x] Control-plane state and HA (§7)
- [x] Allocation policies (§8)
- [x] Wipe semantics (§9)
- [x] Moving a node between clusters (§10)
- [x] OS and bun upgrades across clusters (§11)
- [x] Networking: overlay or not (§12)
- [x] CLI and UI (§13)
- [x] Packaging decision: separate repo, integrated, or hybrid (§3, §14)
- [x] Repo gaps, effort, risks, open questions and phased plan (§14–§17)
- [x] Draft PR opened (#265)
- [ ] Maintainer review

---

## 0. Summary and recommendation

**The problem, concretely.** Ten Wyse 3040s netboot from `relish netboot` and sit in `unclaimed`. You want five in a `home` cluster, three in a `lab` cluster and two spare. Next month `lab` needs two more, so you take the spares. Then one of `home`'s boxes dies, and you want a spare to replace it without anyone walking to a console. Today every one of those steps is a hand-run ceremony: mint a join token, copy `master.key`, decommission, reinstall. The appliance research's claim flow (`relish machines claim`) covers the first allocation. Nothing covers the rest.

**Recommendation: build it in, as a thin layer, in two halves.**

1. **The machine side lives in `bun`'s appliance mode.** It's the only process on the box, so it's the only place a machine agent can live without shipping a second agent (Kairos and Metal³ both do, §2). It extends the claim server the appliance plan already needs with four things: a persistent machine key, a fleet pin, wipe, and a status report.
2. **The fleet side is `relish fleet`**, first as CLI commands with a local state file (v0), later as `relish fleet serve`, a small long-running service in the same binary, backed by `redb`, which we already ship (v1). No new repo, no new binary, no new database, no Raft.
3. **The fleet is off the data path.** If it's down, clusters keep running and nodes stay members; you just can't allocate, move or wipe. So a single instance with backups is enough. Most of its state can be rebuilt from the edges, because machines and clusters each carry their own half of the truth (§7).
4. **Trust is established once per machine, not once per cluster.** Enrolling a machine into the fleet is the physical ceremony: compare the console fingerprint, or accept TOFU inside a short boot window. After that the machine pins the fleet's key, and every allocation, wipe and move is authenticated by keys that already exist. There's no TPM in the Wyse, so hardware attestation is an optional upgrade later, not a foundation (§5).
5. **The fleet never touches `master.key`.** It mints a node-bound, single-use join token with a new scoped `Enroller` credential and pushes it down the machine channel. The node fetches the master key itself after joining (appliance gap G1).
6. **No WireGuard overlay in v0 or v1.** A LAN plus mTLS with pinned keys is enough for one site. Multi-site gets an outbound "phone-home" TLS channel from the machine, not SideroLink-style WireGuard (§12).
7. **Fold the appliance plan's Phase 2 claim flow into fleet v0**, rather than building `relish machines claim` and then a fleet on top of it. That saves roughly two weeks and one migration.

**Not chosen:** a separate optional repo or binary (protocol version skew between two release trains, and a second thing to install, §3); putting fleet state inside one cluster's Raft (a cluster shouldn't own its peers' machines, §7); a Kubernetes-style management cluster (Tinkerbell and Cluster API's pattern, far too heavy for ten thin clients, §2.3).

**Effort:** v0 (CLI only on one LAN: enrol, allocate, wipe, move, pool OS updates) is about **5–7 engineer-weeks** on top of appliance Phases 1, 2 and 2b, or 3–5 weeks more than Phase 2 alone if Phase 2 is folded in. v1 (service, reconciler, Brioche page) is another **5–7 weeks**. v2 (multi-site, TPM, power control) is **6–10 weeks** and only worth doing if someone asks. Details in §16.

---

## 1. What a fleet control plane has to do

Strip away the Omni vocabulary and you're left with five jobs:

1. **Know which machines exist**, what they are (arch, cores, RAM, disk) and what state they're in, including machines that belong to no cluster.
2. **Hand machines to clusters** (allocate) without a human carrying secrets around.
3. **Take machines back** (release): drain, retire the identity, wipe the disk, and put the machine back in the pool.
4. **Move capacity between clusters**, which is jobs 3 and 2 in a row, with safety checks.
5. **Keep the OS and bun versions converged** across clusters and the idle pool.

What it doesn't have to do: schedule workloads (each cluster's Meat does that), replicate app state (Raft does that inside a cluster), or join clusters to each other (that's Franchise, whitepaper §21, still planned).

**Two words we need to keep apart.** A *machine* is hardware. It has a stable fleet identity for its whole life (`m-7f3a9c`). A *node* is a cluster membership: a node id (`home-3`), a certificate from that cluster's Node CA, and a CRL tombstone once it's retired. One machine becomes many nodes over its life. That matters because `decommission-node` retires a node id permanently (`crl.retired_nodes`, `src/cluster/retirement.rs`), so a machine that leaves `home` as `home-3` and comes back later has to come back under a new node id. The fleet tracks machine to node history; clusters never need to know machines exist.

---

## 2. Prior art

### 2.1 Sidero Omni (for Talos)

Omni is the closest thing to what the maintainer described, so it gets the most space.

**Resource model.**
- **Machines** register by booting Talos with SideroLink kernel arguments baked into the ISO or PXE image, or by applying a "Machine Join Config" in maintenance mode. Once joined, "the local Talos API is disabled ... all future configuration changes must be made through Omni". Initial labels (`--initial-labels environment=production`) are baked into the image's schematic. System labels such as `omni.sidero.dev/arch` and `omni.sidero.dev/cores` come from inventory.
- **MachineClasses** are label selectors: `matchlabels: ["omni.sidero.dev/arch = amd64, omni.sidero.dev/cores > 2"]`, with commas meaning AND and separate entries meaning OR. A class is either a manual selector over the existing pool or an `autoProvision` class that asks an infrastructure provider to create machines.
- **Cluster templates** are multi-document YAML (`Cluster`, `ControlPlane`, `Workers`, `Machine`) applied with `omnictl cluster template sync`. A machine set lists explicit machine ids *or* `machineClass: {name, size}`, where size can be `unlimited`. Changing the control-plane set "triggers a rolling scale-up or scale-down". Config patches don't combine with class-based sets yet (issue #2593).

**Returning a machine.** A graceful removal calls Talos's reset API, wipes the STATE partition (machine config and cluster credentials) and reboots into maintenance mode. The machine "appears in Omni as an available, unassigned machine", so it goes back to the pool on its own. **Extra data disks aren't wiped** ("disk wiping is a separate concern"). If the machine is unreachable, Omni "drops its record of the machine without performing a reset", which leaves cluster credentials on a box that's no longer tracked.

**SideroLink.** A point-to-point WireGuard overlay. Talos generates an ephemeral WireGuard key, opens gRPC to Omni, exchanges public keys and gets overlay IPv6 addresses. With `grpc_tunnel=true` it can tunnel WireGuard over the gRPC connection when UDP is blocked. Machines only need outbound 443 plus the WireGuard port. Sidero's own case for it: manage machines behind NAT, and keep machines that are waiting for config from being "open targets" on the network.

**Identity.** All new machines register with a *shared* join token from the kernel command line. After the first connection Omni issues a **unique per-machine token**, which Talos persists when it installs to disk. From then on a mismatched token is rejected, and a UUID collision with an existing machine gets a fresh UUID (issue #840, PR #924). So the identity is the SMBIOS UUID plus an Omni-issued secret. I found no primary source saying Omni uses a TPM for join identity **[unverified]**. The design had real bugs: machines with all-zero or duplicate SMBIOS UUIDs shared a SideroLink address, so maintenance config could reach the wrong machine (issue #3443, fixed September 2026).

**Image Factory.** A *schematic* (extra kernel args, META values, system extensions) has a content-hash id and applies to any Talos version. The factory serves ISOs, disk images, a container registry, and PXE at `/pxe/<schematic>/<version>/<platform>`. Its README warns that schematics can carry sensitive data such as join tokens in kernel args.

**State and HA.** Omni stores its state in etcd, embedded for a single instance or external for HA. With external etcd, several replicas hold an election and **only one is active**. The Helm chart says Omni "currently only supports a single replica". If Omni is down, clusters keep running, and a "break glass" mode gives direct Talos and Kubernetes access, after which the cluster is "tainted" until its CA is rotated. Auth needs Auth0, SAML or OIDC. The server is **BUSL-1.1**.

**Upgrades.** Per cluster: control-plane nodes one at a time with etcd health gates, then workers by `upgradeStrategy` and `maxParallelism`. "Locked" machines skip upgrades, which doubles as a canary tool. I found nothing on orchestrating upgrades *across* clusters.

**Bare-metal infrastructure provider.** It runs a ProxyDHCP and TFTP/iPXE, boots machines into a diskless Talos "Agent Mode" with a metal agent, and drives power over **IPMI or Redfish**. Accepting a machine "will wipe ALL disks". Deallocation PXE-boots the machine into agent mode, wipes its disks and powers it off.

**Known pain (from Omni's issue tracker).**
- Cluster teardown hangs forever on one unreachable machine (#2465, #1044, #583, #2035, #1995). The escape hatch is a flag, `--destroy-disconnected-machines`.
- Machines get stuck in "Deprovisioning" when the provider is offline, and even admins can't remove the finalizer (#2702, open).
- A machine joined only through Machine Join Config loses its Omni connection after a reset, because the reset wipes the join config (#2637, open). A maintenance-mode upgrade can drop the SideroLink kernel args, and the machine never reconnects (#3427, open).

**What we take from Omni:**
- The resource shape: machines with inventory labels, classes as label selectors, per-cluster counts per class.
- An automatic return to the pool after release.
- A control-plane-issued per-machine secret replacing a shared bootstrap credential.
- The fleet being off the data path, and single-active-instance HA being fine.

**What we avoid:**
- Keying identity on SMBIOS UUIDs, which are cheap, duplicated and spoofable.
- Keeping the fleet credential somewhere a reset wipes.
- Teardown that blocks forever on an unreachable machine.
- Credentials in kernel args or image URLs.
- Leaving data disks unwiped on release.
- A second PKI and a mandatory WireGuard overlay.

### 2.2 Metal³/Ironic and MAAS

*Pending the second research briefing; see the checklist.*

### 2.3 Tinkerbell and Foreman

*Pending.*

### 2.4 Incus OS, Kairos AuroraBoot

*Pending.*

### 2.5 What the prior art adds up to

*Pending.*

---

## 3. Where it lives: separate, integrated or hybrid?

The maintainer offered three shapes. Here they are against the things that actually differ.

| | **(a) Separate repo and binary** (`reliaburger-fleet`) | **(b) Fully integrated in bun** (fleet state in a cluster's Raft) | **(c) Hybrid, one repo: machine agent in bun, fleet in `relish fleet`** |
|---|---|---|---|
| Machine-side agent | Still has to be in bun: it's the only process on the appliance. So (a) is really a hybrid across two repos. | bun | bun |
| Protocol versioning | Two release trains. The machine protocol in bun and the fleet that speaks it can skew, so we need a compatibility matrix. | One train | One train. The fleet and machine protocol ship together, tested together in one CI. |
| Install story | A third binary to download, sign and upgrade | Nothing new | Nothing new: `relish` is already on the operator's laptop |
| Where fleet state lives | Its own store | Inside one cluster: that cluster "owns" machines destined for other clusters, and a cluster outage blocks fleet operations everywhere | Its own small store (a file in v0, `redb` in v1), anywhere: the laptop, a spare Wyse, or as an app on a cluster |
| Blast radius | Separate | Fleet credentials for every cluster sit in one cluster's Raft | Separate, like (a) |
| Binary size | bun and relish unchanged | Small growth in bun | Small growth in relish (an estimated few thousand lines, no new heavy crates) |
| Fits "batteries included, one binary" | No | Yes | Yes |
| Who's served | People who want it can opt in | Everyone gets it whether they like it or not | Everyone has it; nobody pays for it until they run `relish fleet init` |

**Decision: (c).** The machine half has to live in bun whichever way we go, so a separate repo only adds version skew and an install step. Putting the fleet's *state* inside a cluster couples clusters that the whitepaper deliberately keeps independent (§21.6: "cluster state is sovereign"). A `relish` subcommand keeps the operator-side code next to the other operator-side code (`relish netboot`, contexts, `relish cluster create --bare-metal`) and needs no new binary.

**What would change my mind:** if fleet grows a multi-tenant SaaS with its own auth, billing and UI (Omni's shape), that's a product, not a battery, and a separate repo makes sense then. The machine protocol should be specified in a design doc (`docs/design/fleet.md`) with a version field from day one, so a later split stays possible.

---

## 4. Machine lifecycle

### 4.1 States

States are from the fleet's point of view. The machine keeps its own local state (a subset of these) on its identity partition (§9.1), and the fleet reconciles the two.

```
                 netboot seen (MAC, arch)
                        │
                        ▼
  ┌──────────┐    ┌──────────┐   install ok   ┌───────────┐
  │  Failed  │◀───│ Booting  │───────────────▶│ Unclaimed │◀───────────────────────┐
  │ (install)│    └──────────┘                └─────┬─────┘                        │
  └──────────┘                                      │ enrol (fingerprint or TOFU)  │
                                                    ▼                              │
                                             ┌────────────┐                        │
                     ┌──────────────────────▶│ Available  │◀───────────┐           │
                     │   wipe verified       │  (pool)    │            │           │
                     │                       └─────┬──────┘            │           │
                     │                             │ allocate          │           │
               ┌─────┴────┐                        ▼                   │  join     │ forget
               │  Wiping  │                  ┌────────────┐  refused / │ timeout   │ (factory
               └─────▲────┘                  │ Allocated  │────────────┘           │  reset)
                     │                       └─────┬──────┘                        │
                     │ bun stopped +               │ token pushed                  │
                     │ identity retired            ▼                               │
               ┌─────┴────────┐              ┌────────────┐                        │
               │Decommissioning│             │  Joining   │                        │
               └─────▲────────┘              └─────┬──────┘                        │
                     │ drained                     │ node Ready in cluster         │
               ┌─────┴────┐                        ▼                               │
               │ Draining │◀─── release/move ┌────────────┐                        │
               └──────────┘                  │   Member   │                        │
                                             └────────────┘                        │
                                                                                   │
   Orthogonal conditions: Unreachable, Upgrading, Quarantined ─────────────────────┘
```

| State | Meaning | Who holds the truth |
|---|---|---|
| **Booting** | `relish netboot` served this MAC an installer, but no bun has announced itself yet | netboot's log |
| **Unclaimed** | bun is up in appliance mode, advertising over mDNS, not enrolled in any fleet. Its machine key exists; its fleet pin doesn't. | the machine |
| **Available** | Enrolled in this fleet, holding no cluster identity, data partition freshly wiped, OS at the fleet's pool pin. This is the pool. | fleet and machine agree |
| **Allocated** | The fleet has chosen a cluster and a node id, and is minting or has minted a join token | fleet |
| **Joining** | The machine holds a token and is enrolling (CSR, then G1 master-key fetch, then starting bun `--cluster`) | machine |
| **Member** | The cluster lists the node as Ready | the cluster |
| **Draining** | The node is cordoned and its workloads are moving elsewhere | the cluster |
| **Decommissioning** | bun has stopped on the machine; the fleet is retiring the node id in the cluster (`POST /v1/nodes/decommission`) | the cluster's CRL |
| **Wiping** | The data partition is being crypto-erased and recreated (§9) | the machine |

**Failure states and conditions:**
- **Failed(install)**: netboot served an installer but no bun ever announced. Retry means a power cycle, so it's a human job without power control.
- **Failed(join)**: the token expired or was refused, or the node never became Ready. The fleet retries once with a fresh token under a *new* node id, then goes to Available via Wiping (the machine might hold a half-written identity).
- **Unreachable** is a condition, not a state: a Member the cluster still sees but the fleet can't reach is fine (the fleet is off the data path); an Available machine that vanished is probably unplugged. The fleet shows how long it's been unreachable and never blocks on it (Omni's #2465 lesson).
- **Quarantined**: the machine's self-report contradicts the fleet (a different key for a known MAC, a fleet pin that isn't ours, a wipe report that doesn't verify, a node id the fleet never assigned). The fleet refuses to allocate it and a human decides. It's the answer to "someone reinstalled this box by hand" as much as to an attacker.
- **Upgrading** is a condition on Available or Member machines (§11).
- **Retired** (terminal): hardware is dead or gone. The fleet keeps the record for history.

### 4.2 Transitions that need care

- **Member → Draining** is the only transition that affects running workloads. It needs a preflight (§10.1).
- **Decommissioning must come before Wiping**, not after. If the disk is wiped first and the cluster is unreachable, the cluster keeps the node's placement and registry obligations until someone decommissions it by hand, and the fleet has lost the machine's proof that bun stopped. When the source cluster is gone for good, `--force` wipes anyway and records a *pending decommission* against that cluster.
- **Unreachable machines never block a cluster-level operation.** Omni's teardown hangs are the anti-pattern. `relish fleet release --unreachable` retires the node id in the cluster immediately (the operator attests the machine is off, as `decommission-node` already requires) and marks the machine "wipe on next contact": when it next announces itself, the fleet wipes it before anything else.
- **Available → Unclaimed** ("forget") needs physical presence or the fleet's signature. Without that rule, anyone on the LAN could send "forget" and then claim the machine.

### 4.3 Sketch in Rust

Following `CLAUDE.md` (state machines as exhaustive enums, no stringly-typed states). This is a sketch for the design doc, not code to merge:

```rust
/// Where a machine is in its life, as the fleet sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MachineState {
    Booting,
    Unclaimed,
    Available,
    Allocated { cluster: ClusterName, node_id: NodeId },
    Joining { cluster: ClusterName, node_id: NodeId },
    Member { cluster: ClusterName, node_id: NodeId },
    Draining { cluster: ClusterName, node_id: NodeId, next: AfterDrain },
    Decommissioning { cluster: ClusterName, node_id: NodeId, next: AfterDrain },
    Wiping { next: AfterWipe },
    Failed(FailureReason),
    Quarantined(QuarantineReason),
    Retired,
}
```

The `next` fields make "move" a single state walk rather than two commands glued together, so a fleet restart mid-move resumes instead of forgetting where it was.

---

## 5. Identity and trust

### 5.1 What we can and can't prove about a Wyse

The Wyse 3040 has no TPM, and Secure Boot is off (appliance research §9.1). So:
- **Hardware identifiers are claims, not proofs.** MAC, SMBIOS UUID and serial number are whatever the software on the box says they are. Omni's UUID collisions (#3443) show they're not even unique in practice. We record them as inventory to help humans match boxes to labels, and never use them to authorise anything.
- **Nothing proves the software is ours.** With Secure Boot off, a LAN attacker who wins the ProxyDHCP race can serve a modified installer (appliance research §4.7). Integrity comes from the Ed25519 checks the installer does, and those only help if the installer itself is ours.
- **What we *can* have is a key that lives on the machine** and is shown on its console. The console is an out-of-band channel an attacker on the LAN can't forge. That's the whole foundation.

### 5.2 Three keys, three trust relationships

| Key | Created by | Lives in | Proves | Rotates |
|---|---|---|---|---|
| **Machine key** (Ed25519) | bun, on first boot of `Unclaimed` | the identity partition (§9.1), mode 0600; sealed to the TPM when there is one (v2) | "this is the same box as before", across wipes and cluster moves | only on factory reset |
| **Fleet key** (Ed25519, plus a small fleet CA for mTLS) | `relish fleet init` | the fleet's state directory (and its backup) | "this command comes from the fleet this machine is enrolled in" | by a signed handover, rare |
| **Cluster identities** (existing Sesame PKI) | each cluster | the data partition, wiped on every move | "this node belongs to cluster X" | as today |

**Fleet enrolment (once per machine lifetime).** The fleet fetches the machine's public key from its claim API and shows it next to what the console shows: a short fingerprint and a QR code. When the operator confirms, the fleet sends an enrolment record signed by the fleet key: `{machine_id, machine_pubkey, fleet_pubkey, fleet_endpoints, enrolled_at}`. The machine stores it and **pins the fleet key**. From then on:
- the machine accepts commands only over mTLS from a certificate chaining to the pinned fleet CA;
- the fleet accepts status only from the pinned machine key;
- another claimer gets `409 Conflict: enrolled in fleet <fingerprint>`, so a LAN attacker can't take a box you've enrolled.

**Cluster allocation (every move, no ceremony).** The fleet mints a join token and sends it over the pinned channel, and the machine uses the existing CA-pinned join (`relish join`'s logic, in process). Both ends already trust each other, so nobody walks to a console.

### 5.3 Enrolment modes

Pick per enrolment, loudest first:

1. **`--confirm` (the default).** The fleet lists each machine's fingerprint, and you compare it with the console or scan the QR code with a phone. That's fine for ten boxes and tedious for a hundred.
2. **`--expect N --window 15m`, a TOFU boot window.** You're netbooting a batch right now. The fleet enrols the first N machines that appear through *its own* `relish netboot` during the window, and refuses (and loudly reports) machine N+1. An attacker has to be on the same LAN, inside those 15 minutes, and would get caught by the count. That's the right default for a homelab rack you're standing next to.
3. **`--allow-mac <file>`.** An inventory list from the purchase order narrows the window further. It's weak against a deliberate attacker (MACs are spoofable), but it stops the colleague's laptop that PXE-boots by accident.
4. **TPM EK allow-list (v2, hardware with a TPM).** Enrolment requires a quote from a TPM whose endorsement key is on the list. That's what `AttestationMode::Tpm` was meant for. It makes enrolment unattended *and* strong, and it's the one mode that survives a malicious installer. It isn't possible on the Wyse.

### 5.4 Threats and answers

| Threat | Without the fleet | With the fleet |
|---|---|---|
| A LAN attacker impersonates an *unclaimed* machine to get a join token (and then `master.key` via G1) | appliance research §4.2 (d): the residual TOFU risk | Same risk, but only at fleet enrolment, once per machine. Modes 1–3 bound it. After enrolment, impersonating a machine needs its private key. |
| A LAN attacker claims an unclaimed machine before you do | You see it as "claimed" and walk to it | Same. The operator sees `enrolled in another fleet` and factory-resets the box at its console. That's denial of service, not compromise: the attacker gets a blank box and no secrets. |
| A LAN attacker impersonates the fleet to an *enrolled* machine | n/a | Fails: the machine pins the fleet CA |
| A LAN attacker impersonates the fleet to an *unclaimed* machine | n/a | Possible, and equivalent to the previous row: the machine enrols with the attacker, and the operator notices it missing. The optional `rb.fleet=<url>#<fingerprint>` on the kernel command line (§6.3) pre-pins the fleet when the netboot server is ours. |
| The fleet host is compromised | n/a | The attacker holds an `Enroller` credential for every cluster (§5.5). It can enrol rogue nodes, which then fetch `master.key` via G1. **So the fleet is as sensitive as cluster admin**, until nodes stop needing the master key (security-sesame's "separation of privilege" target). Keep the fleet on a trusted host, back up its key sealed, and make every enrolment visible in `relish nodes` and the audit log. |
| A stolen Wyse | The disk holds `master.key` | Same while it's a member. A wiped (Available) box holds only the machine key and the fleet pin. Without a TPM, encryption at rest can't protect a member's disk from theft, because the key sits beside the data (§9.2). |
| A malicious installer on the LAN (Secure Boot off) | Possible | Possible. The fingerprint check still stops silent enrolment, and an enrolled machine never re-installs from the network unless the fleet tells it to. |

### 5.5 A scoped credential per cluster: the `Enroller` role

Today `join-token create` and node decommission require `ApiRole::Admin` (`src/sesame/types.rs`; `src/bun/api.rs` tests). A fleet holding Admin tokens for every cluster is a bigger target than it needs to be. Add a fourth `ApiRole`, `Enroller`, allowed only to:
- create, list and revoke join tokens (the list and revoke are appliance gap G6);
- cordon, drain and uncordon nodes (gap F1, §14);
- decommission nodes;
- read `nodes`, `council` and the OS and upgrade status;
- set the cluster's OS pin (§11).

`relish fleet cluster add home` mints one (it needs an Admin context once) and stores it in the fleet's state, encrypted at rest with the fleet key.

---

## 6. Discovery

### 6.1 mDNS on the LAN (v0)

bun in appliance mode announces `_reliaburger-machine._tcp` (the appliance note's `_reliaburger-unclaimed._tcp`, generalised to every state), with no secrets in the TXT record:
- `v=1` (the protocol version);
- `mk=<machine key fingerprint>`;
- `st=unclaimed|available|member`;
- `fl=<fleet fingerprint or empty>`;
- `os=<OS version>`, `bun=<bun version>`, `arch`, `cpu`, `mem`, `disk`.

`relish fleet machines` browses it. mDNS is one L2 segment and dies with AP isolation or VLANs, exactly like ProxyDHCP, so it matches the netboot boundary we already accept. It's embedded in bun (the appliance note suggests the `mdns-sd` crate), not Avahi.

### 6.2 Netboot registration

`relish netboot` already sees every DHCP discover it answers (MAC, client arch). When it runs as part of the fleet (`relish fleet serve` or `relish fleet netboot`), it records **Booting** machines before bun exists, which is what makes the TOFU window (§5.3) and Failed(install) possible. It's also the only discovery that works for a box that never reaches bun.

### 6.3 Phone-home (v1 and multi-site)

The machine dials the fleet instead of waiting to be found. The fleet URL comes from, in order:
1. the enrolment record, for enrolled machines;
2. `rb.fleet=https://host:port#sha256:<fingerprint>` on the kernel command line, which our netboot's iPXE script appends. With Secure Boot on, the UKI's command line is signed and fixed, so the URL would come from a signed UKI addon instead: systemd-stub appends the `.cmdline` of verified `*.addon.efi` files. That's a per-fleet artefact, like Omni's schematic, but it holds only a URL and a public-key fingerprint, **never a token** (Image Factory's README warns about exactly that);
3. mDNS, as a fallback.

### 6.4 Why not gossip?

Mustard gossip authenticates with an HMAC key derived from the cluster's master key (`docs/design/gossip-mustard.md`). Unclaimed machines don't have one and must never get one, and pool machines belong to no cluster. A fleet-wide gossip ring would be a second membership protocol whose only job mDNS and phone-home already do. Not worth it.

---

## 7. The fleet's own state and HA

**What the fleet stores:**
- machine records (id, public key, inventory, labels, current state, node history);
- the fleet key and CA;
- one `Enroller` credential per cluster, plus each cluster's CA fingerprint and endpoints;
- policy: classes, per-cluster targets, the OS and bun pins;
- in-flight operations (the `next` fields in §4.3);
- an audit log.

That's kilobytes per machine.

**What the fleet can lose without real harm.** Machines know their own state, key and fleet pin. Clusters know their members, CRLs and OS pins. So if the fleet disk dies, a restored fleet key plus `relish fleet rescan` (browse mDNS, ask every known cluster for `nodes`, ask every machine for its self-report) rebuilds the machine table. The irreplaceable part is small: **the fleet key, the per-cluster credentials and the policy**. Policy can live in git, where Lettuce-style GitOps already knows how to keep TOML.

| Option | Verdict |
|---|---|
| **v0: a state file on the operator's machine** (`~/.config/reliaburger/fleet/`, TOML or JSON, written atomically) | Right for v0: no daemon, and everything is imperative and short-lived. The laptop's `relish` contexts already live there. |
| **v1: `relish fleet serve` with `redb`**, single instance, sealed backups to object storage (reusing `object_storage.rs` and the council backup sealing pattern) | **Recommended.** `redb` is already a dependency, so no SQLite and no new crate. It runs anywhere: the laptop, a spare Wyse, or as an app on a cluster. |
| Fleet state in one cluster's Raft, as a built-in subsystem | No. The cluster owns its peers' machines, a cluster outage stops fleet operations, and fleet credentials for every cluster sit in one cluster's Raft (§3). |
| A Raft group of fleet replicas (`openraft` is already a dependency) | No for v1. Omni itself runs active/passive with one active instance. The fleet is off the data path, and three replicas triple the hardware for a service you use a few times a week. Revisit only if the fleet grows unattended reconciliation that must not pause, such as auto-replacing dead nodes at a remote site. |
| A Kubernetes-style management cluster (Cluster API, Tinkerbell) | No. That's the dependency stack this project exists to remove. |

**Running the fleet on a cluster it manages.** It's tempting (a Wyse is always on), and it works with one guard: the fleet must refuse to drain, move or wipe the node it's running on, and the node that holds its volume. That's the chicken-and-egg that makes Cluster API "self-hosted management clusters" fiddly, and a single refusal rule is enough to avoid it here **[inference]**.

---

## 8. Allocation policies

### 8.1 Classes and labels

Inventory facts come from the machine's self-report: arch, CPU model and cores, RAM, disk size and type, NIC speed, DMI vendor and product. Operators add labels (`site=shelf-1`, `power=strip-a`). A class is a label selector, following Omni:

```toml
# fleet.toml (policy; can live in git)
[class.wyse]
match = ["product = Wyse 3040", "mem_mb >= 1900"]

[class.big]
match = ["arch = x86_64", "cores >= 8", "mem_mb >= 16000"]

[cluster.home]
endpoint = "https://home-1.lan:9117"
os = "2026.41.0"
[[cluster.home.pool]]
class = "wyse"
count = 5
voters = 3
spread = "power"          # prefer different power strips

[cluster.lab]
endpoint = "https://lab-1.lan:9117"
[[cluster.lab.pool]]
class = "wyse"
count = 3
donor = true              # the fleet may take machines from here for other clusters
```

### 8.2 v0: imperative, with dry-run

`relish fleet allocate --to lab --class wyse --count 2` picks from Available and prints its plan first. Choosing machines within a class:
1. the right OS version (skip an upgrade);
2. `spread` across a label (failure domains);
3. the oldest in the pool, which spreads eMMC wear.

### 8.3 v1: a reconciler

`relish fleet serve` compares `count` per cluster and class with reality:
- **too few**: it allocates from Available;
- **pool empty**: it takes from a `donor = true` cluster only if that cluster stays at or above its own `count`, which a donor's `min` makes explicit;
- **too many**: it releases the newest-joined non-voter.

It never moves a voter automatically, and it never goes below a cluster's council minimum. Each action goes through the same preflight as a manual move (§10.1). It's deliberately dumb: no bin-packing across clusters, no cost model. That's the level of placement MAAS and Omni ship, and the scheduling that matters happens inside each cluster.

### 8.4 Dead-node replacement

When a cluster reports a node Dead for longer than `replace_after` (say 30 minutes) and the fleet can't reach the machine, the fleet can (v1, opt-in):
1. retire the node id with the operator's standing attestation. This is an explicit policy flag, because `decommission-node` exists precisely so a human attests that the workloads are fenced;
2. allocate a replacement from the pool;
3. mark the dead machine "wipe on next contact".

Without power control, fencing is only as good as "it's unreachable", which is the split-brain risk `decommission-node`'s attestation was designed to make explicit. So it's off by default.

---

## 9. Wipe semantics

### 9.1 Layout change: a small identity partition

The S1 layout (PR #259) has an ESP, `/usr` A/B with verity, and one Btrfs data partition that holds `/etc` and `/var`. The machine key and fleet pin have to survive a wipe of that data partition (Omni's #2637: a reset that wiped the join config lost the machine). So add a **16 MiB `reliaburger-machine` partition** (ext4) created by first-boot repart. It holds:
- the machine key;
- the fleet enrolment record;
- the data partition's key file (§9.2);
- a wipe log.

It costs nothing on the 8 GB budget. A factory reset wipes it too, and that's the only thing that turns an Available machine back into Unclaimed.

### 9.2 Crypto-erase

Overwriting eMMC or SSD doesn't guarantee erasure, because wear levelling keeps stale copies of blocks. The reliable, fast answer is to make the data unreadable by destroying its key:
- **Encrypt the data partition with LUKS2** (dm-crypt; the Wyse's Atom has AES-NI, so the overhead should be small **[unverified on the Z8350]**). Use a random volume key, unlocked by a key file on the identity partition, or TPM-sealed where there's a TPM (appliance Phase 4 already plans TPM2-sealed data encryption).
- **Wipe:**
  1. `cryptsetup erase` removes every keyslot and so the volume key. Its man page says the data "will be permanently irretrievable", and also that erase "does not wipe or overwrite the data area".
  2. Delete the key file from the identity partition.
  3. `blkdiscard` the data partition; use `--secure` where the device supports it. On eMMC 4.4+ that maps to secure trim or erase, which also purges garbage-collected copies. Whether the Wyse's eMMC supports it is **[unverified]**.
  4. Recreate the LUKS container with a fresh key and a fresh Btrfs, and reboot.
- **What it buys without a TPM:** complete, instant erasure on reallocation, which is what a move needs. **What it doesn't buy:** protection of a *member's* data from someone who steals the box, because the key file sits next to the data. Say so plainly in the docs.
- **The honest limit.** A wipe is only as trustworthy as the software doing it. A tampered machine can say it wiped and not do it. With Secure Boot off, that's the Wyse's situation, and no protocol fixes it. The mitigations sit on the cluster side (§9.4).

### 9.3 What "verified" means

After a wipe the machine reports, signed with its machine key:
- the old and new LUKS UUIDs;
- `erase_method` (`luks-erase+secure-discard` or `luks-erase+discard`);
- that the identity partition holds no cluster material;
- that bun reports no node identity and no Raft directory.

The fleet checks that the new UUID is new, the report's signature, and that the old node id is in the source cluster's CRL. If any check fails, the machine goes to Quarantined, not back to the pool. It's a consistency check, not attestation. A TPM quote over the measured boot plus the report would make it attestation (v2).

### 9.4 What a wipe can't fix: the master key

A machine that was a member of `home` held `home`'s `master.key` (appliance gap G1 delivers it to every node). Crypto-erase destroys it on an honest machine. But moving a box between clusters of *different owners*, or after it sat somewhere untrusted, means assuming the old master key leaked. The real fix is master-key rotation (appliance gap G5). Until G5 exists, the fleet should say so when a move crosses a trust boundary (clusters tagged with different `owner` labels), and recommend rotation when it's available.

---

## 10. Moving a node between clusters

### 10.1 Preflight (refuse early, explain why)

`relish fleet move m-7f3a9c --to lab --dry-run` checks the following against the source cluster:
1. **Capacity.** Would the remaining nodes still fit the placements? Ask the source scheduler for a what-if, because relish already has `plan` and `diff`. If a proper what-if API doesn't exist, that's gap F7.
2. **Council.** If the node is a voter, the move needs at least three voters left, or a demotion first. How the council reconciler treats a retired voter needs checking before implementation: `src/council/node.rs` refuses retired voters in membership changes **[inference: I didn't trace the reconciler's replacement path]**.
3. **Node-local state.** Btrfs volumes are local to the node (`docs/design/agent-bun.md`). If the node holds volumes, refuse unless `--allow-volume-loss` is given or they've been snapshotted elsewhere. Pickle's replicated registry and Raft are fine, since they're replicated.
4. **Target readiness.** The target cluster is reachable, the `Enroller` credential works, and the target's OS pin is known.

### 10.2 The sequence

| # | Step | Who | Existing code? |
|---|---|---|---|
| 1 | Record `Draining { next: MoveTo(lab) }` | fleet | new |
| 2 | Cordon and drain the node in `home`; wait until its placements are healthy elsewhere, with a timeout | fleet → `home` API | **gap F1**: there's no first-class drain. `relish fault node-drain` (Smoker) sets a drain gate that stops scheduling but keeps transports (`src/bun/agent.rs` test `node_drain_stops_scheduling_but_keeps_cluster_transports`), which is the right mechanism behind a chaos-only door. |
| 3 | Tell the machine to stop bun; it replies with a signed "bun stopped, no workload processes left" | fleet → machine | new (machine protocol) |
| 4 | `POST /v1/nodes/decommission` with `workloads_stopped = true` and reason `fleet move to lab (m-7f3a9c)` | fleet → `home` | exists (`src/cluster/retirement.rs`) |
| 5 | Crypto-erase, recreate, reboot, report (§9) | machine | new |
| 6 | If the machine's OS isn't the target's pin, update it while it's Available (§11) | machine | appliance Phase 3 |
| 7 | Mint a node-bound join token for `lab-4` with `lab`'s `Enroller` credential | fleet → `lab` | exists (`join-token create`) |
| 8 | Push `{token, node_id, ca_fingerprint, endpoints, advertise_address}` down the pinned channel | fleet → machine | the appliance claim payload |
| 9 | Join in process, fetch `master.key` (G1), write `node.toml`, start bun `--cluster` | machine | join exists; G1 is new |
| 10 | Wait for `lab` to report `lab-4` Ready; record `Member` | fleet | new |

Every step is idempotent and recorded before it starts, so a crash anywhere resumes. Steps 3 and 5 need the machine; every other step can proceed against a dead machine with `--unreachable` (§4.2).

**Node ids.** `lab-4` is new, because retired ids are refused forever (`src/sesame/join.rs` refuses ids in `crl.retired_nodes`). The fleet picks the next ordinal per cluster and keeps the machine's history (`m-7f3a9c: home-3 (Oct–Nov), lab-4 (Nov–)`).

---

## 11. OS and bun upgrades across clusters

The fleet doesn't replace bun's orchestrator. Inside a cluster, `src/upgrade/orchestrator.rs` and appliance Phase 3's `os.target_version` pin already do the council-aware rolling, health gates and fallback. The fleet does two things on top:
1. **Keep the pool current.** Available machines belong to no cluster, so the fleet upgrades them directly: the same verify-then-`systemd-sysupdate` path, one machine at a time. A machine then joins at the target's version without a rolling upgrade straight after its first boot.
2. **Roll pins across clusters in waves.** `relish fleet os pin 2026.42.0 --wave lab,home` sets `lab`'s pin, waits for `lab`'s rollout to report done and healthy for a soak period, then sets `home`'s. If a wave fails, the fleet stops and leaves every other cluster where it was. The same applies to bun (`relish upgrade` per cluster). Omni does waves only inside a cluster; across clusters, this is new.

A cluster is never forced. The pins are the cluster's own Raft values, set through its API with the `Enroller` role, and an Admin can override them locally. The fleet reports the drift.

---

## 12. Networking: do we need a WireGuard overlay?

SideroLink exists for three reasons (§2.1):
- machines behind NAT;
- a management channel that works without inbound ports;
- keeping unconfigured machines from being open targets.

For v0 and v1 on one LAN:
- **NAT:** not an issue. The fleet and the machines share a LAN, as netboot and mDNS already require.
- **An authenticated channel:** mTLS with pinned keys (§5.2) gives confidentiality and authentication without a kernel interface, a second address plan or a WireGuard key exchange.
- **Unclaimed machines as targets:** the claim API is the only thing listening on an unclaimed box. It exposes a public key and accepts one enrolment, and it holds no secrets to steal. The appliance note's firewall rules already restrict bun's ports once it's a member.

**Multi-site (v2): phone-home, not a mesh.** The machine keeps one outbound TLS connection (a WebSocket or HTTP/2 stream) to the fleet URL (§6.3), and the fleet sends commands down it. That's SideroLink's useful property (outbound-only) without WireGuard, using the same message types as the LAN push. It carries control only. Cluster traffic between nodes still needs routable addresses, which is the cluster's concern (and Franchise's), not the fleet's. WireGuard becomes worth it only if the fleet must reach *cluster APIs* behind NAT too, and then a hosted relay is a v3 question.

---

## 13. CLI and UI

`relish fleet` replaces the appliance note's `relish machines` family, so there's one vocabulary:

```text
relish fleet init                               # fleet key + CA + state dir (v0: local)
relish fleet machines [--state available]       # inventory: mDNS + netboot + cluster views
relish fleet enrol <machine…|--all> [--confirm | --expect N --window 15m | --allow-mac FILE]
relish fleet label <machine> site=shelf-1
relish fleet cluster create home --class wyse --count 5 --voters 3
                                                # bare-metal PKI on the laptop (appliance §4.4),
                                                # claims the first machine with the bootstrap bundle
relish fleet cluster add lab --context lab      # adopt an existing cluster: mints an Enroller token
relish fleet allocate --to lab (--class wyse --count 2 | <machine…>) [--dry-run]
relish fleet release <machine> [--unreachable]  # drain → decommission → wipe → Available
relish fleet move <machine> --to home [--dry-run] [--allow-volume-loss]
relish fleet wipe <machine>                     # Available machines only
relish fleet forget <machine>                   # factory reset → Unclaimed (needs the fleet signature)
relish fleet os pin 2026.42.0 [--wave lab,home] # also: relish fleet bun pin 0.2.0
relish fleet status                             # per cluster: members by class, pool, drift, in-flight ops
relish fleet netboot                            # relish netboot, recording Booting machines in the fleet
relish fleet serve                              # v1: the long-running service (API + reconciler)
relish fleet rescan                             # rebuild the machine table from the edges (§7)
```

**UI.** v1 serves a small Brioche view from `relish fleet serve`: a machine table (state, cluster, class, OS, last seen), per-cluster capacity bars, and in-flight operations. The TUI (`relish tui`) gets a Fleet tab. Nothing in v0.

---

## 14. Gaps in the repo

Carried from the appliance research (G1–G6) where they block the fleet, plus new ones:

| # | Gap | Needed for |
|---|---|---|
| G1 | Master-key delivery after join (appliance research §4.3) | any unattended allocation |
| G3/G4 | Advertise-address detection; fleet-assigned node names | allocation |
| G5 | Master-key rotation | moves across trust boundaries (§9.4) |
| G6 | `join-token list/revoke` | the `Enroller` role, cleaning up failed joins |
| F1 | A first-class **cordon/drain** API and `relish node drain` (the mechanism exists behind Smoker's fault door) | release, move |
| F2 | The **`Enroller` API role** (§5.5) | a least-privilege fleet |
| F3 | An **identity partition and LUKS data partition** in the image layout (§9.1–§9.2); changes PR #259's repart files | wipe |
| F4 | The **machine protocol** in bun appliance mode: machine key, fleet pin, status report, stop-bun, wipe, apply-allocation; versioned | everything |
| F5 | Council voter demotion before decommission, verified in tests (§10.1) | moving voters |
| F6 | A volume-locality check (does this node hold volumes?) exposed through the API | move preflight |
| F7 | A scheduler what-if ("would the cluster still fit without node X?") | move preflight; the reconciler's donor logic |

---

## 15. Open questions for the maintainer

1. **Fold appliance Phase 2 into fleet v0?** I'd build the claim flow as fleet enrolment plus allocation from the start (machine key on an identity partition, a fleet pin) and call the commands `relish fleet …` rather than `relish machines …`. That changes PR #218's Phase 2 wording and PR #259's partition layout (F3).
2. **The TOFU boot window as the homelab default** (§5.3 mode 2), or `--confirm` everywhere? The first is ten power buttons; the second is ten console checks.
3. **The `Enroller` role**: a new `ApiRole` variant, or a scope on Admin tokens? A new role is clearer and easier to audit.
4. **LUKS on the data partition** for every appliance, given that without a TPM it buys crypto-erase but not theft protection? Alternatives: LUKS only when a fleet is used, or no encryption and `blkdiscard --secure` only.
5. **Where does the v1 fleet run by default?** The laptop (simple, but sleeps), a dedicated spare Wyse (always on, and one more box), or as an app on a managed cluster (with the self-protection rule in §7)?
6. **Dead-node auto-replacement** (§8.4): ship it opt-in in v1, or leave it out until there's power control to fence properly?
7. **Power control.** The Wyse has no BMC. Is Wake-on-LAN plus "press the button" acceptable forever, or should v2 consider smart plugs (an HTTP power driver) the way MAAS has a manual power type **[unverified whether the 3040 does Wake-on-LAN from S5]**?
8. **Franchise overlap.** Should the fleet's multi-cluster status view become Franchise's overview later, or stay separate? I'd keep them separate: the fleet is about machines, Franchise is about services.
9. **Licence and packaging.** Apache-2.0, in-tree, default-on (no cargo feature)? Omni being BUSL is an opening worth keeping.

---

## 16. Phased plan

Effort is in engineer-weeks, including tests-first work and book and manual updates per `CLAUDE.md`. The estimates assume appliance Phase 1 (appliance mode, seed, G1/G2) and Phase 2b (`relish netboot`) exist.

### v0: CLI-only claim, wipe and move on one LAN (~5–7 weeks, or ~3–5 on top of appliance Phase 2 if folded in)

| Work | Estimate |
|---|---|
| Machine side in bun appliance mode: machine key, identity partition (F3), enrolment record and fleet pin, versioned machine API (F4), mDNS TXT, status report | 1.5–2 w |
| Wipe: LUKS data partition, crypto-erase, secure discard where supported, signed report, Quarantined on mismatch | 1 w |
| `relish fleet` v0: local state file, `init`, `machines`, `enrol` (three modes), `label`, `cluster create/add`, `allocate`, `release`, `move`, `wipe`, `forget`, `status`, resumable operations | 1.5–2 w |
| Cluster-side gaps: F1 drain API, F2 `Enroller` role, G6 token list/revoke, F5 voter demotion tests, F6 volume check | 1–1.5 w |
| Tests: state-machine unit tests (every transition, every invalid one), proptest over operation replay after a crash, a Mac VM lab (six aarch64 VMs, two clusters, enrol, move three, wipe, re-allocate), then the Wyse fleet | 0.5–1 w |
| Book chapter section and a manual chapter "Managing a fleet" | 0.5 w |

**Exit criteria (on the ten Wyse 3040s):**
- netboot all ten, enrol with the boot window;
- `cluster create home --count 5` and `lab --count 3`;
- move two machines from `lab` to `home` with no console work;
- verify each wipe report and the CRL entries;
- pull a power cord mid-move and resume.

### v1: the service (~5–7 weeks)

- `relish fleet serve`: `redb` state, the HTTP API with mTLS and the fleet CA, sealed backups, `rescan` (1.5–2 w).
- The reconciler over `fleet.toml` classes and counts, the donor rules and F7 what-if (1.5–2 w).
- Pool OS upgrades and cross-cluster waves (1 w).
- A Brioche fleet page and a TUI tab (1–1.5 w).
- Optional dead-node replacement (0.5 w).
- Docs (0.5 w).

### v2: when there's demand (~6–10 weeks)

- Phone-home outbound channel and multi-site (2–3 w).
- TPM enrolment and wipe attestation on hardware that has one; `AttestationMode::Tpm` finally real (2–3 w).
- A Secure Boot UKI addon carrying the fleet URL (1 w).
- Power drivers: Wake-on-LAN, Redfish or IPMI, smart plugs (1–2 w).
- G5 master-key rotation, if appliance Phase 4 hasn't done it (1–2 w).

---

## 17. Risks

- **The fleet becomes cluster-admin-equivalent** (§5.4) until nodes stop needing the master key. Mitigation: the `Enroller` role, audit logging, and treating the fleet host like a council member.
- **Enrolment TOFU on a hostile LAN.** The boot window and count bound it; only a TPM removes it.
- **A tampered machine lying about its wipe** (§9.2). Without Secure Boot and a TPM this can't be detected. We document it and rely on G5 for moves between owners.
- **eMMC secure discard support varies** by part **[unverified on the Wyse]**. Crypto-erase alone is still sound, because the key is gone.
- **Drain semantics are new** (F1). A drain that says "done" too early moves a node with live placements. The preflight and a Ready-elsewhere wait mitigate it, and the chaos suite should cover it.
- **Scope creep toward Omni.** Auth providers, a SaaS, multi-tenancy. v0 and v1 deliberately have one operator, one fleet key and one LAN.
- **Coupling to the appliance timeline.** Fleet v0 can't start until appliance Phase 1 and G1 land, and it changes PR #259's disk layout (F3). Deciding question 1 early avoids reworking the layout.

---

## Sources

All accessed 28 September 2026.

**Omni and Talos (§2.1)**
- Join machines to Omni: https://docs.siderolabs.com/omni/omni-cluster-setup/registering-machines/join-machines-to-omni.md
- Initial machine labels: https://docs.siderolabs.com/omni/omni-cluster-setup/how-to-set-initial-machine-labels.md
- Machine classes: https://docs.siderolabs.com/omni/omni-cluster-setup/create-a-machine-class.md
- Infrastructure providers (manual vs auto-provision classes): https://docs.siderolabs.com/omni/infrastructure-and-extensions/infrastructure-providers.md
- Automatic cluster scaling blog: https://www.siderolabs.com/blog/automatic-cluster-scaling-with-omni
- Cluster templates: https://docs.siderolabs.com/omni/reference/cluster-templates
- Patches with machine classes, open request: https://github.com/siderolabs/omni/issues/2593
- Wiping and removing a machine: https://docs.siderolabs.com/omni/cluster-management/wipe-a-machine
- SideroLink: https://docs.siderolabs.com/talos/v1.8/networking/siderolink
- PXE registration and kernel args: https://docs.siderolabs.com/omni/omni-cluster-setup/registering-machines/register-a-bare-metal-machine-pxe-ipxe.md
- SideroLink resource types (join tokens, unique tokens, pending machines): https://pkg.go.dev/github.com/siderolabs/omni/client/pkg/omni/resources/siderolink
- Machine registration network requirements: https://docs.siderolabs.com/omni/infrastructure-and-extensions/machine-registration.md
- Sidero security blog (10 Jul 2026): https://www.siderolabs.com/blog/how-talos-omni-makes-talos-linux-more-secure
- Self-hosted Omni ports and etcd: https://docs.siderolabs.com/omni/self-hosted/run-omni-on-prem
- Rotating the SideroLink join token, unique tokens: https://docs.siderolabs.com/omni/security-and-authentication/rotate-siderolink-join-token
- Unique-token impersonation fix: https://github.com/siderolabs/omni/issues/840
- UUID collision bug: https://github.com/siderolabs/omni/issues/3443 and https://github.com/siderolabs/omni/pull/3446
- Image Factory: https://github.com/siderolabs/image-factory and https://docs.siderolabs.com/omni/reference/image-factory-configuration.md
- Omni DB backup: https://docs.siderolabs.com/omni/self-hosted/back-up-omni-db.md
- Omni Helm chart (single active replica): https://github.com/siderolabs/omni/blob/main/deploy/helm/omni/README.md
- Break-glass access: https://docs.siderolabs.com/omni/security-and-authentication/break-glass-emergency-access
- Authentication: https://docs.siderolabs.com/omni/security-and-authentication/authentication-and-authorization
- Omni repository and licence: https://github.com/siderolabs/omni
- Upgrading clusters: https://docs.siderolabs.com/omni/cluster-management/upgrading-clusters.md
- Infrastructure providers announcement: https://www.siderolabs.com/blog/introducing-omni-infrastructure-providers
- Bare-metal infrastructure provider: https://docs.siderolabs.com/omni/omni-cluster-setup/setting-up-the-bare-metal-infrastructure-provider
- Metal agent RAID wipe: https://github.com/siderolabs/talos-metal-agent/pull/32
- Known issues: https://github.com/siderolabs/omni/issues/2465, https://github.com/siderolabs/omni/issues/1044, https://github.com/siderolabs/omni/issues/583, https://github.com/siderolabs/omni/issues/2035, https://github.com/siderolabs/omni/issues/1995, https://github.com/siderolabs/omni/issues/2702, https://github.com/siderolabs/omni/issues/2637, https://github.com/siderolabs/omni/issues/3427

**Wipe and boot (§6, §9)**
- `cryptsetup erase`: https://man7.org/linux/man-pages/man8/cryptsetup-erase.8.html
- `blkdiscard --secure`: https://man7.org/linux/man-pages/man8/blkdiscard.8.html
- eMMC erase, secure erase and secure trim in Linux: https://lkml.iu.edu/1006.3/00069.html
- UKI addons in systemd-stub: https://manpages.debian.org/unstable/systemd/systemd-stub.7.en.html and https://www.redhat.com/en/blog/extending-red-hat-unified-kernel-images-using-addons

**Repo (`main` at `087d882f`)**
- `src/sesame/types.rs` (`ApiRole`, `JoinToken`, `Crl::retired_nodes`), `src/sesame/join.rs` (TTL bounds, retired-id refusal), `src/cluster/retirement.rs` (`DecommissionRequest`), `src/bin/relish.rs` (`DecommissionNode`, `JoinToken`, `Council`), `src/relish/fault.rs` (`node_drain`), `src/council/node.rs` (retired voters refused), `src/upgrade/orchestrator.rs`, `Cargo.toml` (`redb`, `openraft`)
- `docs/whitepaper.md` §21 (Franchise), `docs/design/security-sesame.md` §5.2 (join), `docs/design/gossip-mustard.md` (master-key-derived HMAC)
- `research/appliance-os`: the appliance research note and spike plan; `feat/appliance-image`: `image/` and the S1 plan
