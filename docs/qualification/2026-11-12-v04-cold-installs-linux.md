# V04 cold installs on Linux: PASS

12 November 2026. The published 0.2.8, installed cold on clean Linux hosts
the way a user meets it, three times per architecture, each time on a host
that had never seen Reliaburger
([#288](https://github.com/reliaburger/reliaburger/issues/288)). The target
is a working cluster in under five minutes, and all six runs made it in
under 92 seconds: median 83.2 s on arm64 and 84.8 s on x86_64. Every
host ended exactly as it began. Intel macOS left the matrix when
0.2.2 dropped its build
([#1018](https://github.com/reliaburger/reliaburger/issues/1018)), so its row
stays dropped.

## What ran

Each run is one
[`qualify-cold-install.sh --prepare`](../../scripts/release/qualify-cold-install.sh)
on a fresh host. It installs QEMU and opens `/dev/kvm` (the prerequisites the
[quickstart](../quickstart.md) names, timed apart from the install), records
the host's state, then runs, in the real home directory:

```sh
curl -fsSL https://reliaburger.com/install.sh | bash -s -- --laptop --timings
```

The issue's command, `bash -s -- --timings`, predates the installer's modes:
since [#670](https://github.com/reliaburger/reliaburger/issues/670) a run
without `--laptop`, `--ssh` or `--install-only` explains the choices and
installs nothing, so every run passes `--laptop`.

Once setup says the cluster is ready, the script asks it `relish status -o
json`, `relish apps -o json` and `relish local status --name laptop -o json`,
fetches the sample app from `http://localhost:18080/`, then runs `relish local
destroy --name laptop --yes` and `relish uninstall --yes` and diffs the host
against the state it recorded before the install: `~/.reliaburger`, the
`~/.local/bin/relish` link, the rc files' digests, any `qemu-system`,
`limactl` or `bun` process, and the listening TCP ports.

- Release: `relish 0.2.8 (e423d46)`, bun v0.2.8 on every node
- Bootstrap: `https://reliaburger.com/install.sh`, SHA-256
  `431a96f114252ab97816a5a03140743d9fd0e68077b2a1a7105ce7cff5659e89`,
  the same on every run
- Wall time: from the first `curl` to setup's `cluster laptop ready`,
  measured by the script. Setup's own total (`timings.json`) starts after the
  CLI download, so it's about 3 s shorter.

## Linux arm64: PASS

Three fresh Lima VMs on the Mac Studio, created for one run each and deleted
after it (`rb-v04-arm64-1` to `-3`). Each started from Ubuntu's current Noble
cloud image, so nothing carried over: no cache, no Lima, no QEMU.

- Outer host: macOS 27.2, Apple M5 Max, 128 GiB, nothing else heavy running
- Outer Lima: 2.2.1, `vmType: vz` with `nestedVirtualization: true`, so the
  quickstart's own QEMU VMs get KVM
- Clean host: Ubuntu 24.04.5 LTS, kernel `6.8.0-147-generic aarch64`, 8 vCPUs,
  16 GiB, 60 GiB disk (56 GiB free)
- Prerequisites: `qemu-system-arm qemu-efi-aarch64 qemu-utils` and a udev
  rule opening `/dev/kvm`, 20 s, 26 s and 21 s

| Run | Started (GMT) | Downloads | VM boot | Node setup | Cluster checks | Setup total | Wall time | Under 5 min |
|---|---|---|---|---|---|---|---|---|
| 1 | 06:18 | 16.9 s | 31.3 s | 6.0 s | 18.3 s | 72.4 s | 75.4 s | yes |
| 2 | 06:22 | 16.7 s | 33.0 s | 6.5 s | 24.3 s | 80.6 s | 83.2 s | yes |
| 3 | 06:24 | 16.9 s | 42.2 s | 6.0 s | 24.3 s | 89.6 s | 91.9 s | yes |

Median wall time: **83.2 s**, under a third of the budget. Every run
downloaded the same 878.0 MiB (Lima, the guest image, bun and relish) at
about 52 MiB/s. The spread is the VMs booting under nested virtualisation:
VM boot went from 31 s to 42 s while everything else held still.

Every run's cluster answered: `status` said `healthy` with 3 nodes alive, a
three-voter council with quorum and one app running; `apps` listed `hello`
running 1 of 1; and the sample page came back with the title "Reliaburger
is running".

### Cleanup

`relish local destroy` removed all three nodes on every run, and `relish
uninstall` removed `bin/relish`, `tools`, `cache`, `lima`, `setup.lock`,
`clusters` and `context.lock`, keeping nothing. Afterwards
`~/.reliaburger` was gone, no `~/.local/bin/relish` existed (it never did:
`~/.local/bin` isn't on a fresh Ubuntu's `PATH`, so the installer printed the
`PATH` line instead), the rc files had the digests they started with (no
terminal, so the installer didn't offer to edit one), no QEMU, Lima or bun
process was left, and the listening ports were back to `sshd` and
`systemd-resolved`. Each run's `state.diff` is empty: the host returned to
exactly how it began. A reset host would have served for the next run, but
each run got a new VM anyway.

## Linux x86_64: PASS

Three fresh GitHub-hosted `ubuntu-24.04` runners, one per run, started
together at 06:38:52 GMT by the `Cold install` workflow
([`cold-install.yml`](../../.github/workflows/cold-install.yml)) on this
record's pull request
([run 43169584586](https://github.com/reliaburger/reliaburger/actions/runs/43169584586)).
Each runner is a new VM, so each run is cold. The workflow runs the same
script and kept each run's evidence as an artefact, copied below.

- Clean host: Ubuntu 24.04.5 LTS, kernel `7.0.0-1017-azure x86_64`, 4 vCPUs,
  15 GiB, 86 GiB free; runs 1 and 3 landed on an AMD EPYC 9V45, run 2 on an
  Intel Xeon Platinum 8370C
- Prerequisites: `qemu-system-x86 ovmf qemu-utils` and the same udev rule,
  17 s, 23 s and 16 s

| Run | CPU | Downloads | VM boot | Node setup | Cluster checks | Setup total | Wall time | Under 5 min |
|---|---|---|---|---|---|---|---|---|
| 1 | AMD EPYC 9V45 | 16.9 s | 33.8 s | 10.3 s | 21.0 s | 82.1 s | 84.8 s | yes |
| 2 | Intel Xeon 8370C | 10.2 s | 32.3 s | 10.7 s | 21.2 s | 74.4 s | 75.5 s | yes |
| 3 | AMD EPYC 9V45 | 18.9 s | 33.6 s | 9.5 s | 21.9 s | 83.9 s | 85.2 s | yes |

Median wall time: **84.8 s**. Each run downloaded 926.4 MiB (the x86_64 guest
image is larger than arm64's); run 2's network did it at 91 MiB/s against
about 50 MiB/s for the others, which is the whole of its lead. The VMs
booted in 32–34 s on every runner, and node setup took about 10 s, 4 s more
than on arm64.

The checks matched arm64's on every run: `status` said `healthy` with 3
nodes alive and a three-voter council with quorum, `apps` listed `hello`
running 1 of 1, and the sample page came back.

### Cleanup

The runner's `~/.local/bin` is on its `PATH`, so here the installer linked
`~/.local/bin/relish` to the store instead of printing the `PATH` line, and
`relish uninstall` removed that link along with everything under
`~/.reliaburger` it removed on arm64, keeping nothing. Afterwards
`~/.reliaburger` and the link were gone, the rc files (`.bashrc`, `.profile`
and `.bash_profile`) had their starting digests, no QEMU, Lima or bun process
was left and the listening ports were back to `sshd` and `systemd-resolved`.
Each run's `state.diff` is empty.

## Evidence

Each run's directory (`arm64-run1` to `-3`, `x86_64-run1` to `-3`) under
[`2026-11-12-v04-cold-installs-linux/`](2026-11-12-v04-cold-installs-linux/)
has `host.txt`, `prepare.txt`, the full `install.log`, `timings.json`,
`summary.json`, the `status`, `apps` and `local status` JSON, `destroy.json`,
`uninstall.json`, the host's `before.txt` and `after.txt` and their
`state.diff`.
