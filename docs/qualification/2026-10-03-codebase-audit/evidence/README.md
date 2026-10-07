# Audit reproduction evidence

These are small observation harnesses retained for review and regression handoff. They exercise the current faulty behavior; the CLI/GitOps tests intentionally assert that behavior. Convert them to expected-behavior assertions when implementing a fix.

The probes were built from the current source at `f4757e7789d3672d21f15ca203031d6604d7e11a`, using the current test-profile library `libreliaburger-55ef092f71d4eefb.rlib`. They were not linked against the older September 22 library also present in the target directory. No compiled binaries are included in this bundle.

| Source | Evidence / drafts |
| --- | --- |
| [cli_gitops.rs](cli_gitops.rs) | Seven current-source integration observations: drafts 19–25. [Output](cli_gitops-output.txt). |
| [network.rs](network.rs) | Real production proxy with a local backend, active SSE, and local UDP/TCP DNS responder: drafts 10, 17, 18. [Parent rerun output](network-output.txt). |
| [state.rs](state.rs) | Real probe loop/table rebuild; failed log flush/replay/reopen; 33-endpoint merged map: drafts 05, 09, 11. [Parent rerun output](state-output.txt). |
| [mayo_collision.rs](mayo_collision.rs) | Two production object-store backends on one temporary file:// prefix: draft 06. [Parent rerun output](mayo-collision-output.txt). |
| [batch.rs](batch.rs) | Authenticated real HTTP router, fake agent commands/status only: drafts 03, 08, 15. No submitted script was executed. [Parent rerun output](batch-output.txt). |
| [runtime_pure.rs](runtime_pure.rs) | Debug cron overflow, autoscale validation, volume path identity: drafts 01, 14, 27. [Parent rerun output](runtime-pure-output.txt). |
| [cron_release.rs](cron_release.rs) | Exact production cron source compiled with release overflow behavior: draft 14. [Parent rerun output](cron-release-output.txt). |
| [existing-security-tests-output.txt](existing-security-tests-output.txt) | Five existing scoped-registry/quota/signing baseline tests. These passing tests do not disprove the new gaps. |

## Re-running against a checkout

For the integration observations, temporarily copy `cli_gitops.rs` into `tests/audit_cli_gitops_repro.rs` and run:

```sh
cargo test --test audit_cli_gitops_repro -- --nocapture
```

Remove that temporary test afterward. For a standalone harness, temporarily copy its `.rs` file into `examples/audit_probe.rs` and run:

```sh
cargo run --example audit_probe
```

Use a separate example filename per probe if retaining multiple. Remove temporary example files afterward. Cargo will supply matching current dependencies. The network harness requires permission to bind loopback sockets and runs for about 30 seconds; the probes create temporary local fixtures only.

For the release cron demonstration, compile the retained `cron_release.rs` directly with `rustc --edition=2024 -C opt-level=3 -C overflow-checks=off`, passing the matching `time` crate with `--extern time=<current time rlib>` and `-L dependency=target/debug/deps`. Its relative `#[path]` points to the current production module from this evidence directory. Do not substitute a different cron implementation.

The batch harness injects prior successful status and delays acknowledgement of the new deploy. Its printed success therefore demonstrates status misattribution, not execution of a failing process. The volume probe demonstrates accepted paths mapping to one image; no Linux filesystem was mounted or formatted. The 33-endpoint probe establishes merged-map rejection; no 33-node cluster was launched.

Outputs are the observed parent reruns on macOS. Runtime timing, fixture paths and platform IO error codes may vary. A separate privileged Linux/live-council regression is needed for findings whose drafts explicitly mark that verification limit.
