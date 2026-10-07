# The netboot lab

This is the lab for spike stages S2 to S4 (the plan and log are `docs/plans/2026-09-28-plan-appliance-lab.md`). It builds a small LAN out of QEMU VMs on one Apple silicon Mac:
- a home router that hands out addresses and nothing else;
- a netboot server;
- up to five aarch64 appliance nodes that install themselves over the network and form a cluster;
- a virtual Wyse 3040.

Everything runs as your user. You don't need root, socket_vmnet or Lima.

The scripts keep their state (VM disks, logs, keys, seeds, downloaded artefacts) in `work/`, which git ignores. Nothing here builds images. CI does that (`.github/workflows/appliance.yml`), and the lab stages what a CI run produced.

## What you need

```sh
brew install qemu gh zstd mtools
gh auth login
```

## How the network fits together

```
Mac ── ssh 127.0.0.1:2222 ──> server VM (Ubuntu 26.04 arm64, HVF)
                               ├─ wan: QEMU user network (internet, NAT for the lab)
                               └─ lan ─┬─ br0 192.168.105.1   the "home router": dnsmasq,
                                       │                       addresses only, NAT out of wan
                                       └─ netns nb 192.168.105.2  the netboot server: dnsmasq as
                                                                  ProxyDHCP and TFTP, HTTP on 8080
QEMU hub 0 (inside the server's QEMU) ── 40 unix-socket slots ── nodes and virtual Wyses
```

- The router reserves 192.168.105.101 to .108 for the node MACs `52:54:00:00:01:01` to `:08` (`conf/router.conf`). bun's `bootstrap_peers` need literal addresses. Anything else gets an address from .100 to .199.
- The netboot server never hands out an address. It answers PXE requests with a boot file (`conf/proxy.conf`), exactly as `relish netboot` will next to a real router.

Three things about macOS and QEMU shaped this design:
- **UDP multicast doesn't work.** `-netdev dgram` fails on macOS with `EADDRNOTAVAIL`, so the shared segment is a QEMU hub inside the server's own QEMU process instead.
- **A hub slot can't be reused.** Its stream server doesn't deliver frames to a second client on the same slot. So every QEMU start takes a fresh slot from a counter, and `server-up.sh` resets the counter. There are 40 slots; when you run out, restart the server.
- **Homebrew's aarch64 edk2 can't network-boot under HVF on Apple silicon.** It has no RNG, so its network stack doesn't load. `rbnode.sh install` therefore starts our iPXE with `-kernel`, like booting an iPXE USB stick. Firmware PXE works under TCG, which is what the virtual Wyse uses.

## 1. Build the server

```sh
./build-server.sh      # the first time: fetches the cloud image, runs cloud-init, reboots
./server-up.sh         # afterwards: starts the existing server
./fetch-ovmf.sh        # Ubuntu's x86-64 OVMF, for the virtual Wyse
```

The first build takes a few minutes, most of it cloud-init waiting for the network once. The server has 2 GiB of RAM and an SSH port forward, and `./ssh.sh` gets you in.

## 2. Stage a build

Take a green `appliance.yml` run and stage its artefacts on the netboot server:

```sh
gh run list --workflow appliance.yml --branch feat/appliance-image
./stage-artefacts.sh <run-id> aarch64 <version>          # nodes
./stage-artefacts.sh <run-id> x86_64 <version>           # the virtual Wyse
```

The version is the image version from the run's step summary, for example `2026.40.22`. Staging puts iPXE and `boot.ipxe` on TFTP, and the installer, the signed disk image and its `SHA256SUMS` under `http://192.168.105.2:8080/<arch>/`.

By default TFTP serves iPXE's `snp` build, which drives the NIC through the firmware's own driver. The lab's runs so far used the full-driver `ipxe.efi`; `IPXE=full ./stage-artefacts.sh …` serves that one instead.

Artefacts only last a day, so stage them soon after the run.

## 3. Netboot the nodes

```sh
for n in 1 2 3 4 5; do ./rbnode.sh $n install fresh & done; wait
```

Each node gets a blank 10 GB disk and 2 GiB. It starts iPXE, takes its reserved address from the router and its boot script from the ProxyDHCP, and chains the installer. The installer checks the signature and streams the image onto the disk. On the first run, all five installed at once in 42 s, and none used more than 32 MiB of anonymous memory while streaming. `./show.sh rb1.install.log` shows what happened.

## 4. Form the cluster

Use the release that the image's bun came from; the run's step summary names it. The lab seeds its nodes the way the manual seeds real machines, with `relish cluster create --bare-metal`:

```sh
./fetch-relish.sh <release-tag>          # relish for the Mac (work/relish) and the server
./seed-lab.sh 5                          # the cluster, and a seed per node in work/cluster/stick/seeds
for n in 1 2 3 4 5; do ./rbnode.sh $n run; done
./ssh.sh '. ~/lab/env.sh; relish nodes; relish council'
```

