# Raw job measurements and runtime recovery evidence (superseded)

These development measurements from 8 October 2026 used protocol 49/state 66
and the earlier `isolation` field. The [explicit runtime revision](../2026-10-09-job-runtime-revision/README.md)
and then the [timed job scenarios](../2026-10-09-timed-job-scenarios/README.md)
replaced them; don't resubmit these manifests. The [evidence narrative](../2026-10-07-reusable-job-executors.md)
explains the failed experiments and fixes along the way.

Two correctness results still hold, as at-least-once outcomes:

- **Bun crash with active reusable commands.** A persistent single-node Bun
  received SIGKILL after partial accepted completion. All 96 mixed-resource
  commands completed, the original application kept its PID, and stale control
  returned no slots.
- **Losing one of three workers.** 384 commands across a 100m / 32 MiB and a
  1 CPU / 64 MiB profile ran on three 2 vCPU / 2 GiB VMs. The CLI stopped worker
  3 mid-run. The other two accepted all 384 successes with no terminal failures,
  the `hello` application kept serving, and worker 3's executor intents were
  positively retired when it came back.

The throughput numbers in this generation aren't comparable with later ones:
the baselines were serial debug builds, and the public runs used different
counts and durability contracts. The raw samples, logs, casts and reproduction
drivers were removed from the tree to keep the repository small. They remain
in git history at
[006aca5f](https://github.com/reliaburger/reliaburger/tree/006aca5f9157a3b9432752470d507ca6ecb7e0d5/docs/qualification/2026-10-08-job-measurements).
