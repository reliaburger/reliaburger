# Plan: the appliance, from spike to product (0.3.0)

*Written 1 October 2026, after the spike (S1–S4 in [`2026-09-28-plan-appliance-lab.md`](2026-09-28-plan-appliance-lab.md), PR [#259](https://github.com/reliaburger/reliaburger/pull/259)). It turns the spike's shell scripts and preview tools into Reliaburger proper, in the order of the research note's §5 ([#218](https://github.com/reliaburger/reliaburger/pull/218), `docs/plans/2026-09-26-research-appliance-os.md` on that branch). Milestone: [0.3.0](https://github.com/reliaburger/reliaburger/milestone/6).*

## Where the spike left us

Proven in VMs, as scripts around the image:
- the image: Ubuntu 26.04 via mkosi, x86_64 and aarch64, EROFS `/usr` with dm-verity, A/B slots, UKIs with boot counting, the boot check, `/etc` following each image (`etc-sync`), the appliance profile (zram, `noatime,compress=zstd:1`, journald and binary retention);
- the netboot installer (streams to disk) and pinned iPXE, served by `image/tools/netboot-server.sh` (dnsmasq + Python);
- seeding by `image/tools/seed-fleet.sh` (USB stick or SMBIOS credential, applied by `apply-seed`);
- OS updates staged by hand (`os-stage` over SSH);
- bun upgrades through a launcher on `/var` that never downgrades.

What's missing is the part a user touches: bun and relish doing all of this themselves, with real signing, real publishing and no SSH.

## Decisions

- **Signing:** OS artefacts are signed with the existing release key (`RELIABURGER_RELEASE_KEY`, the secret `build.yml` already uses), not a separate OS key as the research note proposed (maintainer, 1 Oct). Signing still happens in its own job that runs no third-party actions.
- **Secure Boot and TPM-sealed encryption are deferred** to a later plan. The Wyse fleet runs without them.
- **`relish netboot` needs root** for UDP 67, 69 and 4011: run it with `sudo`; on Linux, `setcap cap_net_bind_service` is the documented alternative.
- **No merging before 0.3.0.** Each workstream is a PR stacked on the previous one, starting from #259; every PR references its issue below.
- **No backwards compatibility** with spike-era installs (pre-1.0 policy): a node installed from a spike image may need a reinstall. In particular, its first `etc-sync` has no record and keeps every file that differs.
- **The Wyse 3040 run (S5) is last**, once everything else is green ([runbook](2026-10-01-plan-appliance-s5-wyse.md)).

## Workstreams

Each is one stacked PR, unless it grows too big to review. Estimates are engineer-weeks with tests, manual and book.

### W1. OS builds, signing and the channel (~1.5 weeks), [#401](https://github.com/reliaburger/reliaburger/issues/401)

- **Triggers:** a weekly `appliance.yml` run on main (Monday 04:00 UTC), or a dispatch with `publish`. Version `YYYY.WW.N` (ISO week; N counts that week's releases). Pull requests still build the `lab` profile with a throwaway key, for the QEMU lab.
- **What a release is:** the latest released bun and relish. A week whose package list and build record (bun release, `image/` git tree) match the last release publishes nothing.
- **Signing:** the `sign` job runs only first-party actions (checkout, download and upload artefact) and `scripts/release/os_release.py`. It signs every `SHA256SUMS` with the release key, in the raw Ed25519 form the installer and `os-stage` already check with `openssl`. `SHA256SUMS` lists every artefact, so one signature covers the build. The image and installer ship the release public key (`os-signing-key.pub.pem`, tested against `src/upgrade/keys.rs`).
- **Checking it:** the netboot install test re-runs on the signed artefacts before anything is published.
- **Publishing:**
  - one GitHub pre-release per architecture, `os-<version>-x86_64` and `os-<version>-aarch64`, because both builds name their files the same;
  - a signed `os-channel.json` on the fixed `os-channel` release, naming each architecture's tag and `SHA256SUMS` digest;
  - the last 8 versions are kept.
- **No ISO:** `relish image write` (W3) puts the raw disk or the installer on a stick, which covers USB installs without a second format to build and test.
- **No SSH in production:** published images drop `openssh-server`. The `lab` mkosi profile (`mkosi.conf.d/30-lab.conf`) keeps it for the QEMU lab.
- **The `/usr` budget:** the 1.1 GiB slot is a hard limit (`SizeMaxBytes=`), so a firmware allow-list that outgrows it fails the build.
- **Tests:**
  - `scripts/release/test_os_release.py`: signing, the channel, versions, the quiet-week check, pruning, and the shipped key;
  - `src/os/channel.rs`: verification, including a channel signed by `os_release.py` (a fixture), a wrong key, an edited channel, mismatched tags and digests, malformed `SHA256SUMS`, and version ordering.

### W2. Seed mode in bun (~3 weeks), [#402](https://github.com/reliaburger/reliaburger/issues/402)

- **`bun --appliance`:** a pure state machine in `src/appliance/` with unit tests, replacing `apply-seed`:
  - read the seed (systemd credential, or the RBSEED stick);
  - write `node.toml` with the appliance profile;
  - lay out `/var/lib/reliaburger`;
  - join in-process with the existing join code, not `relish join`;
  - zero the create-seed after first boot.
- **Security gaps** (research §4.3):
  - G1: the master key is fetched after join, over the node-certificate mTLS connection, never carried on the seed;
  - G2: a time-boxed join window on the API port (moved to W5, where claims need it);
  - G3: advertise-address detection from the default route;
  - G4: appliance names from the cluster plus an ordinal;
  - G6: pre-seeded, node-bound, single-use join tokens in the initial security state.
  - (G5, master-key rotation, belongs with CA recovery and rotation, [#362](https://github.com/reliaburger/reliaburger/issues/362), and isn't repeated here.)
- **The tty1 status screen:** state, address, MAC, cluster and node name, and later the claim fingerprint and QR code.
- **Tests:** unit tests for the seed parser and every state transition; the G1 refusals (no node certificate, a retired node); a QEMU boot test that joins two seeded nodes.

### W3. relish for bare metal (~1.5 weeks), [#403](https://github.com/reliaburger/reliaburger/issues/403)

- `relish cluster create --bare-metal`: the cluster's PKI on the laptop, in a bare-metal context (generalising `LocalContext`), with the master-key backup prompt.
- `relish image download | write | seed`: fetch and verify a channel release, write a disk or stick, write seeds. These replace `seed-fleet.sh`, `node-toml.py` and `make-seed-stick.sh`.
- **Tests:** snapshot tests of the generated seeds and `node.toml`, and verification failures.

### W4. `relish netboot` (~2.5 weeks), [#404](https://github.com/reliaburger/reliaburger/issues/404)

- **The server, in Rust:**
  - a ProxyDHCP that never assigns addresses (`dhcproto`), answering on 67 and 4011;
  - a minimal TFTP server with blksize and the RFC 2347–2349 options;
  - HTTP through axum.
  - It serves only verified channel artefacts. It replaces `netboot-server.sh` and dnsmasq.
- **PXE details:**
  - architecture from option 93 (6, 7 and 9 for x86_64, 11 for arm64, 16 and 19 for HTTP Boot);
  - echo option 97;
  - the file name in option 67 and the BOOTP `file` field.
- **Safety rails:**
  - refuse to start when another ProxyDHCP or `bootpd` already answers;
  - an optional `--mac` allow-list;
  - a time limit;
  - a clear error without root.
- **Installed machines get `exit`:** remember them by MAC and SMBIOS UUID, and serve `boot.ipxe` with those as query parameters.
- `relish netboot --node <name>`: the same server run by a bun node.
- **Tests:**
  - unit and property tests for the offer builder and TFTP;
  - the x86_64 CI netboot test switched from dnsmasq to `relish netboot`;
  - the Mac lab.

### W5. Claiming machines over the LAN (~3 weeks), [#405](https://github.com/reliaburger/reliaburger/issues/405)

- **Unclaimed nodes:** a claim server (self-signed key, kept until claimed; its fingerprint on tty1) on port 9119 and an mDNS announcement `_reliaburger-unclaimed._tcp` (`mdns-sd`), with no secrets in TXT records. The first valid seed posted wins.
- **On the laptop:**
  - `relish machines` lists unclaimed machines with their fingerprints;
  - `relish machines claim <dir> --create --name <cluster> ... <mac|ip>...` creates a cluster from the machines it claims, node 1 first;
  - `relish machines claim <dir> <mac|ip>...` joins more;
  - both compare each claim key with the console interactively, or skip that with `--trust-lan`, and post the seed over TLS pinned to the key's full SHA-256;
  - `relish join-token list | revoke` (done in W2).
- **G2, the join window (moved here from W2):** instead of rewriting every node's `bootstrap_peers`, a joining claim asks every node to admit the new address for 15 minutes (`POST /v1/perimeter/admit`, admin, at most 60). The agent's firewall loop re-applies the ruleset when the open windows change. Once the machine has joined, gossip membership keeps it in.
- **Tests:**
  - the claim API (one valid seed, then 409), the pinned verifier over real TLS on loopback (a wrong pin never delivers the seed), target resolution, join windows and the admit handler's bounds;
  - `image/tests/claimed-pair.sh` in the appliance workflow: two unseeded VMs found over mDNS, node 1 claimed with `--create` and no `--network`, node 2 claimed afterwards through the join window;
  - a scripted Mac-lab qualification: netboot five VMs, claim them, run the tour, kill one, and write a record in `docs/qualification/`.
- **Deferred:** a QR code on tty1 (the short fingerprint is what people compare); claiming with `--all`, which would trust whatever answers mDNS.

### W6. OS updates run by bun (~2.5 weeks), [#406](https://github.com/reliaburger/reliaburger/issues/406)

- **Discovery and pinning:**
  - the leader reads `os-channel.json` daily and on `relish os list`, verifying it;
  - `os-update-available` shows in `relish wtf` and Brioche;
  - the pin is `os.target_version` in Raft, set by `relish os upgrade <version>`;
  - nodes report their OS version, and `relish nodes` shows it;
  - nodes installed from an older image move to the pin before taking workloads.
- **Rolling it out:**
  - an `OsSlot` backend for `UpgradeManager`, beside `Symlink`. It downloads, verifies, stages into `/var/lib/reliaburger/os-staging` and runs `systemd-sysupdate`, replacing `os-stage`.
  - The orchestrator's order and quorum rules apply: workers first, council one at a time, leader last.
  - Each node is drained before its reboot. A fallback pauses the run.
- **The boot check:** a shorter timeout on counted boots, sized from the Wyse's measured bun start-up (today three tries of 300 s take about 16 minutes).
- `relish os list | upgrade | status`.
- **Tests:** unit tests for version, channel and pin logic; a CI test in QEMU that updates one version to the next; a broken image that must fall back without hands.

### W7. Docs, lab and the exit test (~1.5 weeks, then the hardware), [#407](https://github.com/reliaburger/reliaburger/issues/407)

- **Docs:** rewrite `docs/manual/14_appliance.md` around the real commands (no "preview" scripts). Update `docs/book/15a-becoming-the-os.md` with the Rust parts and the lessons. Update README, `docs/README.md` and the roadmap.
- **Retire the preview tools:** `image/tools/` and the lab's hand staging go once their replacements are tested. The lab keeps only what QEMU needs.
- **The quickstart guest moves to Ubuntu 26.04**, sharing the appliance's package list (approved 27 Sep).
- **S5 and S6 last:** ten Wyse 3040s from power-on to a cluster with the real commands, measured as in the [S5 runbook](2026-10-01-plan-appliance-s5-wyse.md); then the go/no-go record.

## Order

W1 → W2 → W3 → W4 → W5 → W6 → W7, then S5. W2 and W3 together are the research note's "minimal first version" with W4. Each PR stacks on the previous branch and stays open until 0.3.0.

## Not in this plan

- Secure Boot (our db key) and TPM2-sealed data encryption.
- Fetching OS artefacts from peers instead of the internet.
- Automatic OS updates in a maintenance window.
- Static network configuration beyond what the seed or claim carries.
- Debian 13 as a second base.
