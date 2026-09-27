# 0.1.0 static release review: second pass

Reviewed commit: `0eb6071da897b836d27e19d277a9a8e69a96ccbb`, the observed `main` tip when this pass began. The [first report](2026-09-27-static-release-review.md) reviewed `0a5dfc696a757193e4173af1618c64586c02f0b9`. These are two fixed snapshots, not a claim about the eventual release candidate.

**Coverage and test methods:** the [dedicated testing assessment](2026-09-27-testing-assessment.md) answers review questions 3 and 4 with a subsystem assessment, method inventory, concrete harness/CI findings and prioritised improvements.

**Recommendation:** triage the six new P1 findings below before advertising the affected security and GitOps guarantees. The three P2 findings concern reconciliation, application failures and bounded shutdown. Each includes a proposed fix and regression. This report changes no implementation and does not redirect the release qualification already running.

## Method and limits

This pass traced browser authentication, permission enforcement and GitOps from entry points through validation, state changes and tests. It also revisited current CI selection and selected first-pass findings. The current source was read from a temporary `git archive` snapshot; the report remains in the existing isolated review worktree. No builds, tests, services, workload commands, Lima operations or VM inspection were performed. Read-only GitHub metadata and upstream Git/GnuPG documentation were consulted. Publication changes only the review branch and draft PR.

B11–B19 are additional source-based findings. The examples and regression scenarios were **not executed**. Protocol details for quoted Git paths and GnuPG status records were checked against their upstream documentation. This remains a targeted review, not an exhaustive audit or a fresh coverage measurement. P1/P2 retain the first report's meanings.

## Changes since the first snapshot

The first report must not be read as a current list of ten open defects:

- **B08's missing metric mapping is addressed in source.** Built-in CPU/memory now map to collector series with resource-request normalisation. A real-collector CPU acceptance test now exists, alongside the synthetic fixture. This is evidence of an implementation/test change, not a claim that I ran it. Memory, failures and the remaining autoscaling boundaries still deserve their own evidence. [src/meat/autoscaler.rs:27](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/meat/autoscaler.rs#L27), [src/cluster/orchestrate.rs:1017](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/cluster/orchestrate.rs#L1017), [tests/placement.rs:752](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/tests/placement.rs#L752).
- **B09's lossy conversion is addressed in source.** Histograms retain bucket bounds and summaries retain quantile labels; tests assert exact series. [src/mayo/scrape.rs:47](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/mayo/scrape.rs#L47), [src/mayo/scrape.rs:617](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/mayo/scrape.rs#L617), [src/mayo/scrape.rs:648](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/mayo/scrape.rs#L648).
- **The Btrfs restore selection gap is addressed by the test's name.** `btrfs_snapshot_restore_recovers_corrupted_data` now matches `test-linux`. Snapshot metadata also uses atomic durable replacement. Those changes do not by themselves resolve the separate snapshot identity, restore-ownership and archive-receipt findings. [src/grill/snapshot.rs:328](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/grill/snapshot.rs#L328), [src/grill/snapshot.rs:552](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/grill/snapshot.rs#L552), [Makefile:44](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/Makefile#L44).
- **CI and test organisation changed.** Small integration suites now share one binary. Coverage runs the default-feature portable suite once; no-default-feature compilation/linting remains, but the earlier report's description of combined default/no-default runtime coverage is historical. [tests/suite/main.rs:1](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/tests/suite/main.rs#L1), [Makefile:97](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/Makefile#L97), [.github/workflows/ci.yml:117](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/.github/workflows/ci.yml#L117).

This pass does not re-certify every other first-pass finding or deferred feature. Keep their original commit pins and revalidate them when assigning fixes.

## Additional defects

### B11 — P1: browser sessions outlive revoked or expired parent tokens

