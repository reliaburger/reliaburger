# Quickstart timings, 23 September 2026

Before-and-after measurements for plan items Z3.2–Z3.5
([zero to cluster](../plans/2026-09-23-zero-to-cluster.md)).

Host: Apple M2 Max, 32 GiB RAM, macOS 26.3.1. Three Lima 2.1.0 VZ VMs,
2 vCPUs and 2 GiB each, Ubuntu 24.04 image dated 20260911. Ordinary home
Internet, bandwidth not controlled. Other agents were compiling Rust and
running an 8 GiB Lima VM on the same Mac throughout, so treat single runs as
noisy; the spread is part of the result.

All runs used **development binaries** (`--development-binaries`): Linux
aarch64 `bun` and `relish` built with `cargo build --release --features ebpf`
from 5092fe9 inside a separate Lima VM, unstripped (bun 230 MB, relish 201 MB;
SHA-256 `e12606c5…` and `d7643655…`). They exclude the signed-binary
downloads a real install adds, and they don't qualify a release.

Every run used an isolated `RELIABURGER_HOME=~/.rbz3` and ports
29117–29119/28080/25050, and was destroyed afterwards. "Cold" means an empty
home: Lima and the guest image are downloaded. "Warm" means the download cache
survives but the cluster is new, so every VM is created and provisioned.

## What was measured

- **Before**: 5092fe9 plus only the memory preflight fix (139c5e7). Without
  that fix the base refused to start at all on this Mac (it saw 0.5 GiB of
  32 GiB available; see below), and the fix doesn't touch the timed path.
  Step times come from timestamping its five progress lines.
- **After**: this branch. Step times come from the new progress display and
  `timings.json`.

## Results

| Run | Total | Downloads | VM boot | Node setup | Cluster checks |
| --- | ---: | ---: | ---: | ---: | ---: |
| Before, cold | 262.8 s | 115.4 s | 97.5 s | 38.4 s | 11.3 s |
| Before, warm 1 | 177.4 s | 1.9 s | 116.5 s | 46.2 s | 13.3 s |
| Before, warm 2 | 160.4 s | 2.0 s | 86.9 s | 60.3 s | 11.2 s |
| After, cold (final) | **194.4 s** | 91.9 s | 70.7 s | 21.7 s | 9.7 s |
| After, warm (six runs, logind fix) | 92.5–110.2 s, median 104.5 s | ~2 s | 48–72 s | 18–25 s | 11–13 s |
| After, warm (final three, watchdog) | 89.5, 110.9, 121.5 s | ~2 s | 56–89 s | 18–23 s | 10–13 s |

Warm setup dropped from 160–177 s to about 105 s, and the cold run from
263 s to 194 s. The cold figures differ by 23 s in download time alone, which
is the network, not the code; excluding downloads, cold went from 147 s to
102 s.

Where the saving comes from:

- **VM boot (Z3.2)**: all three VMs start within half a second of each other
  (VM 2 and 3 wait only for Lima's network daemon, typically 0.5 s) instead of
  VM 1 booting alone first. The stage now takes as long as the slowest VM,
  45–72 s, instead of two boots in sequence, 87–117 s.
- **Node setup (Z3.3)**: one tar stream per node (`install files` 2–7 s for
  ~430 MB of binaries plus config) instead of ~5 `limactl` calls per file, and
  nodes 2 and 3 install, enrol and start at the same time. Before: node 1
  7–11 s, nodes 2 and 3 15–31 s each, in sequence. After: node 1 ~3 s, then
  nodes 2 and 3 together in 15–20 s, most of it Bun's own readiness.
- **Cluster checks** didn't change: quorum is immediate, and 10–13 s is pulling
  BusyBox from public ECR and probing ingress.

Warm run 1 of the final three shows the watchdog working: VM 1 never booted,
was restarted after 60 s of console silence, and setup still finished in
121.5 s. Before the watchdog the same failure ended setup after 240 s.

## Downloads (Z3.4)

Cold downloads ran at 5.0–6.8 MiB/s for the 591 MiB Ubuntu image from
cloud-images.ubuntu.com, and 385–520 KiB/s for the 35.5 MiB Lima archive from
GitHub, which finished before the image only because they run concurrently.
At the old 180 s whole-request limit, the image needed at least 3.3 MiB/s
(about 27 Mbit/s); it now needs any byte every 30 s.

Interruption test: the image download was killed at 242.6 MiB. The re-run
printed `590.9 MiB / 590.9 MiB  5.9 MiB/s  (resumed at 242.6 MiB)`, sent
`Range: bytes=254410752-`, verified the whole file's SHA-256 and published it.
The downloads stage took 59.3 s instead of ~100 s.

## Problems the measurements found

1. **The memory preflight refused a 32 GiB Mac.** sysinfo 0.33 computes macOS
   available memory as free + inactive + purgeable − compressor pages. With
   ~10 GiB compressed that was 0.5 GiB while `memory_pressure` said 60% free.
   Fixed by using `kern.memorystatus_level` (139c5e7).
2. **A 90 s logind stall in one VM in two of four runs.** Our provisioning's
   `systemctl restart systemd-logind` sometimes spins in `stop-sigterm` until
   systemd's 90 s timeout; Lima's "user session is ready for ssh" waits with
   it (VM boots of 131 s and 165 s). Fixed by killing logind before the
   restart (aca72b3); the journals of all 30 VMs in the ten runs after it
   show no stop timeout. Plain Lima with a bare image shows a similar 120 s
   stall on its own: 4 of 39 VM starts when started together, 7 of 24 when
   started 5 s apart.
3. **Occasionally a VM never boots.** Lima reports it running, `serialv.log`
   stays empty, SSH never answers. Seen in 3 of 36 quickstart VM starts before
   the watchdog (two in one cold run, one in a warm run), and in 1 of 39
   plain `limactl start` runs with no Reliaburger code involved, and 0 of 24
   when starts were 5 s apart (too few to call). Handled by the 60 s console
   watchdog (c84f45f), which recovered it once in the final runs.
4. **Plain Lima's own SSH-key race is real.** In the first round of the plain
   Lima stress test, without a pre-generated key, two of three simultaneous
   first starts failed immediately. The quickstart never hit this after Z3.2.
5. **After `relish local stop` and a re-run, the hello app didn't come back**
   (seen once): the guest failed to re-pull its image layer with `layer size
   mismatch: expected 1900727, received 0`. That's guest-side image handling,
   not quickstart, and is left open.

## What a pre-baked image (Z3.1) would still save

Guest journals of the final cold run show the kernel reaching a login prompt
in 8–9 s and cloud-init finishing at 37–50 s, so first-boot provisioning (apt
update and install of runc, uidmap, btrfs-progs, nftables, iptables, iproute2
from live Ubuntu mirrors) costs about 30–40 s per VM. The VMs provision in
parallel, so a baked image would cut the boot stage from 56–72 s to roughly
25–30 s, and remove its variance and the dependency on Ubuntu's mirrors.
Pre-pulling BusyBox would take most of the 10–13 s cluster check. A zstd
image smaller than 591 MiB would shorten cold downloads in proportion. Stripping
the two binaries (431 MB unstripped) would shave a few seconds off node setup.
Together that's roughly 40–50 s warm, leaving about a minute.

## Raw logs

Per-run logs (with every step and start offset), the stress-test scripts and
their output are in `~/.cache/rb-z3-host/` on the measurement Mac and weren't
committed.
