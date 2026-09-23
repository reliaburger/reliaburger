# Zero to cluster in five minutes

23 September 2026. A visitor to reliaburger.com reads a short tutorial on the
homepage, pipes `install.sh` to a shell, gets a three-node cluster on their
laptop, and runs a real Kubernetes application on it. Then they read its logs,
watch its metrics, break it on purpose and watch it heal. This plan covers what
that needs, in the order we'd build it.

## Where we are

Three read-only surveys of the current code (website and delivery, the
quickstart path, and what a laptop cluster can demonstrate) found the pieces
mostly exist, but not joined up.

**What works.** `relish setup --quickstart` builds three Lima VMs with a pinned
Lima 2.1.0 and a pinned Ubuntu image, forms an mTLS cluster with per-node join
tokens, deploys a `hello` container, proves it through ingress on
`localhost:18080`, and writes a CLI context. It resumes after a failure.
`relish logs`, `relish status`, `relish routes`, `relish dashboard` (Brioche,
with CPU and memory charts), the TUI, `.internal` DNS with eBPF VIPs, ingress,
rolling deploys and crash restarts all work on that cluster. `relish import`
turns Deployments, Services, Ingresses, HPAs and more into TOML with a
migration report.

**How long it takes.** One measured cold run: 242 s on an M2 Max, excluding the
release binary downloads, which don't exist yet. An earlier cold run failed at
the 300 s deadline. Where the time goes, from the code:

| Step | Estimate | Why |
|---|---|---|
| Guest image download | 50–100 s | ~600 MB stock Ubuntu; a 180 s per-request limit fails below ~27 Mbit/s |
| First VM boot + `apt-get install` | 40–80 s | VM 1 boots alone, to avoid a Lima SSH-key race |
| VMs 2 and 3 boot + `apt-get install` | 40–80 s | Every VM installs runc, nftables etc. from live Ubuntu mirrors |
| Node configuration | 20–60 s | ~140 sequential `limactl` calls; binaries copied to each VM separately; nodes one at a time |
| Quorum, image pull, probe | 10–40 s | Pulls `hello` from public ECR |

Progress output is five plain lines, so a two-minute download looks like a hang.

**What stops the tutorial today.**

1. **Nothing installs until 0.1.0 is published.** `install.sh` downloads a
   versioned release that doesn't exist. There's no staging mirror. It only
   works with `| bash`, not `| sh`, and it doesn't put `relish` on `PATH`.
2. **A real Kubernetes app won't run.** The Runc runtime ignores the image's
   config: `Entrypoint`, `Cmd`, `Env`, `WorkingDir` and `User`
   (`src/grill/oci.rs:334-351`). Every container runs as uid 65534 in `/`. Almost
   every public image relies on at least one of those.
3. **Kubernetes service names don't resolve.** Only `<app>.internal` does;
   `resolv.conf` has no search domain, so `redis:6379` fails.
4. **You can't break anything.** `relish fault` is refused on a quickstart
   cluster: it writes no `[testing]` policy, and the default allows nothing.
   Workload faults also only reach instances on the node that received the
   request.
5. **Some views only see node 1.** `relish logs -f` and `relish top` read the
   endpoint node only. `relish wtf` and `relish trace` dial the guests' own
   addresses, which the host can't reach.
6. **The homepage has no tutorial**, no recording, and a "no JavaScript"
   policy.

## The tutorial we're aiming for

About two minutes of reading and doing after the cluster is up:

```sh
curl -fsSL https://reliaburger.com/install.sh | sh     # ~3 min: installs relish, builds the cluster
relish apply -f https://reliaburger.com/demo/podinfo.yaml   # a real K8s app: frontend, backend, redis
relish status                                          # 3 replicas spread over 3 nodes
open http://podinfo.localhost:18080                    # through the built-in ingress
relish logs podinfo --since 1m                         # logs from every node
relish dashboard                                       # live CPU and memory charts
relish fault kill podinfo --count 1 --acknowledge      # kill a replica ...
relish status                                          # ... and watch it come back
relish local stop <node-3>                             # lose a whole node
relish status                                          # replicas rescheduled on the survivors
relish wtf                                             # what just happened, in one screen
```

