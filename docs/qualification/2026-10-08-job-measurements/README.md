# Raw job measurements and runtime recovery evidence

These raw measurements use development protocol 49/state 66 and the previous
`isolation` field. Keep the evidence unchanged. For protocol 50/state 67, use
[the explicit runtime revision](../2026-10-09-job-runtime-revision/README.md)
and its current recorder manifests.


These are development results from 8 October 2026, not daily throughput
qualification. Every report retains actual accepted counts and failures. No
recording compresses wall-clock pauses. See the [evidence narrative](../2026-10-07-reusable-job-executors.md)
for failed experiments, fixes and differences between execution contracts.

`debug-baselines/` contains five serial paths for the same 1,000 pinned BusyBox
commands on the four-vCPU, 8 GiB Ubuntu 24.04 aarch64 VM. `public-debug/` adds
complete public dispatch. The durable fresh baseline and timed-out public fresh
run are failed evidence. Bare processes have no container isolation, ownership
journal or hard resource limits. Direct paths omit Bun's live namespace-policy
hook. The whole gap cannot be labelled scheduler overhead.

`release-recovery/` records an actual persistent single-node Bun SIGKILL after
partial accepted completion, with active reusable commands. All 96 mixed-resource
commands completed. The original application PID survived; TLS-verified stale
control returned no slots. Reproduce on an explicitly task-owned Linux cluster:

```sh
python3 scripts/demo/verify-job-recovery.py mixed.toml \
  --bun /absolute/immutable/bun --relish /absolute/immutable/relish \
  --config /absolute/node.toml --pid-file /absolute/bun.pid \
  --service-url http://application-address/ --service-name live-service \
  --node-name qualification-node --output /absolute/new-proof-directory
```

Provide ordinary `RELIABURGER_ENDPOINT`, `RELIABURGER_CA_CERT` and
`RELIABURGER_TOKEN` credentials privately. Keep the immutable Bun and original
journals until the corresponding workloads have positively retired.

`three-node-loss/` records 384 commands across small (100m / 32 MiB) and large
(1 CPU / 64 MiB) resource profiles on three actual Linux VMs, each 2 vCPU / 2 GiB.
The public CLI stopped worker 3 after partial completion with verified active
work. The remaining workers accepted all 384 successes, with no terminal
failures. Indexed first/middle/last outcomes were checked for both cohorts.
The `hello` application retained PID 2629 on node 1 and returned HTTP 200 during
observation. Worker 3 returned with a new boot identity; its five original
executor intents were positively retired and its namespace journal was empty.
These are at-least-once executions: accepted successes count unique indexes;
replayed work does not imply exactly-once external side effects. A local VM
stop/start does not simulate every network partition or delayed message.

The saved driver recreates this fixture through public commands. It deliberately
names `feature654` and stops its third VM. Run only against a fresh, task-owned
cluster with the same name and an application on node 1:

```sh
export RELIABURGER_HOME=/absolute/task-owned-context
export RELIABURGER_SOURCE=/absolute/reliaburger-checkout
export RELIABURGER_RELISH=/absolute/relish
relish setup --quickstart --name feature654 --nodes 3 \
  --api-port 31917 --ingress-port 38080 --registry-port 35050 \
  --development-binaries /absolute/matching-linux-binaries
export RELIABURGER_PROOF_OUTPUT=/absolute/new-proof-directory
python3 docs/qualification/2026-10-08-job-measurements/three-node-loss/reproduce.py
```

The quickstart workflow deployed `hello` and verified ingress before submission.
Its image cache began cold; this proof is a correctness experiment, not a
performance comparison. It overlapped the optimised public fresh-path diagnostic
on another VM on the same laptop. Do not use its elapsed time as isolated
benchmark evidence.


`release-baselines/` contains optimised matched 1,000-command paths and a
separate 100,000-process repetition. `release-public/` contains actual full
public retained runs at 1,000, 10,000 and 50,000 commands, plus the failed fresh
comparison. The published 50,000 recording is copied byte-for-byte from that
last directory. Selected Bun RSS reached 225,705,984 bytes; whole-data scans were
all incomplete at their 4,096-entry cap and cannot prove disk bounds.

The 50,000-command manifest can be submitted on a task-owned rootful Linux
cluster with matching binaries and sufficient resources:

```sh
python3 scripts/demo/measure-jobs.py \
  docs/qualification/2026-10-08-job-measurements/release-public/release-public-reused-50000/workload.toml \
  --relish /absolute/relish --service-url http://concurrent-application/ \
  --warmth warm-images-cold-executors --output /absolute/new-measurement
```

Warm the pinned image through the ordinary runtime before measuring, and allow
idle helpers to expire. The workload requests 100m / 32 MiB, limits CPU to one
core, disables swap, permits one attempt, and asks for concurrency 27. Actual
shared-node admission remains authoritative. Keep other benchmarks and
compilation stopped. The optimised serial direct reports use the same command,
concurrency and container resources but different stated durability/isolation
contracts; their image preparation is excluded and recorded separately.
See the manual for the direct driver and use a distinct node subnet when running
it beside another runtime allocator.
