# Baked guest image, 24 September 2026

Measurements for plan item Z3.1
([zero to cluster](../plans/2026-09-23-zero-to-cluster.md)): the quickstart
guest image built by `scripts/release/build_guest_image.sh`, compared with the
stock Ubuntu image it's built from.

## Build

Built once, for aarch64, in the `reliaburger-test` Lima VM (Ubuntu 24.04 arm64,
4 vCPUs, 8 GiB), from the pinned `release-20260911` cloud image
(SHA-256 `7b682958…`), with `qemu-utils` as the only extra host package.
The x86_64 image wasn't built locally; CI builds it on `ubuntu-24.04`.

| | Stock Ubuntu | Baked |
| --- | ---: | ---: |
| File | 591 MiB (619,621,888 bytes) | 604 MiB (633,581,056 bytes) |
| Format | qcow2, zlib clusters, 3.5 GiB virtual | same |
| Build time | | 123–131 s (three builds), including 26–57 s of `apt-get update` |

The recorded build had SHA-256 `9c4d1181b36df317f43eba962fd1d491c2fa74a2707347ef3d8f489e2f09fb5e`.
Rebuilds don't reproduce it (file times and the ext4 journal differ), which is
why the release signs the digest instead of compiling it into the CLI.

Installed versions: runc `1.3.4-0ubuntu1~24.04.1`, uidmap
`1:4.13+dfsg1-4ubuntu3.2` (plus `libsubid4`). btrfs-progs `6.6.3-1.1build2`,
nftables `1.0.9-1ubuntu0.1`, iptables `1.8.10-3ubuntu2` and iproute2
`6.1.0-1ubuntu6.4` are already in the stock image, so the first-boot cost was
almost all `apt-get update`: 42 MB of indexes from 65 files per VM.

The first build, which deleted apt's indexes and cache after installing
instead of keeping them on a tmpfs, came out at 796 MiB: `fstrim` found only
69 MiB free because ext4 hadn't committed the deletions. Keeping apt's state on
tmpfs (and `sync` before `fstrim`) fixed it; the final build trimmed 647 MiB.

## First boot

Host: Apple M2 Max, 32 GiB, macOS 26.3.1, Homebrew Lima 2.1.0 (the same version
the quickstart pins), in an isolated `LIMA_HOME`. One bare VM at a time,
started with `limactl start` from the quickstart's VM settings (VZ, 2 vCPUs,
2 GiB, 10 GiB disk, no mounts, no containerd) and its provisioning script,
without the user-v2 network or port forwards so it couldn't disturb a
quickstart another agent was running on the same Mac. Timed from
`limactl start` to its return (Lima waits for provisioning to finish), then
the quickstart's own guest check (`/run/lima-boot-done`, runc, btrfs, nft,
newuidmap) passed in every run. Each VM was deleted afterwards.

| Image | Start to ready | `cloud-final.service` |
| --- | ---: | ---: |
| Baked | 16.7, 14.0, 17.8 s | 1.2–1.3 s |
| Stock | 31.3, 38.7, 53.3 s | 17.1, 22.7, 37.6 s |

Each baked VM had a different `/etc/machine-id` and generated its own SSH host
keys, and none ran apt. The stock image's spread is Ubuntu's mirrors: the
same `apt-get update` took between 17 and 38 s.

The quickstart boots its VMs in parallel, so this saves the boot stage roughly
the slowest VM's apt time: 15–35 s here, 30–40 s in the earlier
[quickstart timings](2026-09-23-quickstart-timings.md). A full three-node
quickstart with the baked image needs a release (or a staged candidate) to
download it from, so it hasn't been timed yet.

## Not done

- **Pre-pulled container images.** Bun always fetches the manifest from the
  registry and trusts a cached layer file by size once it exists; seeding its
  store from the image build would need a new offline verification path in the
  image store to save one 1.9 MB layer. See the book, chapter 9.
- **An x86_64 build and boot** outside CI, and a timed quickstart from a signed
  candidate.
