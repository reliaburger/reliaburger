# Policy evaluation bench, 24 October 2026

What policy costs the apply path, per app, with 10, 100 and 1,000 policies in
force ([#1311](https://github.com/reliaburger/reliaburger/issues/1311), item
2). One run of `cargo bench --bench policy`
([`benches/policy.rs`](../../benches/policy.rs)), which `make bench` now
includes.

## Host

- Apple M1 Max, 10 cores, 64 GiB, macOS 15.7.3
- rustc 1.98.1, Criterion 0.8.2, the `bench` profile
- Source: `release-0-2-4` at `8c048ab2` plus the exit-test branch
  (`0-2-4/exit-test`), whose only change on this path is the bench itself
- A shared laptop. The load average was about 26 when the run started and 11
  when it finished, from other worktrees' builds. Criterion's intervals are
  tight, but treat the numbers as an upper bound for this machine, not a
  quiet-host baseline.

## What's measured

The set is the four policies of
[`examples/policies/platform.toml`](../../examples/policies/platform.toml),
which match every app, plus copies of the same four shapes scoped to other
teams' namespaces (`match = "app.namespace == 'team-0007'"`), up to the
count. The copies don't match the judged app, but each one's `match` still
runs, and a quarter of them carry a default. The app is in `shop`, pinned,
from a trusted registry, with a memory limit; it passes, and exactly one
field (`env.TEAM`) is filled. The bench asserts both before timing.

- `policy/compile/N`: `PolicySet::compile`, which runs every policy's commit
  check and compiles its CEL. Once per `relish apply`; GitOps keeps the
  compiled set for a sync while the policy generation holds.
- `policy/judge_app/N`: `judge_app` on one app: every matching default, then
  validation of the filled spec, then every `match` and `deny`. Once per app.

## Results

Criterion's estimate, with its 95 % interval.

| Policies | Compile the set | Judge one app | Apps judged per second, one core |
|---------:|----------------:|--------------:|---------------------------------:|
| 10 | 1.21 ms (1.204–1.211) | 69.4 µs (69.37–69.49) | about 14,400 |
| 100 | 11.5 ms (11.46–11.62) | 248 µs (247.8–248.3) | about 4,000 |
| 1,000 | 115 ms (114.6–115.2) | 2.06 ms (2.052–2.070) | about 485 |

## Reading it

- Both costs grow linearly with the number of policies: about 115 µs per
  policy to compile, and about 2 µs per policy per app to judge once the
  fixed cost of the four matching policies is paid.
- At 1,000 policies an apply of one app pays about 117 ms, almost all of it
  compiling. A 100-app GitOps sync pays the compile once and about 206 ms of
  judging. Neither runs on the agent loop.
- Compiling repeats the commit checks, including a default's round trip
  through `AppSpec`, on every apply. A leader that kept the compiled set per
  policy generation for `relish apply`, as GitOps does, would pay it once per
  policy change. That isn't needed at the sizes we expect (tens of
  policies), so it's a possible follow-up, not a fix.
- The four platform policies dominate the 10-policy figure. Two of them use
  `matches`, CEL's regular expressions; we haven't profiled how much of the
  69 µs is regex work.