The demo app is podinfo in its three-tier shape: a frontend, a backend it calls
through `--backend-url`, and redis as a cache through `--cache-server`. It's a
real, widely used Kubernetes demo. It shows service discovery by name, JSON logs,
per-instance hostnames in the UI, and self-inflicted failures (`/panic`,
`/readyz/disable`). We host the manifest, unchanged in spirit from upstream,
with only the edits a real migration would need, and say what they are.

## Plan

One commit per item, stacked on #177. Tests first. Product gaps come before the
tutorial, because a tutorial over a broken path is a lie.

### Phase 1: run real Kubernetes apps (product)

- [x] **Z1.1 Honour the image config on Runc.** Read the OCI image config at
  prepare time. Use its `Entrypoint` + `Cmd` when the app sets no `command`, the
  way Kubernetes does (`command` replaces `Entrypoint`, `args` replaces `Cmd`),
  merge its `Env` under the app's, and use its `WorkingDir`. For `User`, see D1.
  Tests: unit tests for the merge rules, and a VM test running an image that
  needs its entrypoint and env.
- [x] **Z1.2 Resolve Kubernetes-style short names.** Add `search <ns>.internal
  internal` to the container's `resolv.conf`, keeping `ndots` low, so `redis`
  and `redis.default` resolve the way they would in a pod.
- [x] **Z1.3 Import what a real app needs, and say what it drops.** Keep the
  Service `port` → `targetPort` mapping, warn on non-HTTP readiness probes and
  extra ports instead of dropping them silently, and import `args` separately
  from `command` so Z1.1's rules apply.
- [x] **Z1.4 Apply Kubernetes YAML directly.** `relish apply -f app.yaml` (and
  an `https://` URL) runs the import in memory and prints the migration report,
  so "run your Kubernetes app" is one command. `relish import` stays for people
  who want the TOML.
- [x] **Z1.5 A known-good demo manifest.** `examples/kubernetes/podinfo.yaml`,
  three-tier podinfo, pinned by digest, with an integration test that imports it
  and a VM test that runs it on a three-node Runc cluster and checks the
  frontend reaches the backend and redis by name.

### Phase 2: observe and break it from the laptop (product)

- [x] **Z2.1 Workload faults on laptop clusters.** The quickstart writes a
  `[testing]` policy of `safety_class = "development"` with workload faults
  allowed (see D3). Workload faults are forwarded to the node that owns the
  target instance, like node faults already are.
- [x] **Z2.2 Cluster-wide follow and top.** `relish logs -f` streams from every
  node that runs the app (fan-out over the existing SSE endpoint), and
  `relish top` shows every node, with CPU and memory.
- [x] **Z2.3 `wtf` and `trace` from the host.** Route the per-node calls through
  the endpoint node (the API already forwards to peers) instead of dialling
  guest addresses.
- [x] **Z2.4 Name a node for `relish local stop`.** `relish local stop <node>`
  stops one VM, so "lose a node" doesn't mean typing a `limactl` path and a
  generated VM name.

### Phase 3: get the cluster up in about three minutes (quickstart)

- [ ] **Z3.1 A pre-baked guest image.** A release asset built in CI from the
  pinned Ubuntu image with runc, uidmap, btrfs-progs, nftables and iproute2
  already installed, compressed (`qcow2` + zstd), and the podinfo and busybox
  images pre-pulled. No `apt-get` on first boot. See D4.
- [x] **Z3.2 Boot every VM at once.** Generate Lima's SSH key in our `LIMA_HOME`
  before the first boot, then start all VMs in parallel. Done: peers start as
  soon as Lima's network daemon runs (about 0.5 s after VM 1), and a 60 s
  console watchdog restarts a VM that Virtualization.framework never boots.
- [x] **Z3.3 Configure nodes in parallel, in one copy each.** One tarball per VM
  instead of ~5 `limactl` calls per file, and nodes 2 and 3 enrol concurrently
  once node 1 is ready.
- [x] **Z3.4 Progress you can trust.** A live line per step with elapsed time,
  download bytes and speed, and a summary of where the time went. Downloads
  resume after interruption instead of failing at 180 s. Done; the five-minute
  deadline now covers building the cluster only, and downloads fail after 30 s
  without data (30-minute backstop), because their speed is the network's.
