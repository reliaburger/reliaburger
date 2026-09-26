# Implementation Progress

Single source of truth for what's done and what's next. Check off an item only when it compiles, passes tests, and is committed. See [roadmap.md](roadmap.md) for full details on each phase.

Done means implemented, tested and committed. An implemented subsystem can still
have tracked defects or acceptance gates; those stay unchecked. Git history and the
[archived plans](plans/archive/) hold the per-change evidence (regressions, test
runs, CI results) that used to live here.

> **0.1.0 status (22 September 2026):** the codebase completion backlog below is
> done, including C34. What's left before release is the [PR #167 review
> fixes](plans/2026-09-22-pr167-review-fixes.md) and the four acceptance gates
> V01–V04; see [remaining work](plans/2026-09-22-v0.1.0-remaining-work.md). The
> original [release-readiness review](plans/2026-09-16-v0.1.0-release-plan.md)
> defines the supported scope and the clean-install and real-cluster gates.

## Current completion backlog (17 September 2026)

The [codebase completion plan](plans/archive/2026-09-17-codebase-completion-plan.md)
records the evidence, priority, dependency order, completion test and book chapter
for every item below. IDs are scoped to that plan, not the older C1/M1 review IDs.
The [audit record](qualification/2026-09-17-code-audit.md) separates local defect
reproductions from inspected source and previous acceptance evidence.

The work packages distinguish release correctness, optional refactors and future
capabilities. Correctness fixes need explicit release dispositions. Older unchecked
groups below point into this ledger.

Smaller fixes that CI runs turned up along the way:

- Bootstrap and endpoint fixtures bind port zero and read Bun's reported API
  address instead of reserving a port up front.
- The real `bun --compatibility` query compares its typed response with `CURRENT`.
- Live egress monitoring defers missing-binding checks while a workload is
  Pending or Preparing, so pre-start policy installation isn't mistaken for
  policy loss.
- Automatic DNS port allocation retries when UDP picks a port TCP already holds;
  explicit port conflicts still refuse.
- The request-ID echo fixture uses axum and joins both server tasks.
- Integration-agent volume roots are private to each fixture.
- Two discovery routes missing from the authorisation audit matrix now declare
  System scope.

### Correctness and behavioural contracts

- [x] **C01** Preserve every exported log generation with full content-hash object names. Legacy archive objects stay untouched (migration duplicates are documented in chapter 6).
- [x] **C02** Scope export acknowledgements and pruning proof to the destination URL and node prefix.
- [x] **C03** Serialise all exporters with a cross-process lock, reload before export, and atomically persist a private checkpoint with file/directory sync before acknowledgement or pruning. Physical crash qualification belongs to V02.
- [x] **C04** Report directory, entry and file-read failures with context; reject non-regular/invalid-name candidates and skip only concurrent NotFound reads.
- [x] **C05** Persist replacement launch details before health publication, including private durable records, rollback record/port cleanup and Apple launcher provenance. Runtime crash-injection qualification belongs to V02.
- [x] **C06** Reserve one durable cluster-wide node-experiment slot through Raft, bound to the exact request, process identity and an increasing sequence. Expiry starts fencing and confirmed reversal (never automatic release), membership changes wait behind a committed barrier, and stale activation or release refuses.
- [x] **C07** Require explicit matching protocol/state generations for joining, gossip, Raft, reporting and signed upgrade/rollback preflight. Fresh state is stamped durably; development data, snapshots and backups are refused without migration.
- [x] **C08** Publish readiness only after each owner explicitly acknowledges acquired resources; catch startup panics and fence signals by attempt.
- [x] **C09** Refuse busy cleanup immediately with a retryable conflict, leaving the lease intact while the reaper visits other expired leases.
- [x] **C10** Record pending fault ownership before sending, retain unknown receipts after lost responses, and remove receipts only after confirmed exact-ID reversal.
- [x] **C11** Persist leases with private unique files and file/directory sync; retain transaction ownership across caller cancellation and fence mutations after uncertain persistence. Physical crash and filesystem-sync qualification belong to V02.
- [x] **C12** Sweep existing owned pressure cgroups before the disabled-policy refusal, without enabling new faults.
- [x] **C13** Use one whole-second issuance instant and checked lifetime arithmetic for CA, node and general leaf certificates; derive stored CA validity from signing parameters.
- [x] **C14** Renew and hot-reload every served certificate class. Sustained certificate/storage qualification belongs to V02.
  - [x] Serve the ingress leaf with its original root-signed intermediate.
  - [x] Renew cached ingress leaves at their X.509 validity midpoint, recover after idle expiry and bound leaves by issuer validity.
  - [x] Reload operator certificate/key files once per second as validated pairs; keep the previous pair only while it's valid.
  - [x] Disable ingress session resumption so reconnects validate the current certificate.
  - [x] Retire API/registry/ingress TLS connections within one hour, with 30 seconds for HTTP draining and a deadline that survives WebSocket upgrades.
  - [x] Persist node identities as private atomic snapshots with key/chain/node/serial validation and signed validity dates.
  - [x] Provide a shared live node credential handle that validates and persists replacements before publishing them to existing TLS configurations.
  - [x] Wire the live handle into Bun's API/registry, Raft/reporting clients and servers, internal HTTPS clients and diagnostics.
  - [x] Bound locally issued and CSR-signed node leaves by the real issuer validity, including Node CAs reconstructed from cluster state; refuse expired and future issuers.
  - [x] Add leader-only node CSR renewal that requires both the service principal and the actual TLS peer leaf, with quorum-backed identity, expiry and revocation checks.
  - [x] Start automatic renewal at the signed lifetime midpoint, persist before publishing, report live worker health and retry directly through leader changes.
