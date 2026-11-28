# Changelog

What changed in each Reliaburger release, newest first, and what you have to do to move onto it. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/): each release starts with **Upgrading**, then what it added, changed, fixed and removed, and the known issues it shipped with.

Until 1.0.0 every release is a development release. We keep no backwards compatibility: a release that changes the wire or on-disk format needs a fresh cluster, with no migration ([compatibility before 1.0.0](docs/releasing.md#compatibility-before-100)). So read a release's Upgrading section before you install it.

This file is the short version. The longer records are:

- the [roadmap](docs/roadmap.md#releases-after-010), for everything each release contains and what comes next;
- the [release log](docs/release-log.md), for how each release was qualified and what the soaks found;
- [docs/releasing.md](docs/releasing.md#compatibility-before-100), for the full upgrade steps between each pair of releases;
- the [GitHub release pages](https://github.com/reliaburger/reliaburger/releases), for the binaries and every merged pull request.

## [Unreleased] - 0.3.4

Not released yet: this is what the train carries, and it can change before the tag. 0.3.4 is the audit release: fixes from a section-by-section audit of the code, across the council, jobs, metrics and logs, images and the registry, ingress, DNS, gossip, containers, the appliance and CI.

### Upgrading

- 0.3.4 moves the wire protocol from 61 to 62 and the state format from 76 to 77, once for the whole release, so a 0.3.3 cluster can't roll onto it in place: nodes refuse 61/76 peers and data directories, and a 0.3.3 leader refuses `relish upgrade start v0.3.4`. Start a fresh cluster, after the checks in [upgrading from 0.3.3](docs/releasing.md#upgrading-from-033) ([#2367](https://github.com/reliaburger/reliaburger/issues/2367)).
- `[node] name` must be one DNS label (1 to 63 lowercase letters, digits and `-`). Bun refuses to start otherwise, and `relish join-token create`, the join handler and `relish join` refuse such a name. Rename any node that breaks the rule; there's no migration before 1.0 ([#2394](https://github.com/reliaburger/reliaburger/issues/2394)).
- `relish join` needs `--ca-fingerprint`; `--insecure-trust-first-ca` skips the pin and prints the offered fingerprint as a warning before the token is sent. It also writes `council.json` beside the node's identity, so keep the identity directory whole when you move it ([#2397](https://github.com/reliaburger/reliaburger/issues/2397), [#2410](https://github.com/reliaburger/reliaburger/issues/2410)).
- An image app's container and each sidecar get a limit of 4,096 processes and threads. Set `pids` (up to 32,768) on any app or sidecar that runs more, or it fails to fork ([#2386](https://github.com/reliaburger/reliaburger/issues/2386)).
- Rootful runc containers run with no-new-privileges under Docker's default seccomp filter, so an image that starts as a non-root user and relies on `sudo` or another set-uid binary must start as root instead ([#2402](https://github.com/reliaburger/reliaburger/issues/2402)).
- Council backups written before 0.3.4 don't restore: a backup's ciphertext is now stored as base64. Take new backups once the 0.3.4 cluster runs ([#2452](https://github.com/reliaburger/reliaburger/issues/2452)).
- A 0.3.3 Bun can't read `bun --compatibility`'s new `version` field, so a 0.3.3 node can't stage a 0.3.4 binary even apart from the format bump ([#2403](https://github.com/reliaburger/reliaburger/issues/2403)).
- Ingress passes apps the client's `Host`, not the instance's own address, plus `X-Forwarded-Host` and `X-Forwarded-Port`. An app that relied on seeing its own address as `Host` behaves differently ([#2404](https://github.com/reliaburger/reliaburger/issues/2404)).

### Added

- `relish images` has an UNPACKED column, showing what each image's unpacked copies take on the node that answered, and `relish wtf` has an `images` check that warns `image-collection-failing` when a node's image collection keeps failing or being refused ([#2524](https://github.com/reliaburger/reliaburger/issues/2524)).
- `relish council status` shows how many of the 65,536 retired node identities are used, and `relish wtf` warns `council-retired-identities-nearly-full` from 90% ([#2375](https://github.com/reliaburger/reliaburger/issues/2375)).
- `/v1/metrics/cluster` names a live node whose rollups have stopped reaching the council for five minutes as `NodeSilent`, with when its last rollup arrived ([#2536](https://github.com/reliaburger/reliaburger/issues/2536)).
- `relish upgrade start`, `plan` and `status` name the down nodes a cluster upgrade leaves out, and a finished run says how many of the cluster's nodes it covered ([#2417](https://github.com/reliaburger/reliaburger/issues/2417)).
- `relish secret encrypt` reads the value from `--value-file` or standard input, asking without echo at a terminal; the positional argument still works ([#2407](https://github.com/reliaburger/reliaburger/issues/2407)).
- A new `[images] max_files` setting (default 65,536). Hitting the file cap says so, naming the count and the setting, instead of reporting a byte quota ([#2431](https://github.com/reliaburger/reliaburger/issues/2431)).
- Images whose layers are zstd-compressed or uncompressed unpack; an unknown layer format fails with its media type ([#2465](https://github.com/reliaburger/reliaburger/issues/2465)).
- Three more fuzz targets cover batch submissions: the TOML manifest `relish batch submit` reads, `POST /v1/batch/array` and `POST /v1/batch` ([#1414](https://github.com/reliaburger/reliaburger/issues/1414)).

### Changed

- `relish apply`, `relish lint`, `relish scale` and scale proposals refuse more than 10,000 replicas, and an `[autoscale] max` above 10,000 ([#2368](https://github.com/reliaburger/reliaburger/issues/2368)).
- `[deploy] health_timeout` is at most a day and `drain_timeout` at most an hour. A duration in minutes or hours too large to represent is refused instead of panicking or wrapping, and a health wait, drain deadline or stop grace too large to add to the clock falls back to the longest value config accepts ([#2378](https://github.com/reliaburger/reliaburger/issues/2378), [#2480](https://github.com/reliaburger/reliaburger/issues/2480), [#2527](https://github.com/reliaburger/reliaburger/issues/2527)).
- `[health] path` must start with a single `/` and hold no whitespace or control character, and the probe URL is built so the path can't change the host. `metrics.path` also refuses `//` and control characters ([#2401](https://github.com/reliaburger/reliaburger/issues/2401)).
- `relish import` warns about every Kubernetes field it doesn't read, including a StatefulSet's `volumeClaimTemplates`, `volumeMounts`, `imagePullSecrets`, host namespaces, `startupProbe` and the hardening fields of `securityContext`, so `--strict` fails on them. Existing manifests may show more warnings than before ([#2461](https://github.com/reliaburger/reliaburger/issues/2461)).
- `relish wtf` and `relish agent diagnose` no longer suggest `relish pickle gc`, `relish dev shell` or `journalctl -u bun`, or tell you to drain a node ([#2468](https://github.com/reliaburger/reliaburger/issues/2468)).
- `relish logs` without `--tail` returns the newest 10,000 lines and warns when there were more; `--tail` is capped at 100,000. Metric, rollup and log queries have an 8-second limit and a row cap, and a refused query is a 400 asking for a narrower range ([#2382](https://github.com/reliaburger/reliaburger/issues/2382)).
- `/v1/metrics/cluster` reads the last hour by default, pages large answers instead of refusing them past 10,000 rows, and reports a refusing council member with its reason instead of as unresponsive ([#2451](https://github.com/reliaburger/reliaburger/issues/2451)).
- `GET /v1/logs/{app}/{namespace}?tail=N` returns the last N lines of each instance, each under its header, in a stable order, and a cluster log query asks at most 16 nodes at once ([#2453](https://github.com/reliaburger/reliaburger/issues/2453), [#2472](https://github.com/reliaburger/reliaburger/issues/2472)).
- An app's scraped metric with a name the node writes itself (`node_*`, `up` and so on) is stored as `exported_<name>` and can't raise the node's alerts, and `GET /v1/metrics/app` and the app charts match an app's series by its `app` label only, so one app can't show up in another's results. A firing alert whose metric stops reporting resolves after 10 evaluations, and at most 8 webhook deliveries run at once ([#2387](https://github.com/reliaburger/reliaburger/issues/2387)).
- `relish exec` returns the first 1 MiB of a long output followed by `[output cut at 1 MiB]`, instead of an error, and the output never takes more than about 2 MiB on the node's disk while the command runs ([#2392](https://github.com/reliaburger/reliaburger/issues/2392)).
- `relish apply` refuses an image whose layers use a compression no node can unpack, naming the layer's media type, before anything is scheduled ([#2528](https://github.com/reliaburger/reliaburger/issues/2528)).
- An image unpack is bounded at 16 GiB of file content, a million entries, and the image filesystem's free space less 1 GiB, and a failed unpack leaves no partial tree ([#2385](https://github.com/reliaburger/reliaburger/issues/2385)).
- Image layers stream to disk on both pull paths instead of sitting in memory. A slow layer fails only after 60 s with no bytes, resumes where the registry allows, and an image that can't fit under `[images] max_storage` is refused before any download ([#2424](https://github.com/reliaburger/reliaburger/issues/2424)).
- Unpacked image generations, their cached manifests and their pin metadata are collected when no container on the node uses them: hourly after a create from an image, or at once when the image filesystem is 95% full ([#2423](https://github.com/reliaburger/reliaburger/issues/2423)).
- Registry pushes honour a token's `[permission]` block: a token with a block may push only to `<namespace>/<app>` repositories whose app it grants `deploy` on, and a bare name needs a cluster-wide grant. Reads are unaffected ([#2398](https://github.com/reliaburger/reliaburger/issues/2398)).
- The registry refuses repository names and tags outside the OCI grammar with 400 `NAME_INVALID` or `TAG_INVALID` ([#2413](https://github.com/reliaburger/reliaburger/issues/2413)).
- A registry write queued behind four busy slots waits up to six minutes, longer than an upload may take, instead of failing with 429 after 60 seconds; heal copies between nodes have their own slots ([#2466](https://github.com/reliaburger/reliaburger/issues/2466)).
- Ingress: a request body that sends nothing for 30 s gets 408, and bodies being buffered share a 256 MiB budget (503 past it); `[ingress] max_request_body_bytes` and `request_body_budget_bytes` set both. A slow upload no longer holds a rollout's drain open ([#2381](https://github.com/reliaburger/reliaburger/issues/2381)).
- Ingress: the two listeners hold at most `[ingress] max_open_connections` (32,768) connections, and one client address gets at most `max_tls_handshakes_per_source` (16) TLS handshake slots. The ingress and DNS-over-TCP accept loops back off for 100 ms on EMFILE, ENFILE, ENOBUFS or ENOMEM instead of spinning ([#2383](https://github.com/reliaburger/reliaburger/issues/2383)).
- Ingress: the reachability probe counts any HTTP answer, a 5xx included, as reachable, so an app that answers 5xx at `/` stays routable, and a backend leaving or rejoining rotation is logged with the reason ([#2464](https://github.com/reliaburger/reliaburger/issues/2464)).
- DNS: an upstream resolver that times out is tried after the others for 10 s, one source address holds at most 16 of the node's 64 forward slots (and 16 of the 64 DNS-over-TCP connections), and positive external answers are cached for their TTL, at most 30 s and 1,024 entries ([#2384](https://github.com/reliaburger/reliaburger/issues/2384)).
- A namespace quota counts every app's running replicas, converged or not: a refused scale-up or an app that lost a node no longer frees its room for the next app, and a scale-up is checked only for what it adds ([#2462](https://github.com/reliaburger/reliaburger/issues/2462)).
- A placement pass never plans more than the council admits: past 4,096 decisions or 131,072 placements the rest wait for the next pass, two seconds later, instead of the whole pass being refused on every tick ([#2366](https://github.com/reliaburger/reliaburger/issues/2366)).
- An even council that must shrink removes a voter that is down before a live one ([#2374](https://github.com/reliaburger/reliaburger/issues/2374)).
- A committed batch costs each council member one fsync, not two: the committed pointer rides on the next append, with its own write after a second of quiet and on a clean stop. A crashed survivor's `relish council recover` may leave out its newest batch; a cleanly stopped one loses nothing ([#2437](https://github.com/reliaburger/reliaburger/issues/2437)).
- Job state can't grow past the bounds its snapshot decoder enforces: settling a deployment trims completed receipts, and admission refuses a run past 2,048 held arrays or a batch past 120 held batches ([#2365](https://github.com/reliaburger/reliaburger/issues/2365)).
- Finite batches (`relish batch FILE`) count against their jobs' namespace for retention, not `default`; the manual states the 65,536-chunk limit ([#2519](https://github.com/reliaburger/reliaburger/issues/2519)).
- `relish policy audit` judges a job by the image it declared, as it judges an app. A job pinned by tag and digest (`report:v1@sha256:…`) is judged by that whole reference, not by its tag alone ([#1322](https://github.com/reliaburger/reliaburger/issues/1322)).
- An app instance's restart backoff resets after ten minutes of running, as a sidecar's does, so old restarts no longer cost a five-minute wait ([#2373](https://github.com/reliaburger/reliaburger/issues/2373)).
- A node's deploy history keeps at most 1,000 apps, and deploy history and the dashboard's env page no longer show env values or inline config file content; peers still read history whole for a rollback ([#2400](https://github.com/reliaburger/reliaburger/issues/2400), [#2428](https://github.com/reliaburger/reliaburger/issues/2428)).
- The dashboard's live log view keeps the newest 2,000 lines and stops jumping to the bottom while you read back ([#2433](https://github.com/reliaburger/reliaburger/issues/2433)).
- The node relay answers within 30 seconds, body included, and refuses a followed event stream with 400 ([#2393](https://github.com/reliaburger/reliaburger/issues/2393)).
- Bun warns at start-up about other services listening inside `[network] port_range`, which the perimeter closes to non-members, and removes its perimeter tables on a graceful stop that stopped every workload ([#2479](https://github.com/reliaburger/reliaburger/issues/2479)).
- Bun exits non-zero when its agent loop panics or ends, so the service manager restarts it and the new Bun adopts the running workloads ([#2379](https://github.com/reliaburger/reliaburger/issues/2379)).
- GitOps refuses a commit that leaves no `.toml` file under the watched path while it owns resources, instead of removing all of them; commit an empty `.toml` to remove everything on purpose ([#2467](https://github.com/reliaburger/reliaburger/issues/2467)).
- `relish fault disk-io` throttles the whole disk when the volumes directory is on a partition (cgroup `io.max` refuses a partition with ENODEV), and refuses with a reason on Btrfs, overlayfs or tmpfs instead of guessing `8:0` ([#2463](https://github.com/reliaburger/reliaburger/issues/2463)).
- `relish test`: a status read that never answers costs one bounded wait (10 s at most) and is asked again, instead of spending the whole case ([#2128](https://github.com/reliaburger/reliaburger/issues/2128)).
- OS update staging refuses a release whose UKI is built for another architecture than the node's, before anything is written to the spare slot ([#2411](https://github.com/reliaburger/reliaburger/issues/2411)).
- An unclaimed appliance refuses a seed with more than 64 entries or more than 4 MiB unpacked, before holding it in memory ([#2391](https://github.com/reliaburger/reliaburger/issues/2391)).
- `relish netboot --stream-logs` and `relish machines logs` write lines from senders they don't serve to one `logs/unserved.log`, keep at most 64 log files open and stop each file at 64 MiB ([#2390](https://github.com/reliaburger/reliaburger/issues/2390)).
- Performance: council apply shares the big collections copy-on-write instead of copying the whole desired state for a placement pass, a job deployment or an approved proposal, and a placement pass checks every decision before writing any; one-field council reads such as `security_state()` no longer copy it either ([#2435](https://github.com/reliaburger/reliaburger/issues/2435), [#2436](https://github.com/reliaburger/reliaburger/issues/2436)).
- Performance: the leader publishes only the endpoint catalogue's changed services into the Raft log, not the whole catalogue on every health flip ([#2438](https://github.com/reliaburger/reliaburger/issues/2438)), and plans its placement pass after the tick's own catalogue and rollout writes, replanning up to three times a tick when another write lands first ([#2369](https://github.com/reliaburger/reliaburger/issues/2369)).
- Performance: admitting a job no longer copies the whole job state inside the Raft apply, and a waiting `relish apply` reads only its own deployment ([#2445](https://github.com/reliaburger/reliaburger/issues/2445)).
- Performance: a task-array run's template goes to a node once, without the program, arguments and environment derived from it, and sync requests are built outside the state lock; a new leader's first sync round is about 44% smaller (48.2 MB to 27.0 MB for 96 runs on 200 nodes), and cron expressions are parsed once per change, not every second ([#2444](https://github.com/reliaburger/reliaburger/issues/2444), [#2532](https://github.com/reliaburger/reliaburger/issues/2532)).
- Performance: a node measures its task-data size every 30 seconds, off the sync path, and its grant fence keeps only the chunks still in play ([#2429](https://github.com/reliaburger/reliaburger/issues/2429), [#2446](https://github.com/reliaburger/reliaburger/issues/2446)).
- Performance: API reads, the rollup worker, the report aggregator, the autoscaler and `relish top` no longer hold the metric, log or rollup store's lock through their queries, and the autoscaler sends one rollup query per metric series per tick, not one per app ([#2443](https://github.com/reliaburger/reliaburger/issues/2443), [#2449](https://github.com/reliaburger/reliaburger/issues/2449), [#2530](https://github.com/reliaburger/reliaburger/issues/2530)).
- Performance: a metrics query for a recent window reads only the Parquet files that overlap it, and a full metrics or log buffer sheds its oldest rows in constant time ([#2425](https://github.com/reliaburger/reliaburger/issues/2425), [#2450](https://github.com/reliaburger/reliaburger/issues/2450)).
- Performance: the scheduling pass looks up autoscale overrides in a map and checks daemon-set eligibility without copying the cluster cache ([#2458](https://github.com/reliaburger/reliaburger/issues/2458)); registry GC looks up blob holders once per sweep, and a push writes the image catalogue once, as compact JSON ([#2427](https://github.com/reliaburger/reliaburger/issues/2427)); each workload's owner process sleeps until something happens instead of waking 100 times a second ([#2447](https://github.com/reliaburger/reliaburger/issues/2447)).
- Performance: gossip republishes membership only when the table changed (a datagram that changes nothing cost 12.8 ms of the gossip task at 10,000 members, and now costs about 210 ns), and a node joining a large cluster pulls the membership in 512-entry pages, one per cycle: 20 exchanges at 10,000 members ([#2439](https://github.com/reliaburger/reliaburger/issues/2439), [#2442](https://github.com/reliaburger/reliaburger/issues/2442)).
- Performance: ingress probes run 32 at a time and take the routing table's write lock at most once per sweep, so backends on a node that lost power leave rotation in about three probe intervals ([#2448](https://github.com/reliaburger/reliaburger/issues/2448)).
- A container whose image is already unpacked no longer waits for another image's unpack, and different images unpack in parallel ([#2456](https://github.com/reliaburger/reliaburger/issues/2456)).
- CI: every third-party GitHub Action is pinned to a commit, with its version in a comment that Dependabot updates, and the toolchain action takes the compiler as its `toolchain:` input ([#2405](https://github.com/reliaburger/reliaburger/issues/2405)).
- CI: the fuzz nightly fuzzes the newest release train, like the macOS and mutants nightlies ([#2482](https://github.com/reliaburger/reliaburger/issues/2482)). `open_train.py` refuses to open a train while a schedule-only workflow in the tree is missing from `main`, and opens the gate PR as a draft, and `ignored-test evidence` fails a train's gate PR into `main` that's ready for review without the `release-gate` label ([#2481](https://github.com/reliaburger/reliaburger/issues/2481), [#2483](https://github.com/reliaburger/reliaburger/issues/2483)).
- CI: the appliance image builds and tests for changes to Bun, `relish os`, `relish image` and `os_release.py`, and `make test-ci-scripts` (so `make ci`) runs the appliance image's Python tests in `image/tests` ([#2484](https://github.com/reliaburger/reliaburger/issues/2484)).
- CI: job selection counts as code every file under `docs/` that a test reads or relish packs, so changes to the book, design docs, release log, `testing.md`, flakes register, qualification records and concept budget run the Rust suites ([#2485](https://github.com/reliaburger/reliaburger/issues/2485)). A change to the scheduler, the policy engine or their benchmarks runs `make bench`, which now also runs the ingress drain tracker's benchmark ([#2455](https://github.com/reliaburger/reliaburger/issues/2455), [#2493](https://github.com/reliaburger/reliaburger/issues/2493)).
- CI: merge groups and pull requests build the benchmarks with 16 codegen units for the workspace crate, taking about 12 minutes off the merge queue's slowest job, and the privileged Linux jobs return their root-built cache to the runner, so the Ubuntu 26.04 job's dependency cache saves on `main` again ([#1723](https://github.com/reliaburger/reliaburger/issues/1723)).
- CI: the coverage floor (`COVERAGE_MIN_LINES`) is 87.60%, up from 78.65%, one point under what the portable Linux runs measured on 26 and 27 November ([#2487](https://github.com/reliaburger/reliaburger/issues/2487)).
- CI: `build.yml` starts each Linux release `bun` it built and runs one app before uploading it (`scripts/release/smoke-release-bun.sh`) ([#2486](https://github.com/reliaburger/reliaburger/issues/2486)). A pull request into `main` fails `features.yml` while a feature row links an issue it would close ([#2492](https://github.com/reliaburger/reliaburger/issues/2492)). The flakes register check reads every row: a status other than Fixed, Not a defect, Infrastructure or Superseded must link an issue ([#2491](https://github.com/reliaburger/reliaburger/issues/2491)). `registry_durability` fails, rather than passes, when `RELIABURGER_CLUSTER_TESTS` is unset, and a CI-script test keeps early returns out of gated tests ([#2490](https://github.com/reliaburger/reliaburger/issues/2490)).
- The release profile runs on every minor release (and on a patch release that changes the job path or follows a throughput regression), not on every release ([#2488](https://github.com/reliaburger/reliaburger/issues/2488)), and `make test-hollow` has an owner and a cadence in `docs/testing.md`'s manual gates ([#2489](https://github.com/reliaburger/reliaburger/issues/2489)).
- The power-cut endurance harness prints no stray stderr on a VM's first start, times I6 from the start that booted, and records an unreachable writer as undecided rather than as lost data ([#2542](https://github.com/reliaburger/reliaburger/issues/2542)).
- Docs: the installer with no mode and the quickstart say how to netboot machines (`relish image download`, `relish netboot`) instead of calling it unbuilt ([#2495](https://github.com/reliaburger/reliaburger/issues/2495)); the quickstart quotes measured install times for Linux and for 0.3.3 on Apple silicon ([#2503](https://github.com/reliaburger/reliaburger/issues/2503)); the comparison pages say they describe 0.2.2, and the Kubernetes testing table is corrected ([#2496](https://github.com/reliaburger/reliaburger/issues/2496)).
- Docs: the feature status page's dashboard, workload identity, metrics, API tokens, timeline and cordon rows are corrected, and the page check refuses a review date older than the release and an open issue a row cites without linking it ([#2505](https://github.com/reliaburger/reliaburger/issues/2505)). The roadmap and whitepaper no longer defer sidecars and exec secret providers, both shipped in 0.2.4 ([#2502](https://github.com/reliaburger/reliaburger/issues/2502)). docs/README.md points an unknown job outcome at `relish batch replay` ([#2501](https://github.com/reliaburger/reliaburger/issues/2501)), and linux-servers.md and the whitepaper describe `relish inspect <app>` ([#2504](https://github.com/reliaburger/reliaburger/issues/2504)).
- Docs: every design doc opens with a Status, Reconciled against and Sketches header, and sketch listings say they aren't in the tree ([#2511](https://github.com/reliaburger/reliaburger/issues/2511), [#2513](https://github.com/reliaburger/reliaburger/issues/2513), [#2515](https://github.com/reliaburger/reliaburger/issues/2515)). The security design doc no longer describes the workload certificate grace period, multi-resolver egress DNS, `[cluster]` attestation keys, `[security.oidc]` or `relish ca revoke` as shipped, and gives the real procedure for expelling a node ([#2498](https://github.com/reliaburger/reliaburger/issues/2498), [#2517](https://github.com/reliaburger/reliaburger/issues/2517)). The registry design doc no longer presents the unbuilt build socket and `build_push_to` as security controls ([#2499](https://github.com/reliaburger/reliaburger/issues/2499)).
- Docs: the design docs match the code for the Onion eBPF attach points and `[ebpf]` keys ([#2514](https://github.com/reliaburger/reliaburger/issues/2514)), Smoker's `fault_connect_map` ([#2509](https://github.com/reliaburger/reliaburger/issues/2509)), the node `[ingress]` keys ([#2507](https://github.com/reliaburger/reliaburger/issues/2507)), the ingress 30 s upstream idle limit and forwarding headers ([#2516](https://github.com/reliaburger/reliaburger/issues/2516)), the dashboard's types, `--listen` and ingress keys ([#2508](https://github.com/reliaburger/reliaburger/issues/2508)), the scheduler's image locality and stability scores ([#2518](https://github.com/reliaburger/reliaburger/issues/2518)), the test harness and CI since 24 November ([#2494](https://github.com/reliaburger/reliaburger/issues/2494)), Bun's node-config keys, defaults and disk layouts ([#2506](https://github.com/reliaburger/reliaburger/issues/2506)), gossip's Raft and council types ([#2510](https://github.com/reliaburger/reliaburger/issues/2510)), and Mayo's 8 MiB and 20,000-sample scrape limits ([#2521](https://github.com/reliaburger/reliaburger/issues/2521)).
- Docs: the manual's node-config reference names every `node.toml` key with its default, including `[reconstruction]` and `[security] allow_insecure_cluster` ([#2497](https://github.com/reliaburger/reliaburger/issues/2497), [#2512](https://github.com/reliaburger/reliaburger/issues/2512)); it states the registry's push limits (512 MiB and 5 minutes per upload request, four concurrent writes) ([#2520](https://github.com/reliaburger/reliaburger/issues/2520)) and what stops after 60 s without a council leader ([#2500](https://github.com/reliaburger/reliaburger/issues/2500)).

### Fixed

- `relish` accepts only a cluster node's certificate as the API server, so a workload or ingress certificate under the same root no longer receives your token ([#2396](https://github.com/reliaburger/reliaburger/issues/2396)).
- A node with no council seat (every node past the council cap, and every joiner) takes Raft replication only from the council named in its join bundle, or from a voter that council vouches for. Before, any node certificate could feed it a fake council, tokens and CA included ([#2397](https://github.com/reliaburger/reliaburger/issues/2397)).
- A node with no council seat enforces a token's `[permission]` block on its API and its registry. It used to take the tokens from the leader's security view without their blocks, so a narrowed token could deploy and push anywhere through such a node ([#2538](https://github.com/reliaburger/reliaburger/issues/2538)).
- A recovery epoch claimed over gossip no longer fences a voter by itself: the node asks its own voters, and fences only if one of them serves the newer epoch, and a recovered council asks the voters it replaced, so two survivors recovered on the same epoch still find each other. A node with no council seat fetches its security view only from a council voter, over TLS bound to that voter, and gossip leader hints expire a minute after their leader stops re-advertising them, so a forged hint can't stall such a node for good ([#2406](https://github.com/reliaburger/reliaburger/issues/2406)).
- Image-signature chain checks require every issuer to be a CA ([#2409](https://github.com/reliaburger/reliaburger/issues/2409)).
- A followed log, the event stream, their WebSockets, apply and rollback progress and `relish scale`'s progress stream end within about 15 s of their token being revoked or expiring; the scale carries on ([#2399](https://github.com/reliaburger/reliaburger/issues/2399), [#2526](https://github.com/reliaburger/reliaburger/issues/2526)).
- Idle sockets can no longer keep node reports out of the leader. The reporting listener counted a connection before its TLS handshake, so anyone who could reach the reporting port could hold every slot with sockets that sent nothing, until the leader saw every node as stale, emptied the service catalogue and stopped scheduling. Connections that haven't delivered a whole report now have a budget of their own, capped per source address ([#2380](https://github.com/reliaburger/reliaburger/issues/2380)).
- The leader no longer refuses node reports in clusters of hundreds of nodes. Its reporting listener closed every connection past 16 and every report past a 16-deep queue, and its aggregator did work that grew with the square of the fleet, so 500 hollow nodes saw 17,223 reports refused. Excess senders now wait for a slot inside their deadlines, and the same run refuses none ([#1411](https://github.com/reliaburger/reliaburger/issues/1411), [#2440](https://github.com/reliaburger/reliaburger/issues/2440)).
- A `buildah` that hangs no longer stops a node from reporting: the report worker's start-up probe runs asynchronously with a two-second deadline ([#2459](https://github.com/reliaburger/reliaburger/issues/2459)).
- A node with more series than fit one 1 MiB report sends its minute in parts instead of silently dropping out of `/v1/metrics/cluster` ([#2426](https://github.com/reliaburger/reliaburger/issues/2426)).
- Per-app metrics name a node answer that reached the 10,000-row limit in a `Truncated` warning, which `relish metrics` prints, and the merge drops duplicate rows that weren't adjacent ([#2477](https://github.com/reliaburger/reliaburger/issues/2477)).
- Firing alerts survive a Bun restart (`alerts-firing.json` in the data directory), so their resolve is still sent ([#2478](https://github.com/reliaburger/reliaburger/issues/2478)).
- A log export that fails part-way keeps the files it already uploaded, so the next run resumes after them ([#2432](https://github.com/reliaburger/reliaburger/issues/2432)).
- A council member that needs a snapshot now catches up over a slow link: each snapshot chunk gets 10 s instead of openraft's 200 ms, carries 256 KiB instead of 3 MiB, and travels as base64 instead of a JSON number array ([#2367](https://github.com/reliaburger/reliaburger/issues/2367)).
- A follower far behind on large log entries catches up: AppendEntries batches stop at 4 MiB of entries, a frame over 48 MiB asks openraft for a smaller batch instead of looking like an unreachable peer, and a council write over 32 MiB of JSON is refused with a clear error before it reaches the log ([#2370](https://github.com/reliaburger/reliaburger/issues/2370)).
- Gossip drops any incarnation above `u64::MAX / 2` from the wire, and a refutation saturates instead of wrapping. One claim at `u64::MAX` used to pin a node Dead ([#2395](https://github.com/reliaburger/reliaburger/issues/2395)).
- A node back on a new IP under the same name is probed at its new address by every peer ([#2475](https://github.com/reliaburger/reliaburger/issues/2475)); a reaped gossip member leaves a tombstone, so a peer that missed its death can't bring it back ([#2474](https://github.com/reliaburger/reliaburger/issues/2474)); and a refutation, or a restarted node's return, goes out ahead of failure rumours at every hop, so after a correlated failure it reaches the cluster within the suspicion timeout ([#2441](https://github.com/reliaburger/reliaburger/issues/2441)).
- Apps that open IPv6 sockets and reach IPv4 through them (Java and .NET by default) reach services. connect6 rewrites a v4-mapped VIP (`::ffff:127.128.x.y`) as connect4 rewrites the VIP, with the same view-lease fence, Smoker faults and isolation; before, those connects got ECONNREFUSED ([#2460](https://github.com/reliaburger/reliaburger/issues/2460)).
- Ingress: a backend's refusal of a WebSocket upgrade (401, 403, 429 and so on) reaches the client with its status, headers and up to 64 KiB of body, instead of 502 ([#2470](https://github.com/reliaburger/reliaburger/issues/2470)); the HTTPS redirect names the HTTPS port when the `Host` named a port ([#2471](https://github.com/reliaburger/reliaburger/issues/2471)); and a drain on a failover candidate a request never tried no longer holds up or cancels that request ([#2469](https://github.com/reliaburger/reliaburger/issues/2469)).
- Ingress round-robin keeps its position when the routing table is rebuilt or a service's eBPF backend entry is rewritten, so a quiet route no longer sends everything to its first instance ([#2454](https://github.com/reliaburger/reliaburger/issues/2454)), and a full per-SNI certificate cache evicts hosts that lost their `tls = "cluster"` route, then the least recently used, instead of signing a new leaf on every handshake ([#2430](https://github.com/reliaburger/reliaburger/issues/2430)).
- Rootful runc containers run under Docker's default seccomp filter with no-new-privileges, masked `/proc` and `/sys` paths, and a cgroup namespace that shows a container only its own cgroup ([#2402](https://github.com/reliaburger/reliaburger/issues/2402)), with a default `pids` limit ([#2386](https://github.com/reliaburger/reliaburger/issues/2386)). The unpacked image store, the bundle directory and the volumes directory are `0710` root with container root's group, and sized volumes are mounted `nosuid,nodev`, so other local users on a node can't reach container files or set-uid binaries ([#2412](https://github.com/reliaburger/reliaburger/issues/2412)).
- A node keeps its container subnet when its hostname changes, so its running containers stay under Bun's control, and the first subnet chosen skips any that overlaps a host network ([#2420](https://github.com/reliaburger/reliaburger/issues/2420)).
- A workload certificate that expired while the council couldn't sign is renewed as soon as a leader answers again, without restarting the instance ([#2533](https://github.com/reliaburger/reliaburger/issues/2533)).
- Bun no longer exits at start-up when re-adopting a surviving instance takes over 10 s: it stops that instance and starts it again, and adopts the rest ([#2541](https://github.com/reliaburger/reliaburger/issues/2541)).
- The health tick's state sweep no longer starts every remaining runtime read at once after its one-second deadline ([#2457](https://github.com/reliaburger/reliaburger/issues/2457)).
- A build left `Running` for a day, because its runner was lost or its final state never reached Raft, is reported failed and pruned, and the final state write rides out a leader election ([#2376](https://github.com/reliaburger/reliaburger/issues/2376)).
- The autoscaler forgets an app that is deleted or stops autoscaling, so an app re-created under the same name doesn't inherit the old one's cooldown ([#2434](https://github.com/reliaburger/reliaburger/issues/2434)).
- The scheduler reserves memory only for the replicas it places, not every replica an app asks for ([#2368](https://github.com/reliaburger/reliaburger/issues/2368)), and no longer counts a reported instance's GPUs twice ([#2476](https://github.com/reliaburger/reliaburger/issues/2476); latent until GPU requests are allowed, [#1204](https://github.com/reliaburger/reliaburger/issues/1204)).
- A self-upgrade whose new version exits before it reads its config (a renamed key, a stricter check, a removed flag) reverts by itself after `max_boot_attempts` + 1 starts, instead of restarting for ever ([#2415](https://github.com/reliaburger/reliaburger/issues/2415)). A self-upgrade also asks the candidate binary its version and refuses one whose own version differs from its label ([#2403](https://github.com/reliaburger/reliaburger/issues/2403)).
- The cluster upgrade orchestrator bounds every node probe (5 s) and directive (150 s) and probes a group concurrently, so one node that accepts connections and never answers can't hang the run ([#2416](https://github.com/reliaburger/reliaburger/issues/2416)), and it no longer rewrites the whole run to Raft every 3 s once a node is healthy ([#2422](https://github.com/reliaburger/reliaburger/issues/2422)).
- Registry GC keeps the binary of an active (or paused) rolling upgrade; it was deleted an hour or two after upload, and every remaining node then failed its step ([#2419](https://github.com/reliaburger/reliaburger/issues/2419)).
- A digest-pinned image pushed to the cluster or served by the pull-through cache restarts from local disk without the council leader ([#2421](https://github.com/reliaburger/reliaburger/issues/2421)).
- A task array's run no longer fails as "no node can run it" when the only node that could run it misses one sync: a node whose syncs fail holds the run open for the silence timeout ([#2371](https://github.com/reliaburger/reliaburger/issues/2371)).
- A chunk report the leader can't accept no longer leaves a run `Running` for ever: the leader logs it, and after three ticks the chunks go back to the queue. A task whose runner panics is recorded as failed ([#2372](https://github.com/reliaburger/reliaburger/issues/2372)).
- A zero-filled tail after a power cut is treated as a torn tail, not corruption, in a task ledger ([#2377](https://github.com/reliaburger/reliaburger/issues/2377)) and in a task array's failure-output segment, where it had read as empty output for task 0 ([#2525](https://github.com/reliaburger/reliaburger/issues/2525)).
- `relish batch results` no longer lists workers that never ran a large array as unreachable ([#2473](https://github.com/reliaburger/reliaburger/issues/2473)).
- A volume-snapshot sweep no longer trusts an archive already at its key on size alone. On a `file://` destination it hashes the archive, and anything at the key that isn't the archive (cut short, or zeroed by a destination that lost data) is replaced and logged rather than counted as a backup or refused for ever ([#287](https://github.com/reliaburger/reliaburger/issues/287)).
- The GitOps webhook no longer clones the whole desired state or unseals its secret before checking a request's signature, so junk requests to the public endpoint stay cheap ([#2388](https://github.com/reliaburger/reliaburger/issues/2388)), and it refuses a signed delivery resent under a new delivery ID, on every node ([#2408](https://github.com/reliaburger/reliaburger/issues/2408)).
- A token-only GitOps repository URL (`https://TOKEN@host/...`) no longer puts the token in git's arguments or the clone's config, and an existing clone's config is cleaned on the next sync ([#2414](https://github.com/reliaburger/reliaburger/issues/2414)).
- OS update staging gives up on a download that stalls for 60 s, or a `systemd-sysupdate` that runs past 20 minutes, and reports the node `Failed` with the reason, instead of staying in `Staging` until Bun restarts ([#2418](https://github.com/reliaburger/reliaburger/issues/2418)).
- `relish netboot` gives a machine's install ticket only to the address that machine's DHCP request came from, and one ticket per machine, so another host on the LAN can't mark it installed or fill the ticket table ([#2389](https://github.com/reliaburger/reliaburger/issues/2389)).

## [0.3.3] - 2026-11-27

Loose ends before outside testers: the bugs they would see first, Buildah on the appliance, the appliance console, and fixes from the Wyse lab. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.3.3), [release log](docs/release-log.md#033-loose-ends-before-outside-testers).

### Upgrading

- 0.3.3 keeps 0.3.2's wire protocol (61) and state format (76), so a 0.3.2 cluster rolls onto it in place with `relish upgrade start v0.3.3` ([upgrading from 0.3.2](docs/releasing.md#upgrading-from-032)). A context saved by 0.3.2 or earlier lists only node 1; the appliance manual says how to add the others. An instance adopted across the in-place upgrade from 0.3.2 has no recorded host paths, so the restart re-check passes it over until it's next deployed ([#2335](https://github.com/reliaburger/reliaburger/issues/2335)).

### Added

- `relish os boot-logs` shows where each appliance sends its kernel and boot logs, and `--to <address>[:<port>]` or `--off` changes it on a claimed cluster, so turning them on or off no longer means editing a file on the node's EFI partition. It needs a cluster-wide admin token and is audited with the target. `--to` checks that the kernel really streams to the new target and warns with the netlog script's reason if it doesn't; a node that didn't answer is reported as "may not have changed"; a node whose bun predates the command says to upgrade it first; and relish, bun and the boot script read a target the same way, with no sign and no leading zeros ([#2149](https://github.com/reliaburger/reliaburger/issues/2149), [#2323](https://github.com/reliaburger/reliaburger/issues/2323)).
- `relish netboot --stream-logs=units` prints only the boot units' lines and kernel panics, leaving the kernel's other lines to `os/logs/<mac>.log`; `--stream-logs` alone still prints every line. The homepage's netboot demo uses it ([#2149](https://github.com/reliaburger/reliaburger/issues/2149)).
- A development quickstart (`--development-binaries`) can boot a locally built guest image named by `RELIABURGER_DEV_GUEST_IMAGE`, refused unless its digest matches `RELIABURGER_DEV_GUEST_IMAGE_SHA256`. Without `--development-binaries` the variables are refused, so a release quickstart always boots the signed release image ([#2346](https://github.com/reliaburger/reliaburger/issues/2346)).
- A `relish` context on a bare-metal or `relish setup --ssh` cluster lists every node, not only node 1. When the node it asked last doesn't accept a connection within 1.5 s, relish tries the others, says on stderr which it passed over and which answered, and remembers that one; when none answers, the error lists every endpoint and why. `relish mcp` falls over the same way to make or replace its read-only token. A context saved by 0.3.2 or earlier still loads and lists only node 1 ([#2241](https://github.com/reliaburger/reliaburger/issues/2241), [#2304](https://github.com/reliaburger/reliaburger/issues/2304)).
- `relish apps NAME` lists each instance's sidecars under it, with their state (`starting` while their app starts, then `running`, `backoff`, `restarting` or `stopped`) and how often each has restarted on its own, and `-o json` carries them on each instance as `sidecars`. They used to show up only as Restart events ([#1315](https://github.com/reliaburger/reliaburger/issues/1315), [#2336](https://github.com/reliaburger/reliaburger/issues/2336)).
- The appliance's console on tty1 is a read-only dashboard: tabs for status (with `relish status`'s lines and `wtf`'s counts), metrics with a short history, this node's workloads, gossip and Raft with each node's lag, versions, and the journal of `reliaburger` and `reliaburger-boot-check`. Keys `1` to `6` and the arrows move between tabs; nothing on it changes the node. It reads bun as a new read-only principal, `__console`, whose token every node derives from the cluster's master key, so there's no token to make. While bun doesn't answer it shows the screen it always had, and an unclaimed machine's claim key also appears in large letters. `bun appliance console` loses `--tty` and draws on its controlling terminal, so it also runs over `ssh -t` ([#2150](https://github.com/reliaburger/reliaburger/issues/2150)).
- The appliance console's hidden `7` tab is a game with one invader for each running app instance on the node. With `[appliance] console_chaos = true`, a hit kills that instance as an ordinary Smoker fault injected by `__console`, under the node's fault policy and Smoker's rails. The setting is the council's and is off by default: `relish os settings` shows it, and `relish os settings --set FILE` changes it, for an unscoped cluster admin only, audited as `os.settings_set` ([#2151](https://github.com/reliaburger/reliaburger/issues/2151)).
- `relish apply --wait` returns once every app in the file runs its replicas, passes its health checks and has finished rolling out, printing each step (placed, pulling, waiting for the health check, ready). `--timeout` bounds it (5 minutes by default); when it runs out, `apply` names each app that isn't ready and why, and exits 1, while a failed apply exits 3, 4 or 5. With `-o json` it answers in the `relish.agent/v1` envelope. A daemon set or an autoscaled app counts the replicas the scheduler placed ([#2240](https://github.com/reliaburger/reliaburger/issues/2240)).
- `GET /v1/apps/{app}/{namespace}/spec` reads one app's spec back: the one the cluster runs, and the one declared where they differ, with every env value, config file content and recognisable secret (including the value after a flag like `--password`, and a token in a health check or ingress path) left out and each redacted field named. A controller uses it to see an app's image and resources, and `relish tui`'s Config tab now shows it instead of saying the API doesn't expose it. It's a new read, so the wire protocol stays at 61; a follower whose leader still runs 0.3.2 answers 404, saying the leader may run an older release, until the leader is upgraded ([#1322](https://github.com/reliaburger/reliaburger/issues/1322)).

### Changed

- `relish apply`, `POST /v1/apply` and a GitOps sync refuse a spec in which an env value, an inline config file's content, or a word of a command, argument list, script or `pre_stop` (in the app, an init container or a sidecar) is exactly `[redacted]`, the placeholder the spec read writes in place of a secret. Applied, it would have replaced the real value with that text. The error names the field: `app web: env DB_PASSWORD is the redaction placeholder "[redacted]"; supply the real value` ([#2296](https://github.com/reliaburger/reliaburger/issues/2296)).
- The batch-jobs manual opens its host commands section with what a Linux node needs first: mount isolation is on by default, including on nodes `relish setup --ssh` installs, and no host workload runs under it yet, so set `mount_isolation = false` and allowlist the binary, or give the job an image. Running host workloads under mount isolation stays open as [#714](https://github.com/reliaburger/reliaburger/issues/714).
- On Linux with glibc, Bun calls `malloc_trim(0)` on its blocking pool every ten minutes, so its resident memory follows what it holds instead of climbing with the free memory glibc keeps. Under hours of load that kept memory had looked like a leak in `relish nodes`, `relish top` and `top`. A trim that takes 100 ms or more is logged, timed from when the trim starts rather than from when it queued for a blocking thread. macOS builds are unchanged ([#1936](https://github.com/reliaburger/reliaburger/issues/1936), [#2323](https://github.com/reliaburger/reliaburger/issues/2323)).
- The appliance image has Buildah, so `relish build` and the five-minute tour's burger build on a cluster of appliances. Its storage is on the data partition, where Btrfs reflinks the `vfs` driver's layer copies; the image adds `netavark` and `gpg` for it, drops Buildah's 97 MB of test helpers, and grows each `/usr` slot by 54 MB (667 MB to 721 MB of 1,100 MB on x86-64). A multi-platform build can't emulate the other architecture's `RUN` steps on an appliance, so cross-compile or name one `platform`. With no node able to build, the refusal no longer says appliances never can ([#2144](https://github.com/reliaburger/reliaburger/issues/2144)).
- Policy expressions nested more than 32 deep are refused before they're parsed, and `relish apply` checks the admin permission for policy tables before compiling them.
- `relish policy audit` documents what it judges of a job: the definition the cluster keeps, so a one-off job counts while a run refers to it and drops out once its last run is pruned, when it can't run again; and a job pinned by tag and digest is judged by its tag, until the cluster keeps a declared job spec as it does for apps. Tests now cover the audit's job path ([#1322](https://github.com/reliaburger/reliaburger/issues/1322)).
- Apps get first claim on a node's room over batch work. When an app is refused on a node full of job executors and would fit without them, the node holds the room for it: idle executors retire, busy ones retire after their command, and no queued attempt takes it, for up to 60 s and then a 5-minute cooldown. The leader keeps the replica there, and when every node is full of job work it places the app where jobs make the most room. A capacity refusal retries every 5 s while jobs make room and backs off to at most 30 s otherwise, not 300 s. Nothing running is stopped ([#1967](https://github.com/reliaburger/reliaburger/issues/1967)).
- A node that refuses an app's runtime knobs names every knob it refuses, not only the first ([#1316](https://github.com/reliaburger/reliaburger/issues/1316)).
- On Linux, setup, `relish local start` and `relish dev create` refuse QEMU 10.2 or later while io_uring is open to ordinary users. QEMU then aborts under Lima's `user-v2` network (`net_fill_rstate: Assertion 'size == 0' failed`). The message gives the fix, `sudo sysctl -w kernel.io_uring_disabled=1` ([#1495](https://github.com/reliaburger/reliaburger/issues/1495)).
- The quickstart's boot-silence watchdog starts its clock when Lima's host agent boots the VM, not when `limactl start` runs, and gives the console 30 s instead of 60, so a VM that hangs before userspace is restarted about 30 s sooner and a slow disk no longer eats into a healthy boot's time. When it restarts a VM, the note reads "restarted a VM whose console went silent", since the stock image's first-boot network stall trips it on a VM that did boot, and the quickstart guide gives the stock image's real boot times ([#2229](https://github.com/reliaburger/reliaburger/issues/2229), [#2346](https://github.com/reliaburger/reliaburger/issues/2346)).
- The process runtime's owner record keeps only variable names once a launch retires, as runc's journal does; this applies to launches started on 0.3.3.
- The appliance manual and the book describe the two-try fallback, and the Wyse 3040 record has the S6 go/no-go section ([#407](https://github.com/reliaburger/reliaburger/issues/407)).
- CI: the merge queue no longer runs portable macOS. A new nightly workflow, `macos-nightly.yml`, runs it on the newest release train. The release gate PR, the push to `main` and the candidate build still require it.
- CI: cargo-fuzz no longer runs on pull requests, only nightly and by hand.
- CI: draft pull requests, and pull requests from `0-4-0/` branches, run only select jobs, CI policy and lint. Marking a draft ready for review runs the full set.
- CI: mutation testing no longer runs on pull requests. It runs nightly on the newest release train (else `main`), on the lines that changed under `src/` in the last day ([#1723](https://github.com/reliaburger/reliaburger/issues/1723)).
- CI: the Ubuntu 26.04 privileged Linux job is required again, now that it no longer times out ([#1943](https://github.com/reliaburger/reliaburger/issues/1943)).
- `scripts/release/open_train.py` opens a release train in one step: the branch, the merge-queue ruleset entry, the milestone and the gate PR ([#1053](https://github.com/reliaburger/reliaburger/issues/1053)).

### Fixed

- A node checks an app's runtime knobs and host-path sources again each time one of its instances restarts. A restart builds the container from what the deploy approved, so a node restarted with a shorter `[runtime]` or `[storage] allowed_host_paths` list in `node.toml` brought crashed containers back with what it no longer allowed. Now the restart is refused, the instance is `Failed` with the refusal as its reason in `relish apps` and `relish wtf`, and it isn't retried. A `make test-linux` test starts a real runc container with a capability, a network and an IPC sysctl and `/dev/fuse`, and checks each from inside it. The check asks about the host paths the restarting instance itself mounts, so an old instance mid-rollout and one adopted after a Bun restart are both checked. A sidecar that crashes on its own is checked too: a refused one isn't started again, and `relish apps`, the events and a new `relish wtf` warning, `sidecar-refused`, show why, while its app keeps running until its own next restart, which is refused. The check gives up after `[runtime] stop_confirmation_timeout_secs` and retries, so a hung NFS source no longer blocks the instance's stops or piles up threads ([#1316](https://github.com/reliaburger/reliaburger/issues/1316), [#2335](https://github.com/reliaburger/reliaburger/issues/2335)).
- A Bun restart or self-upgrade adopts running sidecars with their app instead of restarting them. Sidecars had no adoption records, so every Bun restart killed them and started them again about a second later. Each now has a record of its own, checked against the runtime like an app's, and it keeps the sidecar's restart count and the rest of its crash-loop backoff, so a Bun restart no longer resets the count to 0 or starts a sidecar in backoff at once ([#1315](https://github.com/reliaburger/reliaburger/issues/1315), [#2336](https://github.com/reliaburger/reliaburger/issues/2336)).
- A Bun restart or self-upgrade no longer kills apps that use `PROVIDER[...]` secrets, or their sidecars. With the process runtime, such an app no longer stops the new Bun starting, which made a self-upgrade revert ([#2334](https://github.com/reliaburger/reliaburger/issues/2334)).
- Bun removes an app's empty cgroup, and its namespace's, once the last workload there has gone, so `relish test` runs no longer leave `rbtest-*` namespace cgroups behind ([#2324](https://github.com/reliaburger/reliaburger/issues/2324)).
- A quickstart VM no longer upgrades itself or restarts Bun without warning. Ubuntu's `apt-daily` timers and unattended-upgrades are masked in the guest image and at first boot, which covers stock and older guest images too, and needrestart, where installed, only lists what it would restart. The guest image build fails if needrestart is installed without that list-only setting. An automatic libc upgrade had restarted Bun mid-run. Security updates arrive with releases, in each release's guest image ([#2274](https://github.com/reliaburger/reliaburger/issues/2274), [#407](https://github.com/reliaburger/reliaburger/issues/407)).
- A batch attempt no longer becomes Unknown, needing a replay, when one exit check misses the process owner's 2 s request timeout on a loaded node. The check asks again, and the attempt becomes Unknown only after 60 s of unanswered checks, with that in the reason. ([#2288](https://github.com/reliaburger/reliaburger/issues/2288)).
- `node_pressure` no longer stays unavailable when Bun starts before systemd enables the cpu and memory cgroup controllers at the root. Bun prepares `/sys/fs/cgroup/reliaburger-chaos` again before each pressure fault, enabling the two controllers in the parent when it offers them. `/v1/capabilities` only checks: it changes nothing on the host, and says what's missing ([#2242](https://github.com/reliaburger/reliaburger/issues/2242), [#2323](https://github.com/reliaburger/reliaburger/issues/2323)).
- `make ci` no longer times out on a machine whose `relish` context names a cluster that's switched off. The examples dry-run test, and the other tests that run `relish` with no cluster of their own, read the developer's context and waited about 4 s per command for it; they now give each `relish` an empty `RELIABURGER_HOME` and drop the endpoint, token and CA variables ([#2290](https://github.com/reliaburger/reliaburger/issues/2290)).
- A token scoped to some namespaces no longer misses its own events on a busy cluster. `GET /v1/events` took the newest `limit` of every node's events merged and only then kept the token's, so another namespace's traffic on one node could push the rest out. Each node's events are now trimmed to the token's scope before the merge ([#1322](https://github.com/reliaburger/reliaburger/issues/1322)).
- `relish apply --wait` works against a cluster whose leader still runs 0.3.2, as it does for most of a rolling `relish upgrade`. A follower passes the old leader's 404 for the rollouts read on as a 404 instead of a 503, and `relish` treats a failed rollouts read as none rather than as a failed look, so the wait no longer times out with no progress. With no rollouts read it counts only the instances running the applied image, so an image update isn't ready while the old version serves, and where it can't tell images apart it warns once that it counts running replicas only ([#2302](https://github.com/reliaburger/reliaburger/issues/2302)).
- `relish token create` refuses the first API token on a plaintext cluster with no master key, which would lock its nodes out with 401; the error names `[security] master_key_path`, and relish prints the cluster's sentence rather than its JSON ([#1248](https://github.com/reliaburger/reliaburger/issues/1248)).
- OS updates on the appliance fall back when the new version never gets a network address, and a broken update now falls back on a Wyse 3040 in a time modelled at about 8½ minutes (two tries, each healthy within 150 s of boot) ([#2245](https://github.com/reliaburger/reliaburger/issues/2245)).
- `allow_from` takes effect on the target's own node as soon as the service catalogue arrives when a caller is removed, as on every other node. That node had kept its local list until the rollout's new instance registered; it now admits only the callers both lists admit, so a removal is immediate and an addition still waits for the rollout ([#2223](https://github.com/reliaburger/reliaburger/issues/2223)).
- `relish apply --dry-run` shows an app a controller owns as owned and skipped (`!`), not as an update, and `/v1/apps` names each app's `owner` ([#1322](https://github.com/reliaburger/reliaburger/issues/1322)).
- A process workload's owner answers every waiting client each turn, up to 32 or 250 ms, instead of one, so a loaded node no longer reports a running workload's pid as unknown, and a backlog of slow clients can't hold off a SIGKILL or the workload's exit for seconds ([#2236](https://github.com/reliaburger/reliaburger/issues/2236), [#2304](https://github.com/reliaburger/reliaburger/issues/2304)).
- A stop asks the process runtime again when its owner doesn't answer in time, instead of failing, so a long disk stall no longer fails stops; a kill nobody answers ends the stop unconfirmed ([#1902](https://github.com/reliaburger/reliaburger/issues/1902)).
- An exec into a runc container (`relish exec`, a pre-stop hook) no longer fails with "process owner request timed out" on a busy node: when the launcher's owner answers late, Bun asks again for up to 30 s instead of refusing the exec ([#2305](https://github.com/reliaburger/reliaburger/issues/2305)).
- A stop's seal no longer fails, or leaves the instance for recovery, when the container launcher's process owner answers a role check late, whether the first or one during the drain. It asks again within the seal's own deadline; an owner that has died still refuses at once. A stop or kill also asks again for the launcher's state when `stop_confirmation_timeout_secs` leaves room for it (not at the default 10 s) ([#2320](https://github.com/reliaburger/reliaburger/issues/2320), [#2328](https://github.com/reliaburger/reliaburger/issues/2328)).
- Two quick Smoker kills of a two-replica app can no longer take the last replica. A killed instance reads `running` until its node's sweep notices it has gone, so a kill now stays on `relish fault list` for 10 s and counts against the replica rail meanwhile ([#2151](https://github.com/reliaburger/reliaburger/issues/2151)).
- `relish netboot --stream-logs` and `relish machines logs` flush each line to the machine's log file as they print it, so a printed line is always on disk.
- `relish apply --wait` exits 4, with `lease_held` and the holder in `-o json`, when another holder's lease refuses the apply, as its exit table says; it had exited 5 as a plain conflict.

### Known issues

- [#2128](https://github.com/reliaburger/reliaburger/issues/2128): one registry deploy case stalled for five minutes after an injected disk fault in 0.3.0's gate. It didn't recur in 0.3.1's, 0.3.2's or 0.3.3's gates, and the cause isn't proven.
- The Wyse lab hasn't run on 0.3.3: the release went out before it, and the lab runs on the released artefacts ([#407](https://github.com/reliaburger/reliaburger/issues/407)).

## [0.3.2] - 2026-11-23

Fixes from the Wyse 3040 lab that need a new appliance image. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.3.2), [release log](docs/release-log.md#032-wyse-lab-fixes).

### Upgrading

- 0.3.2 moves the wire protocol from 60 to 61, because an apply error now carries its real status instead of 500 ([#2157](https://github.com/reliaburger/reliaburger/issues/2157)). Start a fresh 0.3.2 cluster: 0.3.1 and 0.3.2 nodes refuse each other, so there's no rolling upgrade ([compatibility before 1.0.0](docs/releasing.md#compatibility-before-100)).

### Added

- `relish netboot --stream-logs` shows an appliance's kernel and boot logs as it installs and boots, so a machine with no serial port can be watched from the terminal, and `relish machines logs` listens for them on its own. Off by default, because the logs cross the LAN in cleartext ([#2149](https://github.com/reliaburger/reliaburger/issues/2149)).
- `relish tui` has a node page showing what runs on a node and what maintenance would affect ([#2170](https://github.com/reliaburger/reliaburger/issues/2170)), and an app can be scaled from the terminal UI and from `relish` ([#2176](https://github.com/reliaburger/reliaburger/issues/2176)).

### Fixed

- Containers on a node where nothing publishes a port can reach services on other nodes. The node's masquerade rule was created only with the first published port, so a node running only workers or jobs sent their calls out with container source addresses, and they timed out. Network setup now creates it for every container ([#2219](https://github.com/reliaburger/reliaburger/issues/2219)).
- `relish test`'s firewall cases test what `allow_from` does: callers from another namespace, named as `namespace/app`. They put every app in one namespace, where `allow_from` doesn't apply, so on a cluster with eBPF they failed although the firewall worked. A fourth case checks that callers in the target's own namespace aren't restricted, and the case that widens `allow_from` waits until the target's rollout brings the new rule ([#2206](https://github.com/reliaburger/reliaburger/issues/2206)).
- `allow_from` admits listed callers on every node, not only on the node running the target. Each caller's node enforces the list, but only the target's own node knew it, so a listed caller anywhere else was refused (`Operation not permitted`) for as long as it ran. The leader now puts each service's `allow_from` in the service catalogue, which every node fetches every 2 s, and `relish test` has a case with the callers on another node ([#2216](https://github.com/reliaburger/reliaburger/issues/2216)).
- `relish wtf` calls a crash loop only when one instance restarts three times in 15 minutes. Three replicas that restarted once each were a critical `crashloop` for 15 minutes, and the instance a restart named was never read ([#2197](https://github.com/reliaburger/reliaburger/issues/2197)).
- `relish nodes` no longer reports a healthy node as not answering every so often: the alert check releases the metrics store's lock before its query, so the node summary doesn't wait seconds behind it ([#2215](https://github.com/reliaburger/reliaburger/issues/2215)).
- `url` 2.5.9, for RUSTSEC-2026-0592 ([#2213](https://github.com/reliaburger/reliaburger/issues/2213)).

### Known issues

- [#2128](https://github.com/reliaburger/reliaburger/issues/2128): one registry deploy case stalled for five minutes after an injected disk fault in 0.3.0's gate. It didn't recur in 0.3.1's or 0.3.2's gates; 0.3.2 logs what the stalled helper is doing if it happens again, and the cause isn't proven.

## [0.3.1] - 2026-11-21

Fixes from 0.3.0's gate and the first Wyse lab, and a terminal UI you can act from. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.3.1), [release log](docs/release-log.md#031-gate-follow-ups).

### Upgrading

- 0.3.1 keeps 0.3.0's formats (protocol 60, state 76), so a 0.3.0 cluster rolls onto it node by node.
- `relish wtf` prints a compact table by default; `--short` gives only the counts and what isn't OK, and `--explain` says what each check tests. `-o json` is unchanged ([#2180](https://github.com/reliaburger/reliaburger/issues/2180)).
- The built-in `cpu_idle` alert is gone: it fired on idle nodes and marked a healthy cluster DEGRADED. Four node alerts remain, all for high load ([#2171](https://github.com/reliaburger/reliaburger/issues/2171)).

### Added

- `relish tui`: `/` searches the Logs and Metrics tabs and the logs view, highlighting what matches ([#2174](https://github.com/reliaburger/reliaburger/issues/2174)); `k` kills a selected instance after a y/N question, through Smoker's audited kill fault ([#2175](https://github.com/reliaburger/reliaburger/issues/2175)); the nodes view has `relish nodes`' VERSION, CPU, MEM, DISK and ALLOCATED columns ([#2168](https://github.com/reliaburger/reliaburger/issues/2168)); the app detail view shows each instance's image ([#2167](https://github.com/reliaburger/reliaburger/issues/2167)); and Left and Right switch its tabs ([#2169](https://github.com/reliaburger/reliaburger/issues/2169)).
- `relish netboot` puts the local time on every log line ([#2159](https://github.com/reliaburger/reliaburger/issues/2159)).

### Fixed

- `relish apply` says an image isn't in the cluster's registry and how to build or push it, instead of blaming Docker Hub ([#2145](https://github.com/reliaburger/reliaburger/issues/2145)).
- `relish machines claim` waits until the claimed nodes answer before it returns ([#2146](https://github.com/reliaburger/reliaburger/issues/2146)), and a relish context whose files are gone is named in the error and replaced by a new cluster ([#2162](https://github.com/reliaburger/reliaburger/issues/2162)).
- `relish wtf` no longer calls Pickle degraded on a one-node cluster ([#2147](https://github.com/reliaburger/reliaburger/issues/2147)), and `relish nodes` keeps listing a node that shut down cleanly as `left` ([#2164](https://github.com/reliaburger/reliaburger/issues/2164)).
- Node shutdown runs each app's `pre_stop` and stop grace ([#2118](https://github.com/reliaburger/reliaburger/issues/2118)); an OS rollout keeps host-path apps with their data and shows its cordons ([#2119](https://github.com/reliaburger/reliaburger/issues/2119), [#2120](https://github.com/reliaburger/reliaburger/issues/2120)).
- Bun collects a state read that outlived its 1 s sweep instead of queuing a new one behind it, so a crashed container is still seen and restarted on a slow disk ([#2085](https://github.com/reliaburger/reliaburger/issues/2085)).
- Smoker's pause on a runc app freezes the workload, not its launcher ([#2079](https://github.com/reliaburger/reliaburger/issues/2079)).
- An ingress Host header is routed only when its port is digits, and `relish netboot` escapes a TFTP client's error text in its log.
- An exec into a workload has its own deadline inside the client's, so a hung one fails naming the app and the node.

### Known issues

- [#2197](https://github.com/reliaburger/reliaburger/issues/2197): `relish wtf` counts a crash loop per app, so three replicas that each restarted once show as a critical `crashloop` for 15 minutes. Fixed on the 0.3.2 train.
- [#2128](https://github.com/reliaburger/reliaburger/issues/2128): one registry deploy case stalled for five minutes after an injected disk fault in 0.3.0's gate. Two possible causes are fixed in this release, but the cause isn't proven.

## [0.3.0] - 2026-11-20

"Bare metal in an hour": blank machines boot our appliance OS over the network, join a cluster when you claim them, and take signed OS updates one node at a time. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.3.0), [release log](docs/release-log.md#030-bare-metal-in-an-hour).

### Upgrading

- A 0.2.8 cluster can't roll onto 0.3.0: start a fresh cluster. The protocol moves from 58 to 60 and the state format from 74 to 76 ([upgrading from 0.2.8](docs/releasing.md#upgrading-from-028)).
- The root CA is offline. `relish init` writes the root's private key only into the backup you name with `--root-backup`, and without a terminal it needs `--passphrase-file` or `--recipient` too ([#709](https://github.com/reliaburger/reliaburger/issues/709)).
- `relish ca backup` now only re-seals such a backup (`--from`). Keep the backup off the cluster: it's the only copy of the root's key ([#709](https://github.com/reliaburger/reliaburger/issues/709)).

- Appliance clusters start with five council voters, and spare nodes join as Raft learners up to the council cap ([#496](https://github.com/reliaburger/reliaburger/pull/496), [#1469](https://github.com/reliaburger/reliaburger/pull/1469)).
- A memory request no longer sets `memory.high`, and runc refuses a memory limit below runc init's footprint (about 16 MiB on runc 1.4) with a message saying why ([#1955](https://github.com/reliaburger/reliaburger/issues/1955), [#1959](https://github.com/reliaburger/reliaburger/issues/1959)).

### Added

- The appliance OS: our own image on Ubuntu 26.04 with no SSH server, built weekly, signed, and published on an OS channel ([#401](https://github.com/reliaburger/reliaburger/issues/401)). Its A/B updates boot the new version on trial and fall back to the old one by themselves if it doesn't come up ([#406](https://github.com/reliaburger/reliaburger/issues/406)).
- `relish os` rolls an OS version across the cluster one node at a time, workers first and the leader last, moving each node's workloads off before it reboots. `relish os status`, `resume` and `abort` follow and steer it ([#406](https://github.com/reliaburger/reliaburger/issues/406)).
- `relish image download`, `write` and `seed`, and `relish cluster create --bare-metal`, which makes a cluster from blank appliances and confirms the root CA backup before it keeps anything ([#403](https://github.com/reliaburger/reliaburger/issues/403)).
- `relish netboot` installs appliances over the network: ProxyDHCP, TFTP and HTTP from a laptop, macOS included, asking before it wipes a disk that's in use. `--reinstall` reinstalls a machine that already runs Reliaburger ([#404](https://github.com/reliaburger/reliaburger/issues/404)).
- `relish machines` lists unclaimed appliances on the LAN, and `relish machines claim` compares each one's claim key with its console, then sends it a seed ([#405](https://github.com/reliaburger/reliaburger/issues/405)).
- Bun's appliance seed mode (`bun --appliance`) ([#402](https://github.com/reliaburger/reliaburger/issues/402)).
- `relish join-token list` and `revoke` ([#1918](https://github.com/reliaburger/reliaburger/pull/1918), [#1977](https://github.com/reliaburger/reliaburger/pull/1977)).
- JSON and YAML output for every appliance command, and `-o json` everywhere else too ([#1709](https://github.com/reliaburger/reliaburger/issues/1709), [#2025](https://github.com/reliaburger/reliaburger/pull/2025)).
- OIDC signing-key rotation, with an overlap window for workload tokens ([#709](https://github.com/reliaburger/reliaburger/issues/709)).
- Ingress: a route's TLS mode picks its certificate per SNI host, and a drained WebSocket closes with 1001 and carries the HTTP proxy's headers ([#2028](https://github.com/reliaburger/reliaburger/pull/2028), [#369](https://github.com/reliaburger/reliaburger/issues/369)).
- `relish wtf` reports the council's backups, and the last usable admin token can't be revoked ([#2030](https://github.com/reliaburger/reliaburger/pull/2030)).
- The soak gates its API read, API write, rolling-update and app-start SLOs, and checks that every task-array chunk completes exactly once ([#1405](https://github.com/reliaburger/reliaburger/issues/1405), [#1415](https://github.com/reliaburger/reliaburger/issues/1415)).

### Changed

- The quickstart guest moves to Ubuntu 26.04.
- `relish status` exits 3 when the cluster doesn't answer ([#2025](https://github.com/reliaburger/reliaburger/pull/2025)).
- The API takes tokens issued for its own audience only ([#2022](https://github.com/reliaburger/reliaburger/pull/2022)).
- Published ports are held to the cluster's perimeter, and the port map covers local destinations only ([#2026](https://github.com/reliaburger/reliaburger/pull/2026)).
- On boot, Bun waits up to 30 s for the clock before it starts ([#2037](https://github.com/reliaburger/reliaburger/pull/2037)).

### Fixed

- A disk-pressured voter on a full council is replaced, and a replaced voter coming back no longer forces an election ([#1523](https://github.com/reliaburger/reliaburger/issues/1523), [#1524](https://github.com/reliaburger/reliaburger/issues/1524)).
- A stopping Bun no longer launches new job attempts ([#1965](https://github.com/reliaburger/reliaburger/issues/1965)).
- Shared-runc executors start on Ubuntu 26.04: runc 1.4's init was OOM-killed in the helper's 8 MiB ([#1957](https://github.com/reliaburger/reliaburger/issues/1957)).
- A half-written instance record from a power cut no longer fails the node's startup ([#1852](https://github.com/reliaburger/reliaburger/issues/1852)).
- Secret providers are asked again after a brief outage, and only typed references are resolved ([#1308](https://github.com/reliaburger/reliaburger/issues/1308)).
- A join seed sent again gets a fresh join token ([#2002](https://github.com/reliaburger/reliaburger/issues/2002)).
- About 60 fixes from the 16 and 17 November codebase review: council and Raft durability, gossip freshness, council membership on the Raft port, leader forwards with deadlines, layer whiteouts and volume paths resolved by descriptor, ingress HTTP/2 authority, and relish's handling of untrusted text. Each is in the [release's pull requests](https://github.com/reliaburger/reliaburger/pull/490).

### Known issues

- The Raft snapshot, `snapshot.redb`, is stored in plaintext. Only the Raft log is encrypted at rest ([#1775](https://github.com/reliaburger/reliaburger/issues/1775)).
- Node shutdown, which every OS update goes through, skips each app's `pre_stop` hook and gives it 5 s instead of its `stop_grace` ([#2118](https://github.com/reliaburger/reliaburger/issues/2118)).
- An OS rollout moves apps that keep data in a host path away from it, and `relish status` and `relish nodes` don't show its cordons ([#2120](https://github.com/reliaburger/reliaburger/issues/2120), [#2119](https://github.com/reliaburger/reliaburger/issues/2119)).
- A Smoker `pause` of a runc app stops its launcher, not the app ([#2079](https://github.com/reliaburger/reliaburger/issues/2079)).
- In the release gate's 8-hour final, one catalogue deploy from the cluster registry stalled for its whole 5-minute deadline right after an injected disk fault, then recovered by itself with no data lost. The cause isn't verified yet ([#2128](https://github.com/reliaburger/reliaburger/issues/2128)).

## [0.2.8] - 2026-11-12

"Agents you can trust, built faster": an agent proposes a change, and a person or a policy approves it and can undo it. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.8), [release log](docs/release-log.md#028-agents-you-can-trust-built-faster).

### Upgrading

- A 0.2.7 cluster can't roll onto 0.2.8: start a fresh cluster. The protocol moved from 57 to 58 and the state format from 73 to 74 ([upgrading from 0.2.7](docs/releasing.md#upgrading-from-027)).
- `relish status -o json` is now the cluster summary, not a list of instances. A script that read the list should use `relish apps --instances -o json` ([#1788](https://github.com/reliaburger/reliaburger/issues/1788)).
- `relish agent context` has a `data.status` section instead of `data.faults`, `data.alerts` and `data.apps`.
- `relish agent guide`'s default budget is 80 KiB and `relish agent schema`'s is 40 KiB; every other agent command keeps 24 KiB.

### Added

- Proposals: `relish agent propose` asks for an `apply`, `scale`, `rollback`, `stop` or `delete`, the cluster works out the diff, the blast radius and the undo command, and `relish proposal approve` applies it. `relish undo` puts an applied proposal back for 24 hours ([#1682](https://github.com/reliaburger/reliaburger/issues/1682)).
- Agent tokens: `relish token create --agent` makes a token that can propose changes but can't make any directly.
- Policies with `applies_to = "proposals"` can refuse a proposal or approve one without a person, and `relish policy test --proposal` tries them offline.
- Leases: an agent takes a short lease on an app or a namespace, other changes are refused as `lease_held`, and `--break-lease` takes one over. `relish lease` lists, takes, renews, releases and breaks them ([#1686](https://github.com/reliaburger/reliaburger/issues/1686)).
- `relish mcp` makes and renews a read-only token of its own from an admin context, and `relish mcp --propose` adds the proposing and lease tools ([#1730](https://github.com/reliaburger/reliaburger/issues/1730)).
- `relish verify` checks every node's `bun` and your `relish` against the published release, byte for byte, and works on an air-gapped cluster with `--metadata` and `--artefacts` ([#1689](https://github.com/reliaburger/reliaburger/issues/1689)).
- `relish timeline` merges events, the audit log and deploys over a window, and `relish agent explain --placement`, `--restart` and `--refusal` say which rule decided ([#1685](https://github.com/reliaburger/reliaburger/issues/1685)).
- `relish apps` lists apps, problems first, and `relish nodes` gains version, usage and allocation ([#1777](https://github.com/reliaburger/reliaburger/issues/1777)).
- `relish status --check` sets the exit code from the verdict ([#1777](https://github.com/reliaburger/reliaburger/issues/1777)).
- `relish gym run` tests your own agent against known faults in a staging namespace, scores it and cleans up ([#1683](https://github.com/reliaburger/reliaburger/issues/1683)).
- `relish agent patch apply` applies a unified diff to a patch workspace, all or nothing ([#1691](https://github.com/reliaburger/reliaburger/issues/1691)).
- Every agent envelope and `candidate.json` carry the source digest of the build ([#1694](https://github.com/reliaburger/reliaburger/issues/1694)).
- Every release carries a signed `.mcpb` bundle of `relish mcp` per platform ([#1695](https://github.com/reliaburger/reliaburger/issues/1695)).

### Changed

- `relish status` fits the whole cluster on one screen: a verdict (`HEALTHY`, `DEGRADED` or `DOWN`), then one line per part of the cluster. The instance list moved to `relish apps --instances` ([#1777](https://github.com/reliaburger/reliaburger/issues/1777), [#1788](https://github.com/reliaburger/reliaburger/issues/1788)).

### Fixed

- After a SIGKILL during a rollout and then a power cut, a node could exit at every start and never rejoin ([#1849](https://github.com/reliaburger/reliaburger/issues/1849)).
- An agent-loop turn could wait on an fsync of an instance record, or on a blocking file seek in the log reader ([#1853](https://github.com/reliaburger/reliaburger/issues/1853), [#1857](https://github.com/reliaburger/reliaburger/issues/1857)).
- One slow inventory read on a busy node could drop its backends from discovery for a few seconds, so ingress answered 502 ([#1855](https://github.com/reliaburger/reliaburger/issues/1855)).
- A `council_partition` fault cuts Raft in both directions, and healing removes exactly the addresses it blocked ([#1810](https://github.com/reliaburger/reliaburger/issues/1810), [#1820](https://github.com/reliaburger/reliaburger/issues/1820)).
- A new or revoked API token takes effect on every node at once, not after up to 5 s on followers ([#1809](https://github.com/reliaburger/reliaburger/issues/1809)).
- A decrypted secret whose plaintext starts with `PROVIDER[` is no longer sent to a secret provider ([#1819](https://github.com/reliaburger/reliaburger/issues/1819)).
- An app that sets only a short health `interval` gets a matching default timeout instead of a refusal.
- A Smoker `pause` of a process-runtime instance on macOS no longer leaves it `runtime_unknown` for good ([#1772](https://github.com/reliaburger/reliaburger/issues/1772)).
- `brew install reliaburger/tap/relish` works on older Command Line Tools: the formula has bottles ([#1712](https://github.com/reliaburger/reliaburger/issues/1712)).
- Redacting a report twice can no longer expose URL credentials the first pass had hidden ([#1717](https://github.com/reliaburger/reliaburger/issues/1717)).
- `rustix` 1.1.6, for RUSTSEC-2026-0486 ([#1862](https://github.com/reliaburger/reliaburger/issues/1862)).

### Known issues

- Bun's memory grows for the whole of a 24-hour run at 20,000 jobs a second ([#1417](https://github.com/reliaburger/reliaburger/issues/1417)). A later run on 0.2.8 found it's glibc keeping freed memory after log queries, and it levels off.
- The slow agent-loop turn from 0.2.7's final isn't proven fixed ([#1707](https://github.com/reliaburger/reliaburger/issues/1707)).
- `relish mcp` isn't in the MCP Registry yet, and MCP clients show the bundle as unsigned ([#1695](https://github.com/reliaburger/reliaburger/issues/1695), [#1838](https://github.com/reliaburger/reliaburger/issues/1838)).
- The macOS `relish` isn't notarised, so macOS may ask before running it the first time.
- It ships with the `zstd` 0.13 advisories under a reviewed exception ([#1366](https://github.com/reliaburger/reliaburger/issues/1366)).

## [0.2.7] - 2026-11-07

"Built for humans and their agents": a read-only, JSON-only way for an AI agent to read your cluster, with no LLM inside `bun`. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.7), [release log](docs/release-log.md#027-built-for-humans-and-their-agents).

### Upgrading

- A 0.2.6 cluster can roll onto 0.2.7 in place with `relish upgrade start v0.2.7`: it keeps protocol 57 and state format 73 ([upgrading from 0.2.6](docs/releasing.md#upgrading-from-026)).
- Nobody had walked a cluster from the published 0.2.6 to the published 0.2.7 when it shipped. If the walk fails, a fresh cluster still works.

### Added

- `relish agent`, a command family that answers an agent in one JSON envelope within a byte budget ([#1565](https://github.com/reliaburger/reliaburger/issues/1565)).
- `relish agent context` and `diagnose` read the cluster; `manual` and `source` read the manual and the exact source the binary was built from ([#1570](https://github.com/reliaburger/reliaburger/issues/1570), [#1571](https://github.com/reliaburger/reliaburger/issues/1571)).
- `relish agent patch` builds and tries a fix on a throwaway cluster ([#1574](https://github.com/reliaburger/reliaburger/issues/1574)).
- `relish agent report` drafts a redacted bug report for a person to post ([#1575](https://github.com/reliaburger/reliaburger/issues/1575)).
- `relish mcp` serves the same commands as read-only MCP tools ([#1094](https://github.com/reliaburger/reliaburger/issues/1094)), and `relish agent setup` wires them into the agents on a machine ([#1581](https://github.com/reliaburger/reliaburger/issues/1581)).
- Every command takes `-o json` ([#1566](https://github.com/reliaburger/reliaburger/issues/1566)).
- Every release file carries a build provenance attestation ([#1583](https://github.com/reliaburger/reliaburger/issues/1583)).

### Known issues

- One journal turn in the 8-hour final took 1,215 ms against the 1 s budget ([#1707](https://github.com/reliaburger/reliaburger/issues/1707)).
- It ships with the `zstd` 0.13 advisories under a reviewed exception ([#1366](https://github.com/reliaburger/reliaburger/issues/1366)).

## [0.2.6] - 2026-11-06

The rest of the evaluation's bugs, and setup over SSH. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.6), [release log](docs/release-log.md#026-the-rest-of-the-evaluations-bugs-and-setup-over-ssh).

### Upgrading

- A 0.2.5 cluster can't roll onto 0.2.6: start a fresh cluster. The protocol moved from 56 to 57; the state format stays at 73 ([upgrading from 0.2.5](docs/releasing.md#upgrading-from-025)).

### Added

- `relish setup --ssh` builds a cluster on Linux machines you already have, past the seven-seat council cap too ([#1529](https://github.com/reliaburger/reliaburger/issues/1529)).
- Node certificates name the node's address as an IP SAN, so a strict client can connect by address ([#1508](https://github.com/reliaburger/reliaburger/issues/1508)).

### Fixed

- A secure cluster's eighth node no longer restarts for ever: a node with no Raft seat reads the leader's view of the security state ([#1477](https://github.com/reliaburger/reliaburger/issues/1477)).
- Bun with eBPF starts on Linux 7.0 ([#1557](https://github.com/reliaburger/reliaburger/issues/1557)).
- `relish logs --since` stays inside its window for a quiet app ([#1515](https://github.com/reliaburger/reliaburger/issues/1515)).

### Known issues

- A network partition fault can be refused when its source's cgroup lookup is slow ([#1601](https://github.com/reliaburger/reliaburger/issues/1601)).
- One compaction turn in the 8-hour final took 1,232 ms against the 1 s budget ([#1606](https://github.com/reliaburger/reliaburger/issues/1606)).
- It ships with the `zstd` 0.13 advisories under a reviewed exception ([#1366](https://github.com/reliaburger/reliaburger/issues/1366)).

## [0.2.5] - 2026-11-03

Bugs an outside evaluation found: a patch release for product bugs an external evaluation of 0.2.0 ([#1080](https://github.com/reliaburger/reliaburger/issues/1080)) found. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.5), [release log](docs/release-log.md#025-bugs-an-outside-evaluation-found).

### Upgrading

- A 0.2.4 cluster can't roll onto 0.2.5: start a fresh cluster. The protocol moved from 55 to 56 and the state format from 72 to 73 ([upgrading from 0.2.4](docs/releasing.md#upgrading-from-024)).
- `relish logs-export --dest DIR` now writes where `relish` runs, not on the node ([#1499](https://github.com/reliaburger/reliaburger/issues/1499)).

### Changed

- Spare nodes join as Raft learners up to the council cap ([#1494](https://github.com/reliaburger/reliaburger/issues/1494)).

### Fixed

- `relish token create` and the other token commands work on any node, and a new token works at once ([#1496](https://github.com/reliaburger/reliaburger/issues/1496)).
- Issued certificates name their issuer's key, so strict verifiers accept the API ([#1497](https://github.com/reliaburger/reliaburger/issues/1497)).
- The Kubernetes importer keeps an init container's `env` ([#1498](https://github.com/reliaburger/reliaburger/issues/1498)).
- An app can be deleted and applied straight back ([#1500](https://github.com/reliaburger/reliaburger/issues/1500), fixed in part).
- The `relish audit` table names every app an apply touched ([#1501](https://github.com/reliaburger/reliaburger/issues/1501)).
- `relish logs --tail` stays current once an app has logged for a while ([#1493](https://github.com/reliaburger/reliaburger/issues/1493)).

### Known issues

- Delete then re-apply wasn't reproduced on real multi-node runc, so [#1500](https://github.com/reliaburger/reliaburger/issues/1500) stayed open into 0.2.6.
- Secure clusters with more nodes than the council cap still restart the extra nodes ([#1477](https://github.com/reliaburger/reliaburger/issues/1477), fixed in 0.2.6).
- It ships with the `zstd` 0.13 advisories under a reviewed exception ([#1366](https://github.com/reliaburger/reliaburger/issues/1366)), and with RUSTSEC-2026-0433 in `pulldown-cmark`, which it doesn't expose.

## [0.2.4] - 2026-10-30

Extension points: ways to bend Reliaburger without forking it. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.4), [release log](docs/release-log.md#024-extension-points).

### Upgrading

- A 0.2.3 cluster can't roll onto 0.2.4: start a fresh cluster. The protocol moved from 53 to 55 and the state format from 70 to 72 ([upgrading from 0.2.3](docs/releasing.md#upgrading-from-023)).

### Added

- `[policy.*]` rules in CEL that deny, warn or audit, with fill-only defaults and `relish policy audit`, on the one apply path that `relish apply` and GitOps share ([#1302](https://github.com/reliaburger/reliaburger/issues/1302)).
- `[template.*]` and `[use.*]`: templates that expand to ordinary apps ([#1302](https://github.com/reliaburger/reliaburger/issues/1302)).
- Sidecars, stop hooks and gated runtime knobs ([#1302](https://github.com/reliaburger/reliaburger/issues/1302)).
- An exec secret provider ([#1302](https://github.com/reliaburger/reliaburger/issues/1302)).
- A contract for running your own controller ([#1302](https://github.com/reliaburger/reliaburger/issues/1302)).

### Fixed

- A restarting node keeps its replicas instead of the leader moving them ([#1449](https://github.com/reliaburger/reliaburger/issues/1449)).
- The slow agent-loop turn 0.2.3's day run found ([#1419](https://github.com/reliaburger/reliaburger/issues/1419)).
- Capture files the log store has already saved are reclaimed, so they no longer fill a node's disk ([#1418](https://github.com/reliaburger/reliaburger/issues/1418), [#1479](https://github.com/reliaburger/reliaburger/issues/1479)).

### Known issues

- It ships with the `zstd` 0.13 advisories under a reviewed exception ([#1366](https://github.com/reliaburger/reliaburger/issues/1366)).
- `relish logs --tail` can keep returning the same old lines once an app has logged for a while ([#1493](https://github.com/reliaburger/reliaburger/issues/1493), fixed in 0.2.5).

## [0.2.3] - 2026-10-28

Two billion a day: one node runs 14,917 host-job tasks a second and three run 29,350, and a real 24-hour day on three nodes counted 1.81 billion. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.3), [release log](docs/release-log.md#023-two-billion-a-day).

### Upgrading

- A 0.2.2 cluster can roll onto 0.2.3 in place with `relish upgrade start v0.2.3`: it keeps protocol 53 and state format 70, the first release to keep its predecessor's formats ([upgrading from 0.2.2](docs/releasing.md#upgrading-from-022)).
- Expect your apps to restart, and sometimes move, once per node during the walk ([#1449](https://github.com/reliaburger/reliaburger/issues/1449), fixed in 0.2.4).

### Changed

- Host jobs run faster: 14,917 tasks a second on one node and 29,350 on three ([#1030](https://github.com/reliaburger/reliaburger/issues/1030), [#1031](https://github.com/reliaburger/reliaburger/issues/1031)).
- `relish bench` reports per-node successes and fails a node more than 20% over its fair share ([#1362](https://github.com/reliaburger/reliaburger/issues/1362)).

### Fixed

- The two disk writes that could hold a node's agent loop for seconds in 0.2.2 moved off the loop ([#1263](https://github.com/reliaburger/reliaburger/issues/1263), [#1340](https://github.com/reliaburger/reliaburger/issues/1340)).
- `age` 0.11.5 drops the dependency behind RUSTSEC-2026-0387 ([#1333](https://github.com/reliaburger/reliaburger/issues/1333)).

### Known issues

- A restarting node's replicas are replaced, and sometimes moved ([#1449](https://github.com/reliaburger/reliaburger/issues/1449)).
- An upgrade's commit can hold the agent loop while a disk stalls ([#1381](https://github.com/reliaburger/reliaburger/issues/1381)).
- Over a 24-hour run at full rate, Bun's memory grew all day ([#1417](https://github.com/reliaburger/reliaburger/issues/1417)), a node's disk filled ([#1418](https://github.com/reliaburger/reliaburger/issues/1418)) and one turn ran 1 to 2.5 s ([#1419](https://github.com/reliaburger/reliaburger/issues/1419)).
- It ships with the `zstd` 0.13 advisories ([#1367](https://github.com/reliaburger/reliaburger/issues/1367)) and RUSTSEC-2026-0414 in `pulldown-cmark`, which needs an option Reliaburger never sets.

## [0.2.2] - 2026-10-25

UX polish: the CLI review, Kubernetes import and export fidelity, and chaos for jobs. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2), [release log](docs/release-log.md#022-ux-polish).

### Upgrading

- A 0.2.1 cluster can't roll onto 0.2.2: start a fresh cluster. The protocol moved from 52 to 53 and the state format from 69 to 70 ([upgrading from 0.2.1](docs/releasing.md#upgrading-from-021)).
- There's no Intel macOS build any more, and the installer refuses Intel Macs. 0.2.1 is the last release with `relish-macos-x86_64` ([#1018](https://github.com/reliaburger/reliaburger/issues/1018)).
- The installer asks for a mode; pass `--laptop` for the laptop cluster ([#1051](https://github.com/reliaburger/reliaburger/issues/1051)).
- A one-shot `relish exec` exits with the command's exit code ([#522](https://github.com/reliaburger/reliaburger/issues/522)).

### Added

- `relish events` ([#522](https://github.com/reliaburger/reliaburger/issues/522)).
- Smoker can fault jobs and task arrays ([#704](https://github.com/reliaburger/reliaburger/issues/704)).
- A configurable per-command process limit for jobs ([#741](https://github.com/reliaburger/reliaburger/issues/741)).
- Runnable demos and examples for rollouts, GitOps, secrets, shared runc, egress and TLS, showing CPU and memory as they run ([#705](https://github.com/reliaburger/reliaburger/issues/705), [#671](https://github.com/reliaburger/reliaburger/issues/671)).

### Changed

- The CLI review: `relish fmt` keeps comments, with fixes to `-o`, exit codes and `resolve --namespace` ([#522](https://github.com/reliaburger/reliaburger/issues/522)).
- Kubernetes import and export no longer lose data silently, and correlate Services by label selector ([#703](https://github.com/reliaburger/reliaburger/issues/703)).
- `relish lint` and `relish apply` warn when a host app's init container uses a bare command name ([#1047](https://github.com/reliaburger/reliaburger/issues/1047)).

### Fixed

- Host apps' init containers run as host specs and are checked against the allowlist ([#935](https://github.com/reliaburger/reliaburger/issues/935)).
- A namespaced build whose destination is outside its namespace is refused ([#1076](https://github.com/reliaburger/reliaburger/issues/1076)).
- A process job that exits non-zero on macOS is retried ([#934](https://github.com/reliaburger/reliaburger/issues/934)).
- The slow health-probe and discovery journal writes 0.2.1 shipped with ([#1255](https://github.com/reliaburger/reliaburger/issues/1255), [#1266](https://github.com/reliaburger/reliaburger/issues/1266)).
- Outbound HTTPS no longer trusts the distrusted emSign intermediates, RUSTSEC-2026-0381 to -0383 ([#1269](https://github.com/reliaburger/reliaburger/issues/1269)).

### Removed

- The Intel macOS build ([#1018](https://github.com/reliaburger/reliaburger/issues/1018)).

### Known issues

- Two writes still wait on the agent loop, so a slow disk can hold a node's loop for seconds ([#1263](https://github.com/reliaburger/reliaburger/issues/1263), [#1340](https://github.com/reliaburger/reliaburger/issues/1340), fixed in 0.2.3).
- It ships with RUSTSEC-2026-0387 under a reviewed exception ([#1318](https://github.com/reliaburger/reliaburger/issues/1318)), which 0.2.3 removes.

## [0.2.1] - 2026-10-21

Cluster safety: the data-loss and availability bugs the 10 October audit ([#674](https://github.com/reliaburger/reliaburger/issues/674)) found that predate jobs. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.1), [release log](docs/release-log.md#021-cluster-safety).

### Upgrading

- A 0.2.0 cluster can't roll onto 0.2.1: start a fresh cluster. The protocol moved from 51 to 52 and the state format from 68 to 69 ([upgrading from 0.2.0](docs/releasing.md#upgrading-from-020)).
- Tokens have to be reissued anyway: 0.2.1 stores their hashes differently ([#918](https://github.com/reliaburger/reliaburger/issues/918)).
- Clusters are mTLS by default ([#915](https://github.com/reliaburger/reliaburger/issues/915)).
- An app whose health timeout is longer than its interval is refused ([#1019](https://github.com/reliaburger/reliaburger/issues/1019)).
- An image app is refused when the node has no container runtime ([#926](https://github.com/reliaburger/reliaburger/issues/926)).

### Changed

- Rollouts go node by node, keep capacity on an auto-rollback, and mark a failed rollout `RolloutFailed` ([#830](https://github.com/reliaburger/reliaburger/issues/830)).
- The scheduler holds a stale node's placements for a grace period and places what fits, with a reason for the rest ([#869](https://github.com/reliaburger/reliaburger/issues/869)).
- GitOps prunes only what it wrote, and its configuration lives in Raft ([#824](https://github.com/reliaburger/reliaburger/issues/824)).
- The quickstart bundles Lima 2.2.1 ([#983](https://github.com/reliaburger/reliaburger/issues/983)).
- `relish local stop` powers VMs off from inside instead of power-cutting them ([#1023](https://github.com/reliaburger/reliaburger/issues/1023)).

### Fixed

- Registry images stay durable after a node is lost, and untagged images expire ([#826](https://github.com/reliaburger/reliaburger/issues/826)).
- Council recoveries are fenced by epoch and recovery id, and councils stay odd ([#825](https://github.com/reliaburger/reliaburger/issues/825)).
- Gossip is hardened against oversized datagrams and replays ([#854](https://github.com/reliaburger/reliaburger/issues/854)).
- Every chaos fault is reverted when Bun restarts ([#828](https://github.com/reliaburger/reliaburger/issues/828)).
- A power cut while the Raft store grows can't leave a store that won't open ([#1024](https://github.com/reliaburger/reliaburger/issues/1024)).
- A leader that couldn't prove its leadership no longer forwards API calls to itself until it runs out of memory ([#1069](https://github.com/reliaburger/reliaburger/issues/1069)).
- Batch cancel, results and logs are forwarded before a follower answers not found ([#1192](https://github.com/reliaburger/reliaburger/issues/1192)).

### Known issues

- A slow disk can hold a node's agent loop for seconds ([#1255](https://github.com/reliaburger/reliaburger/issues/1255), [#1266](https://github.com/reliaburger/reliaburger/issues/1266), fixed in 0.2.2).
- Outbound HTTPS trusts the distrusted emSign intermediates, RUSTSEC-2026-0381 to -0383, as 0.2.0's does; cluster mTLS isn't affected ([#1269](https://github.com/reliaburger/reliaburger/issues/1269), fixed in 0.2.2).

## [0.2.0] - 2026-10-16

"A million jobs": one-off jobs, cron, deployment hooks and task arrays share one durable lifecycle, and accepted outcomes survive a leader change. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.0), [release log](docs/release-log.md#020-a-million-jobs).

### Upgrading

- A 0.1.6 cluster can't roll onto 0.2.0: start a fresh cluster. The protocol moved from 46 to 51 and the state format from 63 to 68, and a council backup from 0.1.6 can't seed a 0.2.0 cluster ([upgrading from 0.1.6](docs/releasing.md#upgrading-from-016)).
- Nothing the old council held comes across: re-encrypt secrets, create tokens again, trust the new CA, and export job history and snapshot volumes first.
- Every job names its runtime explicitly instead of having it inferred.
- Namespaces are isolated by default, with egress allowlists ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).

### Added

- Compact task arrays: a template and a count, not a record per task ([#266](https://github.com/reliaburger/reliaburger/issues/266), [#588](https://github.com/reliaburger/reliaburger/issues/588)).
- One job lifecycle for one-off jobs, cron, deployment hooks and task arrays ([#638](https://github.com/reliaburger/reliaburger/issues/638), [#642](https://github.com/reliaburger/reliaburger/issues/642)).
- Fresh, shared and host executors, and timed throughput scenarios in `relish bench` ([#654](https://github.com/reliaburger/reliaburger/issues/654), [#639](https://github.com/reliaburger/reliaburger/issues/639), [#640](https://github.com/reliaburger/reliaburger/issues/640)).
- Fair shares across namespaces, bounded job storage, per-definition job history, job metrics and failure alerts ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).
- A durable audit log, read with `relish audit` ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).
- A signed upgrade path ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).
- `relish upgrade check` says when a release needs a fresh cluster instead of a rolling upgrade.

### Fixed

- API authorisation holes closed and host-path sources gated ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).
- Job leases count in app placement ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).

### Known issues

- The quickstart still pins Lima 2.1.0, with its port-forwarder leak ([#982](https://github.com/reliaburger/reliaburger/issues/982)) and CVE-2026-53657; 0.2.1 bundles Lima 2.2.1 ([#983](https://github.com/reliaburger/reliaburger/issues/983)).

## [0.1.6] - 2026-10-07

Operations security: image trust, CA rotation and token lifecycle. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.6), [release log](docs/release-log.md#016-operations-security).

### Upgrading

- A 0.1.5 cluster can't roll onto 0.1.6: start a fresh cluster. The protocol moved from 40 to 46 and the state format from 58 to 63 ([upgrading from 0.1.5](docs/releasing.md#upgrading-from-015)).
- Images bind to a digest at apply. Apply again to pick up a tag that has moved; an apply fails if the registry doesn't answer and the pull-through cache doesn't hold the tag ([#361](https://github.com/reliaburger/reliaburger/issues/361)).
- `deployer` and `read-only` tokens expire after 90 days unless you pass `--ttl-days` or `--no-expiry`, so check what your CI holds ([#363](https://github.com/reliaburger/reliaburger/issues/363)).
- Back up the root CA yourself with `relish ca backup`: `relish ca rotate` asks for the backup ([#362](https://github.com/reliaburger/reliaburger/issues/362)).

### Added

- Upstream trust rules, and cosign signatures checked on upstream images before deploy ([#361](https://github.com/reliaburger/reliaburger/issues/361)).
- Several CAs per role rotated through Raft, `relish ca backup` and `relish ca verify`, and intermediate rotation signed with the root backup ([#362](https://github.com/reliaburger/reliaburger/issues/362)).
- Token rotation with a grace period ([#363](https://github.com/reliaburger/reliaburger/issues/363)).
- A secret key per namespace, opt-in with `secret_key = true` ([#363](https://github.com/reliaburger/reliaburger/issues/363)).

### Fixed

- A consumer sync's inventory read gets an agent-loop turn of its own ([#603](https://github.com/reliaburger/reliaburger/issues/603)).
- Every file lock, the Raft recovery lock included, unlocks before it closes ([#606](https://github.com/reliaburger/reliaburger/issues/606), [#613](https://github.com/reliaburger/reliaburger/issues/613)).
- The quickstart guest leaves its clock to Lima's guest agent ([#608](https://github.com/reliaburger/reliaburger/issues/608)).
- A network partition fault waits until every caller is cut ([#625](https://github.com/reliaburger/reliaburger/issues/625)).

## [0.1.5] - 2026-10-06

Bug fixes: the 0.1.4 soak findings and a codebase audit. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.5), [release log](docs/release-log.md#015-bug-fixes).

### Upgrading

- A 0.1.4 cluster can't roll onto 0.1.5: start a fresh cluster. The protocol moved from 34 to 40 and the state format from 49 to 58 ([upgrading from 0.1.4](docs/releasing.md#upgrading-from-014)).

### Added

- The token list shows scope, expiry and last use, and an hourly sweep removes expired tokens but never the last Admin ([#483](https://github.com/reliaburger/reliaburger/issues/483)).

### Fixed

- Council recovery keeps its restored tokens and recovers from a log that was never snapshotted ([#477](https://github.com/reliaburger/reliaburger/issues/477)–[#479](https://github.com/reliaburger/reliaburger/issues/479)).
- Disk pressure is measured on the real disk, and a voter whose Raft core stopped on a full disk restarts ([#480](https://github.com/reliaburger/reliaburger/issues/480), [#510](https://github.com/reliaburger/reliaburger/issues/510)).
- Remote backends stay routed when a node stops or rolls its last local replica ([#481](https://github.com/reliaburger/reliaburger/issues/481)).
- Snapshot commands go to the node holding the app's volume ([#482](https://github.com/reliaburger/reliaburger/issues/482)).
- A retired runc intent is scrubbed of decrypted environment values ([#476](https://github.com/reliaburger/reliaburger/issues/476)).
- A consumer sync no longer runs over the agent loop's turn budget ([#505](https://github.com/reliaburger/reliaburger/issues/505)).
- A peer TLS connection retires when its client leaf expires ([#509](https://github.com/reliaburger/reliaburger/issues/509)).
- A guest clock step can't crash-loop a node ([#607](https://github.com/reliaburger/reliaburger/issues/607)).
- A codebase audit's 28 fixes, across volumes, logs, metrics, the registry, token scope, ingress, DNS, batch, cron, GitOps and `apply --dry-run` ([#528](https://github.com/reliaburger/reliaburger/issues/528)–[#555](https://github.com/reliaburger/reliaburger/issues/555)).

## [0.1.4] - 2026-10-03

Cross-node logs and events, and security fixes. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.4), [release log](docs/release-log.md#014-cross-node-logs-and-events-security-fixes).

### Upgrading

- A 0.1.3 cluster can't roll onto 0.1.4: start a fresh cluster. The protocol moved from 33 to 34; the state format stays at 49 ([upgrading from 0.1.3](docs/releasing.md#upgrading-from-013)).
- A namespace-scoped token can no longer read the cluster-wide event stream ([#468](https://github.com/reliaburger/reliaburger/issues/468)).

### Added

- `relish logs --until`, `--instance` and a regular-expression `--grep` ([#458](https://github.com/reliaburger/reliaburger/issues/458)).
- Separate stderr capture for process workloads ([#466](https://github.com/reliaburger/reliaburger/issues/466)) and a `--stream` filter ([#467](https://github.com/reliaburger/reliaburger/issues/467)).
- A cluster-wide live event stream ([#469](https://github.com/reliaburger/reliaburger/issues/469)).
- Audit events name who minted or revoked a token or rotated the secret key ([#473](https://github.com/reliaburger/reliaburger/issues/473)).
- Image references with both a tag and a digest ([#455](https://github.com/reliaburger/reliaburger/issues/455)).

### Fixed

- Rollback works over the merged history, at the app's scale ([#459](https://github.com/reliaburger/reliaburger/issues/459), [#489](https://github.com/reliaburger/reliaburger/issues/489)).
- The TUI's job detail shows the selected node's run ([#460](https://github.com/reliaburger/reliaburger/issues/460)).
- Decrypted secrets stay out of world-readable runc bundles and are deleted with the instance ([#475](https://github.com/reliaburger/reliaburger/issues/475)).
- Finalising a secret rotation no longer strands the root CA backup ([#472](https://github.com/reliaburger/reliaburger/issues/472)).

### Known issues

- Consumer syncs can run over the agent loop's turn budget ([#505](https://github.com/reliaburger/reliaburger/issues/505)).
- A pooled peer connection can present an expired leaf ([#509](https://github.com/reliaburger/reliaburger/issues/509)).
- The leader can resign for disk pressure over unexported logs ([#510](https://github.com/reliaburger/reliaburger/issues/510)).

## [0.1.3] - 2026-10-03

Agent loop, observability and test quality: a node agent that never waits inline on slow work, plus cluster-wide views. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.3), [release log](docs/release-log.md#013-agent-loop-observability-and-test-quality).

### Upgrading

- A 0.1.2 cluster can't roll onto 0.1.3: start a fresh cluster. The protocol moved from 28 to 33 and the state format from 47 to 49 ([upgrading from 0.1.2](docs/releasing.md#upgrading-from-012)).
- Instance names are numbered across the cluster: three `frontend` replicas on three nodes are `frontend-0`, `-1` and `-2`, not three `frontend-0`s ([#398](https://github.com/reliaburger/reliaburger/issues/398)).
- `/v1/jobs` respects a token's namespace scope ([#385](https://github.com/reliaburger/reliaburger/issues/385)).
- A rolling upgrade or rollback on a council of exactly two voters is refused before it starts ([#371](https://github.com/reliaburger/reliaburger/issues/371)).

### Added

- Live logs, deploy history, events and jobs answer for the whole cluster, whichever node serves them ([#365](https://github.com/reliaburger/reliaburger/issues/365)).
- A "blocked by quota" status for apps the scheduler won't place ([#326](https://github.com/reliaburger/reliaburger/issues/326)).
- `relish council status`, a council line in `relish status`, and `relish wtf` findings for a fenced node, an epoch split or two leaders ([#424](https://github.com/reliaburger/reliaburger/issues/424)).
- A turn meter, `bun_agent_loop_turn_seconds`, for every agent-loop turn ([#351](https://github.com/reliaburger/reliaburger/issues/351)).
- The installers show what they're downloading ([#395](https://github.com/reliaburger/reliaburger/issues/395)).

### Changed

- Status reads a published snapshot, so `relish status`, the dashboard and the API never queue behind the agent loop ([#351](https://github.com/reliaburger/reliaburger/issues/351)).
- Metrics queries are streamed, so memory no longer grows with history ([#377](https://github.com/reliaburger/reliaburger/issues/377)).

### Fixed

- Restarting Bun no longer moves a managed-volume app onto an empty volume; `relish wtf` reports an app waiting for its volume's node ([#423](https://github.com/reliaburger/reliaburger/issues/423)).
- No split brain after `relish council recover --force`: old voters are fenced on the old epoch ([#424](https://github.com/reliaburger/reliaburger/issues/424)).
- Council snapshots install every entry ([#426](https://github.com/reliaburger/reliaburger/issues/426), [#427](https://github.com/reliaburger/reliaburger/issues/427)).
- The scheduler holds capacity reservations between ticks, and a daemon's own copy no longer counts against it ([#432](https://github.com/reliaburger/reliaburger/issues/432)–[#434](https://github.com/reliaburger/reliaburger/issues/434)).
- Stale healthy endpoints expire, and `cluster stop` waits for workers to confirm ([#431](https://github.com/reliaburger/reliaburger/issues/431), [#435](https://github.com/reliaburger/reliaburger/issues/435), [#436](https://github.com/reliaburger/reliaburger/issues/436)).
- Status never reports a live instance's pid or exit code as missing ([#358](https://github.com/reliaburger/reliaburger/issues/358), [#389](https://github.com/reliaburger/reliaburger/issues/389)).
- A stop no longer fails when the runtime is slow ([#387](https://github.com/reliaburger/reliaburger/issues/387), [#393](https://github.com/reliaburger/reliaburger/issues/393)).

## [0.1.2] - 2026-10-01

Images: multi-platform builds and pulls. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.2), [release log](docs/release-log.md#012-images).

### Upgrading

- A 0.1.1 cluster can't roll onto 0.1.2: start a fresh cluster. The protocol moved from 27 to 28 and the state format from 46 to 47 ([upgrading from 0.1.1](docs/releasing.md#upgrading-from-011)).

### Added

- Multi-platform images: `relish build` builds `linux/amd64` and `linux/arm64` by default, and every node runs the platform that matches it ([#248](https://github.com/reliaburger/reliaburger/issues/248)).
- `relish images` shows each image's platforms ([#248](https://github.com/reliaburger/reliaburger/issues/248)).
- A build cache cap and Buildah storage pruning ([#248](https://github.com/reliaburger/reliaburger/issues/248)).

### Changed

- An upgrade to a release with other formats, or a rollback to a version some node lacks, is refused before a run is recorded ([#339](https://github.com/reliaburger/reliaburger/issues/339), [#350](https://github.com/reliaburger/reliaburger/issues/350)).
- When a node dies its replicas spread over the survivors, and healthy ones stay in place ([#346](https://github.com/reliaburger/reliaburger/issues/346), [#349](https://github.com/reliaburger/reliaburger/issues/349)).

### Fixed

- The pull-through cache holds every platform of a multi-platform upstream image, not just the first puller's ([#353](https://github.com/reliaburger/reliaburger/issues/353), [#354](https://github.com/reliaburger/reliaburger/issues/354)).

## [0.1.1] - 2026-09-30

Bug fixes: every bug known from 0.1.0's soak, early users and a static review. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.1), [release log](docs/release-log.md#011-bug-fixes).

### Upgrading

- A 0.1.0 cluster can't roll onto 0.1.1: start a fresh cluster. The protocol is still 27, but the state format moved from 44 to 46 ([upgrading from 0.1.0](docs/releasing.md#upgrading-from-010)).
- The `logs`, `metrics`, `admin` and `secret-write` permissions now gate every route that needs them, and rollback needs `deploy` ([#298](https://github.com/reliaburger/reliaburger/issues/298), [#325](https://github.com/reliaburger/reliaburger/issues/325)).
- The autoscaler refuses `min = 0` ([#299](https://github.com/reliaburger/reliaburger/issues/299)).

### Added

- `relish inspect` lists every instance across the cluster, with its node ([#306](https://github.com/reliaburger/reliaburger/issues/306)).
- `relish wtf` reports each node's build and warns when nodes run different ones ([#306](https://github.com/reliaburger/reliaburger/issues/306)).

### Fixed

- A redeploy updates the app's ingress host ([#307](https://github.com/reliaburger/reliaburger/issues/307)).
- Log forwarders resume from their checkpoint after a restart ([#308](https://github.com/reliaburger/reliaburger/issues/308)).
- Departed nodes are forgotten, `relish nodes` shows dead nodes as dead, and `bun --version` includes the commit ([#276](https://github.com/reliaburger/reliaburger/issues/276)).
- Snapshot defects from the static review: restores, exports and uploads ([#291](https://github.com/reliaburger/reliaburger/issues/291)–[#294](https://github.com/reliaburger/reliaburger/issues/294)).
- GitOps defects from the static review, including signing that could be bypassed ([#295](https://github.com/reliaburger/reliaburger/issues/295)–[#297](https://github.com/reliaburger/reliaburger/issues/297), [#305](https://github.com/reliaburger/reliaburger/issues/305), [#328](https://github.com/reliaburger/reliaburger/issues/328)).
- The allocation error after a starved deploy ([#309](https://github.com/reliaburger/reliaburger/issues/309)).
- Mayo's memory growth over a long soak ([#310](https://github.com/reliaburger/reliaburger/issues/310)).
- `relish build` works on a laptop cluster ([#327](https://github.com/reliaburger/reliaburger/issues/327)).

### Known issues

- Multi-platform images don't run from Pickle; 0.1.2 fixes it.
- Rootless runc is standalone only.

## [0.1.0] - 2026-09-29

The first release: signed binaries, and a managed three-node laptop cluster from one `curl | sh`. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.0), [release log](docs/release-log.md#010-the-first-release).

### Upgrading

- Nothing to upgrade from. 0.1.0 speaks protocol 27 and state format 44.

### Added

- Two binaries: `bun`, the node agent, and `relish`, the CLI.
- Clustering: gossip membership, a Raft council, a scheduler, automatic rescheduling and rolling self-upgrades.
- Networking: eBPF service discovery with cluster DNS, and an ingress proxy with TLS, streaming and connection draining.
- Security on by default: mTLS between nodes, single-use joins, workload identity, secrets encrypted at rest and signature-gated images.
- A built-in OCI registry (Pickle), metrics you can query with SQL, indexed logs, GitOps, a web dashboard, a terminal UI and a fault injector.
- `relish manual` for offline docs and `relish wtf` to diagnose a cluster.

### Known issues

- Multi-platform images don't run from Pickle.
- Rootless runc is standalone only.

[Unreleased]: https://github.com/reliaburger/reliaburger/pull/2522
[0.2.8]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.8
[0.2.7]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.7
[0.2.6]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.6
[0.2.5]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.5
[0.2.4]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.4
[0.2.3]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.3
[0.2.2]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2
[0.2.1]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.1
[0.2.0]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.0
[0.1.6]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.6
[0.1.5]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.5
[0.1.4]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.4
[0.1.3]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.3
[0.1.2]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.2
[0.1.1]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.1
[0.1.0]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.0
