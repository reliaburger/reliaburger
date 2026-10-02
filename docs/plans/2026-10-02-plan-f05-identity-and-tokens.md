# Plan: F05, namespace-scoped identity and the token lifecycle (0.1.4)

*Written 2 October 2026 for [#363](https://github.com/reliaburger/reliaburger/issues/363) (F05), part of [0.1.4: operations security](https://github.com/reliaburger/reliaburger/milestone/8). The [completion plan](archive/2026-09-17-codebase-completion-plan.md#f05--complete-namespace-scoped-identity-and-token-lifecycle) names four families: per-namespace encryption keys, per-app audiences, token lifecycle automation, and broader audit coverage. Its test: "deliver each boundary independently with cross-namespace denial, rotation/expiry and audit-attribution tests; preserve the existing default-deny API surface."*

## Where we are

**API tokens** (`src/sesame/token.rs`, `src/sesame/types.rs`):
- What exists:
  - tokens are Argon2id-hashed and live in Raft (`SecurityState.api_tokens`);
  - each has a role (Admin, Deployer, ReadOnly), an optional scope (apps, namespaces) and an optional expiry, checked on every request;
  - `relish token create | list | revoke`.
- What's missing:
  - no `last_used`;
  - no rotation;
  - no default lifetime: "tokens don't expire unless you give `--ttl-days`";
  - nothing removes expired tokens;
  - `token list` doesn't show scope.
- `[permission.<name>]` specs narrow a token and are keyed by **name**, so a token revoked and re-created under the same name inherits the old spec silently. Sessions are keyed by principal, so they don't.

**Workload JWTs** (`src/council/node.rs`, `src/sesame/oidc.rs`):
- `aud` is always exactly `["spiffe://<cluster>"]`, and per-app audiences are planned (`security-sesame.md` §6.6).
- One signing key, so there is one `kid` in the JWKS.
- A workload JWT works directly as a read-only bearer, scoped to its own app and namespace.

**Secrets** (`src/sesame/secret.rs`, `src/bun/agent/launch.rs`):
- The consuming side handles namespace keys already: decryption tries the namespace's keys first, then the cluster-wide ones, and re-sealing uses the namespace key if there is one.
- But **nothing creates a namespace key**:
  - `relish secret pubkey` and `rotate` are cluster-wide only;
  - `NamespaceSpec` has no `secret_key`;
  - every namespace decrypts with the cluster key.

**Audit** (`src/bun/events.rs`):
- `record_audit` exists, with `action` and `principal`, but only fault inject and clear call it.
- Token create and revoke, join tokens, secret rotation, identity signing, permission and namespace changes, decommission and secret decryption leave no audit event.
- Events live in a per-node, in-memory buffer of 1024 entries.

**Docs that say more than the code does:**
- `security-sesame.md` §5 describes a per-token `rate_limit_rps`; no code implements it.
- §6 says a decryption audit event is logged and that Bun asks the council to decrypt. Neither is true: Bun decrypts locally and logs nothing.

## What we'll build

One PR each, in this order. Each is independent of the ones after it.

### I1. Audit attribution

- `EventKind` grows `Token`, `Secret`, `Identity` and `Auth`, and a helper builds an `AuditEvent` from an `AuthContext`: `principal_id` as the principal, with the token's name in the details.
- **Events for:**
  - token create and revoke;
  - join-token create;
  - secret rotate and finalise;
  - identity sign;
  - `[permission]` and `[namespace]` applies;
  - decommission.
- Apply records `principal_id` rather than the token's name.
- **Tests:**
  - one per route, asserting the event's `action` and `principal`;
  - a source check in `authz.rs` that every `Admin + Cluster(ADMIN | SECRET_WRITE)` row calls `record_audit`, beside `every_gated_route_checks_its_permission_action`;
  - scoped tokens still can't read events (already pinned by #468's guard).

### I2. Token visibility and safe expiry

- `token list` shows each token's scope and when it was last used.
- A sweep removes tokens past their expiry (plus a grace period). An empty token store opens the API to everyone (the bootstrap window in `auth.rs`), so **the sweep never removes the last Admin, and never empties the store**.
- **Tests:**
  - an expired token gets 401 and is swept;
  - the sweep keeps the last Admin;
  - a store whose every token has expired still refuses an anonymous request, which today only `router_stays_open_when_no_user_tokens_exist` touches from the other side;
  - `last_used` moves forward after a request.

### I3. Token rotation and a default lifetime

- `relish token rotate <name>` (`POST /v1/token/rotate`, a new `RaftRequest::RotateApiToken` appended to the enum).
  - It issues a new secret under the same name, and the old one keeps working for a grace period (24 h by default), then stops.
  - Sessions opened with the old secret end with it, because the principal changes.
  - The `[permission]` spec follows the name.
