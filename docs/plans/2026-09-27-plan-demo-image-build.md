# Demo image build: multi-arch export, Buildah pruning, network notes

27 September 2026. Follow-up work on draft PR #248 (`feat/demo-image-build`),
which adds the Go demo app (`examples/demo/burger`), `relish build` through the
quickstart registry forward, Buildah in the guest image, and a build step in
staged-install qualification. The maintainer asked us to "build multi-arch,
prune the buildah storage, and use your best judgement on the rest".

**Targets the first release after 0.1.0. Don't merge before 0.1.0 ships.**

## Constraints for whoever picks this up

- A release soak may be running on the maintainer's Mac. Don't start, stop or
  use any Lima VM or quickstart cluster, and don't run `qualify-*.sh` locally.
- Keep local builds light (`CARGO_BUILD_JOBS=2`, a separate
  `CARGO_TARGET_DIR`), run focused tests locally and let CI do the rest
  (`full-ci` label; `gh pr checks 248`).
- Commit and push after every step. Never amend or squash.
- The local branch in this worktree is `pr248-multiarch`; it pushes to
  `origin feat/demo-image-build` (`git push origin pr248-multiarch:feat/demo-image-build`),
  because another worktree has `feat/demo-image-build` checked out.

## Goals

1. **Multi-arch.** A build with several platforms (default `linux/amd64` +
   `linux/arm64`) stores every platform in Pickle, not only the builder's own,
   and a node pulls the platform matching its own architecture by bare name.
2. **Prune Buildah storage.** A node no longer keeps about 900 MB per cold
   build. The build's own containers and images go after every build, success
   or failure; base images stay cached only up to a size cap.
3. **Best judgement on the rest:** netavark/nftables interaction, `go test` for
   the demo in CI, the asciicast.
4. Docs: manual `11_images-and-volumes`, book chapter 5, design docs, progress
   register, PR body.

## Design decisions

### D1. Export the whole manifest list

`buildah push <list> oci:dir:tag` exports only the builder's native platform
(confirmed in a VM by the previous session). A multi-platform build now runs
`buildah manifest push --all <list> oci:dir:tag`, which writes the index and
every platform's manifest, config and layers into the OCI layout. A
single-platform build keeps `buildah push`.

### D2. Upload order: blobs, platform manifests by digest, index by tag

`upload_oci_layout` moves from `bun::build_runner` into `pickle::build` (so the
portable suite can drive it against a real Pickle router) and learns indexes:

1. every blob in `blobs/sha256/` goes up as a monolithic blob (as before);
2. when the top manifest is an index, each platform manifest is `PUT` by its
   digest (`/v2/<repo>/manifests/sha256:…`), so it becomes a catalogue entry
   in the same repository (a pull by digest resolves, GC pins its layers);
3. the top manifest (index or single manifest) is `PUT` under the tag.

A nested index (an index inside the index) is refused. For a multi-platform
build, the runner checks the exported platforms against the requested ones,
so a regression back to a single-platform export fails the build instead of
quietly storing one architecture.

### D3. Sign the index and every platform manifest

Deploy-time verification (`verify_image_signature`) looks up the tag the app
names, which now resolves to the index, verifies its signature, and pins the
deploy to `name@<index digest>`. The index bytes name each platform manifest by
digest and Pickle's store is content-addressed (`write_blob` verifies digests),
so the index signature covers the platform manifest a node pulls. We also sign
each platform manifest, so a reference pinned to one platform's digest
(`name@sha256:<arm64 manifest>`) verifies too. That costs one extra Raft
`AttachSignature` per platform. Under `require_signatures`, any signing
failure fails the build (JOB7 unchanged).

### D4. Pull: resolve the index to this node's platform

`ClusterSource::ensure_image_local_with_peers` used to treat an index entry like
an image: the sub-manifests became "layers" and the index blob became the
"config", so a multi-arch image in Pickle couldn't run (this also affected
multi-arch images pushed by `docker push`). Now, when the catalogue entry is an
index, it materialises the index blob, picks the `linux/<arch>` entry for this
node (`std::env::consts::ARCH`, normalised the way external pulls do), looks
that manifest up by digest in the same repository and materialises it. No
match is an honest error naming the architecture.

### D5. A Reliaburger-owned Buildah storage root, pruned after every build

