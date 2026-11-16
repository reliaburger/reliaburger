# BDD evaluation of the documented claims

An executable evaluation in [`eval/`](../../eval/README.md) that checks Reliaburger against what its own docs say. It builds a three-node VM lab with libvirt, installs a pinned release the way the [Linux servers guide](../linux-servers.md) says, and runs Gherkin scenarios (behave) from the operator's workstation. Nothing runs inside the cluster to grade itself.

## What it covers

Four axes, each scenario tagged with its axis, a requirement ID and the claims it tests:

| Axis | Requirements | Examples |
|---|---|---|
| Vision | VIS-001, VIS-002 | install footprint, each built-in subsystem with nothing else installed |
| Extensibility | EXT-001 to EXT-008 | `--output json`, alert webhooks, metrics surfaces, extension points, an external controller, log export, GitOps |
| Interoperability | INT-001 to INT-004 | Kubernetes kinds, a migration, upstream manifests (podinfo, Online Boutique), export back to Kubernetes |
| Operations | OPS-001 to OPS-011 | node loss, redeploy after delete, token roles, network perimeter, chaos safety rails, leader loss, partition, power cuts, full power cycle, rolling deploys |

## Rules

- Every `@claim_` tag resolves to `eval/claims/claims.yaml`, whose quotes are checked at the cited lines of the pinned docs ref before a scenario starts. An unresolved claim makes the scenario BLOCKED.
- Thresholds live in `eval/config/thresholds.yaml`, each with its basis, and are registered before a scenario first runs.
- Statuses are PASS, FAIL, UNSUPPORTED, BLOCKED and NOT_RUN. Pass rates use PASS + FAIL as the denominator.
- Every command is logged with exit code, timestamps and redacted output.

## Tests that prove the harness

- `make test`: offline tests of the harness's own logic.
- `make claims`: every citation found at the pinned ref.
- `make dry-run`: every feature parses and every step matches.
- A full run, recorded under `eval/results/<run-id>/` with a summary in `eval/reports/`.

## Order

1. First full run on v0.2.0, recorded in this PR.
2. Issues for the reproducible failures it finds.
3. The known harness issues listed in `eval/README.md`.
4. A run against each release candidate after staging, alongside the qualification scripts.
