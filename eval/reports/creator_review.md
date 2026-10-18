# Reliaburger v0.2.0: evaluation for the creators

Run `v0.2.0-2026-10-18`, 18 October 2026. Raw results: [`results/v0.2.0-2026-10-18/`](../results/v0.2.0-2026-10-18/). Rendered tables: [`reports/runs/v0.2.0-2026-10-18/summary.md`](runs/v0.2.0-2026-10-18/summary.md).

## Summary

| PASS | FAIL | UNSUPPORTED | BLOCKED | NOT_RUN | total |
|---|---|---|---|---|---|
| 57 | 18 | 4 | 7 | 0 | 86 |

Pass rate of executed scenarios (PASS + FAIL): 57 of 75. After triage, 10 of the 18 FAILs are product behaviour, 6 come from harness defects, and 2 are inconclusive. All 7 BLOCKED rows share one product cause: `relish token create` is not forwarded to the leader. One further result is left out of this public copy and went to the maintainers privately, as `CONTRIBUTING.md` asks.

The install guide works as written, the built-ins work with nothing added, Kubernetes manifests mostly import, and node loss, partitions and power cuts are survived. The weak spots are lifecycle edges (re-apply after delete, rolling updates), CLI consistency, and importer gaps that are not reported.

## Method

- Product: v0.2.0, commit `cc5f64393350f2081f7d88512b95a88316172e5a`, the published release binaries, checked against their `SHA256SUMS`.
- Lab: three libvirt VMs (Ubuntu 24.04 cloud image, 4 vCPU, 6 GiB each) on their own NAT network. Installed per `docs/linux-servers.md`, with the departures listed in `DEVIATIONS` in `harness/cluster.py`. The main one: `operator_cidrs` is set on every node, so the workstation can watch survivors when node 1 fails.
- Assertions run from the workstation: `relish`, HTTP probes through the ingress, TCP probes and the hypervisor.
- Each scenario cites claims whose quotes were verified at the cited lines of the v0.2.0 docs before it ran (69 claims). Thresholds were registered with their basis before the first run and not changed afterwards.
- Three seed claims also cited a privately shared statement of intent during the run. Those citations are removed from this copy; each claim keeps its public sources.
- Statuses: PASS, FAIL (product contradicted an expectation), UNSUPPORTED (capability absent, with evidence), BLOCKED (setup failed), NOT_RUN.

## Results by axis

| Axis | PASS | FAIL | UNSUPPORTED | BLOCKED |
|---|---|---|---|---|
| Vision | 7 | 2 | 0 | 0 |
| Extensibility | 14 | 8 | 4 | 0 |
| Interoperability | 19 | 2 | 0 | 0 |
| Operations | 17 | 6 | 0 | 7 |

## Strengths

- Install: three nodes from clean VMs to a serving two-service app in one pass; the app was ready 27 s after apply, and name resolution between services worked 10 of 10 times (VIS-001).
- Built-ins with nothing added: secrets, the Pickle registry, metrics, `relish top`, logs and task arrays (VIS-002).
- Alert webhooks: one signed `job_run_failed` alert, delivered in 10.5 s, with a valid signature (EXT-003). OpenTelemetry export from an app through its own config: 100 of 100 requests attributed (EXT-001).
- Kubernetes: the migration fixture and 17 single-kind imports pass (INT-001, INT-002). Export back to Kubernetes passes a server-side dry run and rolls out (INT-004).
- Perimeter: the API, gossip, Raft and reporting ports behave as documented for the operator and for a non-member (OPS-004).
- Chaos safety rails: 5 of 6 rows refused or allowed exactly as documented (OPS-005). The sixth never reached the rail (see below).
- Resilience: a minority partition kept serving and rejoined in 2.5 s (OPS-007); three random power cuts each rejoined (OPS-008); an unhealthy rollout rolled back in 72 s without the bad version ever serving (OPS-011); re-apply after a settled delete converged 5 of 5 times (OPS-002 settled).

## Reproducible product failures

