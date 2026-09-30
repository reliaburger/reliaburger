# Demo image build, end to end on a laptop cluster, 30 September 2026

The first end-to-end run of #248 (a Go app built with `relish build` in the
five-minute tour, multi-arch builds and pulls, platforms in `relish images`,
the build cache cap and Buildah in the guest image). It works through the
PR's "Not verified" list.

## Setup

- Host: Apple M2 Max, 32 GiB, macOS 26.3.1. Other agents were compiling on
  the same Mac; the load average reached 29 on 12 cores during the first
  attempt.
- Cluster: dev quickstart, three Lima 2.1.0 VZ VMs (2 vCPUs, 2 GiB each),
  stock Ubuntu 24.04.5 cloud image with the node packages installed at first
  boot (so Buildah 1.33.7 came from Ubuntu, as it does in the guest image),
  kernel 6.8.0-139, runc. Isolated `RELIABURGER_HOME=/tmp/rb248`, default
  ports (API 19117–19119, ingress 18080, registry 15050).
- Binaries: Linux aarch64 debug `bun` and `relish` with `--features ebpf`,
  built in the shared `reliaburger-test` VM from #248 merged with main (the
  tree of 55fe5c0c; `--version` says `a4742ed`, the head before the merge
  commit). The fixes below were checked with a second build (`edf6224`, #248
  plus the variant fix and #332) installed over `/usr/local/bin/bun`.
- Commands: the tour exactly as documented, through its script:

  ```sh
  RELIABURGER_HOME=/tmp/rb248 RELIABURGER_NO_MODIFY_PATH=1 RELISH=<host relish> \
    scripts/demo/tour.sh --setup <linux binaries>
  ```

  `burger.tar.gz` isn't published yet, so the script copied
  `examples/demo/burger` instead, as it says on screen.

## The tour

The first attempt failed in setup: all three consoles stayed silent for the
60 s boot watchdog, and the restart of VM 1 failed on Lima's `user-v2`
socket (#333). Rerunning the same command resumed and finished:
`cluster laptop ready in 243.5s`. The tour then ran start to finish in
624 s including setup and the script's waits, with no step failing:

| Step | Result |
|---|---|
| `relish apply -f https://reliaburger.com/demo/podinfo.yaml` | four apps committed; all running after 44 s |
| `relish status`, podinfo through the ingress | three frontends, one per node; three different hostnames |
| `cp -R examples/demo/burger .` (tarball not yet published) | |
| `relish build burger/burger.toml` | `built and pushed burger:v1` |
| `relish apply burger/burger.toml` | two replicas running after 14 s |
| `curl http://burger.localhost:18080/order` | `{"number":2,"burger":"double smash","cashier":"lima-rb-6cca3e62f1e2-1","kitchen":"lima-rb-6cca3e62f1e2-1"}` |
| `relish path frontend --to redis` | PASS, median connect 0.0 ms |
| `relish metrics frontend` | three instances scraped |
| `relish fault delay redis 300ms --from frontend …`, `relish path … --count 3` | DEGRADED, median connect 300 ms |
| `relish metrics frontend --name http_request_duration_seconds` | 396 ms mean on node 1 |
| `relish dashboard --no-open` | read-only session URL, closed with Ctrl-C |
| `relish fault kill frontend --count 1 …` | back after 16 s, one restart |
| `relish local stop node-3` | three frontends on the two survivors after 36 s |
| `relish wtf` | node 3 dead, council member down, quorum holds |

## "Not verified" items

| Item | Result | Evidence |
|---|---|---|
| Context upload through the forward | Passed | `relish build` without `--registry-port` uploaded to the context's `registry` forward, `https://127.0.0.1:15050` (`context uploaded to Pickle`). Main now carries this fix (#327). |
| The node build | Passed | Node 1 built both platforms from a cold cache in about 71 s (context blob stored 02:07:53, index 02:09:04); warm rebuilds took 22–31 s. |
| Signing | Passed while the build node led; **failed on a follower** (#331, fixed in #332) | With `require_signatures = true` on every node, apps using `burger:v2` (the index) and `burger@sha256:f22d…` (the arm64 platform manifest) deployed, and a Pickle image nobody signed was refused: `image unsigned-probe2:v1 requires a signature (require_signatures is enabled)`. After the leader moved to node 2, a build on node 1 failed: `csr signing failed: linearizable read failed: has to forward request to …`. With #332, a build on node 1 while node 3 led was signed and `burger:v5` deployed on node 2. |
| Pulling `burger:v1` by bare name | Passed | Node 2, which didn't build it, ran a replica; it holds the arm64 layer and not the amd64 one, so it pulled its own platform through the index. |
| The ingress route at `burger.localhost` | Passed | Orders through `127.0.0.1:18080` with `Host: burger.localhost`; the cashier alternated between nodes 1 and 2. |
| `backend` resolving from the burger container | Passed | Every order's `kitchen` names the podinfo backend that answered `http://backend:9898/`. |
| Mixed-architecture pull | **Not arranged** | Lima's VZ driver on Apple silicon runs only arm64 guests, and there is no x86_64 QEMU on this Mac. Partial evidence: the amd64 platform manifest and layer are in Pickle and the amd64 binary is `ELF 64-bit LSB executable, x86-64`. The portable suite still covers the amd64 pull. |
| Networked `RUN` next to the perimeter firewall | Passed | `RUN wget -q -O /fetched https://reliaburger.com/` in a busybox image on node 1 (`reliaburger_fw` tables loaded) fetched 24,229 bytes. Afterwards `nft list tables` still showed `ip reliaburger`, `ip reliaburger_fw`, `ip6 reliaburger_fw` next to netavark's iptables-nft `ip nat` and `ip filter` (75 rules before, 97 after), and the burger orders kept working. |
| Base-image cache under the 1 GiB cap | Passed for the demo alone; see below | After the build only `public.ecr.aws/docker/library/golang` (261 MB) remained in `<storage.data>/buildah/root`: no containers, no manifest lists. The root took 880 MiB on disk (761 MB apparent), and the next build was warm (31 s). |

## Bugs found

- **#331, fixed in #332 (main, 0.1.1):** a build on a node that isn't the
  Raft leader can't be signed. Provisioning the build signer starts with a
  linearised CA read and `AttachSignature` is a Raft write, and neither was
  forwarded. The runner now asks the leader through `POST /v1/build/sign`.
- **#248's own code:** `relish images` showed `burger:v1` as
  `linux/amd64/v8, linux/arm64/v8`. Buildah 1.33 copies the `FROM
  --platform=$BUILDPLATFORM` stage's variant onto every platform it builds,
  so the index said `linux/amd64/v8` (reproduced with a two-stage busybox
  Dockerfile in the build VM; a `FROM scratch` build alone has no variant).
  Pickle ignores variants, but an amd64 Docker or containerd client would
  find no match. Fixed on #248: the runner drops a variant the architecture
  doesn't have before reading the layout
  (`a_builder_stage_variant_is_dropped_from_the_foreign_architecture`).
  Checked live: `burger:v4` lists `linux/amd64, linux/arm64/v8`.
- **#333 (main, 0.1.1), not fixed:** under heavy host load, all three
  consoles stayed silent past the 60 s watchdog, and restarting the three VMs
  at once failed on Lima's `user-v2` socket. Rerunning setup recovered. Not
  reproducible on demand, so the issue records the evidence and candidate
  fixes.

## Observations for the maintainer

- **The 1 GiB cap keeps the demo warm and nothing else.** The Go image uses
  880 MiB of it. The networked-`RUN` build pulled busybox, storage measured
  1,217,778,640 bytes, and the runner wiped the whole cache
  (`Buildah storage holds 1217778640 bytes, over the 1073741824 byte cache cap;
  removing every cached image`), so the next burger build was cold again.
  Working as designed, but any second base image on a quickstart node costs
  the demo its warm cache.
- `relish build` still prints `push: buildah push … docker://localhost:5050/…`.
  That line is display-only; the node exports to an OCI layout (with
  `manifest push --all` for several platforms) and uploads it. It was already
  inaccurate on main.
- Restarting all three agents at once left every app on node 1 (nothing
  rebalances afterwards). Expected today, but surprising to watch.
- Manual chapter 11 said `relish images -o json`; the flag is `--output json`. Fixed on #248.

## Teardown

`relish local destroy --yes` removed the three VMs, `relish uninstall --yes`
emptied the isolated home, and `/tmp/rb248` is gone. The build artefacts in
the shared `reliaburger-test` VM (`/tmp/pr248-*`) were deleted; that VM was
not restarted.