`seed-lab.sh` runs `relish cluster create --bare-metal` on the Mac, naming each node's reserved MAC and address. The cluster's PKI is made there. Node 1's seed carries the cluster's keys, and every other seed a single-use join token, so the joiners enrol with node 1 on their first boot. The cluster's directory is `work/cluster`, and its `secrets/` hold the master key and the admin token. The script then puts the admin token, the root CA and an `env.sh` on the server, because relish runs there: the server is on the nodes' network and in their `operator_cidrs`, and the Mac isn't. `OPERATOR_KEY="ed25519:…"` passes the key cluster bun upgrades need.

`rbnode.sh run` passes a node its seed (`work/cluster/stick/seeds/<mac>.seed`) as the `reliaburger.seed` systemd credential, and bun's `appliance prepare` takes it from there. It also passes the lab's SSH key as the `ssh.authorized_keys.root` credential, the only thing that starts sshd on the appliance. So `ssh -F work/ssh_config root@192.168.105.101` works.

### Or seed from a USB stick, as on real machines

The manual's bare-metal chapter (`docs/manual/15_appliance.md`) carries seeds to real machines on a stick labelled `RBSEED`. To try that path here, turn the cluster's `stick` directory into a FAT image and plug it in with `STICK=`:

```sh
./make-seed-stick.sh work/stick.img work/cluster/stick
STICK=work/stick.img ./rbnode.sh 1 run
```

The node's console then says `seed installed from the RBSEED stick`. To add machines to the running cluster, `relish image seed` writes more seeds into the same directory.

### Or serve the netboot with relish

`relish netboot` can run on the server too, inside the `nb` namespace, in place of the lab's own ProxyDHCP and HTTP services (`sudo systemctl stop l2lab-proxy l2lab-http` first). The manual describes it; CI installs through it on every lab build (`image/tests/relish-netboot-install.sh`).

### Measuring the fleet

`fleet-measure.sh` samples every node over SSH into one CSV per node, for the Wyse lab's 24-hour run: memory, zram, bun's RSS, bytes written to the system disk, disk use, load and temperature. `fleet-nodes.py` tells it which nodes there are, from a cluster directory's `fleet.json` or from `relish nodes --output json` (`--relish`):

```sh
./fleet-measure.sh ~/wyse 300 288          # every 5 minutes for 24 hours
```

It needs root SSH on the nodes, so it only works on lab images, whose seeds carry a key (`--ssh-key`). No relish command samples these numbers yet, which is why the script stays.

## 5. The tour

`relish` runs on the server, which is in every node's `operator_cidrs`:

```sh
./ssh.sh
. ~/lab/env.sh
relish apply -f https://reliaburger.com/demo/podinfo.yaml
relish status
curl -H 'Host: podinfo.localhost' http://192.168.105.101/
relish path frontend --to redis
relish fault delay redis 300ms --from frontend --duration 2m --acknowledge
relish path frontend --to redis --count 3
relish fault kill frontend --count 1 --acknowledge
relish wtf
```

To pull a node's power cord, run `kill -9 $(cat work/rb3.pid)` on the Mac, and then `./rbnode.sh 3 run` to bring it back. The tour's `relish dashboard` step needs a browser, so skip it here.

## 6. OS updates and the fallback

Stage the next build with `--update`, then roll it node by node, leader last:

```sh
./stage-artefacts.sh <next-run-id> aarch64 <next-version> --update
./os-update.sh 192.168.105.105 <next-version>
```

`os-update.sh` runs the image's `os-stage`, which stands in for bun:
1. check the build's signed `SHA256SUMS`;
2. download the UKI and both `/usr` images and verify them;
3. run `systemd-sysupdate`, which writes the inactive slot and the ESP.

Then it reboots the node and waits until the node is back on the new version with `boot-complete.target` reached. That's when `systemd-bless-boot` marks the new UKI good.

For the bad update, build an image whose bun never starts, stage it the same way, and don't wait:

```sh
gh workflow run appliance.yml --ref feat/appliance-image -f broken_bun=true
./stage-artefacts.sh <broken-run-id> aarch64 <broken-version> --update
NOWAIT=1 ./os-update.sh 192.168.105.105 <broken-version>
```

Each of its three tries waits up to 300 s for bun before the boot check reboots it. After the third, systemd-boot falls back to the previous UKI and slot. Watch it with `./show.sh rb5.serial.log`.

`keeper.toml` is a small app with a managed volume that appends a line every time it starts. Deploy it before an update to check that volume data survives.

## 7. The virtual Wyse

```sh
./wyse.sh 1 fresh
./show.sh wyse1.serial.log
```

This is x86-64 under TCG with a Westmere CPU (no AVX, like the Wyse's Atom), 2 GiB and an 8 GB disk. It uses real firmware PXE, with the disk first in the boot order, so one QEMU process installs and then boots the installed system. On the first run the install took 29 s, and bun was healthy 62 s into the installed system's boot.

## 8. Tear down

```sh
./down.sh
```

This kills the nodes and Wyses, shuts the server down cleanly and closes any tunnels. The disks stay in `work/`, so `./server-up.sh` and `./rbnode.sh <n> run` pick up where you left off. To start from nothing, delete `work/` yourself.