**Evidence:** [src/sesame/session.rs:26](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/sesame/session.rs#L26), [src/sesame/session.rs:68](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/sesame/session.rs#L68), [src/sesame/session.rs:94](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/sesame/session.rs#L94), [src/sesame/auth.rs:347](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/sesame/auth.rs#L347), [src/bun/api.rs:5517](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/bun/api.rs#L5517), [src/bun/api.rs:9216](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/bun/api.rs#L9216).

Login copies the token name and scope into a session with an independent twelve-hour expiry. The session contains neither the credential fingerprint nor its expiry. Cookie authentication checks only the session store, even though the middleware has already loaded the current token list. Revocation removes the token through Raft but does not invalidate its sessions.

With another admin credential retained, log in using a short-lived token, revoke that token, and continue reading through the existing cookie. Parent expiry has the same gap. Reissuing the same token name cannot reliably distinguish the old session from the new credential. Access remains read-only, but revocation does not remove the existing read access.

**Promise missed:** the dashboard design explicitly says expired/revoked tokens are rejected on every cookie-authenticated request. [docs/design/ui-brioche.md:1362](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/docs/design/ui-brioche.md#L1362).

**Fix and regression:** bind sessions to immutable credential identity and cap lifetime at the parent's expiry. Check current revocation/scope state on use. Test login followed by expiry, revocation, same-name replacement and replicated token refresh on another node. Existing UI tests cover successful reads, refusal of writes and invalid login, not parent lifecycle. [src/bun/api.rs:15363](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/bun/api.rs#L15363). Book: chapter 4, including why a copied identity is not a continuing authority grant.

### B12 — P1: the public login route bypasses the shared Argon2 work limit

**Evidence:** [src/bun/api.rs:366](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/bun/api.rs#L366), [src/bun/api.rs:5466](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/bun/api.rs#L5466), [src/sesame/auth.rs:201](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/sesame/auth.rs#L201), [src/sesame/token.rs:94](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/sesame/token.rs#L94), [src/sesame/token.rs:139](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/sesame/token.rs#L139).

Bearer authentication performs a cheap shape check and holds a process-wide semaphore permit while its blocking task hashes. `/ui/session` instead calls synchronous `authenticate` directly inside a new `spawn_blocking` task for every request. That path has neither check nor permit and can try Argon2 against each non-expired stored token.

An unauthenticated caller able to reach the login form can therefore submit concurrent invalid tokens and bypass the concurrency defence protecting the API bearer path. Moving hashing to the blocking pool protects async workers from direct blocking; it does not bound CPU/memory admission.

**Fix and regression:** use one bounded verification service for every authentication entry point, including browser login. Reject malformed inputs before hashing and bound queued work as well as active workers. Keep admission ownership until abandoned blocking work actually exits. Exercise mixed login/bearer load with an instrumented verifier, then a separately provisioned real-Argon2 load test. Assert bounded work, health responsiveness and prompt credential refresh, not just 401 responses. Book: chapter 4's async/blocking boundary.

### B13 — P1: editing a multiline script bypasses mandatory GitOps signing

**Evidence:** [src/lettuce/verify.rs:55](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/verify.rs#L55), [src/lettuce/sync.rs:132](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/sync.rs#L132), [docs/design/gitops-lettuce.md:675](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/docs/design/gitops-lettuce.md#L675).

When global signing is disabled, the script-specific gate looks only for added diff lines containing the literal substring `script`. Given an existing TOML multiline `script = """..."""`, changing a body line from `echo old` to `echo new` leaves the key in unchanged context. The added line contains no `script`, so verification is skipped. A deletion-only edit can alter script behaviour without any matching added line either.

This matters where process/script workloads are enabled: the design promises that script changes require trusted signatures even when `require_signed_commits = false`. Independently, the detector ignores a non-zero `git diff` exit status; failed inspection with empty stdout becomes “no script change”.

**Fix and regression:** compare parsed script values in the previous and candidate trees, before applying anything. Treat inability to obtain a complete comparison as an error, including initial sync. Test unsigned multiline replacement, deletion, TOML literal/basic string variants, initial scripts and failed diff/object reads; allow the equivalent changes only with a trusted signature. Existing verification tests exercise key-string matching and missing keys, not real script-change detection. [src/lettuce/verify.rs:92](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/verify.rs#L92). Book: chapter 7, explaining semantic comparison versus textual heuristics.

### B14 — P1: the trusted-signing-key check searches arbitrary diagnostic text

**Evidence:** [src/lettuce/verify.rs:25](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/verify.rs#L25), [src/lettuce/verify.rs:86](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/verify.rs#L86).

After `git verify-commit --raw` succeeds, `is_key_trusted` accepts a configured fingerprint if it appears anywhere in stderr. It does not extract and compare the fingerprint field. GnuPG's raw output contains user-controlled identity text in `GOODSIG` as well as the cryptographic fingerprint in `VALIDSIG`. [GnuPG status format](https://raw.githubusercontent.com/gpg/gnupg/master/doc/DETAILS).

Consequently, a valid signature by a key available to the verifier but absent from Reliaburger's allowlist can pass the second check if that key's user ID contains an allowlisted fingerprint. This is a signer-authorisation flaw, not a claim that an invalid signature or a wholly unavailable public key will verify. The current tests use simplified single-line strings and miss identity text containing a different key's fingerprint. [src/lettuce/verify.rs:111](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/verify.rs#L111).

**Fix and regression:** parse the documented status records and compare exact, validated fingerprints, with an explicit primary-key/subkey policy. Handle SSH verification through its own structured trust mechanism. Use a disposable real GPG keyring containing trusted and untrusted keys; give the untrusted key a user ID containing the trusted fingerprint and require rejection. Also cover fingerprint prefixes, case normalisation, signing subkeys and verification failure. Book: chapters 4 and 7.

### B15 — P2: GitOps counts a committed refusal as a successful application

**Evidence:** [src/lettuce/runner.rs:393](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/runner.rs#L393), [src/council/node.rs:125](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/council/node.rs#L125), [src/council/state_machine.rs:291](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/council/state_machine.rs#L291), [src/council/state_machine.rs:889](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/council/state_machine.rs#L889), [src/lettuce/runner.rs:170](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/runner.rs#L170).

`CouncilNode::write` returns a successful transport/consensus result containing a `CouncilResponse`. `apply_changes` checks only the outer `Err`; `Ok(Refused { reason })` increments the applied count. The runner then records the commit as successfully applied.

A concrete path is a GitOps manifest declaring the reserved `rbtest-a1b2` namespace and an app within it. Generic configuration validation does not perform the HTTP handler's test-lease admission checks. The state machine refuses the ordinary namespace/app writes, but GitOps reports success and advances `last_applied_commit`. The next unchanged poll skips the supposedly applied commit. Other accepted writes in the same sync can make the result partial while still reported successful.

**Fix and regression:** handle response variants explicitly and propagate the refusal reason; advance the commit only after actual application of every intended write. Preflight known lease restrictions too. Test a live, elected in-memory council returning a committed `Refused`, then check desired state, error/history and retry position. The existing failure test uses an uninitialised council and exercises the outer error only. [tests/suite/gitops.rs:330](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/tests/suite/gitops.rs#L330). Book: chapter 7, distinguishing `Result` success from an enum's business outcome.

### B16 — P2: an unchanged Git HEAD prevents drift reconciliation

**Evidence:** [src/lettuce/sync.rs:74](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/sync.rs#L74), [src/lettuce/diff.rs:64](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/diff.rs#L64), [docs/design/gitops-lettuce.md:1077](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/docs/design/gitops-lettuce.md#L1077).

When fetch reports no new commit and HEAD equals `last_applied_sha`, `execute_sync` returns before reading config or comparing it with current desired state. After a successful sync, manually changing an app's image or deleting it from Raft is therefore not corrected by subsequent polls or webhook nudges while Git stays unchanged.

The design says every sync reapplies Git's desired state to repair manual drift. The implementation instead applies changes when Git advances. This is independent of intentional autoscaler overrides, which should remain preserved.

**Fix and regression:** cache validated Git state if useful, but diff it against fresh cluster state on reconciliation ticks. Define any manual-operation exclusions explicitly. Apply a commit, mutate/delete an app through the real manual path, poll with the same SHA and assert convergence. Include unchanged legitimate autoscale overrides as the negative control. Book: chapter 7's reconciliation contract.

### B17 — P1: incomplete Git tree reads can become destructive desired-state deletions

**Evidence:** [src/lettuce/git.rs:233](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/git.rs#L233), [src/lettuce/sync.rs:177](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/sync.rs#L177), [src/lettuce/diff.rs:116](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/diff.rs#L116), [src/council/state_machine.rs:314](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/council/state_machine.rs#L314).

`list_toml_files` checks the `ls-tree` exit status, but two other omissions still produce an incomplete “successful” config:

1. It parses newline-delimited `git ls-tree --name-only` output and checks `ends_with(".toml")`. Without `-z`, Git quotes unusual filenames. A quoted TOML path ends with a quote and is skipped. Renaming an existing file to a name requiring quoting, such as one containing a tab (or non-ASCII under normal `core.quotePath` settings), can make its apps disappear from the parsed tree. [Git pathname output contract](https://git-scm.com/docs/git-ls-tree).
2. A non-zero `git show` result for an individual blob is silently ignored. The parse-error guard cannot detect a file that was never inserted into its input map.

The diff interprets absent app/namespace/permission declarations as intentional removals and applies their delete writes. A filename representation issue or unreadable object can therefore remove live desired state.

**Fix and regression:** use NUL-delimited paths, handle path encoding explicitly, and reject the whole sync if any selected blob cannot be read completely. Test quoted/Unicode filenames, rename without content changes, a failing blob read and a genuine intentional deletion. Assert that incomplete reads preserve existing desired state and do not advance the applied commit. Book: chapter 7, covering byte-oriented process output and fail-closed destructive reconciliation.

### B18 — P1: configured permission restrictions do not govern log and metric reads

**Evidence:** [src/config/permission.rs:1](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/config/permission.rs#L1), [src/sesame/auth.rs:500](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/sesame/auth.rs#L500), [src/bun/api.rs:4659](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/bun/api.rs#L4659), [src/bun/api.rs:4995](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/bun/api.rs#L4995), [src/bun/api.rs:8557](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/bun/api.rs#L8557).

`PermissionSpec` documents an additional action/app/namespace allowlist layered over role and token scope. Selected write routes call `authorize_permission`, but the per-app log, WebSocket log and metric handlers only check token scope. The authentication middleware does not perform the missing action-specific check centrally.

For an otherwise unscoped user token, a matching permission spec with `actions = ["deploy"]` and `apps = ["web"]` restricts deployments but still permits reading another app's logs and metrics. Even an empty action list does not revoke these reads. Token scopes remain useful and are enforced; they are a separate control from the configured permission spec.

**Fix and regression:** define an explicit authorisation policy for every route and enforce the supported permission actions consistently, including UI fragments, streaming and cluster fan-out entry points. Until implemented, refuse unsupported restrictions or document their exact limited effect. Add route-level table tests for role × token scope × permission action/target, using both bearer and cookie identities. Helper tests showing that `PermissionSpec::allows` returns false do not prove a handler calls it. [src/config/permission.rs:113](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/config/permission.rs#L113), [src/sesame/auth.rs:1001](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/sesame/auth.rs#L1001). Book: chapter 4, keeping policy evaluation separate from complete enforcement.

### B19 — P2: Git subprocesses have no deadline or cancellation ownership

**Evidence:** [src/lettuce/git.rs:60](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/git.rs#L60), [src/lettuce/git.rs:99](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/git.rs#L99), [src/lettuce/verify.rs:25](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/verify.rs#L25), [src/lettuce/runner.rs:87](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/runner.rs#L87), [src/lettuce/runner.rs:54](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/src/lettuce/runner.rs#L54).

Clone, fetch and verification use blocking `Command::output` without deadlines. The runner awaits the complete blocking sync, and checks cancellation only outside that wait. A stalled remote, credential helper or verifier can therefore hold the only sync loop indefinitely; polling, backoff and shutdown cancellation cannot recover it while the call remains stuck.

**Fix and regression:** give subprocesses bounded, owned lifetimes with output limits, cancellation, kill/reap handling and non-interactive credential settings. A timeout that merely drops a `spawn_blocking` handle is insufficient because the subprocess keeps running. Test a controlled child that never exits and one that leaves a descendant; require bounded failure, recorded diagnostics, completed cleanup and a subsequent successful sync. Book: chapters 7 and 15, including the distinction between cancelling a future and stopping the work it started.

## What these findings say about coverage

The project already has real Git repositories, in-memory Raft, full API routers, process ownership tests and privileged acceptance. These findings mostly expose missing combinations at subsystem boundaries, rather than a need to replace those foundations.

| Priority | Missing evidence | Recommended addition |
|---|---|---|
| P1 | A credential is valid at login, then changes while its session survives. | Lifecycle tests crossing token storage, replication, session lookup and actual read routes (B11). |
| P1 | Browser and bearer entry points use different hashing paths; read handlers omit a configured policy layer. | One route inventory linking each route to its authentication/admission/permission tests. Exercise shared resource budgets and denied actions through routers (B12, B18). |
| P1 | Simplified signature strings and ASCII filenames hide external-tool contracts. | Disposable real Git/GPG/SSH fixtures, semantic script variants and path-encoding cases (B13, B14, B17). No shared keyrings or operator Git configuration. |
| P2 | A failed consensus operation is tested; a successfully committed refusal is not. | Assert both response and persisted state for every relevant `CouncilResponse`, including partial sync and retry (B15). |
| P2 | Normal new-commit sync is tested, but steady Git with changing cluster state is not. | Reconciliation properties: unchanged input is idempotent; drift converges; incomplete input never deletes; autoscale exceptions remain intact (B16, B17). |
| P2 | Backoff logic exists independently of process termination. | Deterministic stuck-child and shutdown tests with actual reap/descendant assertions (B19). |
| P2 | Ignored-test ownership can drift with test moves/renames. | Generate a manifest assigning every ignored test to an automated gate or explicit manual/deferred category. The real `crane` login/push/pull test is ignored and no invocation or provisioning was found in the checked Makefile/workflows/scripts. Add a provisioned standards-client job or make that manual qualification requirement explicit. [tests/suite/registry_standard_clients.rs:348](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/tests/suite/registry_standard_clients.rs#L348), [Makefile:29](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/Makefile#L29), [.github/workflows/ci.yml:187](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/.github/workflows/ci.yml#L187). |
| P2 | The portable coverage percentage omits platform tests and now omits no-default-feature runtime execution. | Keep the aggregate floor, publish subsystem/changed-line results and distinguish compiled configurations from executed configurations. Add a focused no-default runtime gate for feature-dependent behaviour. [Makefile:97](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/Makefile#L97), [.github/workflows/ci.yml:117](https://github.com/reliaburger/reliaburger/blob/0eb6071da897b836d27e19d277a9a8e69a96ccbb/.github/workflows/ci.yml#L117). |

For these regressions, assert the externally visible outcome and durable state, not just “request failed”, “output non-empty” or “task returned”. Add property/mutation checks specifically around signature parsing, action omission, refused responses and incomplete-input deletion. A clean aggregate coverage gate cannot establish those contracts by itself.

## Recommended order

1. Repair browser credential lifecycle and consistent permission enforcement (B11, B18). Share bounded verification across login and bearer authentication (B12).
2. Repair script/signing identity checks and complete-tree admission together (B13, B14, B17). Until then, qualify the advertised GitOps trust and deletion guarantees explicitly.
3. Handle committed refusals, unchanged-HEAD drift and owned Git process deadlines (B15, B16, B19).
4. Add the targeted regressions to independently provisioned runs when the release effort can accommodate them. Any implementation change needs evidence for the resulting candidate; this review neither reran nor invalidated an existing soak.
5. Reconcile the dashboard and GitOps design claims with the release contract, and update book chapters 4, 7 and 15 with the eventual implementation and test reasoning. Explain any new Rust identity types, enum matching, admission guards and cancellation ownership when first introduced.