- **A default lifetime** for new tokens (90 days, `[security.tokens] default_ttl` in node config), with `--no-expiry` to opt out explicitly.
- `token list` and `relish wtf` warn when a token expires within 14 days.
- **Tests:**
  - the old secret works inside the grace period and gets 401 after it;
  - the new secret works at once;
  - an old session ends;
  - the spec follows the name;
  - the last Admin can be rotated;
  - a token created without `--ttl-days` gets the default;
  - rotation is audited.

### I4. Per-namespace secret keys

- A namespace opts in with `secret_key = true` in `[namespace.X]`. The leader generates its keypair at generation 0 (`RotateSecretKey` with `AgeKeyScope::Namespace`).
- `GET /v1/secret/public-key?namespace=` and `POST /v1/secret/rotate {namespace}`, plus `relish secret pubkey | rotate --namespace`. Rotation and finalising are per namespace, independent of the cluster scope.
- **Once a namespace has a key, its values stop falling back to the cluster key.** Without that, a value sealed with the cluster key still decrypts in every namespace, and the boundary is only nominal. `relish secret` re-seals existing values with the namespace key as part of opting in.
- **Tests:**
  - a value sealed with namespace A's key doesn't decrypt in B, and that deploy fails closed (the test `security-sesame.md` §10 lists);
  - after opting in, a value sealed with the cluster key doesn't decrypt in that namespace;
  - rotation and finalising per namespace;
  - who may rotate (question 4);
  - audit events.
- **Stated limitation:** until the master-key split (F03b) ships, every node can unwrap every namespace's key. The boundary is against other *tenants'* tokens and workloads, not against a compromised node. The manual says so.

### I5. Per-app JWT audiences

- `[app.NAME.identity] audiences = ["sts.amazonaws.com"]`, read from desired state when the leader signs the JWT.
- **Replay:** a JWT whose `aud` holds both the cluster and `sts.amazonaws.com` could be sent to AWS and replayed against our API. So the extra audiences go in a **second** JWT file, one per audience and without the cluster audience, and the API keeps accepting only tokens whose `aud` is exactly the cluster's (question 5).
- **Tests:**
  - the extra token carries exactly the configured audience;
  - the API refuses it;
  - the cluster token is unchanged;
  - an app can't name an audience reserved for the cluster.

### I6. Secret decryption audit, and the docs

- Bun records `secret.decrypted` per instance start: app, namespace, node, key names, scope and generation, and never values.
- Fix `security-sesame.md` §5 (rate limiting: either describe it as planned or drop it) and §6 (the decryption flow).
- **Test:** deploying an app with an `ENC[…]` value yields one event with the right app, node and generation, and no plaintext anywhere in it.

## Compatibility

New `ApiToken` fields (`last_used`, rotation's previous hash and grace end) are added with `#[serde(default)]`. New Raft requests go at the end of the enum. I3 and I4 change the state format, so each bumps `src/compatibility.rs`. No migration, per the pre-1.0 rule.

## Open questions for the maintainer

1. **Audit durability.** The event buffer is per node, in memory, and holds 1024 entries. Is that enough for audit in 0.1.4, or does attribution have to survive a restart (a Raft-replicated audit list, or a Ketchup-style Parquet table)? I1 works either way; this decides whether a durable store comes with it or later.
2. **`last_used` write path.** A Raft write per request is too much. Options: each node keeps it in memory and `token list` asks every node (like the other cluster views); a Raft write throttled to once every 5 minutes per token; or gossip. We'd pick the first.
3. **Token identity.** Should a token re-created under a revoked token's name inherit the old `[permission]` spec, as it does today? We'd refuse the create until the spec is removed or re-applied, so the inheritance is a decision someone makes.
4. **Namespace keys:**
   - opt-in per namespace (this plan) or automatic for every namespace?
   - may a namespace-scoped Admin rotate its own namespace's key, or does that stay with unscoped Admins?
5. **Audiences.** Is a separate JWT per extra audience acceptable to workloads (one more file)? Should there be a cluster-wide allow-list of audiences an app may request, given that any Deployer can apply an app?
6. **The default lifetime and Admin tokens.** Once every Admin token has expired, nobody can create one, and the bootstrap window doesn't reopen (the store isn't empty). Should Admin tokens be exempt from the default lifetime, or is the answer a documented break-glass path (the node's service token on the node itself)?
7. **Rate limiting.** Is per-token rate limiting part of F05's "token lifecycle", or a separate item?
