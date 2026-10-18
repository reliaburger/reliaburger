# Evaluation harness

An executable evaluation of Reliaburger against its own documented claims. It builds a disposable three-node Linux lab, installs a pinned release the way `docs/linux-servers.md` says, and runs Gherkin scenarios from outside the cluster. Every result traces to a claim with a verified source citation.

The evaluation has four axes: vision (is it the integrated platform it says it is), extensibility (can you reach past the built-ins), interoperability (can you bring Kubernetes workloads in and take them out), and operations (does it survive failure, and are its security boundaries real).

It is a Python project of its own (behave, run through uv) and is not part of the Rust build or CI. Run every command below from `eval/`.

## Prerequisites

- Linux host with KVM and libvirt (`qemu:///system`), and your user in the `libvirt` group
- `virsh`, `virt-install`, `qemu-img`, `xorriso`, `ssh`, `curl`
- `docker`, `kind` and `kubectl` (the Kubernetes baseline and the OpenTelemetry collector)
- [uv](https://docs.astral.sh/uv/) (Python 3.12 or newer is fetched on demand)
- `git`
- Passwordless `sudo` for one call: OPS-004 adds a second address to the lab bridge, and removes it afterwards
- About 20 GiB of free memory and 100 GB of disk (three VMs with 4 vCPU and 6 GiB each, plus kind)

`./rbeval setup` checks the tools and stops with the list of anything missing.

## Quick start

```sh
make setup        # check tools; download and verify the release and the guest image
make claims       # verify every claim citation against the source at the pinned ref
make test         # offline tests of the harness itself
make smoke        # the four seed scenarios
make run          # every scenario, then tear the lab down
make down         # destroy the lab if a run was interrupted
```

`make claims` reads the docs at the pinned ref from GitHub, or from a local checkout when `RBEVAL_PRODUCT_REPO=..` is set.

Pick another release with `PRODUCT=`, for example `make run-ops PRODUCT=v0.2.1-rc.39098892640`. Targets live in `config/product.yaml`, each pinned by commit and by the SHA-256 of every binary and of the release's `SHA256SUMS`.

Rerun one scenario against the lab a previous run left up:

```sh
./rbeval run features/operations/redeploy_after_delete.feature --keep-lab
./rbeval run --tags @req_OPS-003 --keep-lab
./rbeval lab env        # export RELIABURGER_* to drive the lab cluster by hand
```

## Layout

| Path | Contents |
|---|---|
| `features/<axis>/` | Gherkin scenarios, tagged `@axis_*`, `@req_*` and `@claim_*` |
| `features/steps/` | Step definitions, one module per area |
| `features/environment.py` | Hooks: claim linking, evidence, status mapping, teardown |
| `harness/` | Lab (libvirt), cluster provisioning, probes, evidence and reports |
| `config/` | Product pins, lab topology, safety bounds, thresholds |
| `claims/claims.yaml` | Every claim with its source quotes and line ranges |
| `fixtures/` | Native TOML, Kubernetes YAML, vendored upstream manifests |
| `results/<run-id>/` | Raw evidence: commands, probes, journals, per-scenario `result.json` |
| `reports/` | Generated summaries and `creator_review.md` |

## Statuses

| Status | Meaning |
|---|---|
| PASS | Every step passed, and the scenario recorded measured values and evidence |
| FAIL | Observed product behaviour contradicted an expectation |
| UNSUPPORTED | The product does not offer the capability, shown with evidence |
| BLOCKED | Setup failed, a claim tag did not link to a verified citation, or the harness errored |
| NOT_RUN | Not executed |

Pass rates use PASS + FAIL as the denominator. UNSUPPORTED and BLOCKED never count as passes.

## Rules the harness keeps

- Claims first. A scenario's `@claim_` tags must resolve to `claims/claims.yaml` entries whose quotes are found at the cited lines of the pinned docs ref, or the scenario is BLOCKED before it starts.
- Thresholds are registered in `config/thresholds.yaml` before a scenario first runs, each with its basis (a claim, or `hypothesis`). They are not tuned after a result.
- Assertions run from the workstation: the CLI, HTTP probes through the ingress, TCP probes and the hypervisor. Nothing runs inside the cluster to grade itself.
- Every command is logged with its exit code, timestamps and output (`commands.jsonl`), with tokens and keys redacted. Operator actions, manual migration steps and documentation deviations are recorded separately.
- Destructive calls check `config/safety.yaml` first: only `rbeval-*` domains, the `rbeval` network and pool, `rbeval-*` containers and kind clusters.

## Results

Each run writes `results/<run-id>/results.json` (one record per scenario), `summary.json` with counts and denominators, and JUnit XML. `reports/runs/<run-id>/summary.md` renders them per axis with a claim traceability table. Run ids starting with `_` are development runs and stay out of Git.

## Limitations

- One lab, one hypervisor, one Linux distribution (Ubuntu 24.04 cloud image). No macOS, Lima or rootless coverage.
- Timings include CLI round trips from the workstation, sampled at 0.5 to 2 seconds.
- Power-cut scenarios cut power at the hypervisor at a random point; they can miss a narrow corruption window rather than prove it closed.
- Some extension points are judged by the absence of a documented interface. Those results carry medium confidence.
- `operator_cidrs` goes in every node's config, where the guide puts it on Node 1 only; see `DEVIATIONS` in `harness/cluster.py` for this and the other departures from the guide.

## Known harness issues

These make a row report FAIL for a reason in the harness, not the product. They stay as they were when the recorded runs ran, so no check changed after its result was seen.

- VIS-002 dashboard: the check requests the printed launch link and expects 200. The link answers 303 and sets a session cookie, so the check has to follow it with that cookie.
- EXT-002 `wtf`: the check treats any non-zero exit as a failure, but `wtf` exits 2 when it reports warnings, as documented.
- OPS-003: each token is used as soon as `relish token create` returns. On v0.2.0 a new token is refused with 401 for about 3 seconds, so a row can fail on that delay before it reaches the role check. The docs state no bound for the delay, so no threshold was registered for it.