- [x] **C15** Replace the process counter with positive 20-byte certificate serials carrying 158 bits of operating-system randomness. Issuance refuses on randomness failure; ingress serials stay outside the 64-bit node revocation contract.
- [x] **C16** Return contextual errors for invalid DNS/common-name SANs and a root role passed to intermediate issuance.
- [x] **C17** Reject overflowing relative durations with checked multiplication and parse units at UTF-8 boundaries, including fault minute/hour values and nanosecond delays.
- [x] **C18** Scan allocator candidates once from a random starting point, preserving concurrency and out-of-range adoption semantics.
- [x] **C19** Count only successful metric/rollup deletions, expose removal and directory-enumeration errors with paths, and ignore concurrent NotFound without counting it.
- [x] **C20** Retain worker/minute/series identity through an owned-rollup endpoint and deduplicate before summing; conflicting copies and malformed peers stay explicit unknown evidence. Oversized queries refuse rather than silently truncate.
- [x] **C21** Choose explicit admission refusal: preflight the 1 MiB/100-event limits, bound the receiver to 16 connections and 16 queued reports, and acknowledge queue admission. Normal rollup failures request five-minute backfill; chunking and event production remain F06.
- [x] **C22** Compact export receipts to current source generations only after a successful scan, in the existing locked durable transaction.
- [x] **C23** Treat encoded expiry as critical regardless of stale rotation labels; require positive healthy rotation evidence to suppress near-expiry warnings.
- [x] **C24** Require observed configured voters for quorum arithmetic; unavailable or degraded council responses stay unknown instead of using stale gossip flags.
- [x] **C25** Report node-local Unix device identity without paths and coalesce only matching identities.
- [x] **C26** Track exact CPU sample identities and compare parsed DNS answer IPs, excluding resolver/name/target text.
- [x] **C27** Render collection failures as unknown and keep polling; handle interruption during collection and retain the last meaningful exit outcome.
- [x] **C28** (P2) Make TUI status reflect the cluster.
- [x] **C29** Include missing baseline metrics in the shared benchmark comparison verdict and name missing/new observations in human output.
- [x] **C30** (P1) Replace unconditional unknown catalogue cases with real evidence: secret/config decryption inside the owning container, workload SPIFFE bundle validation against configured CA anchors, and a runnable leased registry fixture checked through an actual HTTP response.
- [x] **C31** Generate 128-bit random run IDs instead of second-resolution timestamps.
- [x] **C32** Replace magic-string verdicts with `CaseError::{Failed, Unknown}` and reject invalid runner timeouts, parallelism and namespace inputs before side effects.
- [x] **C33** Publish actual bound service origins and use explicit managed host forwards, including an authenticated registry forward configurable with `--registry-port`. Workload clients keep Host/SNI through forwards with normal certificate hostname verification. Exact-candidate VM qualification belongs to V03/V04.
- [x] **C34** Finish durable ownership and confirmed cleanup for every supported test resource. The [completion plan's C34 record](plans/archive/2026-09-17-codebase-completion-plan.md#c34--define-lease-ownership-for-the-remaining-test-resources) and the [closure matrix](plans/archive/2026-09-22-c34-closure.md) map each change to its regressions and platform qualification.
  - [x] Bind test tokens to their exact authenticated lease owner and fingerprint; reclaim them through Raft after client or leader failure.
  - [x] Persist node-local job leases and reserved namespaces; reclaim cron registrations and observed runtime retirement after Bun/client death. Catalogue completion requires an observed zero exit and bounded log publication.
  - [x] Persist cron registration, explicit retirement and pre-launch minute claims. Recovery skips missed/uncertain occurrences without catch-up, as agreed.
  - [x] Fence in-flight deploy/cron workers against stop, retirement and overlapping replacement; stop cancels Pending retries.
  - [x] Require observed cleanup before retrying failed create/start/restart. Preserve ownership on failure and enforce in-memory backoff and retry budgets.
  - [x] Refuse uncertain startup adoption and invalid process selectors. Preserve inspection failures in ProcessGrill, runc and Apple; retain runc ownership through OCI, rootfs and network cleanup failures.
  - [x] Persist Pending placement ownership before queueing Deploy; retain it through uncertain convergence and retirement. Resolve peers through advertised per-node API endpoints.
  - [x] Keep failed/expired uploads fenced until deletion and directory sync succeed. Claim exclusive upload-directory ownership on startup, reclaim recognised abandoned files and refuse uncertain inventory.
  - [x] Wait for durable lease absence after HTTP 202; an empty runtime inventory can't override pending ownership.
  - [x] Keep normal Stop/Retire ownership when identity or adoption-record removal fails; sync both parent directories and retry.
  - [x] Require observed runtime exit on rolling/blue-green retirement. Failed or stalled kills and inspection errors preserve both generations; a later Retire cleans up.
  - [x] Propagate artifact-cleanup errors on rolling/blue-green finalisation, retaining the whole retired fleet as stopped if one artifact fails.
  - [x] Propagate runtime/artifact cleanup failures on halt and rollback. Reserve replacement ownership and ports before preparation, and observe kills and exits off the agent loop with bounded waits.
  - [x] Fence the periodic restart driver before off-loop rollout retirement starts.
  - [x] Prevent rollout generation reuse after adoption and preserve stopped/failed cleanup owners when applying replacements.
  - [x] Reject invalid app/job names and namespace labels before runtime mutation.
  - [x] Validate the full recorded identity inventory before recovery mutations; refuse aliases, unsafe labels and inconsistent app/namespace/replica/spec fields without runtime calls or record deletion.
  - [x] Reject cross-app instance-ID collisions (e.g. a new `worker-g1` app against generation one of `worker`) before replacing an owner, checking the whole replica fleet before port allocation.
  - [x] Record every former cluster placement owner and wait for exact retirement acknowledgements after runtime/artifact cleanup and checkpoint persistence.
  - [x] Start and own the production lease reaper in the placement acceptance harness.
  - [x] Add operator-attested decommissioning with permanent identity retirement and fresh enrolment under a new identity on return. One Raft decision clears the node's lease placements and node-chaos obligation, with a durable operator audit. See the [operator plan](plans/archive/2026-09-19-lease-retirement.md).
  - [x] Persist ordinary-job launch intent, observed outcomes, stop intent and the finite retry budget across Bun replacement. Unknown outcomes survive further restarts and require `relish apply jobs.toml --rerun-jobs`. See the [job recovery record](plans/archive/2026-09-19-job-recovery.md).
  - [x] Keep repository metadata independent when identical manifests share a digest: tag deletion affects only its own repository, and signatures survive copies and reload.
  - [x] Persist registry catalogue mutations before acknowledging pushes; serialise publication and GC deletion under an owned guard and recheck required blobs after waiting. See the [registry retirement plan](plans/archive/2026-09-19-registry-retirement.md).
  - [x] Retry approved registry blob deletion after failure or restart, preserving the last other holder.
  - [x] Refuse unconfirmed clustered manifest pushes with 503 instead of success-class 202.
  - [x] Forward worker/follower registry proposals to the advertised leader with live node TLS and service authority, restricting each writer to its own holdings.
  - [x] Bind upload sessions to the exact authenticated credential, alongside repository and lifecycle checks.
  - [x] Persist bounded repository writer receipts and a workload-retirement barrier in cluster and standalone leases; conditional manifest commits and write fences protect reserved repository namespaces.
  - [x] Preserve complete repository paths in peer upload, HEAD and download requests.
  - [x] Provide authenticated, quorum-backed ownership queries for registry workers.
  - [x] Persist the exact repository lease generation in each catalogue, refusing conflicting claims, stale cleanup and unowned existing metadata.
  - [x] Bind HTTP registry writers to their exact authenticated lease, persist receipts and generations before files, fence final publication and supervise node-local repository retirement.
  - [x] Track runtime P2P and healer writes through repository ownership and owned upload tasks.
  - [x] Refuse ordinary or differently leased app/job dependencies on a leased repository, including init-container images, both in HTTP preflight and when Raft applies app specs.
  - [x] Resolve manifest digest reads through repository metadata before reading shared bytes, so retired disposable repositories return 404.
  - [x] Resolve OCI metadata, peer image lookup and quotas through current authenticated catalogue authority on workers and followers; missing authority refuses instead of returning empty metadata.
  - [x] Make the public image list use current catalogue authority on workers and followers; missing authority returns 503.
  - [x] Fence manifest publication with a durable per-node GC generation, retaining transaction ownership from before arbitration through physical collection or publication.
  - [x] Replace stale healer holder sets with storage-node copy confirmation under the committed GC generation fence.
  - [x] Keep cluster lease cleanup Pending until every registry writer confirms retirement.
  - [x] Qualify Bun death with an abandoned registry client and natural lease expiry: the replacement retires leased metadata and partial bytes and preserves identical ordinary content.
  - [x] Qualify registry lease cleanup through a real three-node TLS leader change, including the dead leader's return.
  - [x] Journal managed test volumes and generated configuration before provisioning; retire them only after confirmed runtime cleanup and keep ordinary Stop/rebalance data. Test snapshots are unsupported. See the [storage record](plans/archive/2026-09-19-managed-test-storage.md).
  - [x] Persist an observed short process-job exit together with positive runtime absence, even when the job exited before a PID record existed. The separate window where a launch was never observed at all is closed by the durable process owner below, which records launch intent before starting.
  - [x] Define the 0.1.0 process-mode scope: foreground workloads whose children stay in the supervised process group. Daemonising or detached workloads must use Linux containers.
  - [x] Establish atomic process identity and complete retirement for the supported foreground process group. See the [foreground ownership plan](plans/archive/2026-09-20-foreground-process-ownership.md).
    - [x] Add the single-threaded owner and durable execution gate as an internal Bun helper.
    - [x] Add a persistent ProcessGrill adapter using durable generation records and bounded owner control.
    - [x] Publish first process intent atomically from a private staging directory, holding the launch lock through rename and directory sync.
    - [x] Preserve durable helper reaping across Bun self-upgrade.
    - [x] Keep process diagnostic logs readable after owner loss without weakening signal/absence checks.
    - [x] Discover every published process generation independently of agent adoption records.
    - [x] Separate durable job preparation from permission to execute, including retries.
    - [x] Select durable owners in production Bun and reconcile launch intent before agent adoption.
    - [x] Require durable application adoption metadata before acknowledging deployment or publishing a restarted instance.
    - [x] Fence queued start, stop, kill and create requests against successor generations.
    - [x] Supervise process exec commands through child owners retained by the workload owner.
    - [x] Make recovery fixtures wait for positive owner state across transient socket failures.
    - [x] Require the first-run fixture's own Bun API announcement before accepting listener readiness.
  - [x] Recover runtime/discovery resources created before their first adoption record. See the [OCI ownership plan](plans/archive/2026-09-20-oci-launch-ownership.md).
    - [x] Persist and supervise external runtime commands before activation, retaining their complete inventory and uncertain cleanup obligations.
      - [x] Add the owned-command adapter with durable input, complete inventory, actual output/exit evidence and bounded waits that retain ownership.
      - [x] Retry transient owner-control failures within an external command's wait deadline, requiring positive retirement before returning output.
      - [x] Retry transient cancellation-request failures within the original retirement deadline, then require positive exit evidence.
    - [x] Scope Runc bundles and state to the node data directory and use its selected image cache.
    - [x] Refuse duplicate Runc preparation while an entry still owns the instance, and refuse existing OCI state before any bundle mutation.
    - [x] Journal original Runc specifications and generations before preparation; retain lifecycle guards through cancelled callers and blocking workers.
      - [x] Add the original-intent journal with cross-adapter filesystem claims, stale-generation refusal and private atomic publication.
      - [x] Connect generation claims to short runtime commands, persist admission sealing and require positive draining before cleanup.
    - [x] Recover owned runc launchers and rootless slirp helpers without signalling recovered PIDs.
      - [x] Bind launcher/helper command identities durably before activation, and validate and drain their complete inventories before confirming cleanup.
      - [x] Route auxiliary runtime commands through the bound role owner without holding the adapter mutex during execution.
      - [x] Replace positively retired rootless helper roles without rerunning the launcher.
      - [x] Integrate owned rootless Runc and slirp with pinned namespace descriptors and a network-readiness gate.
    - [x] Retire namespace/link/forwarding mutators before inspecting kernel-resource absence and releasing address reservations.
      - [x] Run namespace and nftables commands under generation ownership, with shared-handle cancellation and caller-SIGKILL recovery.
      - [x] Prune positively retired short-command records under exclusive generation ownership; retain every uncertain attempt.
    - [x] Build the durable rootful Runc adapter: original launch inventory, complete create/start/cleanup workers, owned launcher/exec/network paths and recovered short exits.
    - [x] Retire predecessor adoption and policy records before automatic application restart creates a successor.
    - [x] Reconcile discovery/egress state before adoption. Direct Apple Container recovery is deferred under F10.
      - [x] Disable direct Apple Container for 0.1.0 and exclude it from automatic detection; explicit selection points to the managed Linux VM.
      - [x] Require positive kernel egress retirement: cleanup propagates enumeration/deletion errors and keeps bindings and adoption records until it succeeds.
      - [x] Complete production kernel/discovery recovery across supported modes (enrolled rootful clusters, standalone rootful and standalone rootless).
        - [x] Add a persistent kernel loader with private original ownership, exclusive claims, complete map/link checks and gap-free program replacement.
        - [x] Persist original per-workload egress ownership before map writes and restore it before adoption.
        - [x] Recover correctly from Bun death after an application crash but before its replacement record is published.
        - [x] Correct generated OCI cgroup paths so the container joins the directory where Bun installs pre-start policy.
        - [x] Resolve namespace rules, `relish path`, source-specific faults and recovered egress through verified container identity.
        - [x] Isolate rolling and blue-green generations in their own cgroups.
        - [x] Propagate refused firewall/namespace deletion from the map helpers.
        - [x] Keep failed namespace/firewall reconciliation keys until confirmed removal.
        - [x] Include outbound-only sources in namespace identity and explicit allow rules.
        - [x] Require positive backend withdrawal before Stop/Retire discards workload records.
        - [x] Persist source ownership and install namespace/allow rules before app/job execution; restore original source inventory across recovery.
        - [x] Route each rootful container through its own veth using `/32` endpoints and explicit host/gateway routes.
        - [x] Withdraw explicit Stop/Retire backends before runtime retirement releases an address.
        - [x] Confirm per-instance kernel backend withdrawal before rollout drain/stop.
        - [x] Refuse deployment completion when final kernel backend publication fails.
        - [x] Exclude local stale catalogue endpoints independently of council membership.
        - [x] Confirm live service destination-grant removal before freeing its allocated VIP.
        - [x] Retain a durable generation-bound network reference across natural runtime exit until discovery confirms release.
        - [x] Fence unsafe execution independently of refused discovery cleanup in the owned runtime.
        - [x] Finish durable service/backend ownership: recover original destination grants across crashes and confirm backend withdrawal before network-address reuse on natural exits, rollouts and recovery.
          - [x] Restore exact saved service allocations, including collision-resolved VIPs.
          - [x] Persist complete discovery ownership under an exclusive private checkpoint, with withdrawal/release transitions and fenced uncertain writes.
          - [x] Move discovery journal opening and writes onto blocking workers with owned filesystem claims.
          - [x] Confirm replacement kernel publication before DNS/Wrapper cutover and predecessor retirement.
          - [x] Propagate initial service registration/publication failures before runtime create/start.
          - [x] Confirm health publication before restart, retry refused updates and publish confirmed userspace health.
          - [x] Permit same-instance backend replacement at capacity without consuming a new slot.
          - [x] Propagate rollout and restart backend insertion errors before reporting completion.
          - [x] Preserve the latest service-map watch snapshot when no reader is attached (`send_replace`).
          - [x] Publish confirmed restart addresses to DNS/Wrapper and keep health-checked replacements unhealthy until a successful probe.
          - [x] Avoid blocking the shared drain tracker on a full completion channel.
          - [x] Track ingress requests, including captured failover candidates, before deployment drain starts.
          - [x] Require captured ingress release before ordinary Stop and automatic restart retire the runtime.
          - [x] Require captured-request release at the shared stopped-runtime artifact cleanup boundary.
          - [x] Persist conservative publication ownership before kernel updates and fence uncertain writes.
          - [x] Persist exact original runtime address references before Start; refuse physical release without durable permission.
          - [x] Persist standalone address-release permission before runtime release.
          - [x] Persist confirmed standalone service withdrawal and owner removal before freeing its VIP.
          - [x] Release the predecessor address through durable permission before automatic restart creates a successor.
          - [x] Expose held/released references in the runtime inventory and correlate them against the discovery journal.
          - [x] Preserve kernel backend lookup failures as errors; only KeyNotFound proves absence.
          - [x] Recover standalone discovery before adoption: reserve original VIPs, refuse unknown kernel owners, confirm withdrawal and replay exact release permissions.
          - [x] Preserve active catalogue VIPs across colliding arrivals/departures and temporary report loss; refuse conflicts and exhaustion instead of sharing an address.
          - [x] Reconcile catalogue publication against committed state instead of a last-attempt cache.
          - [x] Register catalogue consumers in Raft before serving placements; keep offline identities through snapshots and discharge them only on permanent decommission.
          - [x] Derive execution-generation fingerprints from original Process/Runc intent records and reject discovery references that don't match.
          - [x] Carry canonical execution identity through snapshots, reports, catalogue entries and remote routing keys.
          - [x] Bound periodic runtime inventory reads across cancelled or timed-out callers.
          - [x] Retry transient leader-unavailable responses during lease cleanup within its existing 30-second deadline.
          - [x] Commit publication generations and bounded withdrawal obligations in Raft. See the [remote withdrawal plan](plans/archive/2026-09-22-remote-withdrawal-ledger.md).
          - [x] Reserve withdrawn VIPs in the allocator and enforce valid, non-conflicting allocations atomically in Raft.
          - [x] Reject stale catalogue writers with a required generation precondition in Raft.
          - [x] Expose generation-bound withdrawal instructions to registered consumers from the same committed snapshot as their catalogue.
          - [x] Accept authenticated consumer receipts for exact generations, deriving the consumer from its TLS identity.
          - [x] Confirm cluster catalogue/DNS/ingress publication before placement work; keep previous views on refusal and retry lost replies.
          - [x] Retain durable, identity-bound consumer publication attempts in the discovery journal.
          - [x] Carry the committed catalogue generation into Bun and refuse stale or rewritten catalogues before changing any view.
          - [x] Commit permanent producer execution fences atomically with catalogue withdrawal, and confirm release only after every consumer obligation finishes.
          - [x] Add authenticated producer retirement and the durable agent release gate.
          - [x] Add durable consumer publication/withdrawal phases, original ingress, pending/ready receipts and permission-checked compaction.
          - [x] Implement the durable consumer agent boundary: journal before publication, withdraw original views, wait for captured request release, and keep receipt retry and generation fences through recovery.
          - [x] Deliver consumer receipts through the placement reconciler with bounded rotating retries against the current leader.
          - [x] Return proven ready receipts even when a publication is refused or stale.
          - [x] Replay an exact durable cluster release permission without contacting the leader again.
          - [x] Register clustered local services at the exact council VIP and retire the local reservation only after runtime holds and consumer guards drain.
          - [x] Persist execution-generation witnesses for local backends without rootful address holds.
          - [x] Activate durable clustered discovery on normal Bun startup using enrolled identity.
          - [x] Report cancelled streamed HTTP responses as body errors rather than successful EOF.
        - [x] Refresh source/egress policy after each successful init exit before the next init or main start.
        - [x] Retain initialiser ownership through deployment failure and refused cleanup; retire children before parent policy and records.
        - [x] Add the OCI interruption driver to privileged CI.
        - [x] Recover from Bun death and caller cancellation during owned rootful OCI init creation, start and retirement, including admission before a runtime intent exists.
          - [x] Cover Bun SIGKILL during init preparation, launcher admission, a running init, namespace retirement and after main adoption.
          - [x] Cover Bun SIGKILL during a foreground process initialiser.
        - [x] Bind firewall destination identity to the allocated service VIP instead of a bare-name hash.
        - [x] Bind Linux command owners to the kernel boot UUID; prior-boot executions retire with unknown outcome without signalling saved PIDs.
        - [x] Bind OCI intent to its original Linux boot, fence old-boot starts and refuse conflicting live cgroups, namespaces and veths.
        - [x] Recover OCI executions and held addresses across a real VM power cycle.
        - [x] Bind persistent kernel manifests to Linux boot identity, rebuilding only empty prior-boot pin inventories.
        - [x] Select owned Runc, persistent kernel policy and durable discovery on normal rootful Runc startup with eBPF enabled; recovery completes before adoption and readiness.
        - [x] Recover a real Bun with rootful Runc, persistent kernel pins and a published discovery reference across a host power cycle.
        - [x] Refuse to recreate missing kernel lock/manifest authority.
        - [x] Defer clustered startup retirement until API-driven confirmation can progress, retaining original runtime and port ownership.
        - [x] Keep discovery polling and withdrawal receipts progressing while a deployment awaits completion.
        - [x] Activate durable rootful Runc/eBPF cluster startup and standalone rootless Runc startup.
        - [x] Preserve live owned Runc workloads through signed Bun upgrade and explicit rollback.
        - [x] Make clustered backend withdrawal retryable after local removal.
        - [x] Preserve the original execution through automatic revert from a failed upgrade candidate.
        - [x] Carry three enrolled Runc/eBPF nodes through controlled leader-last upgrades and rollbacks without restarting their workloads. The rolling orchestrator and full catalogue belong to V01/V02.
        - [x] Mount bpffs before managed Bun startup, preserving existing mounts.
        - [x] Scope rootless Runc to standalone for 0.1.0: Bun refuses rootless Runc with `--cluster` before ownership recovery, adoption or networking. Rootless clustering needs durable host-port release permission first (F02).
        - [x] Reconcile the C34 checkboxes against committed code and a full hosted CI and Build & Release run.
  Node effects use C06/C10. Image-distribution benchmarks report uncontrolled cache state rather than evicting arbitrary images.
- [x] **C35** Select exact chaos scenario names and require only their capability/operation union.
- [x] **C36** Retire legacy partition/isolation and blanket-heal mutations in favour of the guarded catalogue; every old mutation returns an explicit migration error before contacting a node.
- [x] **C37** Require a fresh direct gossip acknowledgement before committing the local upgrade marker, with an enforced rejoin deadline.
- [x] **C38** Retain target ownership through worker completion and rollback, and add scoped, idempotent `relish cancel-deploy`. Only observed cancellation followed by worker completion becomes Cancelled.
- [x] **C39** Validate development cluster names, ownership, resources, runtimes and saved addresses before Lima operations; quote checkout paths and test filters.
- [x] **C40** Serialise rootless helper replacement and adoption, stop and reap displaced owners, and reject conflicting repeat-adoption records.
- [x] **C41** Collect structured observations for every owned VM and exit 1 for missing/stopped VMs, unhealthy APIs or unknown evidence.
- [x] **C42** Preserve labelled metric identities through queries, alert timers, API/dashboard/diagnostic output and webhook incident keys.
- [x] **C43** Resolve short DNS names from runtime-owned source namespaces; unknown or ambiguous sources get REFUSED on UDP and TCP.
- [x] **C44** Remove unused reporting worker listeners, preserve TLS/framing and bound upgrade-harness HTTP requests. An intermittent upgrade stall stays under V02.
- [x] **C45** Poll readiness publication alongside its subsystem owner, avoiding a fair-lock deadlock in both supervision loops.
- [x] **C46** Preserve authorised namespace/service identities through DNS fault publication and lookup; clearing one owner preserves the others.
- [x] **C47** Persist exclusive ownership of the 509-address rootful pool before network mutation; refuse exhaustion and keep reservations on uncertain cleanup. Physical crash qualification belongs to V02.
- [x] **C48** Confine upload Location responses to the declared registry origin before PATCH or PUT.
- [x] **C49** Reject zero and overflowing token lifetimes before hashing or committing credentials.
- [x] **C50** Enforce scope and Deploy/HostExec permission checks for every app/job target before any part of a manifest applies.
- [x] **C51** Require an unscoped user administrator for permission/quota declarations before any mixed-manifest mutation; followers preserve caller credentials.
- [x] **C52** Require unscoped user administrators for token create/list/revoke, join-token creation, secret rotation and image signing.
- [x] **C53** Require an unscoped administrator to inspect or release another credential's lease.
- [x] **C54** Bound transient direct upstream registry reads to four attempts and one deadline (30 seconds for manifest/config, 120 seconds per layer), keeping permanent failures terminal.
- [x] **C55** Bound cross-node log response bodies and own their cancellation.
- [x] **C56** Verify upstream raw manifests, index digests, platform descriptors and configuration bytes before cache publication.
- [x] **C57** Validate upstream layer sizes before cache accounting or allocation, and require downloaded bytes to match their descriptor length.
- [x] **C58** Give Pickle's upstream reads the direct puller's bounded retry policy.
- [x] **C59** Resolve OCI indexes for the Linux container target, independently of the client's operating system.
- [x] **C60** Hold Pickle registry requests to the caller's token scope (found while landing PR #194). A namespace-scoped Deployer could push to any repository over Bearer and Basic alike. Every write (upload start, chunk, completion, manifest `PUT`, copy confirmation) and every routable read now requires a repository named `<namespace>/<app>` inside the scope; bare names, `cache/…` and malformed names are refused to scoped tokens, except blob-only uploads to `_buildcontext` and `reliaburger-bun`. A scoped reader's blob `GET` must be for a blob the named repository references. `/v1/build` destinations use the same rule (a bare destination no longer counts as `default`), and `/v1/images` lists only readable repositories. Unscoped tokens, the service token and the bootstrap window are unchanged.

### Engineering follow-ups

- [x] **H01** Reconcile scheduler/quota/scrape wiring, roadmap test locations, cleanup evidence and disabled-auth chaos policy with their callers; all-feature Rustdoc builds with warnings denied.
- [x] **H02** Remove the obsolete proxy/autoscaler wrappers and unused memory-pressure allocation calculator, and record callers and contracts for the retained library-only helpers. CA recovery remains F04.
- [x] **H03** Remove inert fields: the node `release_url` (use the CLI's `--url`), Wrapper thread-count/strategy fields and the local gossip timestamp counter. Remote resource/image propagation is F01.
- [x] **H04** Use typed alert and scheduler error contracts through Bun, Relish, the TUI and diagnostics; capacity queries go to the live leader scheduler and return a typed refusal.
- [ ] **H05** (P3) Split modules along existing ownership boundaries.
- [x] **H06** Decode DNS with Hickory while Onion keeps operation, source and namespace admission; keep the distinct duration grammars (C17 fixes numeric overflow separately).
- [x] **H07** Execute the public configuration and endpoint-validation examples as doctests.
- [x] **H08** Require nextest 0.9.145 and pin it in CI, so test process leaks fail the gate.
- [x] **H09** Complete node-pressure diagnostic hygiene: continuous bounded stderr draining and creator-thread checks before pressure starts.
- [x] **H10** Reuse the enforcement check's capability for readiness within one health tick.
- [x] **H11** Declare Rust 1.97 as the minimum, pin release/CI builds to 1.98.0 and add a locked minimum-compiler CI job for both feature configurations.
- [x] **H12** Remove the Thrift advisory path (GHSA-2f9f-gq7v-9h6m) by upgrading DataFusion 45 → 55, which brings Parquet 59 without the Thrift crate. Until DataFusion moves to Parquet ≥ 60, `[patch.crates-io]` pins arrow-rs to a fork of 59.3.0 plus upstream fix apache/arrow-rs#10979, because 59 still aborts on impossible list counts on Linux. `tests/suite/parquet_safety.rs` checks the shipped reader through the public API.
- [x] **H13** Make the autoscaler read the metrics the collector really records. It queried a rollup series literally named `cpu`/`memory`, which nothing produces, so it never scaled a real cluster; its only end-to-end test injected that fake series. `metric` now parses to `AutoscaleMetric::{Cpu, Memory}` (anything else fails validation), reads `process_cpu_percent` (percent of one core) or `process_memory_bytes`, and divides by the per-replica request (HPA convention). CPU with no request is measured against one core (ProcessGrill and rootless nodes refuse `cpu` declarations); memory without a request fails validation, and `relish import` warns about such HPAs. `placement::autoscaler_scales_up_from_real_collector_cpu` scales a busy-looping ProcessGrill app through the real collector → rollup worker → leader path. Limits: main process only (no children); runtimes without a host PID produce no per-app metrics.

### Missing capabilities and longer-term scope

- [ ] **F01** (feature) Propagate GPU capacity and cached-image placement evidence.
- [ ] **F02** (feature) Complete rootless and process-workload isolation. Rootless Runc clusters are deferred beyond 0.1.0: implement durable generation-bound host-port publication/withdrawal/release before lifting the startup refusal. Standalone rootless Runc stays supported.
- [ ] **F03** (feature) Finish upstream image trust and worker key separation.
- [ ] **F04** (feature) Add supported CA recovery and rotation operations.
- [ ] **F05** (feature) Complete namespace-scoped identity and token lifecycle.
- [ ] **F06** (feature) Complete the metrics/query and reporting architecture, including event production and versioned chunking beyond C21's explicit admission limits.
- [ ] **F07** (feature) Finish cross-node views and log-stream capabilities.
- [ ] **F08** (feature) Complete WebSocket ingress parity and certificate automation.
- [ ] **F09** (feature) Implement packet-level delay/bandwidth faults if retained. Delay is done (Z6.3: tc netem on each runc caller's interface); bandwidth remains.
- [ ] **F10** (feature) Provide explicit managed-volume retirement and runtime parity. Direct Apple Container is deferred beyond 0.1.0: finish interrupted CLI/daemon recovery before restoring selection. macOS containers use the managed Linux VM.
- [ ] **F11** (feature) Finish supported Kubernetes translations.
- [ ] **F12** (future) Keep the long-term vision explicitly separate.

### Acceptance and release gates

A 0.1.0 release signing identity is committed and its private key is configured
in repository Actions. Signed candidate qualification is still V03.

- [ ] **V01** (gate) Qualify the complete live three-node catalogue on independent Runc nodes.
- [ ] **V02** (gate) Qualify sustained TLS, storage and upgrade recovery on the release candidate.
  - [x] Wait for the retry workload's own counter write before injecting Bun death in the job crash fixture.
  - [x] Keep the ordinary application running in the initialiser-identity regression.
  - [x] Isolate the physical peer-route fixture from host forwarding policy.
  - [x] Add power-cut fixtures for the exporter checkpoint/lock (C03) and `LocalLeaseStore` (C11), driven by `scripts/release/qualify-storage-power-cut.sh` on a disposable VM, and the `v02-loops` Actions lane ([record](qualification/2026-09-25-v02-power-cut.md)).
  - [x] Sync local-destination exports before the checkpoint acknowledges them; the C03 power-cut fixture found every recent export empty after a cut, with most sources already pruned.
  - [x] Open every object store (exports, metrics, snapshot upload, council backups) through one helper that syncs `file://` writes, and add a council-backup power-cut fixture (every retained backup was empty after a cut before the fix).
  - [x] Write the snapshot uploader's `uploaded` metadata atomically and durably (it used a plain `std::fs::write`).
  - [ ] Qualify the snapshot uploader under power cuts (needs Btrfs volumes).
  - [ ] Run the `v02-loops` Actions lane and record its bounds.
- [ ] **V03** (gate) Publish and install the exact signed candidate.
  - [x] Preserve a complete signed candidate with source/run identity and per-file hashes; promote only the qualified bytes without rebuilding.
  - [x] Add explicit HTTPS candidate mirrors to both installers and managed setup without bypassing checksums or signatures.
  - [x] Stage a verified candidate as a pre-release that promotion refuses, and script the real `curl | sh` qualification against it ([runbook](releasing.md#staging-a-candidate)).
  - [x] Qualify hosted candidate creation, staged HTTPS delivery and actual signed installation before promotion ([Apple silicon, 25 Sep](qualification/2026-09-25-staged-install-apple-silicon.md): candidate run 36078958881, two cold installs passed).
- [ ] **V04** (gate) Measure repeated cold installs on the advertised host matrix.
  - [x] Apple silicon: two cold installs, 125–126 s from the first `curl` to a ready three-node cluster ([record](qualification/2026-09-25-staged-install-apple-silicon.md)).
  - [ ] Intel macOS, Linux x86_64 and Linux arm64.
- [x] **V05** Review all dependency exceptions against the current RustSec database and [record their reachability and migration dispositions](qualification/2026-09-18-dependency-exceptions.md). `make audit` also refuses an active rkyv graph.

### Known flakes

CI runs with `retries = 0`. A failure that passes on one re-run is a flake, and it goes
here the same day with its cause. Remove a row only when the fix has landed and a loop
that used to fail passes.

| Test | First seen | Cause | Status |
|---|---|---|---|
| `oci_crash::normal_clustered_bun_recovers_enrolled_consumer_before_adoption` | 23 Sep | Product: rollout finalisation dropped the service reservation retirement needs | Fixed (C3.1) |
| `placement::concurrent_node_kills_and_leader_change_preserve_reserved_capacity` | 22 Sep | Product: a returning node spread stale suspicions; a clear didn't wait for re-admission | Fixed (C3.2) |
| `cluster_failover::decommissioned_worker_releases_cleanup_and_stays_retired_after_leader_change` | 20 Sep | Harness: waited on the leader's appended, not applied, membership | Fixed (C3.4) |
| `job_recovery::killed_bun_*` (two tests) | 19 Sep | Harness: fixed 15–20 s budgets on loaded macOS runners | Fixed (C3.6) |
| `mustard::protocol::tests::gossip_convergence_five_nodes` | 22 Sep | Unseeded RNG; exposed missing anti-entropy (C6.5) | Fixed: push-pull anti-entropy (C6.5); unseeded again |
| `ebpf::standalone_discovery_recovery_preserves_original_routing_and_cleanup` | 22 Sep | Product: 2 s force-kill deadline too short under load | Fixed (C3.3) |
| `registry_recovery::new_leader_retains_registry_cleanup_until_the_killed_writer_returns` | 23 Sep | Harness: treated the retryable lease-leader 503 as fatal | Fixed |
| `owned_rootless::rootless_port_and_launcher_survive_recovery_and_helper_replacement` | 22 Sep | Product: supervision saw the killed helper still `Running` (its owner reaps every 10 ms), probed its dead API, then reported the owner's late exit as "exited before readiness" (the old helper's log) instead of replacing it | Fixed: a helper that dies while probed is replaced; three bounded starts per pass before an error |
| `placement::fault_injection_rejected_when_quorum_at_risk` | 24 Sep | Harness: asserted the rail's exact count; under CI load the leader briefly suspected the healthy follower too and counted 2 (run on #180, passed on re-run and 12/12 locally). Product: with the isolated voter suspected there was no relay left, and a prober without relays suspected its target after one 200 ms window instead of two | Fixed: any quorum-rail refusal counts; relay-less probes now wait both windows (`late_ack_without_relays_keeps_the_target_alive`) |
| `placement::capacity_refusal_from_the_live_scheduler_forwards_without_committing_an_app` | 24 Sep | Product: a follower returned a forwarded lease creation before its own replica had applied it, so the caller's next leased apply there got 404 "lease not found" | Fixed: the follower holds the 201 until its replica has the lease (`lease_created_through_a_lagging_follower_is_in_its_replica_when_returned`) |
| `mustard::protocol::tests::advertised_disk_pressure_reaches_a_remote_member_via_gossip` and six other two/three-node probe tests | watch | Harness: the answering peers ran as spawned `run` loops on the wall clock, so a starved host could miss the prober's 20 ms window (did not reproduce: 0 failures in 600 runs under 12× and 48× `yes` load) | Fixed: peers answer on the same task under paused time (300/300 at 48× load) |
| `job_recovery::killed_bun_retires_api_exec_and_adopts_the_original_workload` | 24 Sep (local sweep) | Product: a process owner hangs up on a client whose request has not arrived within 100 ms; a Bun starved between `connect` and `write` got "Broken pipe" and the exec failed with 404. Status requests already asked again; exec did not | Fixed: exec re-sends a request the owner never received (2 failures in 28 mixed-load runs before; 130/130 after) |
| `registry_recovery::new_leader_retains_registry_cleanup_until_the_killed_writer_returns` ("lost the reserved-port race 3 times", macOS) and `job_recovery::node_job_lease_reaps_a_surviving_process_after_bun_is_killed` (restart hit EADDRINUSE, Linux) | 23 Sep | Harness: test ports came from `bind(0)`, i.e. the ephemeral range. macOS hands out ephemeral ports sequentially to `bind(0)` and `connect()` alike, so a released block is exactly what the next outgoing connections get | Fixed: `reserve_port_block` picks TCP+UDP-free ports from 27000–29999, outside every ephemeral range (8 × 37 port-reserving tests under 12× load, no port race) |
| `suite::ingress::ingress_serves_https_with_self_signed_cert` | 23 Sep | Harness: waited for the route with a fixed 40 × 20 ms budget (under a second, including TLS handshakes) on a loaded macOS runner | Fixed: route waits poll until a 20 s deadline and report the last status seen (not reproduced locally: 60/60 before and after at 24× load) |
| `owned_rootless::rootless_port_and_launcher_survive_recovery_and_helper_replacement` ("Directory not empty") | 24 Sep | Product: the OCI hook writes `init.json` atomically; a hook killed before its rename leaves a `.reliaburger-*` temporary (one found in the VM holding a PID), and rootless retirement then failed `remove_dir` on every retry | Fixed: retirement deletes abandoned atomic-write temporaries first (1 in 50 under load per the coordinator; not reproduced in 190 local runs; 30/30 after) |
| LEAK-FAIL on in-process tests (e.g. `mustard::protocol::tests::ping_receives_ack`), macOS | 24 Sep (local) | Tool: nextest 0.9.140 on macOS reports leaks for tests that spawn no child processes (3 in 300 loaded runs); the pinned 0.9.145 fixes capture-pipe inheritance and reported none in 600 runs or the full portable suite | Not a product leak: use the pinned nextest |
| `oci_crash::normal_clustered_bun_recovers_enrolled_consumer_before_adoption` ("failed to listen for Bun API on 127.0.0.1:0: Address already in use", Linux) | 24 Sep (#184) | Product: Bun set `SO_REUSEADDR` before binding its API socket even for port zero, so Linux could pick an ephemeral port another reuse socket already held, and `listen()` failed | Fixed: address reuse only for a fixed port (`ephemeral_api_port_is_reserved_without_address_reuse`) |
| `grill::process_control::tests::exec_request_is_sent_again_until_the_owner_listens` (NotFound, macOS) | 25 Sep (candidate run 36067525065) | Harness: the fake owner removed the dead socket file before binding a live one; a connect in that gap saw NotFound, which correctly isn't retried | Fixed: bind beside it and rename over the old path in one step |
| V02 soak dry runs: Bun crash-loops after any restart ("runtime retirement requires recovery before adoption") | 25 Sep (V02 harness) | Product: adoption refused an intent left `Retiring` while discovery still held its address, and Bun exits on an adoption error; the Tokio "context is being shutdown" panic that followed is a side effect of that exit | Fixed: adoption completes a retiring generation's cleanup and keeps the held address (`restart_recovers_a_retiring_generation_that_still_holds_its_address`, 3/3 fail before, 3/3 pass after) |
| V02 soak dry runs: a node back from a hard power-off never runs its pinned app again; redeploys loop every ~2 s ("runtime did not expose a process identity") | 25 Sep (V02 harness) | Product: image unpack wrote the rootfs `.complete` marker without syncing the unpacked files, and cached blobs were renamed into place unsynced. After a power cut the marker survived and `redis-server` was 0 bytes, so every start failed with `exec format error` | Fixed: `syncfs` the unpacked tree, then write the marker and blobs with a synced atomic write. Reproduced on a quickstart cluster (0-byte binary, no recovery) and verified with the fix (binary intact, pinned Redis back ~10 s after boot) |
| V02 soak: every node logged `export checkpoint is busy` from both the periodic log export and disk-pressure relief | 26 Sep (V02 soak) | Product: Bun starts the 60 s export and 300 s disk-pressure timers together, so every fifth export tick collides on the non-blocking checkpoint lock. No data lost (a failed export prunes nothing), but the losing disk-pressure pass skipped pruning for five minutes | Fixed: busy is `KetchupError::ExportBusy`; the periodic export skips it silently and disk pressure waits up to 60 s for the lock (`waits_for_an_in_flight_export_then_exports_and_prunes`, failed before) |
| V02 soak: a cluster upgrade with the leader's Bun SIGKILLed mid-walk stayed `Paused` ("directive to rb-…-2 refused: 409 Conflict: failed to fetch binary … error sending request") until the harness rolled back after 600 s | 26 Sep (V02 soak, candidate eced11c) | Product: the restarted leader's orchestrator directed the next node 3 s after boot, before its own Pickle registry was listening. The node's one fetch attempt failed, it answered 409 like any refusal, and the orchestrator paused on the first refusal | Fixed: a node retries an unavailable binary source (connection error, cut-off body, 5xx/408/429, or a timeout: 5 s for headers, 60 s per attempt, 75 s for the whole fetch) for 10 s, then answers 503; the orchestrator keeps a node that failed transiently `Pending` (holding its place in the order) and re-sends with 3 s doubling to 30 s backoff for 2 min before pausing. Permanent refusals (4xx such as a missing blob, signature/hash mismatch, missing external key, version policy) still pause at once. `transient_directive_failure_keeps_the_node_pending_and_retries`, `prepare_rides_out_a_registry_that_is_briefly_unavailable`, and `self_upgrade_cluster::a_registry_outage_at_directive_time_does_not_pause_the_upgrade` (a 25 s registry outage at directive time) |
| V02 soak dry runs: workload identity never reached containers on followers (empty `/run/reliaburger/identity`) | 25 Sep (V02 harness) | Product: every node signed workload CSRs through its own council, which only works on the leader; followers' errors went to a progress message and the container started without an identity | Fixed: followers forward the CSR to the leader's new `/v1/cluster/workload-csr`, which authenticates the node by certificate, derives the SPIFFE id from the instance and requires the app to be scheduled there. Verified on a quickstart cluster: a follower's instance gets `spiffe://laptop/ns/default/app/soak-identity` from the Workload CA |
| V02 soak dry runs: a failing app is redeployed every ~2 s without limit (g170 in 8 minutes) | 25 Sep (V02 harness) | Product: the placement reconciler retried a failed deploy on every poll; the supervisor's restart back-off never applies to an instance that never runs | Fixed: `DeployBackoff` (5 s doubling to 5 min per placement and specification; a changed spec retries at once). `a_failed_deploy_is_not_retried_on_the_next_poll` deployed 3 times in 4.5 s without it, once with it |
| `self_upgrade::isolated_replacement_reverts_without_restarting_its_workload` failed in 0.4 s (`workload_pid` returned `None`) on the second `make test-upgrade` in a row | 26 Sep (local, PR #226) | Harness: workloads run under detached process owners so they survive Bun's exec, and so they also survived the test. A passing run left the `rejoin` owner and its testapp serving on port 46071; the next run's deploy wait saw that stale listener and its own instance never appeared | Fixed: the harness waits for the stop to retire its workloads, then kills every process whose command line names its temporary root, in `shutdown` and again in a `Drop` guard for panicking paths; `shutdown` asserts none are left. `make test-upgrade` passed twice in a row with no leftover processes |
| V02 soak: a refused cluster upgrade stayed `Paused` for 12 hours, and every later `upgrade start` and `upgrade rollback v0.1.0` got 409 "an upgrade is already in progress" | 25 Sep (V02 soak) | Product: the leader recorded a run every node refused (no `upgrades.external_signing_key`), and nothing could end a paused run except `resume` | Fixed: `start` asks each node first (`accepts_network_upgrades` in `/v1/version`) and refuses with the node's name; `relish upgrade abort` ends a pause that moved no node; `rollback` replaces a paused run (`start_refuses_when_a_node_cannot_verify_network_upgrades`, `paused_upgrade_can_be_aborted_or_replaced_by_a_rollback`) |
| V02 soak: the same 10 of 15 `relish test --profile full-runc` cases failed every cycle from the Mac (registry blobs POSTed to `https://127.0.0.1:5050`; volume, ingress and identity cases timed out, cleanup "could not inspect cleanup on node …") | 25 Sep (V02 soak) | Product: `--endpoint` bypassed the context's registry forward, and the runner dialled every node's advertised guest API address, the entry node's included | Fixed: the entry node uses the run's own connection; peers go through its relay when they don't answer directly (`PeerRoute`, relay allows `GET /v1/deploys/history/{app}`); an explicit endpoint keeps the context's forwards when it pins the same CA (`registry_cases_push_through_the_contexts_forward_with_an_explicit_endpoint`) |
| V02 soak: `relish test --chaos --filter dead_worker_node_has_workloads_rescheduled` from the Mac timed out at 600 s; cleanup "could not inspect cleanup on node rb-…-2: bun agent not reachable at localhost:9117" | 26 Sep (V02 soak) | Product: the scenario dialled each node's guest API address; relayed, it would still have failed, because the relay refuses fault requests and reached only live members, and a node-killed target is dead to gossip | Fixed: relayed runs inject and reverse node faults through the entry node (`TestContext::fault_owner`), the relay reads `/v1/cluster/nodes`, and the relay and forwarded reversals reach any member gossip still knows (`KnownMembers`) (`relayed_node_faults_are_injected_and_reversed_through_the_entry_node`, `the_relay_reaches_a_member_gossip_no_longer_counts_as_alive`); not yet re-run on a laptop cluster |
| V02 soak on eced11c: `dead_worker_node_has_workloads_rescheduled` timed out again at 600 s (cleanup confirmed this time) | 26 Sep (V02 soak, 14:10–14:20 UTC) | Test case: the chaos scenarios served `httpd -h /etc` and health-checked `/hostname`, but the pinned BusyBox image has no `/etc/hostname` (or `PATH`), so the three replicas never ran and the node-kill was never injected. Every node's workload PIDs were identical before and after the case, and the leader never logged "cannot place" for its node-pinned apps. The runner's timeout also swallowed the wait's own "last saw" message | Fixed: chaos specs share the self-contained `container_http_script` with the ingress fixture (`chaos_workloads_answer_their_health_check_without_image_defaults`); timeouts carry the case's last wait via `WaitNote` (`a_timed_out_case_reports_what_its_last_wait_was_waiting_for`). Rescheduling itself works: `a_killed_worker_has_its_replica_rescheduled_on_the_survivors` (cluster suite, ~17 s). Not yet re-run on a laptop cluster |
| Staged-install tour: `relish path frontend --to redis` failed in 2 of 6 fresh bootstraps ("0 of 0 backends healthy", or the destination absent from the service map) | 25 Sep (V02 harness) | Readiness race: every instance was `running`, but Redis hadn't passed its first health check or the catalogue hadn't reached the frontend's node | Fixed: `relish path` waits up to 30 s for a healthy backend and says so; a destination with no instances after a 6 s grace fails at once. On a quickstart cluster, path straight after apply passed after 9–14 s; a typo failed after ~8 s |
| V02 soak dry runs: a volume app came back empty after a whole-cluster stop and start (1,877 lines after 36,318 acknowledged) | 25 Sep (V02 harness) | Product: sized volumes are loop-mounted ext4 images, mounted only when first created; after a reboot the app wrote into the bare mountpoint on the root filesystem, unbounded, while its data sat unmounted in the image | Fixed: provisioning an existing loop volume mounts its image again, and refuses if the bare mountpoint already holds files (`loop_volume_is_mounted_again_after_a_reboot`, 2/2 fail before, 2/2 pass after); confirmed unmounted on a rebooted quickstart node |
| V02 soak dry runs: the official Redis image can't write its sized volume (`appendonlydir: Permission denied`) | 25 Sep (V02 harness) | Product: a new loop volume held ext4's `lost+found`, so Redis's entrypoint refused to take the data directory over and dropped to the `redis` user | Fixed: remove `lost+found` after mounting a new loop volume. Reproduced on a quickstart cluster with the image's entrypoint; with the fix Redis owns `/data` and starts |
| V02 soak dry runs: node labels added to node.toml never showed in `relish nodes`, so the pinned app couldn't be scheduled | 25 Sep (V02 harness) | Product: gossip copied a node's new labels onto its membership record, but the snapshot is republished only when a digest changes, and the digest ignored labels; a restart without a state or incarnation change never published them | Fixed: labels and addresses are in the digest (`a_relabelled_member_is_republished_without_a_state_change` fails before, passes after) |
| `job_recovery::node_job_lease_reaps_a_surviving_process_after_bun_is_killed` (restarted Bun not Ready, macOS) | 25 Sep (#202), again 26 Sep (run 36240913878 on #226) | Harness: the restart reuses the dead Bun's four ports, which then stay free for the 3.1 s lease expiry plus the ~3 s the restarted node waits for its restored membership before binding Raft. Any concurrent test's `reserve_port_block` sees them free and can take one, and Bun exits with EADDRINUSE (the assertion only saw `PortRace`, hence no log). No leaked listener: `lsof` after the SIGKILL shows the surviving process owner holds no sockets | Fixed: the restart goes through `spawn_bun_with_port_retry`, same ports first and freshly reserved ones only after a lost race. Holding the Raft port during the restart failed the old test with the CI assertion and passes the new one via the retry; 50/50 passes under two parallel loops of the other Bun-restarting suites |
| `make coverage` on Linux: `llvm-cov report` failed with "…profraw: invalid instrumentation profile data (file header is corrupt)" / "no profile can be merged" after all 4,708 tests passed | 26 Sep (run 36240913878 on #226) | Harness: crash-recovery tests SIGKILL instrumented Bun, owner and workload processes; one killed while writing its profile at exit leaves a truncated `.profraw`, and `llvm-profdata merge` fails the whole report on any unreadable input by default | Fixed: the report steps pass `--failure-mode all`, which skips an unreadable profile with a warning (a truncated file reproduces the exact CI error without it and passes with it). Dropping it only loses that process's partial counts, which can lower coverage but never inflate it; a run with no readable profile still fails |
| V02 soak dry runs: every quickstart guest's node.toml carried the Mac's temp directory as `[process_workloads] script_dir` | 25 Sep (V02 harness) | Product: relish serialised host-dependent defaults (`script_dir` from `temp_dir()`, and `mount_isolation`, false off Linux) into the guests' config, so Linux guests also ran process workloads without mount isolation | Fixed: both are omitted when they equal the writing host's default, so each node computes its own (`host_dependent_defaults_are_left_for_the_reading_node`) |
| V02 soak dry runs: a node back from a hard power-off never became ready (`discovery:startup-cleanup` waiting forever for "other nodes have not yet confirmed the endpoint's withdrawal"; every deploy refused) | 25 Sep (V02 harness) | Product: when the leader is the node cut, the new leader briefly publishes a catalogue without another node's backends (its report hasn't arrived), records a withdrawal, then publishes them again unchanged. Nodes present at the time confirm; the returning node only ever sees the newer catalogue, which lists the same backends, so its receipt never proves and its own startup retirement waits behind that withdrawal. Reproduced with leader-side and consumer-side tracing | Fixed: a publication newer than a withdrawal no longer counts a backend the leader published again as an exposure (`a_backend_the_leader_publishes_again_no_longer_blocks_its_old_withdrawal`). The stuck node recovered ~20 s after the fixed bun was installed |
| V02 soak triage: `relish exec APP` returned 404 ("app not found") whenever the app ran on a node other than the one the CLI talks to, i.e. for two instances in three on a laptop cluster | 25 Sep (V02 triage) | Product: exec went only to the entry node, and the node relay allowed just diagnostic reads and `path` | Fixed: exec finds the node running the app (as `relish path` does) and goes through the entry node's relay, which now forwards `POST v1/exec/{app}/{namespace}`; the target repeats authorisation with the caller's token. Verified: `relish exec` into a node-3 Redis answered PONG |
| V02 soak triage: after `relish stop`, applying the same config straight away didn't start the app again; there was no way to remove an app except GitOps | 25 Sep (V02 triage) | Product: cluster `stop` deleted the app from desired state and also stopped the leader's replica directly, so that node's reconciler still believed it was converged and skipped the redeploy | Changed (maintainer decision): `stop` scales to zero and keeps the spec (`AppStop`, `stopped_apps`), `apply` clears it; new `relish delete` removes the app; no node stops a replica behind its reconciler's back. Formats bumped to protocol 24 / state 40. Gated `cluster_stop_scales_to_zero_until_apply_and_delete_removes_the_app` passes |
| V02 12 h soak: Bun RSS above 125% of its warm size (nodes 1 and 3) and memory-high alerts | 26 Sep (V02 soak) | Product: the Mayo collector used sysinfo's `refresh_all()`, which never removes exited processes and keeps their `/proc/<pid>/stat` files open (Bun's `LimitNOFILE` makes that cache unbounded). Node 1: 951 MB RSS, 1,532 fds (992 stat files) after 27 min with 138 live processes | Fixed: refresh processes with dead-process removal and disable sysinfo's file cache (`exited_processes_are_forgotten_on_refresh` fails with `refresh_all`, passes after) |
| V02 soak: `relish logs APP --tail N` out of order (`ACK 968, 974, 969, 975`), hour-old lines shown as newest after a Bun SIGKILL or VM power-off, Redis client "stepping by 2" | 25 Sep (V02 soak, 22 of ~1,400 writer tails) | Product, three causes: `LogStore` stamped lines with one-second timestamps and sorted on them alone, so rows sharing a second came back in DataFusion's partition order; a single-node tail was `ORDER BY timestamp LIMIT n`, the *oldest* n; and every Bun restart re-followed adopted containers' capture files from byte 0, storing their whole history again as new lines (the "stepping by 2" tail was an old rolling-deploy instance's re-ingested output) | Fixed: a strictly rising nanosecond `sequence` column orders rows and tails (`ORDER BY sequence DESC LIMIT n`, then ascending); the merge keys on `(node, sequence)`; runtimes report each line's capture-file offset and the store skips offsets it already flushed, checkpointed in `ingest-checkpoint.json` after each Parquet file; tails spanning several instances are labelled `[instance]`. Formats bumped to protocol 25 / state 41 (required `sequence` column, new `/v1/logs/entries` rows). `tail_returns_the_newest_lines_in_emission_order` and `lines_within_one_second_keep_emission_order_across_flushes` reproduced the soak's interleaving before the fix; `restart_does_not_reingest_lines_already_flushed` and `lines_lost_with_the_buffer_are_ingested_again_after_a_crash` cover restarts |
| V02 fast-tier soak on eced11c: right after a graceful whole-cluster stop and start, `relish logs soak-redis-client --tail 20` showed `INCR 749 … 782` stepping by 2 while the counter was past 2,504; correct again 40 s later | 26 Sep (V02 soak, failures/4) | Product: the cluster-wide log query asked only the nodes where the app was placed *now*. The client had moved node 1 → 3 → 2 and spent half an hour on node 2; after the restart it was placed on node 1 again, whose newest stored lines were from when it left (a rolling overlap, hence the twos). No re-ingestion: `graceful_restart_does_not_reingest_a_retired_instances_capture_file` and `graceful_stop_keeps_lines_that_were_only_buffered` pass before and after | Fixed: the query fans out to every live member (`ketchup::query::query_targets`); a placed node missing from membership is still a warning. `tail_after_an_app_moves_back_includes_the_nodes_it_ran_on_meanwhile` and three `query_targets` unit tests fail before, pass after |
| `relish_test_catalogue::secure_catalogue_node_jobs_have_durable_ownership_and_confirmed_cleanup` ("workload …/cron is still owned by deploy …", macOS) | 24 Sep (candidate run 36060321071) | Product: a cron firing started a deploy of the leased job just before lease cleanup, and node-local cleanup treated the agent's "still owned by deploy" refusal as final | Fixed: cleanup retries while a deploy owns the job, within its 10 s step budget (`node_job_cleanup_waits_for_a_deploy_that_still_owns_the_job`) |
| `power_cut::actual_power_cut_preserves_acknowledged_log_exports` and `…_council_backups` (V02 power-cut fixtures, not a flaky test: failed every run) | 25 Sep (V02 C03 fixture) | Product: `object_store::parse_url` opens `file://` destinations with fsync off, so an acknowledged export or backup could be empty after a power cut. Exporters then pruned the only good source (up to 162 of 171 files lost); backup retention deleted every older backup, leaving 3 empty ones | Fixed: every object store opens through `object_storage::open`, which syncs local writes (exporter 25/25 and backups 25/25 power cuts after; [record](qualification/2026-09-25-v02-power-cut.md)) |
| `node_pressure::node_pressure_consumes_capacity_outside_bun_and_cleans_up` ("node uses 1743523840 bytes, below the 1844305633-byte target", Linux) | 26 Sep (run 36185229625 on #216) | Harness: asserted node-wide `MemTotal - MemAvailable` after apply, but the helper sizes its ballast once and other processes on the runner (the previous suite's teardown) freed ~100 MB afterwards. Holding the node-wide figure isn't the product contract: a topping-up ballast would give memory back to a growing workload | Fixed: assert the helper cgroup's `memory.current` holds the delta measured just before apply (and stays under `memory.max`); design doc and book state the one-shot sizing. Lima VM with a 400 MB allocate/free loop running: old test 8 of 15 failed with the CI message, new test 15/15 (and 6/6 without the loop) |
| `relish_test_catalogue::runc_catalogue_deploys_the_exact_image_pushed_to_pickle` (and every other public-image pull in `make test-linux`) | 24 Sep | Product: one stalled registry read spent the whole 30 s budget with no retry. Harness: timed tests pulled from public registries | Fixed: per-attempt deadline inside a larger total; pinned images served from a local digest-verified mirror |
| V02 soak: heavy check "registry: soak/pulse:baseline manifest returned 503 or changed" (twice in 12 h, at ~5.4 h and ~10.8 h) | 26 Sep (V02 soak) | Harness: both fired inside the 300 s quorum-loss window (nodes 2 and 3 powered off). Every pushed tag returned 503 from node 1 and every blob verified, because Pickle resolves tags only through the committed council catalogue and fails closed without a leader; the check counted that as data loss | Fixed: 503 is retried three times 5 s apart, a lasting 503 is recorded as unavailable (info inside a fault window, fail outside one); a changed digest or any other status still fails at once (`test_sustained_check.Registry`, 3 fail before) |
| V02 soak pulse (candidate eced11c): `ingress_removes_route_after_stop` timed out at 300 s | 26 Sep (V02 soak) | Product regression from #211: `relish stop` keeps the spec, and the leader built every node's cluster ingress routes from all apps in desired state, stopped ones included. The route stayed with no backends, so its host answered 503, never the 404 the case waits for | Fixed: `cluster_ingress` skips `stopped_apps` (`a_stopped_app_has_no_cluster_ingress_route`, failed before) |
| V02 soak pulse (candidate eced11c): six passing cases reported "the lease owner did not confirm cleanup within 30 s", and one "could not inspect cleanup … 503 agent status timed out" | 26 Sep (V02 soak) | Harness: the busybox fixtures ran `sleep`/`httpd` as PID 1, which ignores SIGTERM, so every lease retirement sat out Bun's 10 s stop grace on the agent's command loop. The reconciler's 10 s retire budget expired on each ("exceeded ten seconds; ownership retained" for every test app in the journal), confirmation slipped a tick, queued retirements cascaded, and `/v1/status` (5 s) timed out behind them. The 30 s cleanup budget and the "unknown cleanup fails the run" rule are both right | Fixed: fixtures trap TERM (`container_fixtures_trap_sigterm_as_pid_one`, failed before; `sigterm_wrapper_exits_promptly_while_its_child_runs`). Still open (product): any SIGTERM-deaf workload holds its node's agent loop for the stop grace, so status and reports stall meanwhile |
| V02 soak (CI run 36225083621): `redis-cli` got "Could not connect to Redis at soak-redis:6379: Operation not permitted" after a leader kill (2 s, client and Redis both on node 3) and while the Redis node's Bun was SIGKILLed mid-deploy; the Redis container (same PID throughout) never stopped | 26 Sep (V02 soak) | Product: the leader rebuilt the endpoint catalogue only from reports received in its current term. A new term starts with none, and a restarted Bun takes seconds to report, so the rebuild dropped live nodes' backends, committed that, and every consumer's connect hook refused the VIP (`EPERM`: no backends) until the reports arrived. The earlier episode during a power-off was correct: Redis's only node was down | Fixed: a member gossip still counts (Alive or Suspect, same address) that hasn't reported keeps its committed backends; its own report, death, departure, a new address or a producer retirement still removes them (`build_endpoint_catalog_keeps_committed_backends_of_live_unreported_nodes`, fails before) |
| `self_upgrade_cluster::cluster_rollback_returns_every_node_to_previous_version` (400 "address for node \"n1\" is \"127.0.0.1:36685\" but the cluster sees \"127.0.0.1:45611\"") | 26 Sep (#228, run 36244599027) | Product: the leader upgrades last, so it had just restarted. It learnt n1 from a peer's membership sync before n1's own gossip advertised its API address, and meanwhile the membership table guessed it from the leader's own gossip-to-API port offset (57888 + 41165 − 53442 = 45611). Plan validation compared the client's correct address with that guess, and `/v1/cluster/nodes` published it. Every node shares one host with independently picked ports in the test; a uniform-port fleet guesses right | Fixed: membership records whether the address was advertised; plan validation refuses an unadvertised node with a retryable 503 (`unadvertised_address_is_refused_as_not_yet_known`), `/v1/cluster/nodes` omits guesses (`nodes_endpoint_advertises_only_resolved_peer_api_addresses`), and the harness waits until the leader has every node's advertised address before planning. The "registry leader refused proposal: 403" lines in the same log are expected: the harness runs plaintext transports, and registry proposals require a TLS node certificate |
| V02 soak: `/v1/status` 503 "agent status timed out", "snapshot collection failed or timed out" every 5 s, and "retirement of … exceeded ten seconds; ownership retained" while a node retired apps | 26 Sep (V02 soak) | Product: `Stop`, `Retire` and `RetireTestResources` waited out the 10 s stop grace inside the agent's command loop, one instance after another, so a workload whose PID 1 ignores SIGTERM (busybox `sleep`/`httpd`, many shells) stalled every other command; PR #232 only made the test fixtures trap SIGTERM | Fixed: the loop withdraws routing and marks the instances Stopping, a `JoinSet` task drains, signals and escalates every replica at once, and the loop records the exit and releases ownership when it reports back; a second stop joins the pending one, a deploy of a stopping app is refused, shutdown reports pending stops unconfirmed (`status_answers_promptly_while_a_sigterm_ignoring_stop_waits`, `retirement_releases_ownership_only_after_the_process_exits`, `concurrent_sigterm_ignoring_stops_overlap` and two more, all fail before) |
| V02 soak: `/v1/status` 503 "agent status timed out", "snapshot collection failed or timed out" every 5 s, and "retirement of … exceeded ten seconds; ownership retained" while a node retired apps | 26 Sep (V02 soak) | Product: `Stop`, `Retire` and `RetireTestResources` waited out the 10 s stop grace inside the agent's command loop, one instance after another, so a workload whose PID 1 ignores SIGTERM (busybox `sleep`/`httpd`, many shells) stalled every other command; the egress fence did the same. The node reconciler then retired apps one at a time with a 10 s deadline equal to the grace, so each stubborn retirement timed out on its first try. PR #232 only made the test fixtures trap SIGTERM | Fixed: the loop withdraws routing and marks the instances Stopping, a `JoinSet` task drains, signals and escalates every replica at once, and the loop records the exit and releases ownership when it reports back; the egress fence uses the same off-loop stop and fences at once if it fails; a second stop joins the pending one, a deploy of a stopping app is refused, shutdown reports pending stops unconfirmed. The reconciler's retirement deadline is its I/O deadline plus `stop_completion_bound`, and a cycle retires up to four apps at once (`status_answers_promptly_while_a_sigterm_ignoring_stop_waits`, `retirement_releases_ownership_only_after_the_process_exits`, `concurrent_sigterm_ignoring_stops_overlap`, `a_cycles_retirements_wait_out_their_stops_side_by_side` and more, all fail before) |

## Current release checklist

- [x] Commit the release scope and acceptance plan.
- [x] Validate setup liveness and bounded subsystem readiness.
- [x] Stream registry uploads and bound writers.
- [x] Measure registry memory under concurrent pushes.
- [x] Package self-contained Linux agents and native laptop CLIs.
- [ ] Publish verifiable release metadata and signed artefacts.
- [x] Implement secure, resumable managed laptop clusters and installer.
- [x] Fix registry/runtime cache compatibility, restart checkpoints and responsive health probing.
- [x] Route ingress across nodes and report cluster-wide CLI status.
- [x] Propagate storage/bootstrap errors, honour experimental Apple runtime settings, and audit HTTP methods.
- [x] Report unavailable GitOps queues/export failures and show real dashboard replica counts.
- [x] Connect the host browser through pinned TLS and show remote app instances.
- [x] Fix promoted first-run defects and record residual dispositions in the release plan.
- [x] Record an empty-cache development-binary laptop run.
- [ ] Qualify the downloaded candidate on clean hosts and record three-node timing.
- [ ] Pass the real-cluster and final release acceptance gates.

> **Historical reviews:** the [July wiring review](plans/2026-07-02-review-codebase.md)
> found components that existed only as libraries. Later hardening wired many of
> them into the binaries. The dated findings remain useful history; superseded
> inline warnings are not current blockers. Current residuals are tracked separately.

> **Three reviews, three ID spaces.** Finding IDs are scoped to the review that raised them, and
> all three reviews reuse `C1`/`M1`/`O1`. When you see a bare ID, check which section you're in:
> Phases 1–12 use the [2026-07-02 wiring review](plans/2026-07-02-review-codebase.md);
> Phase 15a uses the [2026-07-17 posture review](plans/2026-07-17-review-codebase-current-state.md);
> Phase 15b uses the [2026-07-19 code-logic review](plans/2026-07-19-codebase-review-fable.md).
> Where a later review contradicts an older caveat below, the later one wins — the Phase 15b
> section records which earlier caveats it supersedes.

---

## Phase 1: Foundation

The phase milestones below describe implemented components. Superseded July
inline failure warnings have been removed; their findings and fixes remain in
the later hardening sections and the dated review documents.

- [x] Cargo workspace setup (binary `bun`, library `reliaburger`, test fixtures)
- [x] TOML config parsing (App, Job, Secret, ConfigFile, Volume, Permission, Namespace)
- [x] Grill container runtime interface (runc/Apple/process backends, driven directly — no containerd; OCI extraction, ports, cgroups)
- [x] Bun agent core (process supervisor, health checks, restart logic, GPU detection)
- [x] Relish CLI skeleton (`apply`, `status`, `logs`, `exec`, `inspect`)
- [x] ProcessGrill (cross-platform process-based runtime)
- [x] RuncGrill (Linux-only, calls runc CLI)
- [x] AppleContainerGrill (macOS-only, calls Apple container CLI)
- [x] HTTP health probing (reqwest-based probe with timeout)
- [x] Bun agent event loop (tokio::select, command channels, lifecycle driver)
- [x] Bun local HTTP API (axum on localhost:9117)
- [x] Relish HTTP client (live agent calls with dry-run fallback)
- [x] Integration tests (lifecycle, health checks, restart, CLI)
- [x] `command` field on AppSpec (run custom processes via ProcessGrill)
- [x] TestApp standalone binary (`cargo run --bin testapp`)
- [x] Job execution (deploy, run-to-completion, retry with backoff, failure)
- [x] Init container execution (sequential run, failure prevents main start)
  - [x] Run the same owned init chain for rolling replacements. The actual OCI retry matrix first exposed a replacement reaching main without its initialisers. The shared path keeps failure cleanup and per-init policy refresh. It also preserves the previous serving instance after a failed init.
- [x] Restart re-drive (health check and job restarts re-start instances)
- [x] Exit code tracking on Grill trait (ProcessGrill, MockGrill)
- [x] Example configs (minimal-app, restarts, job-success, job-failure, init-container, volumes, multi-app, full-featured)
- [x] OCI image pulling from Docker Hub (oci-distribution, content-addressed cache, layer unpacking with whiteouts)
- [x] Rootless runc (user namespaces, UID/GID mapping, no-sudo read-only containers) — resource-requiring workloads fail admission because delegated cgroup enforcement is not implemented; the spec omits an undelegated cgroup path rather than failing every start. `slirp4netns` now owns the network namespace and published ports, with persisted ownership and recreation across Bun replacement (M5).
- [x] Streaming apply progress via SSE (real-time deploy feedback instead of blocking response)
- [x] HostPath-style volumes (dual-mode: explicit source for hostPath, managed for auto-provisioned storage) — ~~`M21` managed mode half-wired~~ **fixed in Phase 12 E0**: the agent creates managed volume dirs (and loop mounts) before the OCI spec's bind mounts reference them; `[storage] volumes` config wired; volumes never deleted on Stop (reconciler rebalances would destroy data)
- [x] Relish init command (scaffold reliaburger.toml and app.toml from defaults)
- [x] Log tailing (`--tail N`) and streaming (`--follow`/`-f`)
- [x] Relish exec command (run commands in running instances)
- [x] All Phase 1 tests green

## Phase 2: Cluster Formation

- [x] Shared types: `NodeId`, `AppId`, `Resources`, `NodeCapacity`, `SchedulingDecision` (`src/meat/types.rs`)
- [x] Mustard state machine: NodeState enum, incarnation conflicts, membership table, piggyback dissemination
- [x] Mustard transport and protocol: `MustardTransport` trait, SWIM probe cycle, gossip convergence tests
- [x] Mustard anti-entropy (C6.5): periodic bounded push-pull (`Sync`, 10 s, ≤ 8 datagrams × 8 entries per side) plus a join sync; protocol generation 23; 0 of 100,000 five-node schedules strand (was 73)
- [x] Indirect probe (PING-REQ) ACK routing, proptest for conflict resolution, broadcast count lambda=3
- [x] Dead node reap timer (cleanup_timeout=60s), graceful leave protocol (Left state broadcast on shutdown)
- [x] Raft integration (openraft): storage, network, and state machine adapters; leader election and log replication
- [x] Council selection: stability/zone diversity scoring, deterministic tiebreak, size bounds 3–7
- [x] Reporting tree: `StateReport` to council member every 5s, consistent hash assignment, `watch` channel
- [x] State reconstruction: learning period after leader election, 95% threshold or 15s timeout, diff/correction
- [x] Meat scheduler: Filter → Score → Select → Commit pipeline, bin-packing, labels, daemon mode, quotas
- [x] Scheduler image locality scoring — prefers nodes with cached images
- [x] Scheduler stability scoring — prefers nodes with longer uptime
- [x] Agent integration: wire cluster subsystems into `BunAgent`, extend config, cluster API endpoints
- [x] CLI extensions: `relish nodes`, `relish council` (stub responses, full pipeline)
- [x] CLI extensions: `relish join`
- [x] Chaos tests: council partition, worker isolation (full council loss deferred to Phase 4/8) — Stage 4 W11 replaced the no-op "worker isolation" test with `chaos_isolated_member_misses_writes_until_healed` (now `council::node::tests::isolated_member_misses_writes_until_healed`) (a real router partition), and added `partition_isolates_a_node_for_real` driving the actual runtime's transport blocklists through the HTTP API
- [x] Book chapter + docs: `02-finding-friends.md`, update README and progress

### Cluster runtime wiring (the subsystems above were library-only until here)

Phases 2–11 built the cluster subsystems but the `bun` binary always ran single-node (`BunAgent::new`, never `with_cluster`); they were exercised only by in-memory harnesses. Now wired into the binary behind `bun --cluster`:

- [x] Gossip: `cluster::runtime` binds a real `UdpMustardTransport`, joins by address (no phantom members), membership converges (`tests/cluster_gossip.rs`)
- [x] Raft council: real `serve_raft_rpc` over TCP, bootstrap, a leader-only selection loop grows the council from gossip membership (per-peer Raft address derived from each node's gossip port + offset)
- [x] Reporting tree (flat-star MVP): every node reports state to the leader; the leader's aggregator collects the cluster view
- [x] `relish dev create` forms a real, verified cluster: launches `bun --cluster` under sudo (root for runc/netns and the log file), advertises each VM's inter-VM IP over Lima's `user-v2` network, and builds the binaries from the current tree in the build VM under `sudo -E` (no GitHub download; `--bun`/`--relish` skip the build). Verified end-to-end on a cold 3-node Lima cluster: the council elects a leader and `relish nodes`/`council` show all three members. The Phase 2.1 dev-cluster commands were single-node until here.
- [x] Perimeter firewall fixes (both blocked any multi-node cluster, not just dev): accept loopback before the port drops — local `relish` talks to `127.0.0.1:9117` and its SYN was being dropped, which looked like an API hang; and flush the table before re-applying — `nft -f` appends, so reconciling on every membership change stacked stale boot-time rules ahead of the fresh allow-members rule
- [x] `relish nodes` shows real council membership + leader, derived from the Raft metrics (the gossip-level `is_council`/`is_leader` flags are never set by the runtime), falling back to gossip when Raft isn't wired
- Canonical consistent-hash reporting tree (workers → council → leader, multi-level aggregation) — follow-up
- [x] Durable Raft log and `wrapping_ikm` loading (Phase 12b consensus/PKI wiring; encrypted persistence in Phase 16).

## Phase 2.1: Dev Cluster

- [x] Lima wrapper: VM lifecycle (create, start, stop, delete), platform detection, YAML generation
- [x] Node configuration: generate node.toml per VM with join addresses and cluster ports
- [x] CLI: `relish dev create`, `status`, `shell`, `stop`, `start`, `destroy`
- [x] GitHub release pipeline: cross-compile bun/relish for linux-aarch64 and linux-x86_64
- [x] Docs: whitepaper dev cluster section, README, book getting-started guide

## Phase 3: Networking

- [x] Per-container network namespaces (veth pairs, port mapping)
  - Switch port mapping from individual nftables rules to nftables maps for O(1) lookup at scale — Phase 12
- [x] Onion eBPF service discovery (DNS interception, connect() rewrite, service map)
  - [x] Userspace ServiceMap, VirtualIP allocation, `relish resolve` command
  - [x] Agent lifecycle wiring (deploy/health/stop → service map)
  - [x] eBPF C programs and Rust loader scaffolding (Linux only) — **`[lib-only]` `L8` `onion_ebpf` is permanently `None`; `ebpf` feature not in default build; `.bpf.o` never installed**
  - [x] Wire aya loader for connect rewrite (cgroup/connect4) — **`[lib-only]` `L8` only loaded inside the feature-gated test binary**
  - [x] Userspace DNS responder for `.internal` queries (replaces infeasible in-kernel DNS synthesis) — ~~`[lib-only]` `L9` `run_dns_responder` never spawned~~ (now spawned from `src/bin/bun.rs` when `[dns]` is enabled, and containers get their `resolv.conf` nameserver pointed at it)
  - [x] `relish dev test` runs Linux + eBPF tests from macOS via Lima
  - [x] eBPF integration tests (load/attach, map read/write, connect rewrite, DNS responder)
- [x] Wrapper ingress proxy (host/path routing, load balancing, draining, rate limiting)
  - [x] Routing table (host/path → backend pool, longest prefix match, round-robin LB) — routing table itself is wired (answers `relish routes`)
  - [x] HTTP reverse proxy on dedicated tokio runtime (DDoS isolation, connection limit) — **`[lib-only]` `L7`; the "dedicated tokio runtime" does not exist**
  - [x] Per-client-IP token bucket rate limiting (429 + Retry-After) — **`[lib-only]` `L7` never instantiated**
  - [x] Connection draining protocol (zero-downtime deploys) — **`[lib-only]` `L7`**
  - [x] Agent wiring (routing table rebuilds on deploy/stop/health, `relish routes` command) — **`H2` redeploy leaves 0 backends; routing not rebuilt on health transitions (goes stale)**
  - [x] TLS termination with self-signed certs (rcgen + rustls, Phase 4 adds ACME + Sesame) — **`[lib-only]` `L7` no HTTPS listener**
  - WebSocket upgrade proxying — Phase 9
- [x] nftables perimeter firewall (cluster boundary rules, management access)
  - [x] Ruleset generation (targeted blocking of Reliaburger ports, policy accept)
  - [x] `apply_ruleset()` via `nft -f` (Linux), no-op on macOS
  - [x] Wire into agent (reconcile on gossip membership changes, auto-disabled in rootless mode) — **`C4` CRITICAL: reconcile flushes the shared nft table, wiping container DNAT; `M18` triggers on node count not membership, never applies in standalone mode, drops TCP only (gossip is UDP)**
- [x] All Phase 3 tests green

## Phase 4: Security

> **Phase 4/10 security caveat (`C5`):** the crypto primitives are implemented and sound, but
> almost nothing is **enforced** at runtime — see `L16`–`L18`, `C5`. The `bun`/`relish` binaries
> run with no API auth, no mTLS, no gossip auth, no at-rest encryption, no secret decryption, and
> no signature/CRL verification.

- [x] Sesame CA hierarchy (Root, Node, Workload, Ingress CAs — ECDSA P-256, HKDF key wrapping) — primitives real; `wrapping_ikm` hardcoded `None` at runtime so CA-key ops dead-end (`C5`)
- [x] Node mTLS (join tokens, certificate issuance, mTLS config builders, gossip HMAC)
- Workload identity — deferred to Phase 10
- [x] API authentication (tokens, Argon2id hashing, roles: Admin/Deployer/ReadOnly, axum middleware)
- [x] Secret encryption (age keypairs, `ENC[AGE:...]` decryption at container startup, namespace-scoped keys)
- [x] eBPF firewall rules (`allow_from` resolution, cgroup-to-namespace mapping, BPF map wiring)
- [x] Raft log encryption at rest (AES-256-GCM, HKDF from node cert private key)
- [x] `relish init` generates full PKI + join token; `relish token create`
- `relish token list/revoke` — moved to Phase 10 (requires SecurityState in Raft)
- Join token validation in agent — moved to Phase 10 (requires SecurityState in Raft)
- [x] `relish secret pubkey` and `relish secret encrypt` CLI commands
- `relish secret rotate` — moved to Phase 10 (requires SecurityState in Raft)
- [x] Book chapter 4: "Trust No One"
- [x] All Phase 4 tests green

## Phase 5: Storage & Registry

- [x] Pickle types and Raft state extensions (Digest, ImageManifest, ManifestCatalog, Raft commands)
- [x] Pickle blob store (content-addressed, upload sessions, digest verification, atomic rename)
- [x] OCI Distribution API (push/pull: blob upload POST/PATCH/PUT, manifest PUT/GET, tag list)
- [x] Synchronous replication on push (peer selection, layer transfer via OCI API, mTLS)
- [x] Peer pull (fetch missing layers from peers via Raft layer_locations)
- [x] Garbage collection (sole-copy protection, active reference safety, retention window, GcReport)
- [x] Volume size enforcement (loop mount on Linux, soft warning on macOS)
- [x] `relish images` CLI command, `[images]` config section
- Pull-through cache (Phase 12), P2P downloads (Phase 12), image signing (Phase 10), volume snapshots (Phase 12)
- [x] Book chapter 5: "Where the Images Live"
- [x] All Phase 5 tests green

## Phase 6: Observability

- [x] Mayo TSDB (Arrow RecordBatches + DataFusion SQL + Parquet persistence via object_store)
- [x] System metrics collector (CPU, memory, disk, network via sysinfo)
- [x] Prometheus scraping (prometheus-parse crate, auto /metrics endpoint)
- [x] Metrics API (`/v1/metrics`, `/v1/metrics/summary`, `/v1/metrics/keys`)
- [x] Alert evaluation (5 default rules, Inactive→Pending→Firing state machine)
- [x] Ketchup log collection (append-only files, sparse timestamp index, JSON detection)
- [x] Ketchup queries (grep, tail, time range, JSON field filter)
- [x] Brioche dashboard (server-rendered HTML, dark theme, auto-refresh)
- [x] `relish top` command, `relish logs --grep/--since/--json-field`
- [x] Config: `[metrics]` and `[logs]` sections with object_store_url
- Hierarchical aggregation, full Brioche UI, alert webhooks, PromQL — deferred to Phase 11
- [x] Cross-node log queries (fan-out to nodes, merge by timestamp, dedup)
- [x] Agent wiring (Mayo collection task, Ketchup store, AlertEvaluator, `/v1/alerts`)
- [x] `make observability-demo` for local testing
- [x] Book chapter 6: "Watching Everything"
- [x] LogStore: SQL over logs via Arrow/DataFusion/Parquet (same engine as metrics)
- [x] `/v1/logs/sql` endpoint for SQL log queries
- [x] All Phase 6 tests green

## Phase 7: GitOps & Deployments

- [x] Deploy state machine (9 phases: Pending → Rolling → Completed/RolledBack/Halted/Failed)
- [x] Rolling deploy orchestrator (DeployDriver trait, per-step health-gated replacement)
- [x] Automatic rollback (revert upgraded instances on health failure)
- [x] Dependency ordering (`run_before` jobs complete before rolling)
- [x] Deploy state models and history — the library-side `DeployState` Raft commands remain separate from the binary's desired-state reconciler. The real Bun path records every accepted worker with a stable operation ID, live phase/target and bounded 50-entry outcome history (M6); per-app rollback history records fresh and rolling deploys locally.
- [x] CLI: `relish deploy`, `relish history`, `relish rollback`, `relish lint`
- [x] API: `/v1/deploys/active`, `/v1/deploys/operations`, `/v1/deploys/history/{app}` — active operations now come from the running worker path; operation history and per-app rollback history are explicit separate contracts.
- [x] `make deploy-demo` for local testing
- [x] Book chapter 7: "Ship It"
- Autoscaling, Lettuce GitOps, blue-green, K8s migration — see Phase 9
- [x] All Phase 7 tests green

## Phase 8: Advanced

- [x] Smoker fault injection (safety rails, registry, process/network fault plumbing, scripted scenarios and chaos tests)
- [x] Network security (egress allowlists, eBPF enforcement in connect hook, namespace isolation)
- [x] Process workloads (exec/script apps and jobs, binary allowlist, ProcessManager, OCI spec wiring, validation)
- [x] High-throughput batch scheduling (greedy bin-packing `schedule_batch`, `BatchTracker`, 100K jobs in <1s)
- [x] Build jobs (BuildSpec config, pickle:// destination parsing, namespace-scoped push, buildah command construction)
- Live agent wiring for batch dispatch and build execution — deferred to Phase 12 (the `/v1/batch` and `/v1/build` endpoints return 501 until then)
- [x] All Phase 8 tests green

## Phase 9: User Experience

- [x] Blue-green deploy strategy (parallel start, atomic routing swap, orchestrator dispatch)
- [x] Autoscaling (evaluation logic, hysteresis, cooldown, Mayo query_avg, async task runner, Raft persistence)
- [x] Lettuce GitOps engine (sync loop, git ops, signature verification, diff engine, webhook endpoint, coordinator election, Raft integration, node config)
- [x] Kubernetes migration (`relish import`, `relish export` via k8s-openapi, resource correlation, migration reports, optional `kubernetes` feature)
- [x] `relish compile`, `relish diff`, `relish fmt`
- [x] WebSocket upgrade proxying in Wrapper ingress (detection, dispatch, close frame, draining) — ~~`[lib-only]` `L7` proxy never runs; handshake stub drops the backend stream~~ (12b.4: the Wrapper proxy runs the 101 upgrade and splices the backend stream; a live splice holds the drain open until it ends — `src/wrapper/proxy.rs`, `draining.rs`, ING2/ING4)
- [x] Book chapter 9: "The Full Package"
- [x] All Phase 9 tests green

## Phase 10: Advanced Security

- [x] Workload identity (SPIFFE certs, CSR, automatic rotation, OIDC JWTs)
- [x] Cluster image signing and verification through workload identity; upstream-image trust policy remains a separate boundary.
- [x] SecurityState in Raft (prerequisite for the wiring items below)
- [x] Wire agent-to-council CSR flow during deploy
- [x] Wire automatic keyless signing after build job push
- [x] `relish sign` CLI command (operator-held P-256 key: `relish sign IMAGE --key PATH` resolves the tag to its digest and signs locally; `relish sign keygen` prints the `trust_policy.keys` line)
- [x] `/v1/identity/jwks` and `/v1/identity/sign` API endpoints
- [x] CRL distribution, egress DNS resolution
- TPM sealing — deferred to v2 (requires hardware)
- [x] `relish token list/revoke` (SecurityState in Raft)
- [x] Join token validation in agent (SecurityState in Raft)
- [x] `relish secret rotate` (dual-key transition window)
- [x] Book chapter 10: "Locking It Down"
- [x] All Phase 10 tests green

## Phase 11: Advanced Observability

- [x] Hierarchical metrics aggregation via council (cluster-wide queries)
- [x] Brioche UI pages, HTMX refresh and uPlot charts; cluster app counts/details are wired. Remaining cross-node view and telemetry limits need separate acceptance.
- [x] Alert webhooks (Slack, PagerDuty, generic HTTP)
- [x] Log export to S3/GCS (scheduled Parquet, `relish logs-search` for remote SQL)
- [x] Cross-node log queries over authenticated HTTP (leader fan-out, merge-sort)
- [x] Book chapter 11: "Eyes Everywhere"
- [x] All Phase 11 tests green

## Phase 11b: Review & Tying the Loose Ends

The July 2026 verification pass ([2026-07-02-review-codebase.md](plans/2026-07-02-review-codebase.md)) found that the build
is clean, clippy is silent, and 1590 tests pass — but a large share of the "done" items above
are **library-only** (unit-tested, never wired into the `bun`/`relish` binaries), plus five
critical bugs and a long tail of correctness issues in the wired paths. This phase closes them.
IDs (`C1`, `H2`, `L7`, …) reference the finding table in the review doc. Same tests-first rule:
each wiring item lands with an integration test that drives the **binary**, not the library.

### Stage 0 — Security & data-loss stop-the-bleed

- [x] `C1` Reject `..` components in OCI whiteout unpacking (host-file deletion via malicious layer) — `safe_join` in `grill/image.rs`
- [x] `C2` Sanitise the pickle `upload_id` path segment (arbitrary file append/exfiltration) — `validate_upload_id` in `pickle/store.rs`, 400 in the handlers
- [x] `C4` Give the perimeter firewall its own nft table — perimeter rules moved to `ip reliaburger_fw`, leaving the container `ip reliaburger` table untouched
- [x] `C5(d)` Decrypt `ENC[AGE:...]` secrets in the wired container-startup path — plumbed via `generate_oci_spec_with_decryptor`; fails closed when no key is available (full decryption lights up with Stage 3 `wrapping_ikm`, L18)
- [x] Bind the Pickle registry to loopback / require auth — `[images] registry_bind` defaults to `127.0.0.1`; token auth deferred to Stage 3

### Stage 1 — Correct the wired single-node path

- [x] `H1` Fix restart re-drive: stop the old container before re-create, drive all runtimes (not just ProcessGrill jobs)
- [x] `H2` Rolling redeploy: track new backends/health/host-port so the service isn't left with 0 backends
- [x] `H3` Move `FollowLogs` off the event loop (spawned) + init-container timeout; deploy health-gate now early-exits (full async deploy is a follow-up)
- [x] `H13` Exit-code / `logs` / `follow_logs` / `exec` on RuncGrill (run-and-capture model) and AppleContainerGrill (`exit_code`) — runc behaviour is Linux-verified via `relish dev`
- [x] `H9` Reload Parquet on startup, stop clobbering flush files, bound in-memory batches (Mayo + Ketchup)
- [x] `H10` Pipe container stdout/stderr into the log store (per-instance forwarders → LogRecord channel → LogStore)
- [x] `M9` Crash detection for non-job apps + bounded HealthWait (`HealthWait → Failed` after a startup grace)
- [x] `M10` Release host ports on `remove_app` and redeploy rollback
- [x] `M11` Probe the instance's `container_ip` (loopback fallback) instead of hardcoded `127.0.0.1`
- [x] `M12` Handle SIGTERM; `shutdown_all` escalates SIGTERM → grace → SIGKILL
- [x] `M13` runc `delete --force` + netns/veth teardown on kill and natural exit (in the `H13` commit)

### Stage 2 — Cluster safety

- [x] `C3` Durable Raft log + vote + state-machine snapshot (redb); bootstrap only when the store is fresh; re-seed gossip from the restored membership on restart. **Closes the last of the five review criticals.** Lima-verified: killing + restarting the bootstrap node no longer forms a second leader — it restores its durable `{1,2,3}` membership and rejoins as a follower (all nodes agree on one leader).
- [x] `H4` SWIM: disseminate Suspect with the target's incarnation (not the prober's)
- [x] `H5` SWIM: refute being declared `Dead`, not just `Suspect`
- [x] `H6` SWIM: time suspicion from `state_changed`, not `last_ack`
- [x] `H7` SWIM: publish the membership watch on content change, not just member count
- [x] `M14` Return the allocated cert serial via `CouncilResponse::SerialAllocated` (fixes the read-back race)
- [x] `M15` Council reconciler can't wedge — non-blocking membership ops with timeouts + logged errors
- [x] `L5` Leader-safe, zone/age-aware council selection — reconciler retains current voters (never demotes the leader) and grows via `select_council_candidates`. **Live cluster-formation behaviour needs Lima verification**; resource-aware filtering awaits the reporting tree carrying real usage.

### Stage 3a — Load the security foundation (wire, don't enforce)

- [x] `[security]` config section (`master_key_path`, `bootstrap_path`) + `sesame::bootstrap` loaders (hex master key + JSON SecurityState, fail-closed on world-readable perms)
- [x] `L18` Load `wrapping_ikm` from the master key and seed the bootstrap `SecurityState` into Raft once (fresh bootstrap only; durable Raft makes it idempotent) — the already-wired CA/OIDC/secret-decryption machinery can now unwrap keys
- [x] `L18` Populate `ApiState.council` + a token store — `/v1/identity/jwks` and `/v1/token/list` light up (were 503); other `None` fields (rollup/membership/gitops) are Stage 4
- [x] `X2` Persist `relish token create` to Raft (server-side mint + `CreateApiToken`, plaintext shown once; unreachable agent is a hard error)
- [x] `X7` `relish secret pubkey` reads the real `*-security-bootstrap.json` and prints the cluster age key
- [x] `relish dev create` distributes the master key (every node) + security bootstrap (node 1) into `/etc/reliaburger` (0600) so a dev cluster boots with a real IKM + seeded SecurityState — the Stage 3a verification vehicle

### Stage 3b (auth) — Enforce API authentication

- [x] `C5(a)` Attach `auth_middleware` via a split router (public: `/`, health, `/ui/*`, JWKS; protected: everything else). Bootstrap window kept but keyed on real user tokens only, and documented
- [x] Derived side-channel service token (HKDF of the master key) so bun's own cross-node fan-out authenticates as a `__system` principal without tripping the bootstrap window
- [x] Live-refresh the token store from Raft (5 s) so a token created via `relish token create` engages enforcement without a restart
- [x] Per-route role authorisation (`authorize`): token/secret/identity-sign → Admin; apply/stop/exec/chaos/fault → Deployer; reads open to any valid token
- [x] Relish sends a Bearer token (`--token` flag > `RELIABURGER_TOKEN`) on every request

### Stage 3b (transport) — Enforce transport security

- [x] `C5(c)` Sign + verify the gossip HMAC — key derived from the master secret (every node has it, nothing new distributed); UDP transport signs on send, drops unverified datagrams on recv
- [x] `L17` (signatures) Enforce image-signature **verification** at deploy time — `verify_image_signature` actually verifies the bytes against the cluster root CA (not just presence); gated on the node trust policy, refuses unsigned/invalid Pickle images. Enforced locally in `deploy` (no central scheduler exists yet — that's Stage 4 L1)
The Stage 5 work (PR #77, merged) closed most of this: node identity is persisted and
delivered to joiners, and **mTLS runs on the Raft RPC, reporting and agent-API listeners**
with a Node-CA-pinned client verifier, a Raft-refreshed CRL, keyless-signature CRL checks
and read-only Brioche session auth. The remaining `C5(b)`/PKI items — Pickle registry
auth/TLS, API scope enforcement + bootstrap lockdown, atomic CSR-based join and
expected-peer binding — live in **Phase 12b** below (Themes "API authorisation and
Brioche", "Node PKI, join and mTLS", "Pickle storage and replication durability").

### Stage 4 — Wire the remaining library-only subsystems (one at a time, binary-driven test each)

Implementation plan: [docs/plans/2026-07-07-plan-wiring.md](plans/2026-07-07-plan-wiring.md)

- [x] `L1` Scheduler → placement → remote dispatch: `relish apply` under `--cluster` commits `AppSpec`s to Raft (followers forward to the leader); a leader-only scheduler places replicas and commits `SchedulingDecision`s; every node polls `/v1/placements/{node}` and reconciles its instances (idempotent). `H8` fixed (spread weight 60 > bin-pack 50; test now asserts distinct nodes). Flushed out a latent bug: durable Raft log + council TCP RPC used bincode, which can't drive the config types' `deserialize_any` — both switched to self-describing JSON (matching the snapshot). Binary-driven test in `tests/placement.rs`
- [x] `L2` / `M16` / `X3` — `M16`: orchestrator no longer leaks a failed step's own half-started instance (regression test asserts it's stopped). `X3`: `relish rollback` actually rolls back — deploy history now carries the full `AppSpec` (every path records it, including the first deploy), `POST /v1/rollback/{app}/{ns}` redeploys the previous successful spec via the apply path (Raft in cluster mode). Note: cluster-wide *staged* rollout (max_unavailable gating across nodes) rides on the W6 desired-state reconciler and the per-node rolling redeploy; the imperative `DeployOrchestrator` stays library-side (correct + unit-tested) rather than duplicated as a parallel cluster driver
- [x] `L3` Autoscale loop wired: leader-only task drives the tested pure functions (`evaluate`/`AutoscaleTracker`/`AutoscaleConfig::from_spec` — the library's sync `app_provider` closure can't read async Raft/rollup state), reads each `[autoscale]` app's metric from the rollup store, and commits `AutoscaleOverride` to Raft. The scheduler now targets *effective* replicas (override ∨ spec), so a scale flows through the same placement→reconcile path as apply. End-to-end test: high metric → override → grows to `max`
- [x] `L4` State reconstruction wired into the leader scheduler loop: on the leadership edge it calls `on_leader_elected`, runs a learning period (feeding reports through `on_report_received`/`check_timeout`), and **gates scheduling** until phase == Active. **Post-Phase-12 audit:** the returned `MissingApp`/`ExtraApp` corrections are discarded, the diff loses colocated replica counts and stale reports can satisfy coverage; correction is Phase 12b.
- [x] `L6` / `L11` Reporting + rollups wired: `RollupWorker` spawned per node, aggregator gets a real rollup store, `/v1/metrics/cluster` serves from it; StateReports carry real capacity (`[resources]` now read) and requested-resource usage. Flat-star kept by design (tree deferred, see ch. 11); fixed a latent DataFusion overflow (`unwrap_or(u64::MAX)` time ranges, 4 handlers)
- [x] `L7` Bind the Wrapper ingress listener — `[ingress]` node-config section (off by default), HTTP + HTTPS listeners (self-signed or disk certs), per-client rate limiting wired into the proxy path, WebSocket pass-through; drain-on-deploy integration lands with `L2` (W7)
- [x] `L8` / `L9` Load the Onion eBPF programs in production; start the DNS responder (fix `M8` fragility) — **`L9`+`M8` done**: `[dns]` config section (off by default), responder spawned from bun, full hardening (recv errors non-fatal, per-query spawned forwards behind a semaphore, connected sockets + transaction-ID checks, NXDOMAIN for unmatched `.internal` with no upstream leak, QTYPE honoured, SERVFAIL on dead upstream), runc containers get `resolv.conf` pointed at the responder. **`L8` done**: `[ebpf]` config section (off by default; `program_dir` defaults to the build-time `OUT_DIR` baked in via `build.rs` `RELIABURGER_BPF_DIR`, so dev/Lima builds self-locate their `.bpf.o`), `bun` loads + attaches `OnionEbpf` at startup (load failure logs and continues without enforcement; non-`ebpf` builds warn that enforcement is off). Verified in the `reliaburger-test` Lima VM: `cargo build --features ebpf` compiles the objects and all 9 `tests/ebpf.rs` integration tests pass (load/attach, backend-map read/write/remove, connect→VIP rewrite, no-backend deny `EPERM`, non-VIP passthrough, `.internal` DNS). Not covered by `make ci` (needs root + kernel 5.7+ + cgroup v2)
  - **Backend/fault/egress eBPF wiring landed** (Phase 11b follow-up, P0–P3): the agent writes the live `backend_map`, fault maps and DNS-refresh egress entries. Namespace firewall maps and rolling-deploy egress (with fail-closed programming) are closed in Phase 12b (NET5/NET6); IPv6/CIDR enforcement remains under the 12b network-policy theme.
- [x] `L10` / `M2` Pickle wired: catalog persists to disk + loads at boot; pushes record real raft-id holders and propose to Raft on council nodes (worker proposal forwarding lands with W6); leader replication loop keeps layers at `[images] redundancy`; scheduled two-phase GC — nominate → Raft-arbitrated approval (`CouncilResponse::GcApproved`) → delete, with an orphan grace window for in-flight pushes. `X1` fixed: `relish build` targets the registry port, `/v1/build` executes buildah for real (honest 501 without it)
- [x] `L13` / `H12` GitOps wired: new `src/lettuce/runner.rs` spawns a leader-only sync loop (clone → poll/webhook → `execute_sync` in `spawn_blocking` → apply changes as `AppSpec`/`AppDelete` Raft writes). Webhook endpoint gets a real channel (was unconditional 503); `[gitops]` config now read. `H12`: `is_key_trusted` no longer falls through to `true` — a valid signature from an unlisted key is rejected. Fixed a latent first-sync bug (a fresh clone has nothing to fetch but nothing applied either → now syncs when HEAD ≠ last-applied). Integration tests in `tests/suite/gitops.rs` (real git repo → Raft; webhook triggers sync)
- [x] `L14` / `L15` Smoker safety context, process/network plumbing and chaos transport blocklists wired; Kill/Pause/Resume, eBPF network faults and partitions have binary-driven tests. **Post-Phase-12 audit:** several advertised resource/node faults are no-ops that return success, CPU stress runs in Bun's cgroup and clear/expiry does not reverse every effect; the measurable-effect/cleanup work is Phase 12b.
- [x] `L16` Initial IPv4 egress allowlist programming and DNS refresh wired and Lima-tested. Phase 12b (NET6) made it fail closed, extended it to rolling deploy and crash-restart, and deletes per-cgroup entries on stop; IPv6/CIDR enforcement remains under the 12b network-policy theme.
- [x] `M17` K8s import fidelity (`command`/`args` concatenated, `env.valueFrom` warned not dropped, namespace preserved, same-name-two-namespaces no longer overwrites)
- [x] `H11` Fix `relish fmt` for nested-table configs — recursive section emission + a round-trip guard that refuses to write output that re-parses differently
- [x] `X1`/`X3`/`X4`/`X5`/`X6` CLI mismatches: `X1` (build → registry port + real buildah execution), `X4` (logs `--grep`/`--since`/`--json-field` wired, server + client side) and `X5` (unreachable agent exits non-zero; explicit `--dry-run` flag added) done; `X3` rollback done (W7); `X6` no-args TUI is out of Stage 4 scope by design → [2026-07-06-plan-tui.md](plans/2026-07-06-plan-tui.md)

### Throughout

- [x] Fix the misleading tests — `L15` "worker isolation" (was a no-op) replaced with `chaos_isolated_member_misses_writes_until_healed` (now `council::node::tests::isolated_member_misses_writes_until_healed`), which really partitions a council member and asserts the isolated node misses writes until healed. `H1` restart tests now assert real post-restart behaviour, not just a counter bump: `health_check_triggers_restart` checks the instance reached a live re-created state (`running`/`health-wait`/`unhealthy`, never stuck in `Preparing`), and `job_failed_retries_then_fails` asserts the terminal `failed` state after retries exhaust
- [x] Remove dead config or wire it — wired during Stage 4: `[resources]` (W4), `[reconstruction]` (W9), `[gitops]` (W10), `[images]` (W5), `[metrics]` (W4), new `[ingress]`/`[dns]`/`[ebpf]` (W2/W3/W12). **Post-Phase-12 correction:** node `labels` parse but never travel in gossip, `[process_workloads]` and `[logs] max_file_size_mb` remain dead, and several newer fields are unused; Phase 12b owns them. `[storage] volumes` (M21) was wired in Phase 12 E0.
- [x] Clear each `[lib-only]` tag from the phases above as its subsystem is genuinely wired — the Smoker fault-injection `[lib-only]` tag (Phase 8) is cleared; the eBPF network-fault enforcement + service-map→backend sync gaps were subsequently closed (P0–P3, see the `L8` item)

### Post-Stage-4 audit fixes (July 2026)

A follow-up audit of the wired Stage-4 code surfaced five issues **not** in the
original review. All fixed on this branch, tests-first (each drives the binary/agent path):

- [x] Redeploy stored `oci_spec: None`, so a replica that crashed after a redeploy could
  never restart (the crash-restart driver filters on `oci_spec.is_some()`). Redeploy now
  records the spec — `redeployed_instance_restarts_after_a_crash`.
- [x] `WorkloadInstance.container_ip` was never populated, leaving the M11 probe fix inert
  and every service-map/eBPF backend registered as loopback. Added `Grill::container_ip`
  (runc netns + Apple inspect) and populate it on deploy/redeploy/restart —
  `deploy_records_the_grills_container_ip_on_instance_and_backend`.
- [x] Ingress proxy forwarded hop-by-hop headers and copied the upstream's
  `Transfer-Encoding`/`Content-Length` onto a re-bodied response (framing mismatch). Now
  filters the RFC 7230 hop-by-hop set both directions — `ingress_reframes_chunked_backend_response`.
- [x] WebSocket pass-through was a stub (dropped the backend stream, omitted
  `Sec-WebSocket-Accept`). Now a real `hyper::upgrade` + `copy_bidirectional` splice —
  `ingress_proxies_websocket_handshake_and_bytes`.
- [x] Single global `Mutex<RateLimiter>` serialised every rate-limited request; replaced with
  a 16-way `ShardedRateLimiter`.

## Phase 12: Optimisations

> Detailed implementation plan: [2026-07-06-plan-optimisations.md](plans/2026-07-06-plan-optimisations.md)
> (revised 2026-07-09 after the Stage 4 wiring merge: slice B — Pickle catalog via Raft,
> replication and two-phase GC — landed as-built in #71; 14 remaining implementation steps,
> refreshed ground truth, config/endpoint/test inventories, Lima acceptance runbook).

- [x] Wire `SubmitBatch` into the agent — `bun::batch`: `POST /v1/batch` (leader-forwarded, full job specs in the request — the CLI used to send names only), capacity from the leader's `AggregatedState` (the deploy scheduler's source; standalone falls back to a local-only entry), `schedule_batch` → leader-side `BatchTracker` → direct HTTP dispatch `POST /v1/batch/run` + completion callbacks `POST /v1/batch/{id}/report` (NOT the placements reconciler — it stops "drifted" workloads, which kills run-to-completion jobs); `GET /v1/batch/{id}`; `relish batch` prints the id, new `relish batch-status`. Watcher distinguishes success from failure-in-backoff via a new `InstanceStatus.exit_code` (any exit maps to `stopped`; the first watcher version called failing jobs completed — caught by the failure-path test). 4 integration tests.
- [x] Wire `SubmitBuild` into the agent — `bun::build_runner`: the Stage 4 sync handler body becomes a spawned runner behind a build registry (`202 {build_id}` + `GET /v1/build/{id}`; the sync form strands the CLI past its 300s timeout on real builds); per-stage `[images] build_timeout_secs` (900); builder capability travels as a `has_buildah` StateReport flag (probed once at worker startup) and incapable nodes delegate to a capable peer via `/v1/build/run` with proxied status reads (`Delegated`); no builder anywhere = honest 503; after push the runner signs the manifest via `AgentCommand::SignImage` (best-effort — standalone has no council); `relish build` polls. Buildah-gated Lima test: trivial context → catalog; macOS tests: 503/404/registry lifecycle.
- [x] Switch port mapping from nftables rules to nftables maps (O(1) lookup at scale) — `grill::portmap` (argv generators, executor trait + recording mock, rollback/incremental `PortMapSet`); `ensure_nft_table` creates the map + single lookup rule with guarded probes (also fixing masquerade-rule duplication), removal is O(1) element delete (handle parsing deleted). **Also wired the mapping in** — `add_port_mapping` had zero production callers (the M21 pattern): the port pair rides `OciSpec.port_mapping`, runc installs it beside the netns, tears down on exit/delete, and adoption rebuilds handles without touching the kernel. Lima: 1000-port stress test. Known limits recorded: prerouting DNAT is host-inbound only; rootless proxies don't survive adoption.
- [x] Managed-volume wiring (E0, fixes review `M21`): the agent creates managed volume host dirs (loop-mounted when sized, Linux root) in `spawn_blocking` before spec generation, failing the deploy closed; `[storage] volumes` config wired via `set_volumes_dir`; **no deletion on Stop** (rebalances/upgrades send Stop; deleting would destroy data — explicit cleanup is future work)
- [x] Heal-loop hardening (B5): `pickle::replication::heal_tick` extracted from the bun binary (testable; `cluster::identity::pickle_peers` shared helper), rarest-first ordering + 10-manifest per-tick cap, leader-pull-first (non-leader pushes now gain redundancy), roadmap auto-heal integration test + 2 more, loopback `registry_bind` startup warning (registry has no auth/TLS — keep firewalled)
- [x] P2P multi-source image downloads — pure `pickle::p2p::plan_downloads` planner (rarest-first, least-loaded balancing, dedup, skip-local; proptested over arbitrary topologies) + bounded-`JoinSet` executor with alternate-holder retry, wired into `ImageStore::pull_and_unpack` via a late-injected `ClusterImageSource` (which also fixes cluster-pushed HTTP-only images being undeployable on other nodes — the external client is HTTPS-only); catalog-known images never fall back to external registries; 100MB/5-layer peer pull verified < 5s
- [x] Pull-through cache full wiring (upstream → Pickle → Raft) — `pickle::upstream` (pure `decide()` on `cache_recheck_secs`, HEAD-compare refresh, `UpstreamRegistry` trait over oci-distribution + counting mock, env-resolved `external_registries` credentials) + `ClusterSource::ensure_external_image` fill path (serialised fills with post-lock recheck; stale-serving when upstream is down; commits under `cache/<host>/<repo>` with holders={self}, heal loop replicates); `cache/` repos exempt from `require_signatures` by construction (pinned by test); peer blob-transfer URLs flatten multi-segment names (single-segment registry routes; blobs are content-addressed)
- [x] Volume snapshots (CoW, scheduled jobs, S3/GCS upload) — E3 adds `bun::snapshot_worker`: `[storage.snapshots] { interval_secs, retain, upload_url }` interval loop (cron-expression parser deliberately rejected), pure `prune_plan`, tar.gz in `spawn_blocking`, upload via `object_store` (`file://`/`s3://`/`gs://` — aws+gcp features enabled); the per-snapshot `uploaded` flag is checkpoint, retry policy, and audit column at once; sweeps report per-app failures without aborting. E2: `grill::snapshot` (Btrfs-only, read-only `-r` snapshots under `.snapshots/`, meta.json sidecars, injected-clock naming); restore = delete live + writable snapshot back, refused by the agent while the app has non-terminal instances (409); no-`--volume` snapshots every provisioned volume (sidecar discovery — works for stopped apps); 4 `/v1/snapshots` routes + `relish snapshot create|list|restore|delete`; roadmap create/corrupt/restore test on the Lima loopback-btrfs rig. Scheduled jobs + object-store upload land in E3.
- [x] Btrfs subvolume quotas (alternative to loop mount) — `grill::btrfs` (statfs detection, pure argv generators + decision table); volumes on Btrfs become subvolumes (qgroup limit when sized — subvolumes even unsized, so E2 can snapshot them); backend recorded in a `*.volume.json` sidecar (delete/snapshot dispatch) which also made creation idempotent (restarts were stacking loop mounts); Lima-gated quota test provisions its own loopback btrfs; `btrfs-progs` added to VM provisioning, `RELIABURGER_BTRFS_TESTS`/`RELIABURGER_BUILDAH_TESTS` gates added to `relish dev test`
- [x] Parquet bloom filters for archive equality pruning — on `app`/`namespace` (1% FPP), not `line` (bloom filters answer equality, not substring LIKE; `bloom_filter_on_read` enabled in remote_query)
- [x] Zstd compression for archived logs — via Parquet's native per-row-group ZSTD codec (random access preserved; >5x vs flat text), not a separate seekable-frame container
- [x] Book chapter 12: "Squeezing Every Drop" — complete: logs (zstd + bloom), nftables maps + the wiring discovery, the as-shipped Raft catalog/GC/heal design (M2 TOCTOU), P2P planner + executor, pull-through cache, volumes/quotas/snapshots/scheduled backups, batch + build, and phase-wide lessons
- [x] All Phase 12 tests green — the portable suite (`make ci`) plus the full Lima gated run (`relish dev test`: netns map DNAT + 1000-port stress, btrfs quota ENOSPC, snapshot create/corrupt/restore, real buildah build into the catalog, plus the existing runc/eBPF suites). Remaining acceptance: the live 3-node runbook in [the plan §10](plans/2026-07-06-plan-optimisations.md)

## Phase 12b: Correctness, Security & Convergence

> Consolidated review: [2026-07-10-review-past-phase-12.md](plans/2026-07-10-review-past-phase-12.md).
> It reconciles the renamed [code walkthrough](plans/2026-07-09-review-codex.md),
> [design discrepancy register](plans/2026-07-09-review-design-discrepancies.md),
> the former "Beyond Phase 11b" backlog and the 24 Low findings. M7 and M21/codex-M2
> are fixed. M20 is still open: object-store features are compiled, but Ketchup export
> still uses PathBuf plus std::fs::copy. X6 remains only in Phase 13.

Every top-level checkbox below is one PR-sized theme. Write the binary-driven
acceptance test first, update the relevant book chapter in the same PR, and check
the theme only after default and platform-gated tests pass. Done findings within a
theme are ticked as nested `- [x]` items; a theme's own box is checked only when the
whole theme lands.

### 12b.1 — Stop the bleeding

- [x] **Internal API trust boundary** — central route/role/scope matrix; require node
  identity for batch/build run and report endpoints; make callback and registry
  destinations server-owned; never forward the service token to request data; strictly
  parse build digests/Dockerfile paths and bound, sandbox and clean archive extraction.
  Reject anonymous and ReadOnly execution, callback SSRF, path traversal, sparse/oversized
  contexts and a Buildah process that survives timeout (new JOB1-JOB2, H4/D8).
  - [x] `require_system` on `/v1/batch/run|report` and `/v1/build/run`, `authorize(Deployer)`
    on the submit endpoints, callback bounded to known members, and the cluster service token
    no longer forwarded to caller-controlled URLs (JOB1); build `context_digest` validated as a
    well-formed OCI digest before it becomes a temp path, killing the `sha256:../../x` traversal
    (JOB2).
  - [x] Server-owned registry destination (JOB2 residual): `registry_port` removed from
    `BuildSubmitRequest`, `#[serde(deny_unknown_fields)]` makes a smuggled port a 400, and the
    build runner reads `[images] registry_port` from its own config — a caller can no longer
    point a privileged Bun at an arbitrary localhost service.
  - [x] Bounded, sandboxed, self-cleaning context extraction (JOB6): `pickle::build::unpack_context`
    streams the download to disk under `[images] max_context_bytes` (default 256 MiB), rejects
    absolute/`..`/symlink/hardlink/device/FIFO entries, counts *written* bytes to defeat sparse
    bombs, caps the entry count and strips setuid/setgid bits; per-build `ScopedDir` (RAII `Drop`)
    with a random suffix gives concurrent same-digest builds distinct dirs and cleans up on every
    exit path.
  - [x] Dockerfile path confinement (JOB6): `validate_dockerfile_path` rejects absolute/`..`
    paths in `validate_build`, and `confine_dockerfile` canonicalises the resolved path inside
    the extracted context (catches symlink-directory escapes) before Buildah reads it.
  - [x] Kill Buildah on timeout (JOB6): each stage runs via `run_bounded` in its own Unix process
    group with `kill_on_drop`; on timeout the whole group gets SIGKILL, so Buildah's children die
    with it instead of orphaning — proven by a process-group shim test.
  - [x] Central route→principal matrix (H4/D8 groundwork): `bun::authz::ROUTE_MATRIX` maps every
    mounted route to `Public | AnyToken | Deployer | Admin | System`; a source-scan test asserts
    every `.route(…)` the router mounts has a matrix entry, so the auditable role list can't drift
    from the code. (Scope enforcement stays in 12b.3.)
- [x] **Secret and workload-identity safety** — make rotation generation-aware:
  encrypt with the newest key, decrypt with active generations, re-encrypt and acknowledge
  every stored secret before retiring the old key, and reject malformed/concurrent
  rotations. Issue exact validity windows, rebuild SANs server-side, store identity in
  per-instance tmpfs, preserve it through rolling/adoption and clean it on removal
  (new PKI6-PKI8, D9).
  - [x] Generation-aware secret rotation: encryption picks the newest non-read-only key,
    decryption tries every live generation, finalize refuses to empty a scope, and a malformed
    rotate body is rejected — so a secret sealed under generation N survives the rotation window
    (PKI8).
  - [x] Exact workload-certificate validity (PKI6): `validate_and_sign_csr` takes an injected
    clock and issues `now − 5 min` (skew backdate) to `now + 1 h` as timestamps via
    `time::OffsetDateTime`; the calendar-date helpers that gave a one-hour cert equal midnight
    bounds are deleted, and `cert::check_validity_at` pins the window in tests.
  - [x] Server-side SAN rebuild (PKI6): the signer takes only the public key from the CSR and
    rebuilds DN/usages/serial/SANs from the expected identity, so a CSR smuggling another
    workload's SPIFFE URI or a DNS SAN yields a certificate with exactly the expected URI
    (`smuggled_csr_sans_are_not_signed` parses the issued DER).
  - [x] Per-instance identity directories (PKI7): `{volumes}/.identity/{instance_id}` replaces
    the app-scoped dir (replicas no longer overwrite each other's keys — OCI mount source is
    per instance); prepared at the same pre-create seam as egress programming, tmpfs-backed
    (`mode=0700`, size-bounded) on Linux root with key/token chowned to the workload UID,
    plain `0700` dir elsewhere (documented gap); removed (and unmounted) on stop, rolling
    replacement and rollback; fresh deploys now provision identity at all (previously only
    the rolling path did).
  - [x] Restart-safe rotation (D9): a `meta.json` sidecar (SPIFFE URI + schedule, no secrets)
    lets adoption rebuild each instance's identity and rotation timetable from disk instead of
    `identity: None`; orphaned identity dirs are swept at adoption; a rate-limited
    retry provisions running instances that still lack an identity.
  - [x] Verify-before-retire (PKI8): applying an `AppSpec` records the sealing generation per
    encrypted env value (`SecurityState.secret_seals`, self-describing JSON with
    `#[serde(default)]`); finalize refuses (new `CouncilResponse::Refused`, surfaced as HTTP
    409) while any secret in scope is sealed under an older — or unknown/legacy — generation,
    naming the offenders; re-applying the re-encrypted spec unblocks it (legacy fixture test).
  - [x] One rotation at a time (PKI8): a second `RotateSecretKey` for a scope with an
    un-finalised rotation is refused with instructions; idempotent retries of the same
    rotation are deduped on the generation number.
- [x] **Pickle reference integrity** — include raw manifest/index blobs in holder,
  replication and GC reachability; validate JSON, media types, descriptor digest/size and
  referenced blob existence before returning Created; use canonical repository identity
  and immutable digests through policy, scheduling and runtime pull. Test
  push → GC → peer pull and reject the existing missing-layer Created behaviour
  (new REG1/REG3/IMG1, old Low manifest check).
  - [x] `ImageManifest::referenced_digests()` is the one authoritative "everything this
    tag pins" set (manifest blob + config + layers); holder commits, GC protection, the
    heal loop, replication, peer/P2P pulls and `image_available_locally` all use it, so a
    tagged manifest's own blob is never orphaned and heals to `[images] redundancy` like
    any layer; the pull-through fill stores the raw upstream manifest bytes too (REG1).
    Old persisted catalogues (no manifest-blob holders) load unchanged, GC keeps the blob
    and one heal tick restores redundancy — fixture-tested, no migration needed.
  - [x] `manifest_put` validates before Created: JSON parse, known media type (body or
    Content-Type header; OCI/Docker manifests and indexes), well-formed descriptor
    digests, descriptor sizes matching stored blobs, referenced blobs present (config +
    layers, or sub-manifests for an index), digest-reference PUTs matching the body —
    each rejection an OCI error body (`MANIFEST_INVALID`/`MANIFEST_BLOB_UNKNOWN`/
    `DIGEST_INVALID`), nothing stored or tagged on rejection; the misleading
    missing-layer test now asserts rejection, and manifest GET stays byte-identical
    (REG3, old Low manifest check).
  - [x] Trust lookups use the canonical repository path (no basename stripping: `team/app`
    hits its own policy, external `docker.io/library/app` can't alias a local `app`,
    `cache/…` exempt by construction); `verify_image_signature` returns the verified
    manifest digest and the agent deploys the digest-pinned `repo@sha256:…` reference,
    which parses through `ImageReference`/`ClusterSource` content-addressed — a tag moved
    between verify and pull cannot swap the image (IMG1). Acceptance test drives push →
    GC past grace → manifest GET 200 → peer pull in `tests/suite/pickle_integrity.rs`.
- [x] **Network policy enforcement** — write namespace/cgroup firewall maps for every
  instance; program egress before process start and on rolling deploy; fail deployment
  closed when required policy cannot be installed; reconcile kernel truth and delete every
  per-cgroup entry. Add IPv6/connect6 and CIDR enforcement, safe nftables input, required-map
  validation and IPv4/IPv6 perimeter rules with timeouts (new NET5-NET8, D5, old BPF/nft Lows).
  - [x] Namespace-firewall maps (`cgroup_namespace_map` + `firewall_map`) written and
    reconciled on every deploy/redeploy/restart/stop, so the connect hook actually enforces
    cross-namespace isolation instead of failing open on empty maps (NET5); egress programmed on
    rolling redeploy and crash-restart through one shared `finish_instance_networking` helper,
    failing closed (deny-all) on any programming error, with stop/redeploy deleting the allow
    entries so a recycled cgroup id inherits nothing (NET6).
  - [x] IPv6 egress enforcement: a `cgroup/connect6` program mirrors the policy against an
    `egress6_map` (v4-mapped `::ffff:a.b.c.d` destinations judged against the IPv4 policy),
    the parser keeps AAAA records and accepts bracketed v6 entries, and a deploy with an
    allowlist is refused when connect6 cannot attach — a v4-only allowlist is bypassable
    over IPv6, so it is not enforced (NET7). Lima test pins the old bypass now denying.
  - [x] CIDR enforcement via per-family `BPF_MAP_TYPE_LPM_TRIE` maps keyed by big-endian
    cgroup id + network address; `merge_cidr_ports` folds enclosing prefixes' ports into
    more specific entries (longest-prefix match would otherwise shadow them) with a tested
    8-port cap; the parser validates prefix lengths per family and rejects host bits with
    the normalised form in the error (NET7).
  - [x] No BPF map panics or discarded errors: every `map_mut(...).unwrap()` replaced with a
    typed `BpfMapError`, update/remove results propagated to logging callers, and the loader
    validates all required maps and programs against one list at load time, failing with the
    full roster of what is missing (NET8).
  - [x] nftables hardening: admin CIDRs parsed and re-serialised before rendering (a value
    like `10.0.0.0/8; drop` is a parse error, never an injected rule), the perimeter renders
    both `ip` and `ip6` `reliaburger_fw` tables with family-appropriate sources, and every
    `nft` invocation is bounded by a 10s timeout (NET8, old nftables Lows).
  - [x] Start-window closed on root-mode runc: the agent creates the instance's cgroup
    directory itself, programs egress against its inode, then starts (create → program →
    start) in fresh deploy, rolling redeploy and crash-restart; programming errors delete
    the created container and fail the deploy closed. Gated by `Grill::honours_cgroup_path`;
    ProcessGrill/Apple keep the documented post-start path. Lima test proves pre-start
    programming via a pid-less mock grill.
  - [x] Kernel-truth sweep (`[ebpf] sweep_interval_secs`, default 60, 0 disables): enumerates
    enforcement flags and allow entries in the kernel, scrubs state whose cgroup no longer
    maps to a live instance, rebuilds bindings adopted instances lost across a Bun restart,
    rewrites live bindings (healing lost entries) and prunes stale `cgroup_namespace_map`
    keys; a no-op sweep is silent. Pure `plan_egress_sweep` tested in the default suite.
- [x] **Consensus persistence safety** — version and checksum snapshots, validate the
  snapshot/log boundary, propagate vote/log/initialisation errors and refuse startup when
  compacted state cannot be reconstructed. Test compact → corrupt → restart returns an
  error instead of an empty cluster state (new CP3).
  - [x] A present-but-undecodable Raft snapshot is now a hard startup error instead of a silent
    empty-state boot after log compaction; a genuinely absent snapshot still loads an empty
    default (CP3).
  - [x] Persisted snapshots carry a versioned envelope: format version + SHA-256 payload
    checksum written in the same redb transaction as the payload. Load rejects a checksum
    mismatch (naming both sums) and an unknown version (naming both versions); a store with
    no envelope is refused and left untouched (CP3).
  - [x] Snapshot/log purge-boundary validation at startup (`council::validate_purge_boundary`,
    called from `cluster::runtime::open_raft_storage` before Raft starts): a log purged past
    what the snapshot covers — or purged with no snapshot at all — refuses startup with the
    exact purged/covered indices instead of booting with an unreconstructable gap (CP3).
  - [x] `DurableLogStore::is_fresh` returns `Result<bool, _>` instead of mapping read errors
    to "fresh": an unreadable store is fatal at startup, never a re-bootstrap (the C3
    split-brain through the error path); `truncate`/`purge` propagate row read errors instead
    of silently skipping keys (CP3).
  - [x] Acceptance test through the real startup seam (`tests/suite/council_persistence.rs`): a
    single-node council on durable storage writes state, snapshots, purges the log, then a
    flipped payload byte or a deleted snapshot makes restart return an error, while a clean
    compact restores every entry (CP3).

### 12b.2 — Make the cluster converge

- [x] **Control-plane directory and reporting robustness** — publish authenticated leader
  API/reporting endpoints to every gossip member; make non-voters follow leader failover;
  replace weak/version-dependent node and parent hashes; evict departed/stale nodes; filter
  terminal workloads from running/capacity; stop closed-channel spins and supervise
  long-lived task failures. Acceptance: an 8+ node cluster reconciles and reports through
  leader failover (H1/D1, CP1/CP5/CP6/CP10).
  - [x] Gossip directory extension: every datagram carries the sender's advertised
    API/reporting endpoints plus the highest-term leader hint, HMAC-authenticated, appended
    AFTER the bincode message body so old and new peers keep gossiping in both directions
    (pinned by legacy-decoder tests; the 10k gossip test is untouched).
  - [x] Non-voters follow leader failover (H1/D1/CP1): the leader-target maintainer and the
    placement reconciler resolve the leader through Raft metrics OR the gossip directory
    (`cluster::directory::resolve_leader`); a newer gossip hint outranks a deposed leader's
    stale metrics; the derived `gossip-ip:9117` offset survives only as a propagation-lag
    fallback; workers re-report immediately on re-point.
  - [x] Epoch-scoped honest aggregation (CP5): report freshness judged on aggregator-side
    monotonic receive time (never sender wall clock); entries tagged with the leadership
    term and excluded once the epoch moves on; eviction of members gossip drops
    (Dead/Left) and of entries older than three stale windows.
  - [x] Terminal workloads out of running/capacity (CP6): `build_report` excludes
    Stopped/Failed instances from `running_apps`, request sums and allocated ports;
    Stopping still counts (draining holds resources).
  - [x] Version-stable hashes (CP10): reporting parent assignment moved from
    `DefaultHasher` to a local FNV-1a with the exact mapping pinned by test; Raft-id djb2
    documented and value-pinned (kept: changing it would break durable state and
    mixed-version clusters — see `cluster::identity`).
  - [x] No silent task death, no hot spins (CP10): report/rollup workers stop polling a
    closed leader-target watch (previously a 100%-CPU spin) but keep reporting to the
    last known target — only the shutdown token ends them; cluster runtime tasks wrapped
    in `spawn_supervised` (loud log on unexpected exit; respawn deliberately rejected —
    documented in `runtime.rs`).
  - [x] Acceptance (`tests/cluster_failover.rs`, gated `RELIABURGER_CLUSTER_TESTS=1`):
    nine fully wired in-process nodes (7 voters + 2 workers), all nine report to the
    leader, a daemonset converges onto the workers, the leader is killed, every survivor
    re-points at the new leader, coverage recovers in the new epoch and new placements
    still reconcile. Passes locally in ~18s.
- [x] **Council membership self-healing** — add replacement as learner, wait for catch-up,
  promote, transfer/avoid leadership as needed and remove the dead or unsuitable voter via
  joint consensus. Prove quorum recovers with healthy spares and never removes the active
  leader mid-change (H2/D2).
  - [x] Pure replacement planner (`council::selection::plan_council_action`): observes
    voters/learners, gossip health, ranked spares and replication progress; returns at most
    one action per tick (`AddLearner`/`Promote`/`RemoveVoter`/`RemoveLearner`/`Nothing`).
    Never proposes removing the leader, never plans a change live voters can't commit or
    that drops below `min_council_size`, holds entirely when quorum is already lost (the T3
    disaster-recovery seam), and prefers add-before-remove so quorum never depends on the
    dead node's vote. Proptest pins the invariants for arbitrary observations (H2).
  - [x] Hysteresis via `council::selection::HealthTracker`: eviction requires `dead_window`
    (30s) of continuous death/absence, candidacy requires `candidate_alive_window` (5s) of
    continuous life; observed transitions reset the clocks (flap-proof) and pre-tracker
    members seed from gossip `first_seen` so warm clusters don't re-wait after failover.
    A voter inside the dead window still holds its seat, so a flap adds no learner either.
  - [x] Catch-up gating: promotion compares the learner's replicated log index (openraft
    replication metrics) against the leader's last index within `max_promotion_lag` (64);
    missing metrics count as behind unless the log is empty (D2).
  - [x] Reconciler rework (`cluster::runtime::spawn_council_reconciler_with_config`):
    re-plans each tick from observed state (idempotent, timeout-bounded, errors logged —
    the M15 non-wedging property), executes exactly one action, feeds the health tracker
    on followers too so a new leader starts warm. Voter eviction uses non-retaining
    `change_membership` (`CouncilNode::change_membership_evicting`) so dead nodes leave the
    membership entirely; dead learners are dropped via `CouncilNode::remove_learner`.
  - [x] Tests: 17 planner + 5 tracker unit tests and the planner proptest; add-then-evict
    joint-consensus composition against the in-memory Raft harness; gated acceptance suite
    (`tests/council_self_healing.rs`, `RELIABURGER_CLUSTER_TESTS=1`) — kill-a-voter heals
    to three healthy voters with writes committing throughout and the leader never removed,
    a learner killed mid-catch-up (Raft-layer partition) blocks no healthy replacement, and
    a flapping node inside the window causes zero churn.
- [x] **Council disaster recovery** — full-council-loss recovery, encrypted external
  backup/restore, explicit reconstruction thresholds and disk-pressure council resignation.
  Black-box loss/recovery exercised, not just candidate-selection helpers (D21/CP12).
  - [x] Encrypted external backup: leader-only periodic export of the state-machine snapshot
    (`src/council/backup.rs`), sealed with an HKDF-derived key
    (`reliaburger-council-backup-seal-v1`) + AES-256-GCM over the #83 snapshot payload,
    uploaded via `object_store` (`file://`/`s3://`/`gs://`) with retention pruning.
    `[cluster.backup] { url, interval_secs, retain }`, off by default. Tests: seal→tamper
    refuses, round-trip restores identical `DesiredState`, retention prunes oldest, disabled
    by default, store upload/list/prune/latest.
  - [x] Full-council-loss recovery (`src/council/recovery.rs`): `relish council recover
    --data-dir --from <url> [--master-key] [--force]` restores from a sealed backup or the
    node's own durable snapshot, refuses when a live voter answers (unless `--force`), wipes
    the dead cluster's log, and stamps a **new recovery epoch** into `DesiredState`. Next
    start re-bootstraps a single-voter Raft that the #88 reconciler regrows. Tests:
    state-machine restore + re-bootstrap, live-council refusal guard, epoch bump across
    repeated recoveries.
  - [x] Explicit reconstruction thresholds: `[reconstruction]` gains bounds validation
    (`0 < coverage ≤ 100`, positive timeouts), validated at bun startup; the recovery path
    and the leadership edge share the same values. Tests: config parse + bounds.
  - [x] Disk-pressure council resignation: a sustained-pressure state machine with hysteresis
    (`src/bun/disk_pressure.rs`); a new `disk_pressured` planner input replaces a pressured
    follower add-before-remove (never below quorum — the existing proptest holds with the new
    input); a pressured leader deposes itself first via the verified `trigger().elect()`
    mechanism. Tests: resignation windowing, planner pressure cases, gated leader-deposition.
  - [x] Disk-pressure signal path wired end-to-end (12b.2 T3 follow-up): the resignation
    mechanism above shipped complete, but production `start()` fed the reconciler a permanently
    empty pressured set — a voter only knows its own disk locally, and nothing carried that to
    the leader. Now the node advertises a `disk_pressured: bool` on the authenticated gossip
    directory extension (same trailing, position-versioned, HMAC-covered wire model as
    `labels`); the bun disk-pressure loop drives it off the hysteresis state machine; and a
    consumer task on the reconciler node folds the directory's pressured set with the live voter
    set into the Raft-id set the planner acts on. Tests: gossip wire round-trip + both-direction
    legacy tolerance + flipped-bit HMAC rejection, directory fold/prune, `pressured_voter_ids`
    filtering, and a gated end-to-end test driving pressure through real gossip into the set.
  - [x] Verified openraft 0.9 `trigger().elect()` deposes the current leader (gated step-0
    test) — no version bump needed.
  - [x] Gated black-box acceptance (`tests/council_disaster_recovery.rs`,
    `RELIABURGER_CLUSTER_TESTS=1`): three voters + backup, kill all three, recover on a
    survivor, council re-forms with restored state and a fresh epoch and regrows; plus a
    reconciler-driven leader-deposition-under-pressure test.
- [x] **Scheduler truth, labels, quotas and autoscaling** — advertise authenticated node
  labels/resources; use one mutable reservation cache per planning pass; revalidate
  generation, resources, labels, readiness and cordon; converge daemon workloads against
  eligible nodes; wire namespace quotas. Validate autoscale bounds/durations, use structured
  namespace/app metrics and configured windows, commit before cooldown and clear stale
  overrides. Reject numeric overflow (CP7-CP8, DEP8-DEP9, D13).
  - [x] Labels travel (CP7): the gossip `DirectoryExtension` now carries the sending node's
    placement labels (`BTreeMap`, bounded to 16 keys / 64-byte fields / 512 total bytes so
    datagrams stay well under the MTU), HMAC-authenticated like #89's endpoints and appended
    as trailing bytes so old and pre-labels peers keep gossiping in both directions (pinned by
    a legacy-extension decoder test). Received labels land on the stamping node's membership
    record and the `NodeDirectory`, so `filter_nodes`' label filtering and zone-aware council
    selection have live input instead of the empty map Mustard used to insert. `[node] labels`
    flows from node.toml → `ClusterParams` → `set_advertised_endpoints`.
  - [x] One reservation cache per planning pass (CP8): the leader builds the cluster cache
    ONCE per tick and plans every app against a single mutable reservation view — each
    committed placement subtracts before the next app plans, so two apps that together exceed
    one node's headroom no longer both land on it (regression test: exactly one is placed, the
    other refused). `apply_upgrade_cordon` is wired into the pass (Phase 14's helper finally
    has a caller); a cordoned node receives nothing. Each decision is revalidated against the
    LATEST membership immediately before the async Raft write, so a node that dies mid-pass is
    dropped rather than assigned. Pure `plan_scheduling_pass` is unit-tested for all four cases.
  - [x] Daemon convergence: a daemon app re-plans over the currently eligible nodes each tick
    (a placement targeting a departed/cordoned node counts as stale), so it gains an instance
    when a node joins/becomes eligible and loses one when a node leaves. Tests cover node-join
    growth and departed-node re-plan.
  - [x] Namespace quotas wired (enforcement seam): `QuotaLedger` accumulates per-namespace
    usage cumulatively across the pass and refuses an app that would bust its quota, with a
    clear deploy-time error. **Handoff closed in T6:** namespace resources are now desired
    state, and the scheduling pass builds its ledger from `desired.namespaces` via
    `ledger_from_namespaces` (`orchestrate.rs:150`), so a declared quota actually rejects an
    over-budget app on the apply path.
  - [x] Autoscale lifecycle (DEP8): `AutoscaleConfig::from_spec` returns a validated `Result`
    (rejects `min > max`, zero max, zero/unparseable evaluation window — cooldown may be zero,
    out-of-range threshold) and config validation surfaces it on `relish apply`; the cooldown
    starts only *after* a successful Raft write (a failed write no longer suppresses the next
    attempt); stale overrides are cleared in the state machine when an app's replica baseline
    changes or the app is deleted (an image-only redeploy keeps the override); the rollup query
    uses the configured `evaluation_window` instead of a hardcoded five minutes.
  - [x] Numeric overflow rejected (DEP9): `parse_num` uses checked multiplication (a huge
    memory string is a validation error, not a wrapped small value) and the quota arithmetic
    saturates throughout.
- [x] **Transactional desired state and deployment** — make instance identity include
  namespace/generation/ordinal; apply AppDelete through Raft on cluster stop; consume
  count/generation-aware reconstruction corrections; wait for terminal deploy/stop outcome,
  retry/reschedule failures and rebuild applied state after restart. Move image/init/runtime/
  health waits outside Bun's command loop and honour surge, unavailable, drain and rollback
  semantics (H7/D10, DEP1-DEP6, codex-M3).
  - [x] PR 1 — Namespaced instance identity + AppDelete-on-stop + durable applied-state
    (DEP1/DEP2/DEP3).
    - [x] `InstanceIdentity { namespace, app, generation: Option<u64>, ordinal }`
      (`grill::mod`) with one canonical string form, `{namespace}__{app}[-g{gen}]-{ordinal}`
      (`__` is illegal in a DNS-1123 label, so the namespace parses back out unambiguously
      even for a hyphenated app name). Replaces both old formats (`{app}-{i}` and the canary
      `{app}-g{gen}-{i}`) at every construction site (supervisor app/job, agent canary,
      reporting ordinal, record replica-index, upgrade inventory). Round-trip + hyphenated-app
      unit tests; DEP1 collision regression (two same-name apps in different namespaces
      coexist in the supervisor).
    - [x] Adoption checks each record's id against the canonical id rebuilt from its
      structured `namespace`/`app_name`/`replica_index` fields and refuses a mismatch; the
      upgrade marker records each instance's canonical `full_id`. (Pre-release, namespace-less
      records and markers are not read; T4.9 deleted the compatibility path.)
    - [x] Cluster stop proposes `AppDelete` through Raft (`stop_handler` → `cluster_stop`,
      leader-forwarded like `apply`), so desired state clears and no reconciler resurrects the
      app; a leader with no local replica still clears cluster state (no spurious 404). The
      local container stop is best-effort after the delete commits. Standalone mode keeps the
      local-only stop. Gated 3-node test: deploy, stop through any node, desired state clears
      and stays clear across reconcile ticks.
    - [x] Durable, terminal-outcome applied-state (`cluster::applied`, DEP3/H7/D10): the
      placement reconciler marks a placement applied only on the deploy's terminal `Complete`
      event (a failed deploy is retried next tick), and persists the applied map to a
      self-describing JSON checkpoint (atomic temp+rename, schema-versioned) reloaded on boot —
      so a restart resumes converged work without double-deploying or forgetting in-flight
      work. Unit tests: round-trip, corrupt-loads-empty, restart-skip-vs-redeploy semantics.
    - [x] Snapshot compat: the identity/applied-state changes touch no council/Raft state, so a
      pre-theme snapshot loads unchanged (the #83 envelope loader / existing fixture holds).
    - [x] `make ci` green (fmt, clippy `-D warnings`, 2387-test default suite); gated
      `RELIABURGER_CLUSTER_TESTS=1` `placement`/`cluster_failover` suites green. Book: chapter 2
      (instance identity), chapter 7 (applied-means-done, AppDelete-on-stop).
  - [x] PR 2 — Deploy work off the command loop (DEP4/codex-M3).
    - [x] Each `Deploy` command now spawns a `DeployWorker` task instead of awaiting `deploy`
      inline in the command arm, so image preparation, init-container polling, runtime
      create/start and the rolling health wait no longer block the Bun command loop. The loop
      keeps servicing health checks, crash restarts, status and further deploys while a pull is
      in flight; concurrent deploys interleave rather than serialise.
    - [x] The supervisor state machine stays authoritative: the worker owns only the blocking
      grill I/O (grill+port allocator are `Arc`-backed clones), and drives every state
      transition and every supervisor / service-map / networking mutation back through a
      `DeployOp` message channel (`deploy_ops_rx`) drained in the loop's `select!` and
      dispatched to the same `&mut self` helpers the old serial path used. No deploy logic
      moved — only where it runs.
    - [x] Preserved behaviours: create → program-egress → start ordering (#86), the #87
      per-instance identity lifecycle, health-gating, crash-restart, rolling rollback on an
      unhealthy new replica, and PR 1's namespaced identity + durable applied-state. The fresh,
      rolling and job paths were split into loop-side (`prepare_fresh_instance`,
      `finish_fresh_instance`, `prepare_rolling_instance`, `finalise_rolling_deploy`,
      `rollback_rolling_deploy`, `finish_job_instance`) and task-side (`drive_fresh_instance`,
      `drive_job`, `rolling_redeploy`) halves.
    - [x] Failing-first tests: `slow_deploy_does_not_block_the_command_loop` (a 3s image-pull
      deploy still answers a `Status` in <500ms) and `concurrent_deploys_interleave` (two
      deploys' creates are both in flight after one delay). `MockGrill::set_create_delay`
      simulates a slow pull. Both fail against the old inline deploy.
    - [x] `make ci` green (fmt, clippy `-D warnings`, default suite); gated
      `RELIABURGER_CLUSTER_TESTS=1` `placement`/`cluster_failover` suites green. Book: chapter 7
      (a slow deploy shouldn't freeze the node).
  - [x] PR 3 — Deploy semantics: drain / surge / rollback and exit-aware stop (DEP5/DEP6).
    - [x] The library-only `DrainTracker` now governs the live path. A `SharedDrains`
      (`Arc<tokio::sync::Mutex<DrainTracker>>`) is owned by the agent and handed to the Wrapper
      proxy (`bind_proxy_with_drains`, `agent.drains_handle()`). The proxy bumps a draining
      backend's in-flight count around each forwarded request/WebSocket via a `DrainGuard` RAII
      type whose `Drop` releases the count on every exit path; `select_backend` now carries the
      `instance_id` so the count keys correctly.
    - [x] DEP5 rolling retire drains through the live path: `finalise_rolling_deploy` registers
      the new backends and rebuilds the routing table *first* (so the proxy routes new traffic to
      the fresh instances), then `retire_with_drain` starts a per-instance drain, waits for
      in-flight requests to finish (up to `drain_timeout`) via `wait_drained`, and only then does
      an exit-aware stop. Surge-first ordering keeps the serving count from dipping below
      `replicas - max_unavailable`; the existing unhealthy-replica auto-rollback path is
      preserved.
    - [x] DEP6 exit-aware stop: `stop_runtime_instance` (SIGTERM → poll for exit up to
      `STOP_GRACE_SECS` → SIGKILL) is shared by operator stops, retirements and the rolling
      retire; operator stops and retirements await it off the command loop (`app_exit_wait`).
      The supervisor records `Stopped` only after the runtime confirms the exit, so container and
      supervisor state cannot diverge.
    - [x] Failing-first tests: `retire_waits_for_in_flight_request_before_kill` and the live
      `live_proxy_holds_drain_open_while_a_request_is_in_flight` (a real bound proxy holds an
      in-flight request open and the drain does not complete until it returns);
      `rolling_redeploy_never_drops_below_target_availability` (new `start` precedes old
      `stop`/`kill`); `stop_escalates_to_kill_when_process_ignores_sigterm` and
      `stop_reports_stopped_after_exit_without_kill` (`MockGrill::set_ignore_stop` simulates a
      process that refuses SIGTERM). All fail against the pre-PR kill-immediately path.
    - [x] `make ci` green (fmt, clippy `-D warnings`, 2405-test default suite); gated
      `RELIABURGER_CLUSTER_TESTS=1` `placement`/`cluster_failover` suites green. Book: chapter 7
      (wiring drain/surge/rollback into the live path, why stop must wait for exit). Theme
      complete.
- [x] **Complete declarative resources** — validate and apply apps, jobs, builds, namespaces
  and permissions through the same local/Raft/GitOps path; enforce namespace and permission
  resources plus build scope; reject or remove every parsed field that cannot affect the
  binary. Acceptance: one configuration containing every resource kind converges identically
  through manual apply and GitOps (DEP7/D12). **This theme closes the 12b.2 "Make the cluster
  converge" tier.**
  - [x] Namespaces and permissions as desired state (DEP7): append-only serde-default
    `NamespaceSpec`/`NamespaceDelete`/`PermissionSpec`/`PermissionDelete` Raft variants and
    `namespaces`/`permissions` `BTreeMap`s in `DesiredState`. `Config::validate`
    rejects negative/overflowing namespace budgets, zero caps, unknown permission actions,
    and permissions/builds referencing a namespace that exists in neither the config nor
    committed desired state (`validate_against`).
  - [x] Shared apply path (the linchpin): one `council::config_to_desired_writes` turns a
    parsed `Config` into the ordered write set (namespaces, then permissions, then apps).
    BOTH manual `cluster_apply` (`bun/api.rs`) and Lettuce call it, so the two paths cannot
    diverge by construction. Deletion semantics: manual `relish apply` is additive (writes
    what's in the file, prunes nothing — matching how app apply already behaved); GitOps
    reconciles the whole repo and prunes namespaces/permissions consistently with apps.
  - [x] Namespace quota handoff closed (T4 seam): the scheduling pass builds its
    `QuotaLedger` from `desired.namespaces` via `ledger_from_namespaces`
    (`orchestrate.rs:150`), so an app exceeding its namespace CPU/replica budget is refused
    at deploy time with a logged reason; a namespace with headroom admits it. `quota_from_spec`
    parses the config's `"8000m"`/`"16Gi"` strings into the scheduler's integer quota.
  - [x] Build scope wired (DEP7): `Config::validate` runs `validate_build_namespace`, and
    `/v1/build` submission runs `check_namespace_scope` against desired-state namespaces so a
    build pushing to a `pickle://ns/name` destination must target an existing namespace.
  - [x] Lettuce through the unified path, atomically (D12): `resource_change_to_request` was
    replaced by `change_to_request`/`payload_to_request`/`remove_to_request` handling apps,
    namespaces and permissions (no more silent `None` skip); the app write now keys on the
    spec's own namespace, not a hardcoded `default` (the divergence the acceptance test
    caught). `last_applied_commit` advances ONLY when every write in the sync succeeds
    (`apply_changes` returns `Err(id)` and stops on first failure); a partial/failed sync
    leaves the commit unadvanced and re-applies (idempotently) next tick.
  - [x] Parsed-but-dead audit: `NamespaceSpec` (all five fields feed the quota),
    `PermissionSpec` (actions/namespaces validated, stored as desired state; principal→
    permission *binding* is AUTH1/12b.3, noted in the book), `BuildSpec`
    (destination/namespace/context validated). Nothing removed — every field now affects the
    binary.
  - [x] Acceptance: `manual_apply_and_gitops_converge_identically` asserts one every-kind
    config produces byte-identical declarative `DesiredState` via manual apply and GitOps.
    `make ci` green (fmt, clippy `-D warnings`, 2431-test default suite); gated
    `RELIABURGER_CLUSTER_TESTS=1` `placement`/`cluster_failover` suites green. Book: chapter 7
    (why apply and GitOps must share one path; why `last_applied` advancing on partial failure
    silently drops resources). Theme complete; 12b.2 tier complete.
- [x] **Durable batch and build execution** — use one authoritative namespace; persist
  monotonic IDs, trackers and terminal state; include unschedulable jobs; bound and retry
  dispatch/callbacks; make duplicate reports idempotent and run GC. Transfer build context
  to the chosen builder, derive registry endpoints from node config, isolate/clean tempdirs,
  bound CLI polling, retry another builder and make required signing part of terminal
  success. Test leader restart, lost callback and delegation from a non-builder entry node
  (D18, JOB3-JOB7, old batch-order Low).
  - [x] One authoritative job namespace (JOB3): resolved once at submit (conflicting
    submission/spec values are a 400), written into both the record and the spec so
    dispatch, deploy and the watcher agree; non-default-namespace batch completes e2e.
  - [x] Raft-durable trackers and IDs (JOB4): `batch_state`/`build_state` in `DesiredState`
    (serde defaults; pre-theme snapshot fixture-tested through the envelope loader),
    append-only `BatchRegister`/`BatchJobUpdate`/`BuildRegister`/`BuildUpdate` variants,
    monotonic Raft-allocated ids that survive restart, deterministic register-time GC
    (retention window + cap-at-50, keyed on the request's clock), `/v1/build/track`
    leader-forwarding for follower builders.
  - [x] Bounded, retried dispatch and callbacks; validated idempotent reports (JOB3):
    dispatch retries then fails jobs honestly, callbacks retry with backoff, reports are
    transition-validated (forged status 400, unknown batch/job 404, conflicting terminal
    409, duplicate 200 no-double-count), unschedulable jobs are first-class terminal
    records in the batch summary.
  - [x] Pull backstop + restart resumption (JOB3/JOB4): leader-side per-batch watcher polls
    assigned nodes; lost-callback batch still terminates; a restarted leader respawns the
    watcher from the durable record on status reads; a Running build with no live runner is
    rewritten to an honest durable failure.
  - [x] Build context transfer + builder retry (D18/JOB5): confirmed the residual (bare
    `_buildcontext` blobs are uncatalogued and never replicate), closed it by copying the
    blob to the chosen builder's registry before dispatch (addresses from membership +
    config only); delegation walks all capable peers before an honest 502.
  - [x] Signing as part of terminal success (JOB7): builds sign keylessly with a
    Workload-CA-issued certificate (verified against the root CA before attaching);
    `AttachSignature` for an unknown digest is Refused, not a no-op; under
    `require_signatures` a signing failure fails the build with the reason.
  - [x] Deterministic allocation (old Low): `BTreeMap` profile groups + name-sorted input,
    pinned by a same-input-same-plan test.
  - [x] Bounded CLI polling (JOB6 residue): `relish build --timeout` and
    `relish batch-status --wait --timeout` with Ctrl-C handling; timeout exits non-zero
    with the last known state.
  - [x] Registry chunked-upload PATCH/PUT responses now carry the spec-required
    `Location` header — found by the Lima buildah gate: containers/image 5.29
    (buildah 1.33) reads it strictly, so every real `buildah push` against Pickle
    failed (pre-existing on main; the gated e2e build now passes in the VM).

### 12b.3 — Secure every boundary

- [x] **API authorisation and Brioche** — enforce app/namespace scopes in reusable
  extractors; cover every mutation endpoint; refuse unsafe non-loopback bootstrap; replace
  the shared cluster-Admin service token with route-limited per-node capabilities; index and
  verify one Argon2 token in spawn_blocking under a concurrency bound; use real dashboard/node
  state and escape each HTML/attribute context including apostrophes (H4, D8, AUTH1-AUTH5, AUTH7-AUTH8).
  - [x] Authenticated read-only browser sessions + `/`+`/ui/*` route lockdown; env values
    masked (Stage 5, PR #77 — closes H6/AUTH6).
  - [x] Scope enforcement (AUTH1/H4) via `authorize_scoped` on apply/stop/exec/rollback/snapshot;
    fail-closed non-loopback bootstrap refusal at bind time (AUTH3); snapshot-mutate + rollback
    now Deployer-gated (AUTH2); the `__system` service principal refused on token/secret/sign/
    chaos/fault via `authorize_user` while node fan-out keeps its System routes (AUTH4);
    off-lock bounded Argon2 with a format short-circuit + `spawn_blocking` semaphore (AUTH5);
    dashboard/node fragments read the live gossip membership (AUTH7); `escape_html` now escapes
    `'` for single-quoted chart attributes (AUTH8).
- [x] **Node PKI, join and mTLS** — the listener/identity half landed with Stage 5; the join
  hardening and peer binding below landed with 12b.3 (registry auth/TLS moves to the Pickle
  storage theme).
  - [x] Persist the bootstrap node leaf/key/Node CA/root CA + config paths; deliver a bundle
    to joiners with TOFU fingerprint (Stage 5 — PKI1).
  - [x] Handshake-true, CRL-aware mTLS builders with live-update tests, Node-CA-pinned client
    verifier, and **mTLS wired onto the Raft, reporting and agent-API listeners** behind the
    `require_mtls` mode matrix (Stage 5 — PKI1/PKI2, C5(b) listeners, L17 peer-CRL refresh).
  - [x] CSR-based join keeping the key on the joiner: the joiner generates its keypair + CSR,
    the issuer signs it and returns a key-free bundle, the joiner assembles its identity from
    its own key (12b.3 — PKI4).
  - [x] Atomic consume-and-serial join: one Raft `ConsumeJoinTokenForIssue` entry marks the
    token consumed and allocates the serial together, so racing joiners with one token issue
    exactly one cert; the issuer DN is derived from the stored Node CA (12b.3 — PKI5).
  - [x] Expected-peer node-id binding: node certs carry a `spiffe://reliaburger/node/<id>` URI
    SAN, and the Raft connector builds a per-target verifier that asserts the peer leaf's
    node-id SAN matches the node it is dialling (12b.3 — PKI3).
  - [x] Transactional bundle install/validation: the identity bundle writes a commit marker
    last and `load` refuses a marker-less (partial) or non-chaining bundle (12b.3 — PKI9).
  - [x] Per-connection handshake/read deadlines on the Raft and reporting accept paths, on top
    of the existing frame bounds, so a stalled peer is dropped (12b.3 — CP11).
  - [x] Constant-time join-token comparison (12b.3 — PKI10 slice; IMG1-IMG3 remain in the
    image-trust theme).
- [x] **Image trust policy** — distribute authoritative trust state to workers and
  standalone mode and fail closed; validate every intermediate, time, revocation, EKU,
  issuer and SPIFFE/OIDC identity; enforce issuer/audience/algorithm/kid; use a persistent
  configured or workload key for build signing and require the signature write to apply.
  Use constant-time join-token comparison (IMG1-IMG3, PKI10, old keyless/OIDC Lows).
  - [x] Fail-closed image verification: a node with `require_signatures` set that has no
    council handle (a standalone node) can't reach the catalogue or root CA, so an image
    deploy is *refused* rather than skipped; a process workload (no image) still passes. No
    cluster-state distribution needed — clustered nodes already replicate the trust anchors
    via the CouncilNode, so only genuine standalone mode hits the refusal (12b.3 — IMG2).
  - [x] Full keyless chain validation: `verify_keyless` now walks every adjacent link
    (signature + issuer-DN binding) to the trust anchor, checks each cert's validity at an
    injected clock, requires the leaf's code-signing EKU, and binds the leaf's SPIFFE URI SAN
    to the identity the signature declares — reusing `cert::validate_chain_at`,
    `check_code_signing_eku`, `subject_uri_sans` and the existing CRL check (12b.3 — IMG3).
  - [x] OIDC constraint hardening: `verify_jwt_with_constraints` checks the JOSE `alg`/`kid`
    header *before* trusting the signature, then enforces `iss`, `aud`, and `iat` skew/age
    bounds on top of the signature + `exp` (defence-in-depth: no production caller yet)
    (12b.3 — PKI10 OIDC half).
  - [x] Persistent build signer: one code-signing identity per namespace
    (`spiffe://…/job/build-signer`) provisioned via the council and cached, reused across
    builds instead of a fresh ephemeral CSR each time; its leaf carries the code-signing EKU
    so a signature it produces passes the tightened deploy-time check (build-sign → deploy-
    verify round-trip). The `require_signatures` build-failure gate is preserved: no
    policy-trusted signature, no `Completed` (12b.3 — JOB7 follow-up).
  - [x] Constant-time join-token comparison + CSR join landed in Theme 2 (#100).

> **12b.3 "Secure every boundary" is complete** — all three themes done: API
> authorisation and Brioche (#99); Node PKI, join and mTLS (#100); Image trust policy.

### 12b.4 — Finish the data plane

- [x] **Global namespaced service catalogue and DNS** — introduce ServiceId { namespace,
  name } through Onion, DNS, eBPF, Wrapper, firewall and APIs; replicate healthy endpoints;
  allocate VIP/container IPs with collision, exhaustion and release handling; bind DNS on
  a container-reachable address and fail startup/deploy if unavailable. Add DNS TCP and
  source ACLs, and either provide a portable non-eBPF VIP path or reject that configuration
  up front (H3/codex-M1, D3/D5/D6/D7-routes, old M5/M6).
  - [x] **PR 1 — namespaced local service identity.** `ServiceId { namespace, name }`
    (canonical `{namespace}__{name}`, aligned with `InstanceIdentity`) re-keys the
    `ServiceMap` and all ~64 agent call sites; VIP derives from the qualified identity with
    collision-probe + release-on-stop; DNS resolves `<app>.<namespace>.internal` (bare name
    → default namespace), adds a TCP listener and a source ACL (public sources REFUSED), and
    fails closed on bind; Wrapper ingress keyed by `(namespace, app)`; the eBPF `(vip,port)`
    key is unchanged (namespaced VIP keeps it collision-free, no recompile); `[dns]` without
    `[ebpf]` rejected at config validation; whitepaper §10 D6 doc-drift fixed. Tests-first:
    distinct VIPs + independent resolve, VIP release, namespaced routes, DNS ACL refusal,
    fail-closed bind, config reject. `make ci` green.
  - [x] **PR 2 — replicated global endpoint catalogue.** New `EndpointCatalog`
    (`onion/catalog.rs`) — namespaced services → VIP + cluster-wide backends, cluster-wide
    deterministic collision-free VIP allocation. Added a distinct `endpoint_catalog` field
    to `DesiredState` (separate from Pickle's `manifest_catalog`) plus a
    `RaftRequest::PublishEndpoints` variant + apply (wholesale replace). The leader builds the catalogue from aggregated
    health reports (namespace/app/port/health) + gossip node IPs and publishes it via Raft
    only when it changes. Every node overlays it onto its local service map
    (`ServiceMap::with_cluster_catalog`, non-mutating merge) so DNS + ingress resolve
    services on other nodes with no DNS/routing changes; workers get it piggybacked on the
    `/v1/placements` poll. Tests-first: catalogue rebuild/collision/JSON, non-mutating merge
    (remote-only + merge-into-local + dedupe), leader build across nodes, apply + snapshot
    fixture; gated (`RELIABURGER_CLUSTER_TESTS=1`) cross-node resolve that survives a leader
    change. `make ci` green.
- [x] **Ingress transport and draining** — carry per-route TLS mode into routing; implement
  ACME/cluster-CA or the documented explicit certificate contract; redirect HTTP except
  challenges; stream request/response bodies with limits/backpressure; hold connection
  permits through TLS/WebSocket lifetime; add handshake/idle timeouts. Replace untrusted
  forwarded headers, use boundary-correct deterministic route/rate keys, parse IPv6 Host
  correctly and wire deployment draining (D7/D10, ING1-ING5).
  - [x] **ING1/D7 — per-route TLS mode + HTTP→HTTPS redirect.** New `TlsMode` enum
    (`Disabled`/`Cluster`/`Explicit`) parsed from `IngressSpec.tls`, carried into
    `PathRoute`. Unsupported modes (`auto`/`acme`) are rejected at routing rebuild rather
    than silently served plaintext; a TLS-marked route reached over plain HTTP gets a 308
    to HTTPS on every path. Cluster-CA path: `tls::issue_ingress_cert`
    issues a server-auth ingress cert from the Sesame Ingress CA (reuses the existing CA
    hierarchy, no parallel scheme). Explicit certs keep the existing disk-file contract.
    Every plaintext path now redirects for TLS routes: the old ACME challenge
    exception was removed because v1 has no challenge responder.
  - [x] **ING2 — connection permits through the full lifetime.** TLS handshakes are bounded
    by a semaphore and a per-handshake deadline (`tokio::time::timeout`), so a slow-handshake
    flood can't exhaust tasks. The proxy holds a connection permit for the whole request; for
    a WebSocket the permit (and drain guard) move into the splice task and release when the
    splice closes, not at the 101.
  - [x] **ING3 — streamed bodies with limits.** Response bodies stream via
    `Body::from_stream(resp.bytes_stream())` instead of buffering whole (SSE/gRPC/large
    downloads flow with backpressure). Request bodies keep a configurable
    `max_request_body_bytes` cap; over-cap requests get 413.
  - [x] **ING4/D10 — WebSocket drain.** `increment_websocket`/`decrement_websocket` wired at
    the 101 and splice end; `check_completions` now waits for both the HTTP and WebSocket
    counts to reach zero (or the deadline), so a rolling retire honours `drain_timeout` for a
    live WebSocket, extending #97's HTTP drain.
  - [x] **ING5 — trusted forwarded headers + correct route/rate keys.** Client
    `X-Forwarded-For`/`-Proto`/`Forwarded` are stripped and replaced with the proxy's real
    view; route matching is segment-boundary correct (`/api` no longer matches `/apievil`)
    with deterministic tie-break ordering; rate buckets key on `(host, path, client IP)` not
    the IP alone; zero/overflow rates are rejected at rebuild; IPv6 Host (`[::1]:port`) is
    bracket-aware. `make ci` green.
  - **12b.4 tier complete** — all three themes (namespaced catalogue, Pickle durability,
    ingress transport) done. The data plane is finished.
- [x] **Pickle storage and replication durability** — stream/hash off the async runtime;
  add authenticated principal/repository quotas and upload expiry; write unique temp files,
  fsync and rename; reverify cache and isolate immutable rootfs generations; constrain
  redirects to same-origin relative paths and bound peer reads. Recheck references at
  deletion, report degraded redundancy honestly, define acknowledged push semantics and
  support multi-segment repositories (D11, REG2/REG4-REG8, old cache/upload Lows).
  - [x] REG5 — durable persistence: `write_blob`/catalogue `persist_to`/`complete_upload`
    write a unique temp, fsync the file, rename, fsync the parent dir; cache hits and the
    deploy path revalidate the digest (`revalidate_blob`); rootfs unpacks into an immutable
    content-addressed `gen-{hash}` directory so a tag move can't clobber a live container.
  - [x] REG2 — one authoritative catalogue: `manifest_get`/`tags_list` read
    `catalog_snapshot()` (the council's Raft catalogue when clustered), so a peer's commit is
    visible everywhere without waiting for a heal tick.
  - [x] REG4 — auth, TLS, quotas, limits: registry writes reuse `sesame::auth` (bearer +
    service token; Deployer role); the listener serves over TLS via `build_api_server_config`
    when the node has an mTLS identity, peers address `https://`; per-repository/registry byte
    quotas; whole-blob hashing moved to `spawn_blocking`; upload-session TTL + sweep (REG8).
  - [x] REG6 — same-origin redirects + bounded reads: `resolve_same_origin_location` refuses
    an absolute/cross-host/protocol-relative upload `location`; peer body reads are capped and
    wrapped in the timeout.
  - [x] REG7/D11 — push semantics: `201`+`OCI-Replication: pending` means authoritative metadata with replication still owed. Unconfirmed Raft writes now return retryable `503`, replacing the older `202`+`raft-uncommitted` response that generic clients could misread as success. GC rechecks the full catalogue reference set before deletion.
  - [x] REG8 — multi-segment repositories: the router captures the whole `/v2/*rest` path and
    splits the OCI operation suffix, recovering `team/app` names end to end; upload sessions
    expire.

### 12b.5 — Make automation and observability truthful

- [x] **Metrics, logs and object storage** — parameterise every DataFusion query; add
  typed/bounded raw-log access; push predicates into Parquet and move flush/compaction off
  async locks. Persist/prune epoch-aligned idempotent rollups; collect per-app/scraped
  metrics; distinguish stale telemetry in alerts; URL-encode fan-out and return partial
  failures; deduplicate by stable node/instance/event identity. Implement real S3/GCS log
  export, one Bun-owned checkpoint, durable object IDs, provider webhook payloads and final
  shutdown flush; remove/consolidate legacy Ketchup calendar/config (codex-M4/D13,
  old M3/M4/M19/M20/M24/X8, OBS1-OBS8).
  - [x] Cluster rollup metric-name queries escaped via `escape_sql_literal`, closing the
    `/v1/metrics/rollup` SQL injection where `params.name` was interpolated raw (OBS1).
  - [x] **PR O1 — Metrics.** OBS1-remainder: `metrics_app_handler` escapes both `?name=` and
    the `namespace/app` path before interpolation (injection can't bypass the tenant/time
    predicate). OBS2 rollups: buffer bounded, windows epoch-aligned, ingest idempotent per
    `(node, window)` so a re-sent window doesn't double-count, and the flush counter recovers
    past existing files on restart (no clobber) with history read back from Parquet. OBS3:
    the collection loop now collects per-app process metrics (labelled `namespace/app`) from
    the agent's running instances, so autoscaling/dashboards have a signal. OBS4 alerts:
    stale telemetry no longer resolves a firing alert (value-not-boolean state machine),
    Slack (attachments) and PagerDuty (Events API v2) get provider-shaped payloads, and a
    zero `evaluation_interval_secs` is rejected at startup. OBS5/M3: flush drains under a
    brief lock then writes via `spawn_blocking` off the lock (queries proceed during a
    flush), and a corrupt Parquet file is skipped per-file rather than failing every query.
  - [x] **PR O2 — Logs + object storage.** OBS5: `/v1/logs/sql` goes through a bounded path
    (`query_sql_json_bounded`) — read-only `SELECT`/`WITH` only, `logs`-table-only session,
    outer-`LIMIT` row cap and a working-memory limit; a rejected query is a `400`. OBS6
    fan-out: `app`/`namespace` percent-encoded as path segments and query values through
    reqwest's encoder (a `grep` with `&`/`?` travels intact); node HTTP/JSON/task failures
    return a partial `FanOutResult` (which nodes failed) instead of silent empty; dedup keyed
    on stable `(node, timestamp, stream, line)` identity, so two replicas' identical lines
    survive while one node's retransmit collapses. OBS7/M20/X8: `export_logs` ships via
    `object_store::parse_url` (`s3://`/`gs://`/`file://`/bare path), one Bun-owned checkpoint
    shared with `relish logs-export` (`CHECKPOINT_FILENAME`), durable content-hash object ids
    so a filename reused after retention isn't skipped, and a final shutdown flush of both
    metrics and log buffers. OBS8/M24: the dead `KetchupStore` (+ its `index`/`json` modules
    and `logs.max_file_size_mb`) removed; `LogStore` is the sole live path. Real S3/GCS lives
    behind a named `#[ignore]` manual test, not a silent gate.
- [x] **GitOps convergence and webhook security** — expose a signature-validated HMAC
  webhook route with replay/rate controls; make diff namespace-aware and deterministic;
  apply every resource through the unified desired-state path; never advance last_applied
  after skip, partial result or failed write; keep jobs stable. Verify reused clone
  remote/branch, terminate Git options safely and wire coordinator/backoff semantics
  (D12, GIT1-GIT4, old Git/Jobs Lows).
  - [x] GIT2 namespaced convergence: the diff keys apps by full `AppId` (namespace + name),
    and the `resource_id` encodes `app.<namespace>/<name>`, so a `prod/web` removed from git
    deletes `prod/web` and never `default/web`. Two same-named apps in different namespaces
    converge independently (`diff.rs`, `runner.rs`).
  - [x] GIT2b jobs: dropped the phantom job `Add` the diff emitted every sync against an
    always-empty set. Jobs run to completion (not reconciled desired state), so the diff
    emits nothing for them — no re-add, no inflated `summary.added`.
  - [x] GIT3 public HMAC webhook: `WebhookValidator` wired into the live handler; the route
    is public (exempt from the 12b.3 bearer middleware — GitHub/GitLab can't send a bearer)
    but HMAC-gated in the handler. Validates `X-Hub-Signature-256` (GitHub) or `X-Gitlab-Token`
    (GitLab) over the raw body, rejects replayed delivery ids (401), and rate-limits (429).
    Fails closed (503) with no `[gitops] webhook_secret`. Constant-time compares throughout
    (`ring::hmac::verify`; explicit CT for the GitLab token).
  - [x] GIT4 git safety: `--`/`--end-of-options` terminators before every user-controlled
    ref/path/SHA (no option injection); reused clone verified against remote URL + branch
    and re-cloned on drift; deterministic sorted-path file merge with a duplicate-resource
    error instead of hash-order overwrite.
  - [x] GIT4 durable errors + backoff + coordinator: hard sync failures recorded in
    `SyncState` (`last_error`, `consecutive_failures`, `phase = Error`) so relish/UI see a
    broken sync, not just stderr; `backoff_delay` wired into the retry; `select_coordinator`
    wired to stamp the elected coordinator (complements leader-only sync — the leader still
    drives writes).
  - GIT1 (last_applied advances only on full success) already landed in #98; not redone.

### 12b.6 — Platform, upgrades and documentation

- [x] **Process workloads and platform capabilities** — pass [process_workloads] into the
  supervisor; default-deny host exec/script; enforce executable allowlists and mount
  isolation before runtime creation. Implement or explicitly reject rootless networking/
  resource limits, GPU detection/device isolation and unsupported Apple/runtime adoption;
  make gpu_enabled effective (H8/D15/D17, old M22/M23).
  - [x] D17/H8: `[process_workloads]` now reaches the supervisor. `bun.rs` calls
    `agent.set_process_config(...)` → `WorkloadSupervisor::set_process_config`, so the
    operator's allowlist governs host execution instead of the ignored constructor default.
  - [x] Default-DENY host exec/script: `ProcessWorkloadsConfig::is_binary_allowed` no longer
    treats an empty allowlist as all-allowed — an empty/absent list refuses every host
    binary. A `script` is host execution of `/bin/sh`, so the interpreter must be allowlisted
    too. Enforced in `WorkloadSupervisor::admit_process_workload` for both `deploy_app` and
    `deploy_job`, before anything is allocated; container (runc/apple) workloads are
    unaffected. Mount isolation is checked at admission: config asking for it on a non-Linux
    node is refused rather than run unprotected.
  - [x] D15 GPU: real `NvidiaGpuDetector` (probes `/dev/nvidia0`, parses `nvidia-smi
    --query-gpu`, falls back cleanly to none, OS-guarded). `gpu_enabled` made effective —
    the supervisor **rejects** a GPU-requesting workload when `gpu_enabled = false` or the
    detector found no device, instead of silently scheduling onto a node reporting zero GPUs.
    (OCI `/dev/nvidia*` device passthrough deferred; placement is now honest.)
  - [x] M22 rootless: **reject** path chosen. A rootless node
    refuses a workload declaring cpu/memory limits (rather than silently dropping them via
    `make_rootless`'s `resources = None`), with a clear error naming the fix. slirp4netns
    networking and published-port adoption landed later in M5; workload DNS and
    delegated rootless cgroup limits remain unsupported.
  - [x] Reserved resources: `reserved_cpu`/`reserved_memory` were already wired
    (`bun.rs::node_capacity` → `set_node_capacity`); confirmed, nothing dead to remove. GPU
    capacity is not yet threaded through the reporting protocol (orchestrate seam) — the
    supervisor gate makes `gpu_enabled` effective in the meantime.
  - Deferred (not this theme): unsupported Apple/runtime adoption (Self-upgrade seam).
- [x] **Smoker effects and cleanup** — stop returning success for no-op memory, disk,
  drain and node-kill faults; apply CPU stress to the target workload cgroup; make every
  persistent effect reversible on clear/expiry, including pause/resume. Acceptance measures
  each advertised effect and its removal (CHAOS1).
  - [x] Thread instance ns/app/ordinal into `apply_fault` (`target_instance_cgroups`) so
    resource faults target the workload's own cgroup, not a bare service name.
  - [x] CPU stress caps the target cgroup's `cpu.max` (was: burning Bun's own CPU); the
    saved quota is restored on clear/expiry.
  - [x] Memory pressure lowers the target cgroup's `memory.high` toward `memory.max` (was:
    a genuine no-op returning Ok); the saved soft limit is restored on clear/expiry. An
    `oom` request is rejected as irreversible (use a Kill fault instead).
  - [x] Disk-I/O throttle writes `io.max` on the target cgroup keyed by the volumes-dir
    device; the throttle is lifted on clear/expiry.
  - [x] Pause auto-resumes: the frozen PIDs are recorded and SIGCONT'd on clear/expiry
    (`expire_faults` and both clear paths), so a paused workload never stays frozen.
  - [x] Node drain / node kill initially returned an honest rejection instead of a fake Ok.
    Phase 15 M8 now supplies their authenticated cluster-level effects and reversal.
  - [x] `DnsNxdomain` acts in the userspace `.internal` resolver (was: wrote a `fault_dns_map`
    entry into a never-loaded eBPF object, so it did nothing on any config — caught by the
    12b.6 gate). The agent publishes the faulted-service set on a `watch` channel
    (`onion::dns::DnsFaultState`); the responder returns NXDOMAIN for a targeted service before
    the service-map lookup, and clear/expiry republish the smaller set so it reverses like every
    other fault. `requires_ebpf()` is now `false` for `DnsNxdomain`; the dead `fault_dns_map`
    (Rust + eBPF C) is removed. Portable tests drive the resolver and the full UDP path.
  - [x] Reversal state (`FaultReversal`) rides on the registry's `FaultRule` (runtime-only,
    `#[serde(skip)]`), captured at apply time via `record_reversal`/`get_mut`.
  - [x] Portable tests cover cpu-quota computation, drain/kill/oom rejection, pause
    auto-resume (real child + `waitpid`), off-Linux resource rejection and reversal
    round-trip; Linux cgroup-effect + reversal tests are `#[ignore]` under
    `make test-linux` (`cgroup_*`, `RELIABURGER_CGROUP_TESTS=1`).
- [x] **Self-upgrade convergence and adoption** — wire scheduler cordon; calculate quorum
  headroom from live voters; derive roles/addresses server-side; prove gossip rejoin and
  finish Apple/rootless adoption. Make progress/book describe the implemented in-place or
  leadership-transfer sequence consistently (D20, UPG1-UPG2).
  - [x] Scheduler cordon (UPG1, verified): `meat::filter::apply_upgrade_cordon` IS called
    from the leader scheduling loop (`cluster/orchestrate.rs`) against a `ClusterStateCache`
    fed from Raft — a node in `Directed`/`Verifying` takes no new placements. (The audit's
    "no caller" was stale.)
  - [x] Live-voter quorum headroom (UPG1): `upgrade::orchestrator::quorum` now counts LIVE
    reachable voters — configured voters cross-referenced against gossip `Alive` via the
    stable `raft_id_from_name` hash — not just configured ones. With one voter already dead,
    upgrading another that would drop live voters below quorum is refused
    (`live_quorum_headroom_ok`, unit-tested for the 3-voter one-dead and 5-voter two-dead
    boundaries).
  - [x] Server-derived roles/addresses (UPG2): `upgrade::plan::derive_upgrade_nodes` rebuilds
    each node's role (from the Raft voter set + current leader) and address (from gossip
    membership) server-side and validates the client's start request against it. A spoofed
    address or a claim that crosses the leader boundary is rejected; a worker↔council relabel
    among non-leaders is corrected to the authoritative role. The plan the orchestrator walks
    never depends on the client's claim.
  - [x] Gossip-rejoin verification (UPG2): a node reaches `Healthy` only when it is on the
    target version, HTTP-healthy AND back in the gossip mesh (`Alive`). One that comes up
    HTTP-healthy but never rejoins gossip is held (and times out → pauses the run), not
    counted done. Unit-tested both ways.
  - [x] Apple Container adoption (UPG2): `grill::apple::AppleContainerGrill::adopt` re-tracks
    a running Apple workload via `container inspect` (the VM survives bun's exec; the workload
    is not a bun child pid, so pid-liveness doesn't apply). An adopted Apple instance survives
    an exec-in-place swap instead of restarting; a removed container declines adoption. Parse
    logic unit-tested; end-to-end adoption gated behind `make test-apple`. (Rootless is a
    spec-modifier over runc, not a separate adoption path — n/a.) The 12b acceptance gate ran
    `make test-apple` on real Apple silicon and caught that the inspect parser had guessed the
    wrong JSON shape: the CLI returns a single-element array with a top-level lowercase `status`
    and the IPv4 address as CIDR under `networks[0].ipv4Address`. Parser corrected against the
    real output (fixtures replaced with the captured schema), so adoption now works on hardware.
  - [x] D20 prose reconciled: `docs/design/agent-bun.md` §5.5 and the Phase 14 lines below now
    describe the shipped authenticated-HTTP / `GET /v1/version` poll / leader-last-in-place
    implementation (no leadership transfer, same order on rollback), and the book chapter 14
    matches.
- [x] **Documentation and book truth pass** — corrected to the shipped reality:
  userspace DNS in whitepaper §10 and `discovery-onion.md` (the in-kernel `onion_dns`
  program is now clearly a decision-log note, not live behaviour); Grill drives
  runc/Apple/process directly, not containerd (`agent-bun.md` overview, dependency
  table, decision-log note); the Raft log/RPC/snapshot use self-describing JSON, so
  book ch.2a/10/14 no longer teach the stale bincode-index rule; GPU qualified to the
  real `/dev/nvidia0`+`nvidia-smi` detector with effective `gpu_enabled` and refusal
  (OCI device passthrough still deferred) in whitepaper §2, `agent-bun.md` §5.4 and
  book ch.1; batch dispatch/completion marked a Phase 12 deliverable in `scheduler-meat.md`
  §5.2 and whitepaper §5.2; recovery §§8.2–8.3 split into shipped (learning period,
  backup/restore, disk-pressure step-down) vs the pre-seeded-candidate architecture
  proposal; process-workload deny-by-default made explicit in whitepaper Q6. Stale
  progress annotations reconciled (DNS responder spawned, WebSocket proxy wired, Grill
  runtime). The headline test count already reads as a suite taxonomy (#106). X6 stays
  in Phase 13 (D6/D14-D15/D18/D20-D22).
- [x] **Phase 12b acceptance gate** — ran the full matrix on the merged programme
  (main at #118). Results:
  - [x] Sandbox suites (macOS host): `cargo fmt --check` clean; `cargo clippy
    --all-targets -- -D warnings` clean (full `--all-features` clippy is Linux-only —
    `aya`; it runs in CI); portable default suite **2628 passed / 39 skipped**;
    `--no-default-features` **2609 passed**; doctests (no rustdoc examples); in-process
    cluster (`RELIABURGER_CLUSTER_TESTS=1`: `cluster_failover`, `cluster_gossip`,
    `council_self_healing`, `council_disaster_recovery`, `placement`, `chaos`) **20 passed**
    — 8+ node leader-failover and council recovery; in-process upgrade
    (`RELIABURGER_UPGRADE_TESTS=1`: `self_upgrade`, `self_upgrade_cluster`) **8 passed**
    — failed-voter/gossip-rejoin/adoption; wall-clock acceptance (`make test-slow`)
    **4 passed / 29 skipped**. The mTLS/auth, registry push-GC-peer-pull, GitOps
    partial-failure and Smoker effect/clear checks run inside the portable + cluster
    suites above.
  - [x] Full CI on the merge commit: **all 11 test jobs green** — coverage floor
    (`--fail-under-lines 78.65`) held, 10k-member scale, fast/large benchmarks,
    multi-node cluster, single-node + cluster upgrade, wall-clock, portable Linux/macOS,
    privileged Linux.
  - [x] Linux platform gate: CI's **privileged Linux** job *is* `sudo -E … make test-linux`
    (eBPF/runc/netns/Btrfs/Buildah/cgroups) — **green** on the merge commit. (A local Lima
    run was redundant and only tripped a guest `~/.cargo` permission quirk, not code.)
  - [x] Apple platform gate: `make test-apple` **2/2 passed on real Apple silicon**
    (`pinned_test_workload_runs_under_apple_container`,
    `adopt_re_tracks_a_running_apple_container`),
    run twice back-to-back (idempotent). The gate caught the inspect-schema bug fixed in
    #118 — a green unit test had hidden it.
  - Gate findings fixed before close: DnsNxdomain userspace no-op (#117) and Apple
    inspect-schema adoption failure (#118). This closes the entire Phase 12b review
    programme (12b.1–12b.6).

## Phase 13: Relish TUI

Implementation plan: [docs/plans/2026-07-06-plan-tui.md](plans/2026-07-06-plan-tui.md)

- [x] Full interactive terminal UI (ratatui + crossterm) — `src/relish/tui/`
  (app loop, input/keys, navigation, live event/log streams, terminal, theme/palette)
- [x] Dashboard, apps, nodes, jobs, events, logs, routes, search, help views
  (plus app-detail and snapshots) under `src/relish/tui/views/`
- [x] Book chapter 13: "A Room with a View"
- [x] All Phase 13 tests green (TUI unit tests in the portable suite)

## Phase 14: Self-Upgrade

> Detailed implementation plan: [2026-07-06-plan-self-upgrade.md](plans/2026-07-06-plan-self-upgrade.md)
> (12 commit-sized steps, decision log, type definitions, test inventory, gotchas checklist).

- [x] Rolling binary replacement (exec-in-place; workers → council → leader-last; state in Raft; `relish upgrade` command set). There is no leadership transfer: the leader upgrades itself last, in place (openraft 0.9 can't gracefully hand off against a live leader), and the returning process finishes the run via poll-first idempotency. Cordon, live-voter quorum headroom, server-derived identity and gossip-rejoin verification landed in 12b.6 (see above).
- [x] Dual-signature verification (embedded Ed25519 release key set + external operator key from node.toml; air-gapped `--binary` needs embedded only)
- [x] Automatic rollback on failure (crash-loop boot budget reverts the symlink; nodes refuse previously-reverted upgrade ids; leader pauses the run; `upgrade resume` retries with a fresh id)
- [x] Version retention and GC (keep newest `retain_versions`, rollback targets protected)
- [x] Workload adoption across the swap (ProcessGrill pidfile records + runc `state` adoption + Apple `container inspect` adoption; pid+start-time fingerprinting for pid-based runtimes, container liveness for Apple VMs; file-backed process logs). Rootless runc persists and restores its `slirp4netns` owner and host forward; `make test-rootless-runc` exercises a real non-root replacement. Apple adoption remains gated behind `make test-apple`.
- [x] Book chapter 14: "Changing the Tyres at Full Speed"
- [x] All Phase 14 tests green (unit tests in the portable suite; 5 single-node + 3 cluster real-binary integration tests are ignored by default and owned by the required `upgrade-node` and `upgrade-cluster` CI jobs). The jobs use nextest resource groups and no retries, so contention or convergence flakes remain visible.

The Phase 14 deferred seams are closed in 12b.6: scheduler cordoning is wired
against a live `ClusterStateCache`, quorum headroom counts live voters, node
roles/addresses are derived server-side from gossip + Raft, and post-upgrade
the cluster orchestrator requires observed gossip rejoin before marking a node
Healthy (see the "Self-upgrade convergence and adoption" theme under 12b.6).
The replacement process still commits its local boot marker after workload
verification alone; its separate gossip-rejoin deadline remains unfinished.

## Phase 15: Testing, Benchmarking & Diagnostics

> Detailed implementation plan: [2026-07-06-plan-chaos.md](plans/2026-07-06-plan-chaos.md)
> (20 numbered steps plus prerequisite tranches, test catalogue, data structures,
> acceptance runbook).
>
> The harness, diagnostic commands and registry upload streaming are implemented.
> Four catalogue cases still return unknown unconditionally, and the complete
> three-node acceptance run remains open. Implementation milestones below do not
> certify that every diagnostic verdict or cleanup edge case is correct.

- [x] Test harness audit and suite taxonomy: nextest local/CI profiles, JUnit, timeouts,
  zero retries, serial resources and no-tests-selected failures; portable, wall-clock,
  privileged Linux, cluster, upgrade and manual Apple suites have explicit ownership
- [x] Truthful gating and cleanup: platform code uses `#[cfg]`; provisioned tests use
  reasoned `#[ignore]` plus failing preflight; duplicate/demo/field-access tests removed;
  Relish has black-box exit/stdout/stderr coverage
- [x] Deterministic async harness: fixed portable waits replaced by channels, watches,
  notifications, barriers or bounded predicates; ProcessGrill and TCP reporting own and
  cancel their child tasks; listeners use ephemeral ports and isolated temporary paths
- [x] Benchmark and coverage split: deterministic seeded gossip simulation shared by
  Criterion and scale acceptance; P2P correctness no longer has a host-speed threshold;
  combined default/no-default LLVM coverage publishes LCOV and HTML artefacts
- [x] Required CI and release validation: portable Linux/macOS, privileged Linux, cluster,
  node/cluster upgrade, coverage and three benchmark/scale jobs; release publication waits
  for the same reusable workflow. Direct Apple Container is now deferred beyond 0.1.0; its manual tests remain development-only
- [x] `relish test` ordinary catalogue: 39 cases across all 13 planned groups,
  bounded parallel execution, filtering, human/JSON/YAML reports and four
  acceptance profiles. Report schema v2 separates `Pass`, `Fail`, typed
  `Skipped` and `Unknown`; timeouts, panics and untyped runtime skips are
  unknown, cleanup has an independent verdict, and full profiles reject
  required skips or missing observed evidence
- [x] `relish test --chaos`: five serial live scenarios for leader loss,
  worker rescheduling, minority partition, bounded node pressure and node death
  during deploy. Pure preflight refuses fewer than three nodes, protected
  server policy, missing operation grants, missing container/node-failure
  evidence and non-interactive runs without `--yes`; there is no client-side
  production override. The runner and case task share exact fault-id ownership,
  refresh capability evidence before each destructive case and reverse only
  their own faults after pass, failure, timeout or panic
- [x] `relish bench` (scheduler, service data plane, network, deploy, state reconstruction benchmarks)
  - [x] Versioned report and comparison contract: topology and per-node
    build/runtime/kernel fingerprints, metric-method parameters, strict schema
    parsing, direction-aware >10% changes, explicit missing metrics and
    compatibility refusal. Different version/Git SHA is comparable; noisy
    hosted runs are marked informational.
  - [x] Public-API benchmark suites and CLI orchestration: one durable lease
    per suite (and additional bounded leases for capacity), 60/300-second
    inherited deadlines, panic/timeout-safe fault and resource cleanup, and
    schema-v2 failed-suite evidence. Discovery runs `nslookup` inside a source
    workload; throughput runs `wget` from there through `.internal` DNS and
    the service VIP. Leader reconstruction requires `--disruptive --yes`;
    capacity requires `--capacity --yes`. The image metric honestly records
    the current cache state of the pinned multi-architecture OCI daemonset;
    deterministic cold-cache Pickle replication still needs leased image
    deletion/eviction ownership.
    Bun independently marks and authorises every capacity apply: durable lease,
    Admin, `saturate_capacity` and protected-cluster policy all have to agree.
- [x] `relish wtf` (automated cluster health diagnosis)
  - [x] Schema-v1 pure diagnosis engine with separate critical, warning,
    unknown and OK outcomes; timestamped crashloop detection, deploy/log
    correlation and honest evidence requirements for all catalogue checks
  - [x] Authenticated schema-v1 local telemetry: configured storage-domain
    capacity, bounded cgroup throttled-time deltas and public node-certificate
    metadata without paths, certificate bodies or key material
  - [x] Authenticated cross-node collection with ten-second bounds, desired-app
    and service evidence, rendering, app scope, watch mode and 0/1/2 exit contract
- [x] `relish path` (end-to-end connectivity debugging): source-node
  discovery; fixed bounded DNS/TCP probes inside the source workload; live
  userspace service state and attached Linux eBPF backend/firewall reads;
  explicit observed/inferred/unavailable evidence; strict schema-v1
  human/JSON/YAML output and 0/1/2 exit contract. External probes fail closed
  behind Admin, an independent server permission, protected-cluster policy and
  an exact `host:port` allowlist
- [x] Book chapter 15 test-harness and benchmarking foundations: Rust attributes, ignored
  versus compiled-out tests, deterministic async tests, nextest, benchmarks and coverage
- [x] Complete chapter 15 with the built-in diagnostics commands (`relish test`, `wtf`,
  `path`), including safety, evidence provenance and acceptance limitations
- [ ] All Phase 15 tests green (current backlog: V01)

## Phase 15a: Current-State Hardening

> Prioritised follow-up plan:
> [2026-07-18-plan-codebase-review-follow-up.md](plans/2026-07-18-plan-codebase-review-follow-up.md)
>
> This is a hardening gate before the unfinished Phase 15 diagnostic commands,
> not a declaration that Phase 15 is complete. The source review is
> [2026-07-17-review-codebase-current-state.md](plans/2026-07-17-review-codebase-current-state.md).
>
> **`M`/`O` IDs in this section belong to the 2026-07-17 review, not the 2026-07-19 one in
> Phase 15b.** The 15b review re-verified this review's four security P1s as fixed by H1–H7
> (SEC-1 empty-token non-loopback bind, SEC-2 mTLS opt-in, SEC-3 egress fail-open, NET-1 DNS
> unreachable) plus the non-executable first-run path. Its dependency-advisory P1 (H0) was not
> re-scanned there.

### High-value / must fix

- [x] Patch, assess and continuously detect known dependency advisories (H0) — all compatible fixes applied, including newer RustSec findings missed by GitHub; `make audit` denies new vulnerability/maintenance warnings in change and release CI plus a weekly scan. The August review removed the patched `lru`/`anyhow` exceptions. Five named exceptions remain in `.cargo/audit.toml`, including lock-only `rkyv`; their next enforced review is 18 November 2026. September dependency updates and the current hosted audit pass.
- [x] Contain the API authentication bootstrap window (H1 / SEC-1) — Bun now owns one token store in standalone and cluster modes; an empty store permits only IP-literal loopback. Five real-binary startup tests cover standalone/clustered bootstrap, and the 2,633-test portable suite passes.
- [x] Fail closed when a declared egress policy can't be enforced (H2 / SEC-3) — four cgroup hooks plus per-workload enforcement are proven before start, live loss fences affected workloads, and independent expiring capability evidence keeps placement fail closed. Merged in PR #122.
- [x] Make `.internal` DNS reachable, supervised and schedulable (H3 / NET-1) — rootful runc derives and mounts a veth-gateway resolver, Bun pre-binds supervised UDP/TCP with `IP_FREEBIND`, and independent rolling-safe readiness leases gate placement. A checked-in two-workload rootful-runc/netns acceptance test resolves `redis.internal`; it also forced fixes for colliding long veth names and destructive repeated rootfs extraction. Unsupported runtimes and addresses fail before creation.
- [x] Make generated clusters use mTLS by default (H4 / SEC-2) — normal `relish init` enables mTLS and writes its security bootstrap with the `0600` mode Bun requires (the init-to-Bun acceptance caught the old self-rejecting `0644` output); explicit init/Lima development plaintext configs and Bun startup warn. Peer API calls now present the node certificate and check the live CRL. A three-node real-runtime acceptance proves mTLS Raft, reporting and peer API traffic, rebased on merged H3.
- [x] Isolate writable runc root filesystems per workload (H6) — rootful runc now mounts one private OverlayFS upper/work pair per writable image instance over the shared content-addressed generation. Real privileged tests prove concurrent replicas can't observe each other's writes, same-instance restart and Bun adoption preserve only that instance's files, and natural exit, kill and failed create leave no visible mounts. Writable rootless image workloads fail closed; read-only image roots remain shareable. The adoption proof also fixed `runc exec` placing `--` after the container id, where runc treated it as the command.
- [x] Replace the broken published first-run sequence with an executable one
  (H5 / DOC-1) — portable ProcessGrill and secure one-voter mTLS sequences now
  execute as real Bun/Relish black-box tests on ephemeral endpoints. Init
  creates its output directory, the generated BusyBox app has an explicit
  working command/port, remote endpoints require HTTPS, and clap/doc guards
  reject the old `apply -f`, port-9443 join and missing-`--node-id` shapes.
- [x] Add authenticated post-bootstrap join-token issuance (H7) — dedicated
  Admin-only `relish join-token create --ttl 15m`, bounded `1s..=1h`; only the
  hash and expiry enter Raft and plaintext is returned once after commit. A
  real Bun/Relish test enrols two CSR-bearing nodes, proves Deployer, reuse and
  expiry refusal, starts both joiners and observes a three-voter mTLS council;
  a three-member Raft test proves follower refusal and post-failover issuance.
- [x] Rerun the source review matrix on hosted CI: Linux/macOS portable suites, rootful/rootless Linux, cluster, upgrades, wall-clock acceptance, examples and dependency audit. Signed-candidate and full catalogue qualification remain separate open gates.

### Medium-value

- [x] Live subsystem readiness/capability evidence and supervised failure (M1)
  — critical owners publish `Starting`, `Ready`, `Degraded` and `Stopped`
  evidence with failure times; authenticated readiness/capability APIs and an
  independently expiring report lease fence scheduling fail closed. Only the
  reconstructible security refresher retries, within explicit budgets.
- [x] Executable examples, coherent cross-platform lint and advisory scanning (M2)
  — all 21 repository configs pass the real Relish dry-run path, failures retain
  their diagnostics, and hosted Linux owns the check. Aya code now compiles only
  for Linux plus `ebpf`, so macOS enforces the advertised all-feature Clippy
  contract. The existing pinned RustSec change/release/weekly gate passes.
- [x] Peer-reachable clustered registry defaults (M3) — cluster mode derives
  the safe standalone loopback default to the gossip-advertised IP and rejects
  an explicit mismatched interface. Remote reads and writes require authentication from
  first boot and fail closed if the service token is absent; authenticated
  capabilities expose listener, TLS/P2P, membership and achieved redundancy
  evidence for future `wtf` checks.
- [x] Configured workload trust domain (M4) — `[cluster].name` now flows from
  generated config into immutable agent/API state and every app, job, JWT and
  build-signer SPIFFE identity. Bun validates malformed domains at startup; a
  non-default `payments.prod` acceptance issues and verifies a real leaf.
- [x] Rootless proxy adoption across Bun replacement (M5) — schema-v2 instance records own the slirp API socket, port mapping and PID/start-time fingerprint. Adoption reclaims a surviving proxy or recreates it before returning success; a real non-root runc test proves the host port before and after replacement.
- [x] Real deployment operation state (M6) — accepted SSE events carry stable IDs; live phase/current-target/start evidence and newest-first bounded outcomes are exposed by authenticated APIs. Overlapping same-target workers are refused, disconnected clients don't erase outcomes and missing terminal evidence records `unknown`.
- [x] Explicit v1 ingress/TLS contract (M7) — `cluster`, `explicit`, and
  deliberate plain HTTP are the complete v1 set; `auto`/`acme` remain rejected.
  The whitepaper, component designs, book and examples now say so. Kubernetes
  imports translate a TLS stanza to `cluster` and warn that the referenced
  Secret wasn't imported. TLS routes redirect every plaintext path, including
  the unused ACME challenge prefix. The audit also records certificate renewal,
  hot reload and expiry evidence as Phase 15 prerequisites rather than claiming
  they already work.
- [x] Implement Phase 15 prerequisite infrastructure and catalogue commands (M8). Four case fixtures and the full-catalogue acceptance gate remain open.
  - [x] Result/evidence/profile contracts, inherited absolute deadlines,
    panic-safe runner ownership, verified cleanup reporting and typed
    server-owned `[testing]` safety policy. Unknown clusters fail protected;
    the client-side production override was removed
  - [x] Fresh expiring capability/evidence API — authenticated schema-v3 local
    snapshots separate available, unavailable and unknown facts with 15-second
    expiry. Authenticated cluster fan-out uses the service identity, one shared
    deadline and an explicit evidence/unknown result for every expected node.
    Reports include build/runtime/topology fingerprints and server policy
  - [x] App/namespace resource leases and hermetic OCI workload. Wider resource ownership remains open; the shipped lease enum covers apps and namespaces.
    - [x] Durable, bounded app/namespace leases: authenticated
      creation/renewal/release; lease-only `rbtest-*` namespaces; standalone
      fsync persistence and Raft ownership; restart/leader-safe reapers;
      bounded cleanup steps.
    - [x] Runner lease adoption: one sufficient-lifetime lease per case;
      leased applies and confirmed release after pass, failure, panic or
      timeout; server reaping after client death.
    - [x] Pinned multi-architecture OCI workload: BusyBox 1.37.0's immutable
      OCI index is used by every container case and accepted on real runc
      (`linux/amd64`) and Apple Container (`linux/arm64`). ProcessGrill keeps
      the installed Bun helper as a separate profile.
  - [x] Ordinary 39-case catalogue and `relish test`
  - [x] Authenticated real node drain/kill: Admin plus the server-owned
    `alter_node_state` grant and explicit acknowledgement; target-side repeated
    authorisation; scheduler withdrawal for drain; shared gossip/Raft/reporting
    quiesce for kill; TTL and privileged manual reversal. A three-node acceptance
    observes a follower fail and rejoin, and rejects a second voter failure.
  - [x] Node-scoped pressure before the chaos catalogue: Admin plus the
    independent `saturate_capacity` grant and explicit acknowledgement;
    zero-by-default server CPU/memory ceilings capped at 90%; target-side
    authorisation; one dedicated rootful-Linux cgroup/helper outside Bun;
    total-node CPU quota and memory-usage target; clear/TTL/shutdown,
    parent-death and startup-sweep cleanup. A privileged cgroup-v2 acceptance
    proves the effect, isolation and restart cleanup. Unsupported platforms
    publish unavailable evidence instead of claiming a green primitive.
  - [x] Fault authorisation and structured audit: ordinary workload injection
    requires a Deployer-or-higher credential, the server-owned
    `inject_workload_faults` grant and explicit acknowledgement. Node/council
    and pressure paths retain their stricter matching operations. Reversal
    requires the same role and server grant but never destructive
    acknowledgement. Bun ignores client-supplied audit identity and emits
    additive `action`, stable authenticated `principal` and machine-readable
    `details` fields for every successful inject and clear path. Council
    partitions take the same `/v1/fault` path.
  - [x] Guarded five-scenario chaos catalogue: serial execution on the
    digest-pinned runc/Apple workload; server policy and consent preflight;
    missing node kill/pressure prerequisites refuse rather than green-skip;
    fresh per-case capability snapshots; and runner-owned exact-id reversal
    after pass, failure, timeout or panic. Council partitions are ordinary
    node faults on `POST /v1/fault`, so catalogue cleanup reverses their exact
    ids; the separate `/v1/chaos/partition` and blanket `/v1/chaos/heal`
    endpoints are gone.
  - [x] Fingerprinted benchmark report and comparison contract: exact schema,
    topology and per-node build/runtime/kernel evidence, direction-aware
    thresholds, metric-method compatibility and informational hosted results.
  - [x] Seven-suite benchmark runner and CLI: real source-workload DNS/service
    paths, strict failed-versus-skipped outcomes, fresh preflight evidence,
    explicit destructive consent and unconditional server-owned cleanup.
  - [x] Authenticated source-workload path probe and CLI (`relish path`): fixed positional-argument
    DNS/TCP probes run off the agent command loop; live userspace and attached
    kernel state are distinguished from inference; incomplete evidence is
    `Unknown`; external destinations use exact server-owned authorisation.
  - [x] Phase 15 command documentation across both READMEs, the Relish/Bun
    designs, chapter 15 and the implementation plan. Real three-node catalogue,
    chaos, benchmark and path acceptance remains the unchecked phase gate.

### Optional

- [ ] Ownership-based module splits (O1) (current backlog: H05)
- [x] Library-backed DNS parsing and evaluated retention of duration grammars (O2; H06)
- [x] Public API doctests (O3) — H07 adds and executes configuration/client endpoint examples in both Linux feature configurations and on macOS.
- [ ] Production TC DNS fast-path evaluation if profiling justifies it (O4) (current backlog: F09)
- [x] Audit shipped/planned/experimental claims (O5, Phase 16 Sections D–G). Mechanical drift prevention and new findings remain ongoing work.

## Phase 15b: Code-Logic Review Hardening

> Source review: [2026-07-19-codebase-review-fable.md](plans/2026-07-19-codebase-review-fable.md)
> (16 Critical, 29 Medium, 20 Optional). Written independently of the 2026-07-17 posture
> review and against `main` *after* the Phase 15a H1–H7 PRs landed, so the two barely
> overlap: 15a is a **posture** review (fail-open defaults, docs drift, dependency shape),
> 15b is a **code-logic** review (what the code actually does on the exploited path).
>
> Findings were worked as six stacked PRs, one commit per finding. Every theme box below is
> checked only when its PR merged with `make ci` green.

### Merged (PRs #133–#140)

- [x] **Phase A — stop the bleeding** (#133): `C1` relish trusted public CAs while disabling
  hostname checks (admin-bearer MITM) — now `tls_built_in_root_certs(false)` on the
  `--ca-cert` path; `C16`+`M29` upgrade GC could delete the running binary and
  `max_boot_attempts = 0` made an upgrade uncommittable; `C2` auth middleware failed **open**
  on an empty token store, reachable by revoking the last token — last-Admin-revoke floor
  added (broader bootstrap-flag hardening deferred); `C11` GitOps signature verification
  failed open under `require_signed_commits`; `C10` a GitOps parse error or failed `ls-tree`
  wiped cluster desired state and reported success; `C4` disaster recovery restored **empty**
  state when no snapshot existed (silent total data loss) — now an error.
- [x] **Phase B — data-loss and quota holes** (#134): `C9` Pickle GC deleted a blob
  re-referenced between Raft approval and physical delete (reference set now re-checked
  immediately before each delete); `C12` namespace quota was bypassed once apps converged +
  `C13` GitOps skipped the validation manual apply enforces; `C15` managed-volume paths were
  unvalidated (host path-traversal write as root); `C14` process-workload mount isolation was
  admission-gated but never implemented — host workloads requesting it are now refused (real
  mount-namespace isolation deferred).
- [x] **Phase C — authenticate the cluster plane** (#135, #136): `C7` Raft/reporting/gossip
  ran plaintext and unauthenticated when identity/master-key was unset — `--cluster` on a
  routable address now fails closed; `C8` an untrusted gossip leader-hint term ratcheted the
  reporting epoch into a permanent cluster-wide wedge — terms are bounded against the local
  Raft window; `C6` state reports were trusted by self-declared `node_id` (one peer poisoned
  the whole cluster view) — report identity is now bound to the authenticated TLS client
  cert; `C5` disaster recovery had no protocol-level fence, so a partition split-brained a
  live cluster — implemented as a transport-layer `RaftRpcEnvelope` stamped with the sender's
  recovery epoch, with different-epoch RPCs dropped on accept (fencing at the RPC boundary
  rather than inside openraft's `Vote` type).
- [x] **Phase D — deploy/runtime correctness** (#137): `M14` the placement reconciler wedged
  forever on a hung deploy (terminal-event wait now timed out); `M7` rolling redeploy capped
  the health wait at 5s and ignored the configured `health_timeout`; `M22` ProcessGrill
  silently ignored cgroup CPU/memory limits (now refused at admission); `M23` adopted-process
  polling had no start-time recheck (pid reuse → wrong-process kills); `M24` `deploy_app`
  leaked ports and orphaned instances on mid-loop failure (allocation is now transactional);
  `M25` scheduler pass-cache and daemon-set convergence defects; `M26` the autoscaler's metric
  lookup was a namespace-blind substring match.
- [x] **Phase E — chaos, registry, supply chain, upgrade** (#138): `M1` Smoker faults were
  unreliable and could leave a node damaged — `fault partition` was a silent no-op, `chaos
  heal` cleared faults without reversing them, TTL expiry never healed the legacy partition,
  partial cgroup application leaked limits, and safety rails were skipped with no cluster
  handle; `M2` Pickle replication never authenticated (images stayed at one copy in any
  cluster with tokens); `M3` the `cache/` namespace wasn't reserved on push (poisoning bypassed
  `require_signatures`); `M10` storage quota was bypassable via chunked/bare-blob uploads;
  `M11` whole-blob buffering enabled OOM (peer pulls now stream); `M4` join tokens weren't
  bound to a node id, so a token holder could mint a cert impersonating any node —
  `--node-id` is now mandatory on `join-token create` and `init` mints no bootstrap token;
  `M5` the single-node network upgrade path silently downgraded dual-signature to
  embedded-only; `M6` leader self-upgrade bypassed the live-quorum gate.
- [x] **Phase F — observability, ingress, CLI** (#139): `M8` ingress `tls = "cluster"` served
  a self-signed `localhost` cert — a per-SNI `IngressCertResolver` now issues from the cluster
  Ingress CA (which finally gave the previously-dead `issue_ingress_cert` a caller); `M9` the
  WebSocket upgrade path bypassed `X-Forwarded-*` sanitisation, letting a client spoof the
  trusted client IP; `M12` gossip incarnation reset to 1 on restart, leaving a restarted node
  stuck Suspect/Dead (refute now seeds from `max(seen) + 1`); `M13` a relayed ACK recorded the
  probed node at the *relay's* address, falsely evicting a healthy node; `M15` reconstruction
  counted stale reports as live actual state and `M16` its coverage shortcut could skip
  learning entirely; `M17` rollup idempotency was in-memory only (restart double-counted
  cluster sums); `M18` alert-webhook failure logs leaked the full Slack URL; `M19` the
  "memory-capped" bounded log SQL materialised the whole archive into a MemTable first (now
  streams over a `ListingTable`); `M21` the CLI couldn't manage non-default namespaces and
  `history`/build-upload bypassed auth; `M27` GitOps never advanced local HEAD (Raft churn
  every 30s), skipped replay protection without a delivery ID and leaked credential-bearing
  URLs into Raft; `M28` `relish import` split on the substring `---` and skipped `#`-prefixed
  documents, so Helm output imported nothing and exited 0.
- [x] **Optional hardening, first tranche** (#140): `O15` bootstrap secret-file permission
  check ignored ownership; `O18` out-of-range reserved ports counted toward allocator
  exhaustion; `O16` `validate_chain` didn't assert the leaf is `CA:FALSE`; `O8` peer poll and
  reporting send relied on TCP defaults with no timeout; `O4` `prepare_rollback` exec'd a
  stored binary with no signature re-check; `O17` `mbps`/`kbps` meant MiB/s (~8.4× too loose)
  and fault TTL had no upper bound; `O13` `relish logs-search` ran raw operator SQL with no
  memory guard; `O12` retention pruned Parquet by file mtime instead of the data's own newest
  timestamp; `O14` dev clusters shared VMs regardless of `--name` and `dev destroy` deleted
  the shared build VM; `O19` `relish top` claimed "live resource usage" and printed none;
  `O5` (partial) consumed join tokens were never pruned.

Test counts at #140: portable nextest 2,809, no-default 2,790, doctests green; `cargo fmt
--check` and default-feature clippy `-D warnings` clean.

**Supersedes:** the Phase 4/10 `C5` "nothing is enforced" caveat is now historical for the
cluster plane (Phase C closed the plaintext/unauthenticated paths). The Phase 8 Smoker caveat
is *partly* addressed — Phase E made faults reversible and safety-checked; whether every
advertised resource fault has a measurable effect is still owned by the 12b.6 smoker-effects
work, not by `M1`.

### Critical

- [x] **`C3` — token namespace/app scope was enforced only on mutations; every per-app read
  ignored it.** Raised in the review but **omitted from the review's own recommended
  implementation order**, so Phases A–F worked around it — the last unfixed Critical. A token
  scoped to `team-a` could read `team-b`'s logs, plaintext env, status, metrics, snapshots and
  deploy history; no admin mistake required. Eleven per-app read handlers now take `auth` and
  call `authorize_scoped`: status, logs, log entries, cross-node log query, WS log stream,
  metrics, snapshot list, deploy history, and the three UI routes (app detail, env, instances
  fragment). `/v1/logs/sql` takes no app to scope against and arbitrary SQL can't be rewritten
  into a tenant-filtered query, so a scoped token is refused outright
  (`sesame::auth::require_unscoped`) and pointed at `/v1/logs/query/{app}/{namespace}`;
  unscoped tokens keep it. `/v1/deploys/history/{app}` filtered on the bare app name, which
  since DEP1 spans namespaces — it now takes `?namespace=` and filters on both (`relish
  history --namespace`, threaded through the client and the TUI, whose history cache is keyed
  `namespace/app`).
  - [x] Drift guard: `every_per_app_route_checks_the_callers_scope` scans the router's own
    source and fails if a route pattern naming `{app}` dispatches to a handler that never
    calls `authorize_scoped` — the convention that failed here is now a test. A companion
    test pins `/v1/logs/sql` to `require_unscoped`.
  - [x] Runtime tests: a `team-a` token gets 403 on all ten per-app read routes for `team-b`
    and non-403 for its own; the WS stream is refused through a real handshake against an
    ephemeral listener (a hand-built request can't reach the handler — `WebSocketUpgrade`
    rejects it with 426 first); raw log SQL is refused scoped and allowed unscoped; deploy
    history no longer returns two namespaces' entries.

### Open — deferred from merged PRs

- [x] `M7` residual — `max_surge`/`max_unavailable` parsed, validated and changed nothing: the
  rolling path started *every* replacement then retired *every* old instance, so a 3-replica
  app peaked at 6 containers however `max_surge` was set (the default asks for 4). Worse than
  unsupported, since an operator who set it because the node lacked the headroom had been told
  their constraint was honoured. A pure `plan_rolling_step` now drives the rollout: `max_surge`
  gates starting (how far above target `total` may go), `max_unavailable` gates retiring (how
  far below `serving` may fall). Retirement and backend publication were split out of
  `finalise_rolling_deploy` into per-instance `DeployOp`s so they interleave with replacement.
  - [x] Both bounds at zero is unsatisfiable — no legal move in either direction. The planner
    returns a distinct `Stuck` and `DeployConfig::validate` rejects the pair at apply time,
    instead of a rollout that looks live and never progresses
  - [x] Interleaving hazard: the command loop gets a turn between retirements, and a
    deliberately stopped instance still in `supervisor.instances` is indistinguishable from a
    crashed one, so the restart driver would resurrect it. `Supervisor::retire_instance` drops
    it in the same turn that stops it
  - [x] Tests: the planner's envelope (peak total, minimum serving) rather than step
    sequences, plus a proptest over every validation-permitted combination; agent-level tests
    replay the grill call log to count live containers (3 replicas × `max_surge = 1` peaks at
    4, and at 6 against the old code — verified). Book chapter 7's claim that surge was "real
    rather than aspirational" was itself the drift, and is corrected
- [x] `M20` — alert evaluation lacked per-value freshness and collapsed metrics by name across
  labels. `gather_latest_values` now keeps the newest reading per `(metric_name, labels)`
  series and hands them to a pure `collapse_series`, which (a) applies an explicit
  `MAX_VALUE_AGE_SECS` freshness bound separate from the query window — the window says how
  far back to look, not how stale an answer may be; (b) computes derived percentages
  **within** a label set, so `node_memory_usage_percent` can't divide one series' `used` by
  another's `total`; and (c) breaks collapse ties on the label string, so the result never
  depends on row order. **Residual, deliberately:** the evaluator still takes one value per
  metric name. Giving each labelled series its own alert state changes that contract (alert
  keying becomes rule+series) and is not attempted here — recorded rather than silently
  skipped
- [x] `M27` residual — a `[gitops] repo` of `https://token@…` put the token in `git clone`'s
  argv, where `/proc/<pid>/cmdline` is world-readable to any local user, and git then wrote the
  whole URL into the clone's `.git/config` where it outlived the process entirely. `split_credentials`
  keeps the username in the URL (git needs it; it isn't the secret) and moves the password into
  the child's environment, read by a `credential.helper` that names the variable rather than
  containing the value — so nothing secret reaches argv and no temp askpass file needs
  creating or cleaning up. `fetch` supplies it the same way, since the stored remote is now
  credential-free. **Regression caught while writing it:** `reused_clone_matches` compares the
  stored remote against the configured URL, so storing a sanitised remote would have
  re-cloned the repository on every startup — both sides are now sanitised before comparison,
  which also makes a pre-change clone reusable rather than forcing one re-clone on upgrade
- [x] `M28` residual — `relish export` silently dropped ten field families, so an export that
  looked complete produced a materially different workload. Now translated: `namespace` (every
  resource landed in `default`, collapsing two namespaces' same-named apps into one — the DEP1
  collision reintroduced on the way *out*, and the Service would never have found its pods),
  `command` (the pod ran its image's default entrypoint instead), and `memory`/`cpu`/`gpu` as
  `resources` — `ResourceRange`'s request/limit pair maps onto Kubernetes' two fields exactly,
  so that one is lossless. The rest (`health`, `volumes`, `init`, `config_file`, `placement`)
  are reported in a new `dropped` list, kept **separate from `unsupported`**: "Kubernetes can't
  express this" and "we haven't written this bit yet" mean different things to whoever reads
  the report, and conflating them sends an operator hunting for a workaround they don't need.
  Silence was the actual bug
- [x] `O5` residual — the mustard dissemination heap accepted enqueues without limit while
  draining at most `MAX_PIGGYBACK_UPDATES` per outgoing message, so churn about the same nodes
  could outrun it forever. Compaction coalesces to the newest incarnation per node (an older
  update is exactly what the newer one supersedes), which bounds the queue at the number of
  distinct nodes — which cluster membership bounds in turn. The trigger scales with
  `cluster_size` above a small-cluster floor, because **one queued update per member is the
  normal shape of a first dissemination**: the first version of this used a flat 4096 cap and
  truncated the excess, which silently dropped ~6,000 members on a 10,000-member cluster.
  `tests/gossip_10k.rs` caught it in CI, not locally, because `make test` doesn't run the
  ignored scale suite. A `HARD_CAP` of 65,536 remains as a backstop against updates about
  non-members, and it logs what it drops rather than trimming in silence.
  `crl.entries` gained `expires_at` (`#[serde(default)]`, so old state loads and unknown
  expiries are never pruned) and `RevokeCertificate` drops entries whose certificates have
  since expired — an expired cert fails validation with or without a CRL entry. The prune
  clocks off the incoming entry's own `revoked_at`, never `SystemTime::now()`: Raft apply must
  be deterministic or replicas prune different sets and diverge (pinned by a test). The
  reporting aggregator maps were already bounded by 12b.2's CP5 eviction; the membership table
  only admits `Alive` nodes and reaps Dead/Left, so it's bounded by cluster size
- [x] `O17` residual — `CpuStress --cores` was parsed and thrown away while the quota maths
  assumed exactly one core, so on a 4-core node "80% stress" took 95% of the workload's CPU.
  `cpu_stress_quota` now scales by cores, and `None` means "every core the workload has",
  derived from its current `cpu.max` via a new pure `baseline_cores` (unlimited → host cores;
  a quota → quota/period; unparseable → 1, since under-reading makes the fault weaker than
  asked, which is the wrong way to be wrong)
- [x] `TODO(Phase 15)` — Smoker's service-to-service `Partition` no longer reports success
  without an eBPF data path. Bun resolves every running source-app instance to its cgroup-v2
  id server-side, installs one exact source-cgroup/VIP/port key per instance, rolls partial
  writes back, records those keys for clear/expiry/shutdown, and propagates map failures. A
  root-only Linux acceptance test proves the key blocks only the selected source cgroup and
  that deleting it restores the connection. The old Raft/gossip transport partition now has
  a distinct `CouncilPartition` variant and a real three-node quorum-rail test, so it can no
  longer borrow the service fault's safety semantics. Delay and bandwidth commands now
  refuse honestly until a TC packet hook exists; the connect hook cannot implement either.
- [x] Pickle push-side request-body streaming: bounded writers, streamed temporary files and digest verification, with bounded memory under concurrent large uploads.

### Open — Optional list

- [x] `O1` anonymous Pickle reads were cluster-wide on a non-loopback bind — any client that
  could route a packet could enumerate and pull every image, including `cache/` copies of
  private upstreams pulled with the operator's credentials. `PickleState.require_read_auth`
  (set from the bind address; only an IP literal that *is* loopback keeps reads open, so
  `0.0.0.0` and hostnames count as routable) gates every GET/HEAD through one choke point in
  `dispatch_v2` plus the `/v2/` version probe. The bar is any valid token — pulling is what a
  ReadOnly token is for — not the Deployer that writes require. Loopback is unchanged
- [x] `O2` Pickle build-context URLs hardcoded `http://` and `buildah push` hardcoded
  `--tls-verify=false` — against a TLS registry a delegated build failed outright, and where
  plaintext did reach a listener the build context (the caller's source tree) crossed the
  network in clear while the push never checked the certificate it was pushing to. All four
  context-URL builders take the registry's scheme and `execute_build` takes
  `registry_over_tls`; `ApiState.registry_scheme` is server-owned beside `registry_port`, the
  CLI derives it from its own client, and `bun` derives both once beside `cluster_http` so
  they can't drift. The runner's context download also moved off a bare `reqwest::get` onto
  the CA-trusting cluster client
- [x] `O3` upgrade binary fetch/push was plaintext `http://` — integrity was never at stake
  (sha256 gate + embedded release signature run on every path), but a plaintext fetch simply
  fails against a TLS-only registry. `UpgradeManager` gained a `cluster_http` (late-injected
  via `with_cluster_http`, since the manager is built before the node's identity is known) and
  `relish upgrade`'s `push_blob` now goes through the authenticated `BunClient`, which also
  gets it past O1's read gate and the existing write gate. Tested through the real fetch seam
  against a closed port, asserting the attempted URL's scheme
- [x] `O6` Raft RPC pre-allocated an attacker-controlled ≤64 MiB buffer per connection with no
  connection cap — the frame reader now grows in 64 KiB chunks with the bytes that actually
  arrive (four bytes no longer buy 64 MiB), and the accept loop holds an owned semaphore
  permit per connection, acquired *before* `accept()` so excess peers wait in the kernel
  backlog. Tests: multi-chunk reassembly, a declared-maximum frame with a ten-byte body, and
  a listener capped at one connection serving three sequential peers (verified to fail when
  the permit is leaked on purpose)
- [x] `O7` security-relevant reads were served from local follower state — **partially
  addressed, deliberately.** New `CouncilNode::security_state_linearizable()` proves the read
  is current via `ensure_linearizable()` and errors rather than answering when it can't, and
  workload-CSR signing (where the read *is* the decision: it yields a valid certificate) now
  uses it. The two remaining local readers stay local **by design**, documented at the
  accessor: the CRL refresh ticker is advisory and already 5s stale by construction, and the
  join-token pre-check is a fast-fail whose authoritative decision is the
  `ConsumeJoinTokenForIssue` Raft write. A follower cannot do a linearizable read at all, so
  forcing one there would break validation rather than tighten it
- [x] `O9` SWIM `refute()` didn't apply the fresh Alive to the local record (a refuting node
  told the cluster it was alive while its own membership table — what the scheduler and
  council reads go through — still held it Suspect/Dead), and `Left` won at any incarnation,
  making it the one claim a node could not refute. Gossip HMAC stops forgery but not replay,
  so a captured departure datagram evicted a rejoined node until the 60s reap. `Left` now
  loses to a strictly higher incarnation — safe because only a node mints its own incarnation
  numbers — and `leave()` sets a flag so a node's own departure echoing back off a peer isn't
  mistaken for a replay to refute. The `resolve_conflict` proptest invariant moved from "Left
  is terminal" to "Left wins at an equal or higher incarnation"
- [x] `O10` `relish fmt` wrote non-atomically (`fs::write` truncates first, so an interrupted
  write left a half-file where a valid config used to be — one the node reads at startup) and
  deleted comments in silence. It now writes to a sibling temp file and renames (same
  filesystem, so the rename is atomic) and warns once when the input had comments. `compile`
  merged with `extend`, so a same-named app from an earlier file was silently replaced — it
  now reports each overwrite as a warning, while two apps of the same name in *different*
  namespaces stay legal (DEP1). `_defaults.toml` parse errors were swallowed by `.ok()?`,
  making a typo indistinguishable from "no defaults"; they surface as warnings now
- [x] `O11` (IPv4 forwarding) DNS forwarded from a hardcoded `0.0.0.0:0`, so an IPv6 upstream
  was unreachable and every non-`.internal` query SERVFAILed — a failure that reads as "DNS is
  broken" rather than "your upstream is v6". The forward socket now binds the upstream's
  address family, proven against real v4 and v6 loopback resolvers.
  **Closed by C43:** bare names now require a runtime-owned source namespace on both
  transports. Unknown/ambiguous sources fail explicitly; qualified names remain available.
  Network admission/enforcement remains separate from DNS naming.
- [x] `O20` stale/misleading docs and dead code sweep — one genuine bug, one leak, two
  honesty fixes, one deletion, one already-fine:
  - [x] **Bug:** the gossip datagram was bincode-deserialised before its HMAC was checked (it
    has to be — the tag is *inside* the message), and the deserialiser was unbounded, so a
    length prefix in a 1500-byte datagram could claim gigabytes and bincode would try to
    reserve it. The same "a number is a promise, not a fact" mistake as `O6`, one layer down.
    The decode budget is now the datagram's own length, which can't reject anything
    legitimate. Switching off bincode's deprecated `config()` meant explicitly re-pinning
    `with_fixint_encoding().with_little_endian()` — the builder API defaults to *varint*, a
    silent wire-format change that compiles fine and stops talking to every peer — so
    `fixed_width_wire_bytes_decode_unchanged` pins the shape against `bincode::serialize` output
  - [x] **Leak:** `AppDelete` cleared apps, scheduling, autoscale overrides and secret seals
    but left `active_deploys`/`deploy_history`, so Raft state grew for the cluster's lifetime
    and an app recreated under the same name inherited the dead one's history
  - [x] **Honesty:** `[gitops] recursive` isn't ignored so much as redundant — `git ls-tree -r`
    always descends — which makes `recursive = false` the misleading case, promising a shallow
    sync and delivering a deep one. (Since removed outright: `recursive` is no longer a
    `[gitops]` key, so `deny_unknown_fields` rejects it at parse time.) `SyncState::history`
    (never written by the runner) and `coordinator_node_id` (informational — leadership is what
    gates syncing) are documented at the fields, so the next reader doesn't trust them
  - [x] **Deleted:** `smoker/node.rs` — `DrainPlan`/`KillPlan` had only self-tests and no
    production callers, and described an "agent executes this plan" design that CHAOS1
    explicitly rejected in favour of refusing node-level faults honestly
  - [x] **Already fine:** the Raft-id djb2 collision risk is thoroughly documented at
    `cluster::identity::raft_id_from_name` (12b.2/CP10), including why changing it is a
    protocol and state generation change. No action

The review flagged `O6`/`O7`/`O9` as the security-adjacent ones to prioritise within this list.

## UX Track: Learning Curve & Demonstrability

Post-12b user-experience work (not a roadmap phase). Plan:
`docs/plans/2026-07-17-plan-ux-improvements.md`.

- [x] `relish setup` — guided install and first configuration: detects bun on
  PATH / in `~/.reliaburger/bin` / running, installs or upgrades through the
  dual-signed Phase 14 pipeline (embedded release signature; a running node is
  deferred to `relish upgrade start`), and writes a minimal `reliaburger.toml`
  from stdin answers that round-trips through `NodeConfig::parse`. `--yes`
  takes every default non-interactively.
- [x] `relish manual` — eight starter chapters (`docs/manual/*.md`, 00–07) embedded
  with rust-embed; one pulldown-cmark parse renders both the terminal reader
  and the `--web` single-page HTML view; a new shared reader TUI (list +
  content + nucleo fuzzy search, pure keyboard-tested reducer) that
  `relish source` will reuse; `relish manual examples` extracts the embedded
  example configs without clobbering local edits.
- [x] `relish source` — the whole `src/` tree embedded (rust-embed,
  compressed in release builds) and browsed through the shared reader; each
  file's first line is its styled path so fuzzy search covers paths and
  content; `relish source ebpf` opens with the query pre-seeded.
- [x] README revamp — pitch-first top-level README (one-binary story,
  setup/manual/source showcase with an asciicast placeholder, prominent
  book); the components table and repo layout moved into a new
  "Under the hood" manual chapter; the status wall replaced by a
  progress.md link.

## Phase 16: Post-Phase-15 Audit — Truthfulness & Hardening

> Source audit: [2026-08-08-audit-post-phase15.md](plans/2026-08-08-audit-post-phase15.md)
> (five parallel doc-vs-code sweeps run against `main` at the PR #151 merge:
> whitepaper, the seven+eight component design docs, and the manual/READMEs).
> The Phase-15 code-review bugs it references are staged separately in
> [2026-08-06-plan-phase15-followup.md](plans/2026-08-06-plan-phase15-followup.md);
> Phase A of that plan (green CI) is done — its Phases B–H feed Section C below.
>
> The audit's one finding that shapes everything: **the code is more honest than
> the docs.** Reliaburger repeatedly fails *closed* — refusing an unsupported
> operation — where the whitepaper and design docs claim a working feature. So
> the through-line of Phase 16 is truthfulness: make the docs describe the system
> that exists, wire (or explicitly mark as unbuilt) the features documented as
> shipped, and fix the real defects the sweeps surfaced along the way.
>
> Work top-to-bottom by section. Sections A, F and G are cheap and should land
> first; each `[ ]` box below is roughly one PR and is checked only when it
> merges with `make ci` green.

### Section A — Cheap correctness now (config + mechanical doc sweeps)

- [x] **Reject unknown config keys** — `#[serde(deny_unknown_fields)]` on every
  TOML-deserialised config struct: all of `src/config/` (app/node trees + their
  sub-specs), plus the externally-defined sections `GitOpsConfig` (`[gitops]`),
  `BackupConfig` (`[cluster.backup]`) and `ClusterTestPolicy` (`[testing]`). The
  runtime configs in `smoker`/`mayo`/`ketchup`/`wrapper` are built *from* those
  TOML sections, not parsed from TOML, so they didn't need it; `deny_unknown_fields`
  is a no-op for bincode, so Raft/wire round-trips are unaffected. A typo or a
  removed key is now a parse error naming the field instead of a silent no-op.
  Fallout fixed: the removed-key test is now `parse_logs_section_rejects_removed_max_file_size`, which asserts
  rejection; `make examples` was running `relish apply` on a fault-scenario
  file that only "passed" because the old parser swallowed its fields into an empty
  config — the target now routes `[[step]]` files to `relish fault scenario
  --dry-run` and validates them for real. Added positive tests for a mistyped app
  field and a mistyped top-level table.
- [x] **`/api/v1` → `/v1` doc sweep** — real API/UI routes have no `/api` prefix.
  Fixed metrics-mayo, gitops-lettuce and all of ui-brioche. Left the two
  book/03 `/api/v1` mentions untouched: they're a generic ingress path-routing
  example (longest-prefix match), not the management API.
- [x] **Port `9443` → `9117` doc sweep** — `9443` is gossip; the API/UI is
  `9117`. Fixed only the API/UI-context uses (cli-relish:30,116; ui-brioche
  API-port/bookmark/config/firewall lines). Left every legitimate gossip/join
  `:9443` (whitepaper member lists, book ch2, manual, README dev-cluster). The
  fictional `~/.config/relish/config.toml` `cluster = ":9443"` example is left
  for the Section D cli-relish rewrite (the config file itself doesn't exist).
- [x] **eBPF errno sweep** — a BPF `return 0` deny surfaces as `EPERM`, not
  `ECONNREFUSED`/`ENETUNREACH`. Fixed chaos-smoker (drop/partition faults + ASCII
  diagrams + fidelity claim), discovery-onion (connect-hook denies), the code
  comments in `ebpf/onion_connect.bpf.c` and `src/smoker/types.rs`, security-sesame
  firewall test, and book chapters 03/08. Deliberately KEPT genuine no-listener
  refusals (no-eBPF case, stale-but-real backend address) and DNS `EAI_NONAME`.
- [x] **Dependency-table sweep** — deleted every crate row absent from
  `Cargo.toml` across all 14 design docs (git2, sequoia-openpgp, ssh-key, keyring,
  indicatif, chrono, tonic/gRPC, dashmap, arc-swap, sled, sigstore, memmap2, ulid,
  containerd-client, libbpf-rs, and more), corrected `libbpf-rs`→`aya`, and killed
  every claim of a gRPC *transport* (reporting/rollups are bincode-over-TCP;
  registry/logs/queries are HTTP). Kept honestly-labelled "alternatives evaluated"
  rows (raft-rs, foca, instant-acme). Left user-gRPC *proxying* text (ingress,
  deployments) alone. Deeper prose that still names deleted crates (storage-engine
  debates, the Askama UI narrative) belongs to the Section D wholesale rewrites.
- [x] **`allow_from` syntax sweep** — code accepts `"namespace/app"` or bare
  `"app"`. Fixed the two wrong-syntax examples (discovery-onion `app.api@production`
  → `production/api`; security-sesame `["app.api","app.admin"]` → `["api","admin"]`).
  book/04's `["api", "frontend/web"]` was already the canonical correct form.

### Section B — New code defects found by the audit

- [x] **Ingress default-cert SNI resolver** — the resolver now mints/caches a
  cluster-Ingress-CA leaf only for a hostname that currently has an ingress
  route (`RoutingTable::contains_host`, read via a non-blocking `try_read` on
  the shared routing table), and the cache is capped (`MAX_SNI_CACHE`). An
  unknown SNI — or no SNI, or a routing table momentarily locked for a rebuild
  — gets the self-signed default, which a hostname-validating client rejects.
  This closes both the cert-minting oracle and the unbounded-cache memory DoS.
  Chose the self-signed-default fallback (fail-safe, least behaviour change)
  over a hard TLS alert; reconciled `ingress-wrapper.md` §TLS. New tests prove
  an unconfigured SNI never mints or caches, and a configured host still gets a
  minted cluster cert.
- [x] **Build-namespace scope check is a no-op** — `build_submit_handler` now
  binds every image push to the caller's namespace scope via `authorize_scoped`,
  using the *destination's* namespace (`destination_scope`, bare names → the
  `default` namespace; C60 later refused bare names to scoped tokens) rather than a self-declared build field. A Deployer token
  scoped to namespace `a` can no longer push into namespace `b`'s repository or
  to a bare-named repo; an unscoped token is unaffected. Runs in both single-node
  and clustered mode. Tests cover cross-namespace refusal, the bare-name bypass,
  and the unscoped pass-through.
- [x] **Firewall perimeter port range** — `BunAgent::with_cluster` now sets the
  perimeter's `host_port_range` from the port allocator's actual range (new
  `PortAllocator::range()`), so the ports dropped for outsiders exactly match the
  host ports Bun hands out. The `PerimeterConfig` default was also aligned to the
  network default (10000-60000) so the fallback isn't misleading. Tests updated.
- [x] **Silent backend drop** — the five `let _ = self.service_map.add_backend(…)`
  sites now log the (only) error, the per-service backend cap, instead of dropping
  it silently; a backend that won't receive traffic is now visible in the log.
- [x] **Stale `upgrade/orchestrator.rs` module comment** — rewrote the module doc
  to match the shipped flow: the leader upgrades **in place last** (openraft 0.9
  has no graceful leadership transfer), not the leadership hand-off the comment
  described. The code (lines ~145-165) already documented the real behaviour.
- [x] **`auto_rollback` default and wiring** — the wired rolling path now honours
  the flag. On a failed rollout the `if new_failed` block branches: `auto_rollback
  = true` (default, unchanged behaviour) reverts; `auto_rollback = false` calls the
  new `halt_rolling_deploy` op (DeployOp variant + forwarder + loop handler + impl)
  which keeps the healthy new + surviving old instances, tears down only the
  incomplete one, and records `DeployResult::Halted`. `deployments.md` corrected to
  say the default is `true` (active revert). A wired MockGrill test induces a
  failed generation-1 instance under `auto_rollback = false` and asserts the halt,
  the old instance's survival, and the Halted history entry.
- [x] **`live_council_voter()` / gossip `is_council`** — wired. The cluster runtime
  now derives the live voter set + leader by name from Raft metrics
  (`spawn_council_roles_publisher`, mirroring `spawn_leader_hint_publisher`) and
  feeds it through a `watch<CouncilRoles>` into the gossip node, which applies
  `MembershipTable::set_roles` each publish cycle — so `is_council`/`is_leader`,
  `council_members()`, `leader()` and `live_council_voter()` return real data
  instead of always-`false`. **Scope note (from a codebase sweep):** the audit's
  framing was imprecise — `relish council recover`'s live-voter guard already uses
  the metrics-correct HTTP node list (`commands.rs`), not the gossip flag, so this
  is honest-flags / defense-in-depth, not a live-bug fix. `live_council_voter` and
  `council_recover` were deliberately **not** re-pointed at each other (that would
  regress the guard's `dead`/`left` state check). Unit-tested via `set_roles`;
  end-to-end exercised by the gated cluster suite.

### Section C — Carry-forward Phase 15 review defects

> These are the confirmed bugs from the PR #151 deep review, detailed in
> `2026-08-06-plan-phase15-followup.md`. Land them as that plan's Phases B–H.

- [x] **Registry regressions that break routable clusters** — `plan_registry_bind`
  now binds the unspecified address (not the advertised IP) when the config is the
  loopback default, so localhost keeps a listener; the build pipeline exports an OCI
  layout and uploads it with the service-token bearer (Pickle only accepts bearer
  auth, so buildah `--creds` couldn't work); `ClusterHttp` gained an optional bearer
  and `UpgradeManager::fetch_binary` uses it (no more self-upgrade 401); a keyless
  cluster now warns and the startup banner is honest. New gated
  `tests/suite/registry_routable_push.rs` proves bearer-less push is 401 and the bearer
  round-trip succeeds.
- [x] **Cluster lease-cleanup snapshot race** — `cleanup_cluster_lease` re-reads
  `desired_state()` *after* `TestLeaseBeginCleanup` commits and iterates that fresh
  owned set; `TestLeaseFinishCleanup` refuses to remove the record while any owned
  app/namespace still exists; `config_to_leased_writes` now rejects non-owned kinds
  instead of silently dropping them. State-machine test covers the raced attach.
- [x] **`cluster_apply` treats Raft `Refused` as committed** — the write loop now
  matches `Ok(Refused)` and streams an `ApplyEvent::Error`, stopping the apply.
- [x] **Clear-all fault authorisation gaps** — added `FaultType::requires_admin_reversal`
  (node ops + `NodePressure`); `clear_workload_faults`/`clear_by_service` skip those,
  and `fault_clear_all_handler` rejects an empty `?service=` with 400. Unit test
  proves NodeKill + NodePressure survive a Deployer clear.
- [x] **NodePressure TTL-expiry wedge** — the controller now frees the `active` slot
  even when the cgroup directory removal fails, queuing it for `retry_pending_cleanup`
  (run each health tick, off the runtime via `spawn_blocking`); a transient failure
  can no longer wedge future pressure faults. Linux-gated retry test.
  **Node-kill quorum TOCTOU** — the `state.node_name = None` mis-route is fixed
  (refuses rather than applying to the wrong node); the short-TTL fault ledger to
  union in-flight kills is a **tracked residual** (needs `ApiState` state; narrow
  race — two kills within the SWIM window).
- [x] **`relish wtf` can never exit 0** — new `Evidence::AvailableWithCaveat`; the
  restart/deploy collectors and the api.rs certificate telemetry use it for inherent
  limitations (rendered as caveats, not `unknown`), so a healthy cluster exits 0.
  **Benchmark probes ignore exec exit status** — a shared `testkit::probe` marker
  helper wraps probes, fails the sample on non-zero exit, and requires a DNS answer
  that names the app; a broken data plane now fails instead of scoring well.

### Section D — Honesty rewrites: docs describing unbuilt features as shipped

> Each item: either add a clear "planned / not-yet-wired" marker in the doc, or
> wire the feature (Section E overlaps where wiring is the better call). No item
> should keep presenting an absent capability in the present tense.

All items done: every claim was verified against `src/`, false/overstated
present-tense claims were corrected, and unbuilt features are marked
`**Status: planned — not yet implemented.**` (vision preserved).

- [x] **Registry push durability** — whitepaper §12/§Q7 and `registry-pickle.md`
  rewritten to async eventual replication (`oci-replication: pending`, ~60s
  leader heal loop, `redundancy=2` = pusher + 1 peer); survive-single-failure
  guarantee and `push_sync` removed; pull-through creds corrected to startup env.
- [x] **Worker-node trust model** (`security-sesame.md`) — rewritten: every
  clustered node loads the master key and unwraps age + Workload-CA keys locally;
  a real key split is marked planned.
- [x] **CLI command tables** (whitepaper §16, `cli-relish.md`) — reconciled to the
  real clap surface; nonexistent commands moved to a "Planned commands" section;
  config-file/env/keychain fiction replaced with the real `RELIABURGER_*` env story.
- [x] **metrics-mayo + logs-ketchup wholesale** — rewritten to Parquet + DataFusion
  SQL reality (no PromQL/TSDB/binary-log); scraping/fan-out/rollup-retention/ingress
  metrics marked planned; stderr-not-distinguished, substring `--grep`, local-only
  follow, epoch/duration `--since` corrected.
- [x] **gossip-mustard topology** — §3–§6 rewritten: ports 9443/44/45, UDP-seed join
  (no TCP full-state), no LEAVING state, flat-star reporting (two-level marked
  planned), council steady-state 7, resource summaries never gossiped.
- [x] **Security controls absent** — audit-log scope, TPM, `relish ca` rotation,
  token expiry/rotation, per-namespace age keys + per-app JWT audiences (node claim
  is literally `"local"`) all marked planned in the whitepaper + security-sesame.
- [x] **Process-workload isolation** (whitepaper §17, `agent-bun.md`) — rewritten to
  the real enforcement (deny-by-default binary allowlist + honest refusal); the
  namespace/seccomp/`burger`-user stack marked planned.
- [x] **Franchise** (whitepaper §21) — marked planned/future; vision preserved.
- [x] **Stale "deferred" markers flipped to shipped** — ingress WebSocket, `secret
  rotate`, onion IPv6/sendmsg egress hooks.

### Section E — Wire the library-only features

Every one of these had working, tested code with no production caller — Section D
honestly marked them "planned". Section E **wires all eight** so the docs'
original claims become true, and flips those "planned" markers back to
present-tense. Delivered in one PR, the bulk fanned out across parallel agents on
disjoint file clusters and integrated behind a single build + `make ci` (3216
tests) + cluster suite (22/22).

- [x] **Raft log encryption** — `sesame::raft_encryption` (AES-256-GCM) now
  encrypts the durable Raft log at rest whenever a cluster master key is present.
  `DurableLogStore::open_with_key` encrypts entry values on write and detects a
  per-entry marker on read, so plaintext and encrypted logs interoperate: a
  keyless node stays plaintext, and an encrypted entry opened without the key
  errors rather than reading as an empty (fresh) log (CP3-safe).
- [x] **cgroup resource requests** — `build_resources` / `generate_job_oci_spec`
  emit `cpu.weight` and `memory.high` into the OCI `linux.resources.unified` map
  from a workload's request range; limit-only specs stay byte-identical.
- [x] **Cron / scheduled jobs** — a `schedule = "0 3 * * *"` job is parsed by the
  new dependency-free `meat::cron` (five-field crontab, Vixie DOM/DOW rule) and
  fired by the bun event loop on its UTC minute, once per match. Scheduled jobs
  no longer run at deploy time.
- [x] **Blue-green deploy** — `strategy = "blue-green"` dispatches to a real
  `blue_green_redeploy`: the whole green fleet comes up and health-checks while
  blue keeps serving, then an atomic routing swap (via `finalise_rolling_deploy`)
  retires blue. A green failure tears green down and leaves blue untouched.
- [x] **`run_before` dependency ordering** — a job's `run_before = ["app.x"]`
  now runs it to completion (clean exit gated, bounded by a timeout) before
  app x deploys; a prerequisite failure aborts the deploy.
- [x] **Prometheus scraping + fan-out + rollup retention + ingress metrics** —
  bun spawns a scrape loop over `[[metrics.scrape_targets]]`, the metrics query
  handlers fan out across council members, the disk-pressure loop prunes rollups
  past `rollup_retention_hours`, and the collector folds the wrapper's ingress
  request counters into the time series.
- [x] **GitOps coordinator failover + sync history + Brioche GitOps view** — the
  coordinator now tracks the Raft leader (failover rides election), each sync
  records a bounded history entry (commit, duration, result), and `/ui/gitops`
  renders phase / coordinator / last-applied-commit / history.
- [x] **Ingress health probes / retry / headers / ALPN-HTTP2 / drain** — a
  background L7 probe loop flips each backend's local-health verdict, the proxy
  fails over on connect errors (never replaying a sent request), sets
  `X-Real-IP` / `X-Request-ID`, negotiates HTTP/2 via ALPN, and force-terminates
  draining backends past their deadline. WebSocket parity for the header and
  drain-termination paths remains a tracked follow-up (`websocket.rs`).

### Section F — User-facing doc fixes (manual + READMEs)

> Copy-paste correctness matters most here — a user runs these verbatim.

- [x] **Fixed broken manual snippets:** `join-token create` gained the required
  `--node-id` (manual 02); `dev create` corrected to rootful/sudo (docs/README + the
  `src/relish/dev.rs` module comment); `relish top` corrected to state/PID/restarts
  (docs/README, manual 01/04); the `apply --dry-run` sample now shows the real
  `ApplyPlan` display.
- [x] **Added a diagnostics chapter** — new `docs/manual/07_diagnostics.md` covers
  `wtf`/`trace`/`bench` (flags verified against clap, 0/1/2 exit contract), auto-embedded
  via the rust-embed glob; docs/README command table extended with `wtf`, `trace`,
  `bench`, `token *`, `logs-export`, `logs-search`, `compile`, `diff`, `fmt`, `import`,
  `export`, `council recover`, `secret rotate`, `dev test|disk|clean`, `fault
  cpu|memory|disk-io|resume`.
- [x] **Small consistency fixes:** README "Twelve subsystems" → 13 (Lettuce added);
  the "six starter chapters" note below is now corrected to eight (00–07); `init`
  corrected to "generates cluster PKI/CAs/identity/config".

### Section G — Time-boxed

- [x] **Advisory-exception review (done 11 Aug 2026, before the 18 Aug deadline)** —
  re-reviewed every exception. Two were **fixed by upgrades**, not re-ignored:
  `ratatui` 0.29 → 0.30 pulls `lru` 0.18.2, patching RUSTSEC-2026-0002 **and** the
  newer RUSTSEC-2026-0253 (`LruCache::pop` UAF, which the review surfaced — the
  audit was already failing on it); and `anyhow` 1.0.102 → 1.0.104 patches
  RUSTSEC-2026-0190. Both removed from `.cargo/audit.toml`. The ratatui bump was a
  one-line migration (`draw` now returns the backend error type) with unchanged TUI
  snapshots. The five that remain (`bincode`, `rustls-pemfile`, `paste`,
  `proc-macro-error` — all unmaintained, no patched release — and lock-only `rkyv`,
  still no active compiled path) have no upgrade available; re-affirmed with the
  same reachability rationale and a fresh **18 November 2026** re-review date
  (`make audit` gate + `.cargo/audit.toml` bumped). Follow-up plan table updated.

- [x] **Close the Phase 16 gate** — ran the full doc-vs-code sweep (whitepaper,
  all design docs, manual, book, READMEs) after every section landed. Six
  parallel auditors cross-checked present-tense claims against the code; 18 were
  confirmed and fixed. The manual and both READMEs were clean.
  - **whitepaper** (5): `build_push_to`→`destination`; `relish export --format
    kubernetes`→`export -f`; removed the fictional live-cluster `import`
    flags/`--dry-run`; `relish plan`→`apply --dry-run`; `relish volume
    snapshot`→`snapshot create`.
  - **chaos-smoker** (3): CPU stress and memory pressure described a burn-loop /
    `mlock` mechanism; the code caps `cpu.max` and lowers `memory.high` (the burn
    loop is node-level pressure only). §3.2 diagram + §5.2.1 rewritten.
  - **ui-brioche** (6): PromQL/`/v1/metrics/query?expr=`→name-parameterised
    `/v1/metrics`; Ketchup "index"→SQL over Parquet; WS log route + cross-node
    multiplex corrected to local-only real routes; cookie HKDF-encryption→opaque
    session id; CSRF section marked planned (only `SameSite=Strict` ships).
  - **ingress-wrapper** (2) + **book ch.09** (1): WS Close-1001 drain-termination
    is built but not sent on the live splice — reworded to a follow-up; corrected
    the `tokio-tungstenite` "used by Wrapper" claim (Wrapper hand-rolls it).
  - **discovery-onion** (1): `[ebpf] host_port_range` isn't a real key →
    `[network] port_range`.
  - **metrics-mayo** (bonus, understatements the Section E pass missed): flipped
    the remaining "scraping inert / fan-out not wired / rollup retention inert"
    notes that now denied shipped features.

**Phase 16 is complete** — every section (A–G) plus the closing gate is done.

---

## TODO: Codebase Audit Backlog (16 Aug 2026)

Findings from a full-tree audit (six parallel auditors over all 24 modules, ~169k
lines). Every item was verified by reading the surrounding code, not just grep hits.
Severity reflects blast radius, not effort. `[x]` = fixed; unchecked = open. Group
the fixes into a staged plan the way earlier review tiers did; several cluster in a
handful of files (`bun/api.rs` authz, `mayo`/`ketchup` durability, agent-loop
blocking calls).

> **Scope caveat:** the eBPF/Apple/runc code paths were read but not all run — treat
> platform-specific findings as "read-confirmed", not "reproduced". The areas skimmed
> in the first pass (`src/relish/` command handlers + `wtf` + `k8s_export`/`k8s_import`,
> and all of `src/testkit/`) were re-audited line-by-line on 16 Aug 2026 — see the
> **Deep re-audit** subsection at the end of this backlog.

### Critical — security & authorisation

> **All eight landed (16 Aug 2026, branch `phase16-critical-security`)** as a
> single PR of six commits, one per fix. CI green: `cargo fmt`, `clippy -D
> warnings`, portable nextest (3257 passed), doctests, `--no-default-features`,
> plus gated `test-slow` (4) and `test-cluster` (22, incl. chaos fault
> injection and placement/join). Every fix carries unit tests.

- [x] **Scoped token widens to cluster-wide via `/ui/session`** — `src/bun/api.rs:3269`
  `ui_session_handler` exchanges *any* valid token (including an app/namespace-scoped
  one) for a session cookie whose `AuthContext` is built by `readonly_session_context`
  (`src/sesame/auth.rs:313`) with `scoped_apps: None, scoped_namespaces: None`, and
  `None` means "allow everything" in `authorize_scoped` — it even passes
  `require_unscoped`. A tenant-scoped holder can POST its token to `/ui/session` then
  read every tenant's `/ui/app/{app}/{ns}`, `/v1/logs/{app}/{ns}`, `/v1/metrics/app/…`,
  and the whole-cluster `/v1/logs/sql` that explicitly refuses scoped bearer tokens (C3).
- [x] **A token literally named `__system` bypasses scope confinement** —
  `src/sesame/auth.rs:46-48` documents `__system` as reserved and "never stored", but
  nothing enforces it: `create_token` (`src/sesame/token.rs:44-79`) and the handler
  (`src/bun/api.rs:5938-5999`) accept any name, and authorisation matches
  `ctx.token_name == SYSTEM_PRINCIPAL`, so a real token named `__system` passes
  `require_system` (auth.rs:450-459) and skips `authorize_scoped`/`require_unscoped`.
- [x] **Fault injection ignores scope** — `src/bun/api.rs:3795` `fault_inject_handler`
  never calls `authorize_scoped` on `target_service` (and `FaultRequest` in
  `src/smoker/types.rs:465` carries no namespace), so a Deployer scoped to one namespace
  can kill/latency/error-inject any tenant's service; same in `fault_clear_all_handler`'s
  `?service=` path (`api.rs:4388`). The C3 test misses it because `/v1/fault` has no
  `{app}` path segment. Related: `src/bun/agent.rs:3747-3800,4074-4080` matches fault
  targets by `app_name` only (no namespace), so "web" hits every namespace's "web".
- [x] **Metrics/events endpoints serve every tenant to any token** —
  `src/bun/api.rs:4610` `metrics_query_handler` (plus `/v1/metrics/keys|rollup|cluster`
  and `/v1/events`) take no `auth` parameter, contradicting the C3 doctrine that
  `logs_sql_handler` applies via `require_unscoped` (api.rs:5069). A scoped token reads
  all tenants' per-app metrics and audit events.
- [x] **`relish join` leaks the join token before the fingerprint pin is checked** —
  `src/relish/commands.rs:619-626` + `src/sesame/join.rs:206-232` use
  `danger_accept_invalid_certs(true)` and send the one-time token + CSR *before* the
  `--ca-fingerprint` check runs on the response, so a MITM can capture and replay the
  still-unconsumed token even when a fingerprint was pinned.
- [x] **Rollup reports skip identity binding (metrics poisoning)** —
  `src/reporting/aggregator.rs:179-183` `MetricsRollup` is the only report type that
  skips the C6 `report_identity_ok` check, so under mTLS node X can submit a `NodeRollup`
  claiming node_id Y, poisoning Y's cluster metrics and pre-claiming Y's `(node, window)`
  dedup keys so Y's genuine rollups drop as duplicates (`rollup_store.rs:168`).
- [x] **`[permission]` section is enforced nowhere** — `src/council/state_machine.rs:581`
  permission specs are parsed, validated, GitOps-synced, and committed to Raft, but
  nothing reads `state.permissions` to authorise anything (authz is purely token scopes).
  `whitepaper.md:764`'s "`script` fields require `host-exec` permission" is false — gating
  is the node-level `[process_workloads]` allowlist (`supervisor.rs:256`). Decide: enforce,
  or document stored-but-unenforced and fix the docs + `src/config/permission.rs:1`.
- [x] **Constrained JWT verifier is unwired** — `src/sesame/oidc.rs:172-240`
  `verify_jwt_with_constraints` + `JwtConstraints` (the PKI10-hardened alg/kid/iss/aud/iat
  checks) has zero call sites; the only JWT verification anywhere is the weak `verify_jwt`,
  inside a `#[cfg(test)]` block (`src/council/node.rs:1288`).

### High — correctness / availability

> **All nine landed (17 Aug 2026, branch `phase16-high-correctness`)** as a
> single PR. Highlights: `NodeConfig::validate()` now runs at startup; the
> identity-rotation deadlock and inline `relish exec` stall are fixed; join
> seeds resolve `hostname:port` and refuse to silently bootstrap; zero intervals
> can't panic; council rollups are persisted and metrics prune by data
> timestamp; the firewall keys by `(namespace, app)`; and `object_store_url`
> now really backs metrics with S3/GCS/local object storage. Every fix carries
> tests.

- [x] **Whole-cluster node config validation is dead** — `src/config/validate.rs:328`
  `NodeConfig::validate()` is never called; `run_agent` (`src/bin/bun.rs:546-565`) runs
  only five section validators. So absolute-storage-path checks, inverted `port_range`,
  `retain_versions == 0`, `max_boot_attempts == 0`, the testing policy, unparseable
  `reserved_cpu`/`reserved_memory` (which then falls into `unwrap_or(0)` at bun.rs:361,
  over-reporting capacity), and the "dns without ebpf is a VIP black hole" rejection all
  never run.
- [x] **Identity rotation wedges the agent loop** — `src/bun/agent.rs:6584-6588`
  `check_identity_rotation` makes `mpsc::channel(1)` whose receiver (`_dummy_rx`) is never
  read, then loops `provision_identity` (one send per call); the second send blocks
  forever, permanently wedging the agent event loop when 2+ instances need rotation.
- [x] **`relish exec` with a long command stalls the whole agent** —
  `src/bun/agent.rs:2765-2772` `AgentCommand::Exec` runs `grill.exec()` inline on the
  command loop with no timeout, so `relish exec app -- sleep 3600` stalls health checks,
  restarts, and all commands (`Trace` was explicitly moved off-loop with an 8 s timeout
  for exactly this).
- [x] **Cluster-join seeds silently dropped → node bootstraps a new cluster** —
  `src/bin/bun.rs:121-126` parses `cluster.join` with
  `.filter_map(|s| s.parse::<SocketAddr>().ok())`, so any unparseable entry (including a
  natural `hostname:port`) is dropped; if all fail, `seeds` is empty and the node silently
  forms a brand-new cluster instead of joining.
- [x] **Zero-valued intervals panic at startup** — `src/bin/bun.rs:1328,1460`
  `[metrics] collection_interval_secs = 0` or `[logs] export_interval_secs = 0` is fed
  straight into `tokio::time::interval`, which panics on a zero period (alerts fixed this
  class under OBS4; scrape is guarded `.max(1)` at bun.rs:1402; `rollup_interval_secs` at
  bun.rs:185 has the same exposure).
- [x] **Council rollups are never persisted** — `src/mayo/rollup_store.rs:252`
  `RollupStore::flush()` has only test call sites, so cluster rollups live only in the
  in-memory buffer: a restart loses all rollup history (despite flush's doc),
  `hydrate_seen_windows` always finds an empty dir, `MAX_BUFFER_ROWS` silently drops the
  oldest data at 1M rows, and the "Rollup retention (E)" `prune_expired` tick
  (`bin/bun.rs:1584`) prunes Parquet files that never exist.
- [x] **Metrics retention prunes by mtime, not data timestamp** — `src/mayo/store.rs:588`
  `MayoStore::prune` (the O12 fix keying retention on the data's own max timestamp) has no
  callers; the binary prunes the metrics dir by mtime via `check_and_relieve`
  (`bun/disk_pressure.rs:122`, wired at `bin/bun.rs:1556`), reinstating the
  touch/copy/clock-skew hazard O12 fixed.
- [x] **`object_store_url` is dead config the book advertises as working** —
  `src/config/node.rs:735` `[metrics] object_store_url` is parsed/defaulted/round-trip-tested
  but read by zero code, yet `docs/book/06-watching-everything.md:52`,
  `docs/design/metrics-mayo.md:380`, and checked `progress.md:167` all claim it redirects
  metric persistence to S3. Wire it into MayoStore or delete it and fix the three docs.
- [x] **Namespace firewall keys rules by bare app name** — `src/sesame/firewall.rs:60-139`
  `resolve_firewall_rules`/`resolve_cgroup_namespace_entries` key cgroup IDs by bare app
  name (`src/bun/agent.rs:1867-1880` builds `HashMap<String /*app*/, Vec<u64>>` from
  `i.app_name` alone), so same-named apps in different namespaces collide — an `allow_from`
  rule leaks to every namespace's app of that name, and map insert order decides which
  namespace a cgroup lands under. Routing/service-map already fixed the identical collision
  (D3/codex-M1, `wrapper/routing.rs:6-10`).

### Medium — data-plane bugs, blocking calls, unwired subsystems

> **Most landed (17–18 Aug 2026, branch `phase16-medium-dataplane`, PR #162).**
> The deletions removed ~800 lines of unwired deploy engine + the superseded
> coordinator; M7 moved Argon2id, pickle blob reads, LogStore flush, the openraft
> redb commits, snapshot btrfs, and the per-step rolling-deploy retire drain off
> the async runtime. The residuals branch then closed the rest — the rollup
> backfill-window redesign (per-minute emission), the `finalise_rolling_deploy`
> bulk retire (drained on the deploy worker; blue-green cut-over split into
> publish → drain → bookkeeping), and the M5 deploy health gate — **completing
> the Medium tier**. Small tracked residuals live inside their items.

- [x] **Btrfs snapshot restore is destructive on failure** — `src/grill/snapshot.rs:245-247`
  `SnapshotManager::restore` deletes the live subvolume first, then creates the writable
  snapshot; if the second `btrfs` call fails the live volume is gone with no rollback.
- [x] **Rootless/runc port-mapping failures reported as "started"** —
  `src/grill/netns.rs:391-395` rootless `add_port_mapping` returns `Ok` before
  `run_tcp_proxy` binds (line 594, spawned task), so a taken host port yields a
  "successfully started" container whose port silently doesn't listen (error only
  `eprintln!`'d; `accept?` at 600 also kills the proxy loop on any transient error). Same
  shape root-mode: `src/grill/runc.rs:336-347` DNAT failure is only logged, supervisor
  reports Running with a dead port.
- [x] **Blob cache accepts truncated blobs after a crash** — `src/grill/image.rs:400,427`
  cache validity is `blob_path.exists()` and blobs are written non-atomically to the final
  path; a crash mid-write leaves a truncated blob treated as a valid cache hit forever
  (digest verified only on first download). Lines 408-415 also buffer the whole layer in
  memory.
- [x] **Secret decrypt failure starts the container anyway** — `src/grill/oci.rs:284-295`
  injects `{key}=DECRYPT_ERROR:{e}` (or raw ciphertext when no decryptor is present) into
  the workload env and starts it, silently running with a broken secret.
- [x] **New deploys go live before their health check runs** — the deploy worker's
  `wait_instance_healthy` now gates both rolling and blue-green replacements: after the
  runtime reports Running, the app's configured HTTP probe must pass `threshold_healthy`
  consecutive times (same `HealthCheckConfig` the steady-state tick uses, honouring
  `initial_delay`, bounded by the deploy's `health_timeout`) before the instance is
  announced healthy or published as a backend; failure takes the existing
  rollback/halt paths, so the old instances keep serving. Apps without a health check
  keep the Running-only wait. Regression tests drive the real `probe_health` against a
  local responder (200 completes, 500 rolls back on both strategies). _Tracked residual:
  `RegisterRollingInstance`'s `persist_instance_record` no-ops mid-deploy (the instance
  isn't in the supervisor until finalise), so a crash mid-rollout loses the new
  instances' on-disk records._
- [x] **Parquet flushes are unfsynced and Mayo drops samples on a failed write** —
  `src/mayo/store.rs:62-76`, `src/ketchup/log_store.rs:294-303`: no `sync_all`/temp+rename/
  dir fsync despite explicit durability claims (contrast pickle REG5). And
  `src/mayo/store.rs:244-257` `take_flush_batch` clears the buffer + bumps the counter
  *before* the write, so a failed write permanently discards the drained samples
  (LogStore/RollupStore clear only after success).
- [x] **Blocking work on the agent event loop / runtime worker**  _(Argon2id token create+verify, pickle blob_get read, LogStore flush, the openraft redb commits, snapshot btrfs, and every deploy retire drain — per-step, rolling scale-down surplus, and the blue-green bulk cut-over — now run off-lock/off-runtime; `retire_with_drain` is deleted and `finalise_rolling_deploy` is bookkeeping-only, with the blue-green ordering "publish green → drain blue → finalise" pinned by a test.)_ (project rule: no blocking
  in async):
  - `src/bun/agent.rs:2890-2919,1731` + `src/bun/snapshot_worker.rs:93-137` — snapshot
    create/restore/delete run sync `btrfs` subprocess + `std::fs` walks on the loop
    (btrfs.rs's own doc says "call from `spawn_blocking`").
  - `src/bun/agent.rs:7580-7586` — `DeployOp::RetireOldInstance` runs `wait_drained` +
    `stop_and_wait_for_exit` (100 ms polls up to drain+10 s, per instance, sequentially) on
    the loop, contradicting the DeployOp "fast `&mut self` steps only" doc.
  - `src/ketchup/log_store.rs:284-309` — `LogStore::flush()` does sync Parquet encode +
    ZSTD + `std::fs` on the runtime while holding the write lock (`bin/bun.rs:1432-1435`),
    starving all log appends/queries; only Mayo got the OBS5/M3 `spawn_blocking` treatment.
  - `src/council/durable_log.rs:261-277` (+ `save_vote`/`truncate`/`purge`,
    `state_machine.rs:109-123`) — every openraft async storage method does a synchronous
    redb `begin_write()`/`commit()` (fsync) on a runtime worker, uncommented.
  - `src/pickle/api.rs:461` — `blob_get` calls sync `read_blob` (`std::fs::read`) and
    buffers the whole blob (up to 512 MiB) per GET on a runtime worker.
  - Argon2id inline: `src/bun/api.rs:5980,3282` `create_token`/`authenticate` hash/verify
    on the async handler without `spawn_blocking`.
  - `src/sesame/identity.rs:281-299,312-321` — `mount`/`umount` via blocking
    `std::process::Command::status()` from async agent paths.
- [x] **LogStore buffer is unbounded** — `src/ketchup/log_store.rs:194-199` has no cap, so
  a persistently failing flush (disk full — logged every 60 s) grows the buffer without
  bound; RollupStore got `MAX_BUFFER_ROWS` for exactly this, LogStore didn't.
- [x] **Rollup backfill double-counts or drops on aggregator reassignment** — the
  backfill is now emitted as per-minute `NodeRollup`s (`generate_backfill`), each stamped
  at its own minute start, so the aggregator's `(node, timestamp)` dedup drops exactly the
  minutes it already holds and keeps the rest; no wire change (bincode discriminants are
  pinned), and the worker clears the backfill flag only after every send succeeds
  (partial-failure re-sends are idempotent). The silent-lost-push half was fixed in the
  main Medium PR. C20 now deduplicates worker/minute/series ownership across
  aggregators at query merge. A single rollup exceeding `MAX_REPORT_SIZE` (1 MiB)
  still cannot be pushed; bounded chunking is tracked as C21.
- [x] **Placement reconciler orphans instances on a failed stop** —
  `src/cluster/orchestrate.rs:847-857` fires `AgentCommand::Stop` with the response oneshot
  dropped and unconditionally does `applied.remove(...)` even if the send/stop failed, so no
  later tick retries — asymmetric with the deploy path, which waits for terminal `Complete`.
- [x] **SWIM dissemination can drop failure notifications** —
  `src/mustard/dissemination.rs:127-154` `compact()` keeps the first-drained entry at equal
  incarnation, but `BinaryHeap::drain()` yields arbitrary order, so a `Dead`/`Suspect`
  update can be discarded for an `Alive` — inverting the SWIM precedence `resolve_conflict`
  enforces everywhere else. Also `src/mustard/protocol.rs:801-836` `wait_for_relay_ack`
  applies piggybacked updates but skips the self-refutation logic (`handle_message:580-588`),
  so a Suspect-about-self during a relay wait is never refuted.
- [x] **Non-deterministic Raft apply** — `src/council/state_machine.rs:512`
  `RevokeCertificate` sets `crl.updated_at = SystemTime::now()` inside `apply_request`, so
  replicas diverge on that field (the comment eight lines up explains why wall-clock in
  apply is forbidden; the pruning beside it correctly uses in-log `revoked_at`).
- [x] **Upgrade start/rollback races and trusts client-supplied node identity** —
  `src/bun/api.rs:1685` `upgrade_start_handler` is check-then-write over a last-writer-wins
  `UpgradeUpdate`, so two concurrent starts (or a start racing rollback at 1897) both pass
  the guard and the second clobbers the first plan mid-flight; and `api.rs:1865`
  `upgrade_cluster_rollback_handler` copies client-supplied `node_id`/`address`/`role`
  verbatim into the replicated plan, bypassing the UPG2 validation `upgrade_start_handler`
  applies.
- [x] **`GET /ui/node/{name}` ignores the requested node** — `src/bun/api.rs:4931`
  `node_detail_handler` renders the *local* agent's instances with hardcoded
  `state: "alive"`, so clicking node B in a cluster shows node A's workloads labelled B.
- [x] **Ingress certs are unrevocable (hardcoded serial)** — `src/wrapper/tls.rs:70-74`
  every ingress cert (including per-SNI resolver-minted certs) uses `SerialNumber(1)`, so
  they're indistinguishable by serial and unrevocable via the serial-keyed CRL. (Known
  deferral — comment admits it, but track it.)
- [x] **WebSocket ingress has no connect/handshake timeout and ignores drain** —
  `src/wrapper/websocket.rs:69,162-180` has no timeout on `TcpStream::connect`/handshake/
  `read_http_head`, so a backend that accepts TCP but never responds pins the handler,
  permit, and drain guard forever; and `src/wrapper/proxy.rs:489,503-514` drops the drain
  `terminate` token on the WS path, so the drain deadline never tears a WS splice down and
  no 1001 close frame is sent (the `websocket_close_frame` builder at
  `draining.rs:237-239` is unwired).
- [x] **`upgrade_cluster_rollback`/`manager` swallow failures that disable rollback** —
  `src/upgrade/orchestrator.rs:757` discards a failed Raft `UpgradeUpdate` write with no
  log; `src/upgrade/manager.rs:313-314` swallows a failed symlink-restore after `execv`
  fails while archiving the marker anyway, so a restart execs the broken binary with no
  boot-check marker to trigger rollback.
- [x] **Unwired parallel deploy engine** — `src/meat/orchestrator.rs` `DeployOrchestrator`,
  the `DeployDriver` trait, and `src/meat/blue_green.rs` `execute_blue_green` (~800 tested
  lines) have zero call sites outside their files; the agent implements rolling and
  blue-green independently (`bun/agent.rs:7767`). Same for the replicated deploy state:
  `RaftRequest::DeployUpdate`/`DeployComplete` (`council/types.rs`, applied at
  `state_machine.rs:239-270`) are never written in production, so `DesiredState.active_deploys`
  / `deploy_history` stay empty forever. Decide: wire, or delete and stop maintaining two
  deploy implementations that can silently diverge.
- [x] **Superseded coordinator module still shipped** — `src/lettuce/coordinator.rs`
  `select_coordinator` ("prefer non-leader" election) is test-only dead code; production uses
  `coordinator_for_leader` (`sync.rs:353`), and the module doc still describes the opposite
  behaviour. Delete the module + fix the doc.
- [x] **GitOps auto-enforce admits unverified commits** — `src/lettuce/sync.rs:139-142`
  the script-modifying-commit gate admits `SignatureStatus::NotChecked`, so with no trusted
  keys (default when `require_signed_commits = false`) the "modifies script but not signed"
  protection is a no-op — the exact "verification never ran" case the rationale at
  sync.rs:50-53 calls unsafe to admit.
- [x] **Smoker BPF cleanup is a no-op** — `src/smoker/bpf_maps.rs:97-114`
  `cleanup_all_fault_maps` prints and does nothing (`let _ = map_ref;`) despite its
  hot-restart "safety net" doc, and has zero callers; stale fault-map state can survive a
  restart.
- [x] **Argon2/reqwest/GC swallowed failures** —
  `src/bin/bun.rs:2253-2256` GC treats a dead agent task's failed `ActiveImages` send as
  "no active images" (`unwrap_or_default()`) and collects everything; `bin/bun.rs:2425-2443`
  final-flush comment claims the feeding tasks are joined but the log-drain (1309) and
  metrics (1326) tasks are bare spawns not in the `join!`, so buffered records can be
  appended after the final flush and lost; `bin/bun.rs:1636-1637` swallows a
  `create_dir_all` error and uses `config.storage.data` directly rather than the computed
  `data_base` fallback.
- [x] **Dead / mislabelled config keys** — beyond `object_store_url` and `[permission]`
  above: `[upgrades] release_url` (`node.rs:316`) claims to feed `relish upgrade check` but
  nothing reads it (relish uses its own `--url`); `upgrades.gossip_rejoin_secs` (`node.rs:324`)
  parsed but never read (TODO(Phase 14) at bun.rs:1698); `[reporting_tree] max_events_per_report`
  (`node.rs:535`) plumbed into ClusterParams but no consumer/cap code; `[images] max_storage`
  parsed `unwrap_or(0)` where 0 = unlimited so a typo disables the cap.
- [x] **Config error messages name fields that don't exist** —
  `src/config/node.rs:592-606` `ReconstructionSection::validate` reports against
  `coverage_percent`/`timeout_secs` but the real TOML keys are `report_threshold_percent`/
  `learning_period_timeout_secs`; `node.rs:352` doc says node name defaults to hostname but
  it's `node-{gossip_port}`; `node.rs:503` doc says `advertise_address` is auto-detected but
  bun silently falls back to `127.0.0.1` (which then passes
  `enforce_cluster_transport_security`).
- [x] **`--output json|yaml` ignored by most relish subcommands** — `src/bin/relish.rs:16-18`
  the global format flag is threaded only into apply/nodes/council/test/bench/wtf/trace;
  `status`/`top`/`images`/`deploy`/`history` ignore it, so `relish --output json status`
  prints human output with exit 0.

### Low — hygiene, minor bugs, stale docs

- [x] **TUI log switching and viewport sizing** — subscription generations reject late lines/errors; both log views use the actual terminal height (reducer and viewport regressions).
- [x] **Stale TODO / doc drift** — H01 updates cordon/quota/scrape comments against their production callers, corrects the scheduler weights in source and chapter 2, and gives Phase 15's actual test locations plus a Phase 16 roadmap entry. Current tests already cover the dissemination code; no editing note remains there.
- [x] **Bootstrap, GitOps and disk-pressure export errors** — fresh council failures propagate, unavailable sync queues return 503 without consuming delivery IDs, and failed exports are logged.
- [x] **HTTP health-probe pooling and attribution** — pooled requests retain deadlines and distinguish local client errors from workload refusal.
- [x] **The listed startup/testapp/client-construction panics** — storage errors propagate, occupied testapp ports return errors, and invalid client trust configuration fails explicitly. This is not a claim that all public panic paths are eliminated.
- [x] **Unwired library-only helper inventory** — H02 removes obsolete alternatives and records supported library callers/tests. It does not implement the F04 CA recovery/rotation workflows.
- [x] **Dead wire/config fields** — H03 removes unused Wrapper settings and node release URLs, and explicitly reserves the unchanged legacy gossip timestamp slot. See the current ledger for test evidence.
- [ ] **Remote resource/image propagation** (current backlog: F01) — remote cached-image and GPU capacity evidence remains planned; resolving inert API fields does not implement it.
- [x] **Authorisation matrix checks methods as well as paths**, and dashboard desired counts come from desired state.
- [x] **Remaining minor-correctness group** — C01–C04, C13, C18–C19 and C22 are implemented and verified on this branch: archive preservation/scope/durability, bounded port allocation, precise certificate validity, truthful prune counts and bounded export receipts. See their current-ledger evidence above.
- [x] **Native Apple mounts and publication** — bind/config mounts, process identity, working directory, read-only root and published ports are translated and live-tested. Broader runtime parity remains experimental.
- [ ] **Planned capability boundaries** (current backlog: F06/F08) — PromQL, remote read, extra rollup tiers and live WebSocket drain-close remain unimplemented. Labelling them planned completed the documentation task, not these features.


### Deep re-audit of the previously-skimmed areas (16 Aug 2026)

Four agents line-audited what the first pass only skimmed: the `relish` command
handlers, the `wtf` diagnostics, `k8s_export`/`k8s_import`, and the whole `testkit`
tree (treated as production code — it ships as the `relish test`/`bench` runner).

Headline: the runner's **safety gate and lease/deadline arithmetic are genuinely
sound** (fail-closed authorise path, no expiry gap/overlap, atomic lease persistence),
the **`wtf` diagnosis rules and all bench math are correct**, and no path turns a
broken test run green by accident (deferrals report `Unknown`, which fails a full
profile). The real defects are a Kubernetes-conversion path that silently drops fields,
and a handful of test cases whose assertions are too loose to catch the bug they name.

> **All Medium items below landed (20 Aug 2026, branch `phase16-deep-audit-mediums`)**
> as one PR: k8s export/import fidelity (incl. the `cpu: "1"` 1000× quantity
> under-read), the logs-export agent endpoint, the wired dry-run diff (with the
> Destroy→not-in-config honesty rename), and the testkit false-green rewrites. The
> Low items remain tracked below.

**Medium — Kubernetes export/import silently loses data:**

- [x] **Exported Ingress has no namespace** — the Ingress `ObjectMeta` now carries the
  app's namespace like every other kind; the namespace regression test includes an
  Ingress so this class can't silently return.
- [x] **Export drops quotas and Job resources without a report entry** —
  `export_namespace` emits a real `ResourceQuota` (`cpu`/`memory` → `limits.*`, `gpu` →
  `requests.nvidia.com/gpu`; `max_apps`/`max_replicas` reported unsupported — K8s counts
  objects per kind, not apps); `export_job` shares the app path's env/resources helpers
  and reports `exec`/`script`; a DaemonSet app with `[autoscale]` gets an unsupported
  entry instead of an HPA targeting a nonexistent Deployment; `parse_target_pct` accepts
  the fraction form config validation accepts, and an unconvertible target omits the
  metric with a report entry instead of emitting `Utilization` with no
  `averageUtilization`. Bonus: the CLI's export-report gate now also prints when only
  `dropped` entries exist.
- [x] **Import correlates HPAs by the wrong name and drops uncorrelated resources** —
  `find_hpa_for_workload` scans by `spec.scaleTargetRef` (name + kind, empty kind matches
  any), so `api-hpa`→`api` correlates; StatefulSets now get Service/Ingress/HPA
  correlation and DaemonSets get Ingress; a post-conversion sweep warns for every
  Ingress/HPA no workload matched (mirroring the ConfigMap/Secret sweeps — warnings trip
  `--strict`, which is honest, the data is lost).
- [x] **Import drops fields on non-Deployment workloads** — a shared `pod_spec_to_app`
  helper gives DaemonSets/StatefulSets everything Deployments already imported
  (command+args, env with `valueFrom` warnings, resources, probes, initContainers,
  nodeSelector), with warnings filed under `{kind}/{name}`; Job/CronJob share
  `pod_to_jobspec` (env/resources/namespace/args survive; CronJob `suspend`/
  `concurrencyPolicy` warned). Resources read **requests and limits** as real K8s
  quantities via new `parse_k8s_cpu_millicores`/`parse_k8s_memory_bytes` — the old path
  fed K8s strings to `ResourceRange::parse`, under-reading `cpu: "1"` 1000× (bare int =
  millicores there) and silently dropping `"0.5"`/`"512M"`; unparseable quantities now
  warn. `apply_ingress` warns per dropped rule/path, non-Prefix `pathType`,
  `defaultBackend` and `ingressClassName`; the design-doc "path rules preserved" claim
  fixed.

**Medium — relish command handlers:**

- [x] **`relish logs-export` doc describes an agent interaction that doesn't exist** —
  built the agent path (the described design was better than the doc lie): Admin-only
  `POST /v1/logs/export` runs `ketchup::export_logs` server-side against the store's real
  directory with the Bun-owned checkpoint (destination `s3://`/`gs://`/path resolves
  agent-side; a failed checkpoint save is reported, not swallowed); `BunClient::logs_export`
  + the CLI calls it when `health()` answers and otherwise falls back to the direct local
  read — now in bun's own path preference order (the old code tried them reversed and
  ignored custom `[storage] logs`, which remains reachable only via the agent).
- [x] **`plan.rs` current-state diff is unreachable** — wired: `GET /v1/apps` serves the
  currently deployed resources in plan format (council desired state merged over the local
  agent's `deployed_specs`/jobs via the new `AgentCommand::CurrentResources`);
  `apply`/`deploy --dry-run` fetch it whenever the agent answers `health()` and diff for
  real (unreachable keeps the all-create plan). **Semantics correction:** apply is
  additive (removal is `relish stop`/GitOps), so the plan's `Destroy` action was a lie —
  renamed to `not_in_config`, rendered `*` with an "apply leaves them" note, and the
  footer now reports unchanged counts. Also fixed: `apply --dry-run --output json`
  appended the human "(dry run)" trailer, corrupting the JSON document. Round-trip test
  drives endpoint → client → `generate_plan` against a live standalone agent.

**Medium — test cases with assertions too loose to catch the named bug (false-green risk):**

- [x] **Scope/quota cases accept any error as success** — the scope case now passes only
  on the AUTH1 refusal itself (`ApiError { 403 }` with "token scope does not allow" in
  the body — status alone is insufficient, a role failure is also 403); transport errors
  become `unknown` and other errors fail. C34 now uses a read-only scope probe
  and server-owned token cleanup, independent of client survival. The quota case's premise was wrong — **quota is enforced at
  scheduling time** (the placement pass skips and logs; no council refusal reason
  exists), so the second apply *succeeds* — rewritten to assert the real observable:
  quota-b stays at `scheduled_replicas == 0` (via `desired_apps` evidence, with a settle
  re-check for late grants) while quota-a runs untouched; empty evidence → `unknown`.
  The placement case treats a failed node `status()` as `unknown` instead of "not
  hosting", and positively asserts the hosting set is exactly the labelled node. Bonus,
  same class: `service_discovery`'s stopped-backend case accepted *any* resolve error as
  "service gone" — now only a genuine 404 passes, transport errors at the deadline are
  `unknown`.

**Low — hygiene and minor bugs from the deep pass:**

- [x] **Relish liveness, client configuration, streamed UTF-8 and offline export errors** — health validates the response; invalid trust settings return errors; split UTF-8 survives SSE framing; explicit offline export validates paths and reports checkpoint failures.
- [x] **Remaining CLI robustness** — C17 rejects duration overflow, C23 reports encoded certificate expiry, C39 rejects invalid development paths before mutation, and C15/H01 correct the misplaced CA API documentation. Their regressions and verification are recorded above.
- [x] **Cluster collection, deadlines, deployment history and ingress acceptance** — incomplete collection fails, waits share the case deadline, history must contain both tested versions, and ingress polls exact responses using credential-free clients and isolated hosts. Expired queued capabilities refresh; three live ingress cases passed.
- [x] **Remaining test-harness robustness** — C29, C31–C33 and C35–C36 are complete; H01 now describes cleanup evidence and disabled-auth workload/node policy accurately. C30 catalogue fixtures are complete; C34 resource ownership and final hosted qualification are complete; H02 now records every helper disposition.
