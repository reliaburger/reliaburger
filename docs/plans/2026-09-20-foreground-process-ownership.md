# Foreground process ownership for 0.1.0

The supported workload stays in the foreground and keeps its descendants in
the supervised process group. Daemonising, changing group/session and handing
work to an external service manager require Linux containers. The user approved
this scope on 20 September. It does not relax crash-recovery or cleanup evidence.

## Ownership before execution

A PID and a timestamp are observations, not a retained kernel identity. Bun
must not reconstruct signal authority from them after a restart. A small
single-threaded owner process retains the actual child until retirement. It
runs through a hidden Bun subcommand before Tokio starts, so no unrelated task
can reap that child.

The owner locks and reloads a private generation record, then starts a child
behind an execution gate. It persists the child's identity with file and
directory sync before sending activation over a private pipe. The gate checks
that its own PID matches that record before replacing itself with user code.
EOF or mismatched activation refuses execution. A duplicate owner cannot acquire
the live generation; a completed generation cannot execute again.

Control uses a private Unix socket and a generation capability. Requests are
bounded in size and time. Only the live owner signals its unreaped child/group.
Malformed, abandoned and stale-generation clients do not terminate the owner.
Recorded PIDs remain informational to every other process.

## Completion evidence

The owner observes exit without reaping the root first. Linux uses a subreaper
to adopt orphaned descendants, kill owned children and reap until the kernel
reports no children. A process-list snapshot alone never proves completion.
On macOS, the owner retains the root while taking a complete process-group
snapshot that includes zombies. A full buffer triggers a larger retry. Only
the retained root remaining permits its final reap.

The root's actual exit code is recorded only after supported descendants have
gone. A signal has no exit code. Failed inspection or persistence never publishes
retirement. If the helper itself dies after activation, a replacement must
retain uncertainty; disappearance of a socket or lock is not proof that the
workload stopped. The foreground contract is cooperative supervision, not a
security boundary against hostile same-user processes.

## Delivery and remaining integration

- [x] Add the internal owner, execution gate and nine actual-binary regressions.
  The original four tests fail before implementation. Coverage includes short
  jobs without agent adoption records, surviving children, wrong generations,
  duplicate/retired owners, malformed/stalled clients, absent/mismatched gate
  activation and failed terminal persistence. Native CI-profile tests pass;
  Linux tests and strict Clippy pass. A macOS loader probe measured 5.88 seconds
  before startup of the large debug binary; the fixture allows 15 seconds for
  startup and separately requires descendant retirement within two seconds
  after an explicit parent-exit trigger.
- [ ] Persist agent/runtime launch intent and an unpredictable generation before
  starting the owner; retain the record across caller cancellation. Keep Unix
  socket paths short and private even when the data-directory path is long.
- [x] Add the explicit `ProcessGrill::with_owner` adapter for start, status, stop,
  kill, logs and adoption through the owner. Three recovery regressions fail
  against the old adapter before implementation. Nine adapter regressions and
  all nine owner tests pass on macOS/Linux. Coverage includes long paths,
  cancelled launch callers, stale delayed helpers, damaged intent and owner
  loss. Positive absence evidence permits recovery after failed socket cleanup;
  a missing Running owner never permits a recovered-PID signal.
- [x] Publish first intent atomically from a private, synced staging directory.
  An oversized-record regression fails before the fix; all ten recovery cases
  and strict Clippy pass on macOS/Linux afterwards. The operation lock spans
  rename and parent sync; start
  re-establishes parent durability before execution. Existing malformed entries
  refuse instead of becoming fresh generations.
- [x] Reparent durable helpers through a short bootstrapper, and reap that
  bootstrapper before acknowledging start. A real parent-`exec` regression fails
  first with an unreaped child. All eleven runtime recovery and nine owner
  cases plus strict Clippy pass on macOS/Linux afterwards; user workloads remain
  foreground-only.
- [x] Preserve diagnostic log access after owner loss. The regression fails first
  because log reads required live control; all eleven recovery tests and strict
  Clippy pass on macOS/Linux after separating validated file reads from signalling.
- [ ] Select the owned adapter in production Bun only alongside complete
  pre-adoption reconciliation. Distinguish the latest job attempt from older
  completed runtime generations.
- [ ] Discover generations before an agent adoption record exists, distinguish
  provably unactivated preparation from uncertain activated owners, and join
  recovered state to app/job cleanup obligations. Advance state compatibility
  when persistent production records change.
- [ ] Qualify actual Bun death at the preparation/activation/adoption boundaries,
  helper loss, short jobs, cron and complete group retirement on macOS/Linux.
  Re-run the runtime, agent, job-recovery and upgrade suites.

The helper and explicit runtime adapter are implemented. Bun still selects its
existing launch/adoption path until the production integration items land. C34 and the
0.1.0 release qualification gates remain open.