Buildah used the host default root (`/var/lib/containers/storage`), shared
with anything else on the host (an operator's podman). Pruning there could
delete someone else's images, so builds now run with
`--root <storage.data>/buildah/root --runroot <storage.data>/buildah/run`.

After every build (success or failure), under a node-wide build lock:

1. `buildah rm --all` (build containers);
2. `buildah manifest rm <list>` (multi-platform) or `buildah rmi --force <tag>`;
3. `buildah rmi --prune` (the unnamed per-platform and stage images);
4. if the storage root is still over `[images] build_cache_max_bytes`
   (default 1 GiB; 0 keeps nothing), `buildah rmi --all --force`.

Named base images (the `FROM` images) stay cached below the cap, so a warm
build stays warm. The demo's Go base image is about 750 MB in vfs, under the
cap. Cleanup failures are logged and never fail a build.

The lock makes builds on one node run one at a time. Before, concurrent builds
shared the storage and nothing pruned it; with pruning, one build's cleanup
would race another's build. Builds are rare, and the per-stage timeout still
applies, so queuing is the simplest safe choice.

### D6. Netavark and Reliaburger's nftables coexist (documented, not changed)

A `RUN` step that uses the network makes Buildah (netavark on Ubuntu 24.04)
create a `podman0` bridge on `10.88.0.0/16` with its own iptables-nft rules.
Reading the code:

- the perimeter firewall (`src/firewall/rules.rs`) uses its own tables
  (`ip[6] reliaburger_fw`), an `input` hook with policy `accept`, and only ever
  deletes its own tables, so it neither removes netavark's rules nor drops
  forwarded build traffic;
- container networking (`src/grill/netns.rs`) uses its own `ip reliaburger`
  NAT table and masquerades `10.0.0.0/8`, which also covers `10.88.0.0/16`
  (a second masquerade is harmless);
- Reliaburger gives every container a `/32` host route, so a node whose
  container `/23` happens to fall inside `10.88.0.0/16` (1 node in 256) still
  routes its containers correctly; the only clash is a build container and an
  app container getting the same address on that node, which is rare and
  short-lived;
- a build container reaching the node's own Bun API or registry ports comes
  from `10.88.x.x`, which the perimeter treats as a stranger and drops.

So we leave the default network in place and document it. `RUN --network=none`
stays the recommendation for steps that don't need the network (the demo does
this).

### D7. Small calls

- CI runs `go vet` and `go test` for `examples/demo/burger` in a small job
  gated on the same change filter as the code jobs.
- The `relish build` registry-forward fix stays as it is.
- `assets/tour.cast` can't be re-recorded without a VM: left as an open item.

## Steps

- [x] 0. Plan (this file).
- [x] 1. Multi-arch export + index-aware upload in `pickle::build`, runner
      switched over, platform check, signing of index + platform manifests.
      Unit tests with fake OCI layouts; portable-suite test uploading a
      two-platform layout to a real Pickle router
      (`registry_routable_push::a_multi_platform_layout_publishes_every_platform`).
- [x] 2. Index-aware cluster pull (`ClusterSource`), unit + portable tests
      (`pickle_cluster::a_multi_platform_image_pulls_the_nodes_own_platform`,
      `…_without_the_nodes_platform_is_refused`; `p2p::tests::platform_selection_*`).
- [x] 3. Dedicated Buildah storage root, build lock, cleanup commands,
      `[images] build_cache_max_bytes`. Unit tests for commands and the cap.
      (Landed in the same commit as step 1: the runner rewrite covers both.
      `BuildSettings` replaced the `build_timeout_secs` argument of
      `router_with_upgrade`.)
- [x] 4. Gated real-Buildah tests (`tests/build.rs`, CI privileged Linux job
      runs them under `make test-linux`): two-platform build lands as an index
      with two catalogued platform manifests; signed build signs all three;
      storage pruned (no containers or images left in the test's own root).
      Needs the `full-ci` label on this stacked PR to run.
- [x] 5. CI: `demo app` job in `ci.yml` (`go vet` + `go test`, gated on `code`).
- [x] 6. Docs: manual 11 (platforms, pruning, network), book ch. 5 (new
      subsections replacing the "problem we haven't fixed" paragraph, plus test
      notes), `registry-pickle.md` (§3.3 pull flow, new §5.8.1, config note),
      `docs/README.md` config sample, progress register (UX track entry).
      Main was merged into the branch (merge commit; one conflict in
      `docs/linux-servers.md`, resolved to main's package list plus the PR's
      Buildah line).
- [ ] 7. PR body updated; CI green with `full-ci`.

## Needs a VM later (don't do it during the soak)

- Run the tour end to end on a quickstart (`relish build burger/burger.toml`,
  apply, `curl …/order`) and check `buildah --root /var/lib/reliaburger/data/buildah/root images`
  after the build: only the Go base image should remain.
- On a two-architecture cluster (or with `platform` forced to the foreign
  architecture), check that each node pulls its own platform.
- A build with a networked `RUN` step on a node with the perimeter firewall
  on: confirm the step reaches the internet and `nft list ruleset` still shows
  both Reliaburger's and netavark's tables afterwards.
- Re-record `assets/tour.cast` with `scripts/demo/tour.sh`.

## Open questions

- Is 1 GiB the right default cache cap? It keeps the demo warm on a 10 GiB
  quickstart disk.
- Should `relish images` hide the per-platform digest entries a multi-arch
  push creates (they show up as digest "tags", as they already do for a
  `docker push` of a multi-arch image)?
- Should the homepage still call it a "five-minute" tour (open question 5 in
  the PR)?