| Area | Row | Observed |
|---|---|---|
| Lifecycle | OPS-002 immediate | `relish delete` then an immediate `relish apply` of the same app wedged retirement ("original service withdrawal is unproven"); 3 of 4 cycles converged, then the fourth stuck until bun was restarted. Same result in 2 of 2 development runs. |
| Rolling update | OPS-010 | A healthy change: old frontends were `stopping` on all three nodes while the new ones were `starting`, and every ingress returned 503 for about 3 s (availability 0.77). The manual promises one at a time with `max_unavailable = 0`. OPS-002's wedge ran just before this row, so contamination is not ruled out. |
| Tokens | OPS-003 (7 BLOCKED), OPS-005 read-only row | `relish token create` against a follower returns `503 not the leader`. `apply` is forwarded to the leader (`src/bun/api/apply.rs`); `token_create_handler` is not. No role check could run. A development run also saw a new token refused with 401 for about 3 s. |
| CLI | EXT-002 | `--output json` is ignored by `inspect`, `routes`, `resolve` and `fault list`; 11 other read commands honour it. |
| Audit | VIS-002 audit | Applying a file with two apps records `app.applied @default` with no app name; the manual says each record carries the app it touched. |
| Log export | EXT-007 | `relish logs-export --dest <dir>` prints "exported 12 file(s) to <workstation dir>/node-01", but the files go to that path on the agent host. The manual's example reads them locally with `logs-search`. |
| Import | INT-003 Online Boutique | The importer keeps an init container's image and command but drops its `env`, so `loadgenerator`'s init loop never finds the frontend. The migration report does not mention it, which contradicts "drops reported, not silent". The frontend also listens on 8080 where upstream maps 80. |
| TLS | EXT-004, EXT-006 | Python 3.13 refuses the node certificate: "Missing Authority Key Identifier". Strict X.509 clients cannot talk to the API without turning verification down. |

## Harness defects

These rows say nothing about the product. They stay as recorded, because no check was changed after its result was seen.

| Row | Defect |
|---|---|
| VIS-002 dashboard | Requests the launch link and expects 200; the link answers 303 with a session cookie that the check does not follow. |
| EXT-005 admission | FAILs on any doc hit for "admission" or "validat"; the hits were `relish lint` and an unrelated sentence. It should be UNSUPPORTED. |
| EXT-006, EXT-004 | The Python client's strict TLS (see above) stopped every request, so `/v1/events` and the metrics paths were never reached. The source has no `/metrics` or `/v1/read` route, so UNSUPPORTED is likely right but unproven by this run. |
| EXT-008 GitOps | The app was committed under `apps/`, so GitOps placed it in namespace `apps` (documented directory namespaces) while the check looked in `default`. Bun fetched and applied the commit. |
| INT-003 podinfo | The vendored manifest lost a `---` separator inside `reconciler-rbac.yaml`, so it is not the unmodified upstream it claims to be. |
| OPS-009 power cycle | Takes one snapshot 33 s after power-on instead of waiting out the 300 s budget; whether the apps would have converged is unknown. |

## Inconclusive

- OPS-006 leader loss: "election took 11.6 s" against a 5 s target from the whitepaper. Two 5 s CLI timeouts to the survivor came first, so the data cannot separate a slow election from a slow API.
- OPS-001 council-leader node stop: availability 0.94 against 0.95. Four 2 s probe timeouts 5 to 13 s after the kill are real, but the 0.95 threshold is a hypothesis with no doc behind it. The non-leader case scored 0.98 and passed.

## Observed in passing

- EXT-008: the first GitOps sync retired `backend` and `frontend`, which had been applied with `relish apply` outside GitOps (journal of node-03, 12:10:27 to 12:10:32; inferred from timing, not confirmed). The manual says deleting an app from the repository removes it, not that GitOps removes apps it never managed.
- Following the guide literally, only node 1 admits the operator, so `relish` from a laptop has nowhere to fail over when node 1 is down.

## Limitations

- One lab, one hypervisor, one distribution. No macOS, Lima or rootless runs.
- Timings include CLI round trips from the workstation, sampled at 0.5 to 2 s.
- Power cuts land at a random moment; three cycles can miss a narrow corruption window.
- Scenarios share one cluster in sequence; a wedge left by one row can affect the next (see OPS-010).
- The 0.2.1 candidate, which includes the redb power-cut fix, was not run.

## Adoption implications

For a small team whose needs match the built-ins, the install and day-to-day path work as documented. A team migrating from Kubernetes should expect manual review of init containers and port mappings, which the migration report does not flag. Teams that script against `relish` need the JSON contract to cover every read command, and RBAC automation needs `token create` to work against any node.

## Recommendations

1. Fix the delete-then-apply wedge and add a regression that re-applies the same app immediately after delete.
2. Hold old replicas until their replacements are ready during a healthy rolling update, and test it under ingress load.
3. Forward `token create` (and any other leader-only write) the way `apply` is forwarded.
4. Honour `--output json` in every read command, and add a test that runs each one.
5. Report every importer field that is not carried over, starting with init container `env`.
6. Name the app in apply audit records when one request carries several apps.
7. Decide where `logs-export --dest` writes, then make the CLI message and the manual agree.
8. Add the Authority Key Identifier extension to issued certificates.

## Claims not tested

- Upgrades between releases and the 0.2.1 redb fix.
- Behaviour at the 200-node end of the stated range; the lab has three nodes.
- macOS and Apple Container, Lima, and rootless runc.
- Custom resource kinds, CLI plugins and admission hooks: no documented interface exists, so these were checked for absence only.
