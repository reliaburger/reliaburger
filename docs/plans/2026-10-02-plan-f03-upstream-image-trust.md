# Plan: F03a, upstream image trust and digest binding (0.1.4)

*Written 2 October 2026 for [#361](https://github.com/reliaburger/reliaburger/issues/361) (F03), the first item of [0.1.4](https://github.com/reliaburger/reliaburger/milestone/8). F03 has two halves, and the [completion plan](archive/2026-09-17-codebase-completion-plan.md#f03--finish-upstream-image-trust-and-worker-key-separation) asks for separate designs. This is the first: upstream trust with digest binding. The second, taking the master key's authority off workers (F03b), gets its own plan once this one has shipped.*

## Where we are

Image trust today covers images built into or pushed to Pickle, and nothing else.

- **The policy is per node.** `[images.trust_policy]` in `node.toml` (`require_signatures`, `keys`) is checked by Bun before a deploy (`enforce_image_signature`, `src/bun/agent/launch.rs`). A Pickle image that passes is pinned to its digest (`pin_image_reference`).
- **Upstream images skip it.** `lookup_pickle_manifest` (`src/meat/scheduler.rs`) treats `cache/` repositories as absent and anything outside Pickle as allowed. `TODO(F03, #320)` marks the spot. The manual says so: "Images from external registries and the pull-through cache aren't checked, so pin those by digest."
- **Tags are never bound.** An upstream app keeps `nginx:1.27` in its stored spec and OCI spec. A restart pulls again (`restarts.rs` → `grill.create` → `pull_and_unpack`), the pull-through cache re-checks the tag after `cache_recheck_secs`, and a new replica on another node resolves it fresh. So one app can run two different digests on two nodes, or change bytes on a restart nobody asked for.
- **Nothing records the digest that ran.** Deploy history, instance status and Raft hold the image string as written.
- **No cosign.** Pickle signatures are our own format (P-256 over the digest string, kept in the Raft catalogue). `registry-pickle.md` §5.6 calls it "compatible with the Sigstore/cosign ecosystem" in one place and not in another; it isn't.

## What we'll build

### U1. Bind every image to a digest at apply

The node that handles `POST /v1/apply` (and the GitOps runner, which writes the same `RaftRequest::AppSpec`) resolves each app's image to a manifest digest **before** the spec goes into Raft, and stores the reference with both: `docker.io/library/nginx:1.27@sha256:…`. OCI references allow a tag and a digest together, and the digest wins, so every node, restart and replacement pulls the same bytes, while people still read the tag.

- Resolution goes through the same path a pull does: the Raft catalogue for Pickle images, the pull-through cache (`cache/<host>/<repo>`) when it's on, or a manifest `HEAD` upstream. A multi-platform index binds to the index digest, so each node still picks its own platform.
- A reference that already carries a digest is left alone. A Pickle image that the trust policy pins today is bound the same way, so that special case goes away.
- Applying again re-resolves, which is how you pick up a moved tag on purpose. `relish apply` prints each binding (`web: nginx:1.27 → sha256:3f2a…`), so the change is visible in the output and in deploy history.
- **Unreachable upstream:** the apply fails with the registry error, rather than storing an unbound tag. A tag the cache already holds is resolved from the cache, as a pull would be.
- **Where the digest shows:** deploy history (`DeployHistoryEntry.image` carries the bound reference), `relish inspect`, and the instance's `image` in status. A rollback restores the bound reference, so it runs the bytes that ran before.
- **Standalone nodes** (no council) bind in the agent's deploy path instead, so a single node gets the same guarantee.

`ImageReference` (`src/grill/image.rs`) holds a registry, a repository and one `tag` slot that may hold a digest, so it can't read `repo:tag@sha256:…` today. U1 gives it a separate `digest: Option<String>`: the pull uses the digest when there is one, and the tag stays for display. The field in Raft is the same `image` string, but a node without the new parser would misread a bound reference, so U1 bumps the protocol and state formats.

### U2. A policy for upstream images

`node.toml` grows rules for images outside Pickle. Trust roots stay in node config on purpose: `security-sesame.md` §5.10 keeps keys out of anything an API token can change.

```toml
[images.trust_policy]
require_signatures = true          # Pickle images, as today
keys = ["BASE64..."]               # Pickle operator keys, as today

[[images.trust_policy.upstream]]
match = "docker.io/library/*"      # repository pattern; the most specific wins
require_signatures = false         # allowed, bound to a digest, not signed

[[images.trust_policy.upstream]]
match = "ghcr.io/acme/*"
require_signatures = true
cosign_keys = ["-----BEGIN PUBLIC KEY-----\n..."]   # PEM, ECDSA P-256

# Anything matching no rule:
[images.trust_policy.upstream_default]
allow = true                       # today's behaviour; set false to allow-list
```

- With no `upstream` rules, behaviour is today's plus U1's binding: every upstream image is allowed and bound.
- `allow = false` turns the rules into an allow-list: an image that matches nothing is refused, by name, at apply and at deploy.
- The check runs twice, as Pickle's does: at apply on the node handling it (an early, readable refusal), and in Bun before every deploy (the enforcement). Nodes with different policies disagree; the manual says to keep them the same, and `relish wtf` flags a node whose policy hash differs (a follow-up, not in this plan).

### U3. Cosign signatures, key-based

For a rule with `require_signatures = true`, the node verifies a cosign signature over the bound digest:

- Fetch the signature manifest at tag `sha256-<hex>.sig` in the same repository (the classic cosign layout), through the pull-through cache when it's on.
- Each layer of type `application/vnd.dev.cosign.simplesigning.v1+json` is a payload whose `critical.image.docker-manifest-digest` must equal the bound digest, and whose `dev.cosignproject.cosign/signature` annotation is a base64 ECDSA P-256 signature over the payload bytes.
- Accept when any layer verifies against any of the rule's `cosign_keys`. `ring` already does ECDSA P-256, so no new crypto crate.
- **Not in this plan:** keyless signing (Fulcio certificates and Rekor transparency-log proofs), which needs network trust roots and their rotation; and the newer Sigstore bundle stored as an OCI referrer. Both are worth having; neither is needed to say "only run what our key signed". Whether the bundle format should come first depends on what the images people use actually publish, so we'll check a sample (Chainguard, distroless, our own release images) before starting U3.

### U4. Docs that match

- Manual `10_security.md` "Signed images" and `11_images-and-volumes.md`: binding, the upstream rules and cosign, replacing "pin those by digest".
- Book ch10 "Image signing": the binding and the upstream policy, and the correction that Bun enforces at deploy (the text says Meat refuses to place). Ch05's note that the pull-through cache is deferred is stale too.
- `registry-pickle.md` §2, §5.4 and §5.6, and `security-sesame.md` §5.10: the same, and remove the "cosign-compatible" claim for Pickle's own format.
- Move the `TODO(F03, #320)` in `scheduler.rs` to #361 or remove it when U2 lands.

## Tests first

- **U1:**
  - unit tests for the binding: tag to `tag@digest`, a digest left alone, an index bound to the index digest, an unreachable registry failing the apply, a cached tag resolving offline;
  - an API test that `POST /v1/apply` stores the bound reference in Raft and returns the binding;
  - an agent test that a restart after the tag moves runs the old digest;
  - a rollback test that the restored spec carries the digest.
- **U2:**
  - table tests for rule matching (most specific wins, the default);
  - refusals at apply and deploy with the image named;
  - the allow-list case.
- **U3:**
  - a fixture signed by `cosign sign --key` checked in under `tests/fixtures/cosign/` (the payload, signature and public key), so the verifier is tested against real cosign output, not only our own;
  - a wrong key, a payload for another digest, a tampered payload, and a missing `.sig` all refuse;
  - a local registry test (`tests/suite`, the in-process registry the Pickle tests use) that pulls a signed image end to end.

## Order and size

U1 first (about a week): it fixes the drift on its own and everything else builds on the bound digest. Then U2 (a few days), U3 (about a week, after the format check), and U4 alongside each. One PR per step, stacked, each with its book section.

## Open questions for the maintainer

1. **Bind at apply, or at first deploy?** This plan binds at apply so every node agrees from the start. The alternative, letting the first node to deploy record the digest in Raft, avoids a registry round trip in the apply but is more machinery.
2. **Upstream down at apply:** fail (this plan), or store the unbound tag and bind at the first successful pull?
3. **Policy in `node.toml` only?** It keeps trust roots off the API, at the cost of nodes being able to disagree.
4. **Cosign format:** classic `.sig` tags first (this plan), or the Sigstore bundle as an OCI referrer?