- [x] **Z3.5 Measure it.** A `--timings` report, recorded for cold and warm runs
  on Apple silicon (and Intel and Linux when available), feeding release gate
  V04. Target: under three minutes cold on 100 Mbit/s, leaving two for the
  tutorial. Apple silicon done; Intel and Linux still to run. Measured on an M2 Max with development binaries
  ([results](../qualification/2026-09-23-quickstart-timings.md)): cold
  263 s → 194 s (147 s → 102 s excluding downloads), warm 160–177 s → about
  105 s. The runs also found and fixed a memory preflight that refused busy
  Macs, a 90 s logind stall in provisioning, and VMs that never boot. Z3.1
  would take roughly another 40–50 s: first-boot apt costs 30–40 s per VM.

### Phase 4: install in one line

- [ ] **Z4.1 `| sh`, not just `| bash`.** Rewrite the bootstrap and the
  installer in POSIX sh.
- [ ] **Z4.2 Put `relish` on `PATH`.** Install to `~/.local/bin` when it's on
  `PATH`, otherwise print the one line to add, and offer (with a prompt when
  interactive) to add it to the shell's rc file. See D6.
- [ ] **Z4.3 A staging mirror before 0.1.0.** So the whole path can be tested
  end to end before the release: publish a signed candidate to a pre-release
  (or Pages path) and point the bootstrap at it with
  `RELIABURGER_RELEASE_BASE_URL`. See D5.
- [ ] **Z4.4 Uninstall.** `relish local destroy` exists; add
  `relish uninstall` for the CLI, tools and image cache.

### Phase 5: the homepage

- [ ] **Z5.1 An expandable tutorial.** A `<details>` section on the homepage
  ("Try it in five minutes") with the commands above, what each shows, and
  links to the manual. Works without JavaScript.
- [ ] **Z5.2 The same tutorial in the product.** `relish manual` gets a
  "Five-minute tour" chapter generated from the same source, and the quickstart's
  final message points at it.
- [ ] **Z5.3 A recording.** A scripted, reproducible run of the tutorial
  (`scripts/demo/tour.sh`) recorded with asciinema and embedded on the homepage with the
  vendored asciinema player (D2); the site README and footer say where JavaScript
  is used and why.
- [ ] **Z5.4 Keep it honest.** A CI check that every command in the homepage
  tutorial exists in `relish --help` and that the demo manifest imports, so the
  page can't drift from the product.

### Phase 6: show what Kubernetes can't do (killer features)

The first draft proved Reliaburger does what Kubernetes does. The tour should
lead with what you'd otherwise have to install, wire and learn separately:
tracing a connection hop by hop, metrics scraped without a Prometheus install,
and network faults between two services. A survey on 23 September found each
beat needs product work first:

- Network faults (`drop`, `partition`, `dns`) were installed only on the nodes
  running the *target*, but the eBPF connect hook acts on the *caller's* node,
  so a frontend replica elsewhere never saw the fault. Faults also only affect
  new connections, and a pooled Redis client keeps its old ones. `delay` is
  refused: there is no traffic-control hook.
- `relish trace` doesn't report faults on the path, and its latency figure
  times the whole `runc exec`.
- Apps' own Prometheus metrics are only scraped from a static node list. The
  importer ignores `prometheus.io/*` annotations, histograms are summed wrongly,
  there's no `relish metrics`, and the dashboard's app charts are always empty
  (brioche.js expects an array and gets `{data, warnings}`).

- [x] **Z6.1 Install network faults where the callers are.** Destination-wide
  faults go to every node; source-scoped ones (`--from`) to the nodes running
  the source. Filter sources by namespace, and apply a running fault to source
  instances that start while it's active.
- [x] **Z6.2 Affect existing connections.** When a drop or partition lands,
  destroy the matching established sockets in each source container's network
  namespace, so pooled clients reconnect into the fault.
- [ ] **Z6.3 A latency fault.** `relish fault delay redis 300ms --from frontend`
  adds a `tc` netem qdisc on each source container's `eth0`, filtered to the
  destination's backend addresses and port, removed on expiry or clear, and
  applied to new source instances while active. It works on existing
  connections too.
- [ ] **Z6.4 Trace shows the fault.** A new "Active faults" step names any
  fault on the path (id, kind, remaining time). `--count N` repeats the probe
  and reports how many connects succeeded; the latency figure measures the
  connect itself; the output names the backend the VIP chose and trims the raw
  resolver noise.
- [x] **Z6.5 Metrics without a Prometheus install.** The app spec gains
  `metrics = { port, path }`, filled by the importer from `prometheus.io/scrape`,
  `port` and `path`. Each Bun scrapes its own local instances of such apps
  every few seconds, labels samples with app, namespace, instance and node, and
  handles histograms properly. `relish metrics <app>` lists what was scraped,
  and `--name` shows one metric per instance with a sparkline and rate for
  counters.
- [x] **Z6.6 Charts that draw.** Fix the dashboard's empty app charts, draw one
  series per instance, show counters as rates, and add a requests-per-second
  and latency chart for apps with scraped metrics.
- [ ] **Z6.7 Traffic and the new tour.** The demo manifest gains a tiny load
  generator that calls the frontend by service name, so there's always traffic
  for metrics and faults to act on. The tour becomes: install, apply, status,
  open, **trace frontend → redis**, **metrics frontend**, **delay redis for the
  frontend**, trace again (the fault is on the path), metrics again (latency
  jumps), dashboard, kill a replica, lose a node, `wtf`. Checked end to end on a
  real laptop cluster before the homepage copy changes.

## Decisions

Answered on 23 September 2026: D1 honour the image user in a user namespace;
D2 vendor the asciinema player; D4 build our own guest image; D5 build now and
stage it. D3, D6 and D7 take the recommendation.


- **D1. What user do containers run as?** Today everything is uid 65534.
  *Recommended:* honour the image's `User` (and Kubernetes `runAsUser`) but
  never map to host root: containers run in a user namespace, so "root" in the
  image is an unprivileged host uid. That's what lets nginx-style images bind
  port 80. *Alternative:* keep 65534 and require images that run unprivileged,
  which rules out a lot of the Kubernetes ecosystem.
- **D2. How to show the recording.** *Chosen:* vendor the asciinema player
  (about 100 KB of JS, no third-party requests) and relax the "no JavaScript"
  policy for that one embed; the tutorial text itself still works without
  script. *Not chosen:* an animated SVG from `svg-term`.
- **D3. Faults on by default for laptop clusters?** *Recommended:* yes, for
  quickstart clusters only, with the policy visible in `relish local status`.
  They're throwaway development clusters and "break it on purpose" is a selling
  point. Server installs keep the protected default.
- **D4. Build our own guest image?** *Recommended:* yes, in CI, from the pinned
  Ubuntu cloud image, signed and mirrored like the rest of the release. It
  removes apt from the critical path and cuts the download. *Alternative:* keep
  stock Ubuntu and cache apt packages, which saves less and still depends on
  Ubuntu's mirrors.
- **D5. Ship before or with 0.1.0?** The tutorial can't honestly say
  "curl | sh" until the release is published. *Recommended:* build all of it
  now, test it end to end through a staging mirror (Z4.3), keep the homepage
  section labelled "arrives with 0.1.0" until then, and count Z3.5's
  measurements towards gate V04.
- **D6. Where does `relish` go?** *Recommended:* `~/.local/bin` if it's on
  `PATH` (common on Linux and with Homebrew-era macOS setups), otherwise
  `~/.reliaburger/bin` with a one-line instruction, and an opt-in rc-file edit.
- **D7. Linux laptops.** Quickstart needs QEMU and `/dev/kvm`, installed by the
  user. *Recommended:* keep that for 0.1.0, detect the distro and print the exact
  install command, and qualify Linux after macOS.

## What we're not doing now

- A single VM hosting three nodes. Nodes use fixed guest ports, and the story is
  "a real cluster of machines".
- Running unmodified multi-service demos with sidecars and gRPC probes (Online
  Boutique). The import report tells users what to change.
- PromQL in the tutorial. The dashboard's charts show metrics; PromQL is
  deferred for 0.1.0.
