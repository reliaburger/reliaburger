# Lease-owned managed test storage

Implement separately from ordinary-job recovery. C34 remains open until both
storage cleanup and physical runtime ownership have been qualified.

Test namespaces are server selected and cannot be deployed without their lease.
Record managed-volume provisioning intent before creating directories, loop
images/mounts or Btrfs subvolumes. Ownership must survive a crash at every
provisioning boundary. Refuse existing unowned paths, symlinked parents,
malformed journals and unsupported backends; never guess that an arbitrary
application directory belongs to a test.

A normal Stop or rescheduling retirement preserves data, even for a test whose
lease remains active. Add an explicit lease-retirement operation. Standalone
cleanup calls it under the lease operation guard. Cluster reconciliations call
it only for an exact committed lease retirement, before persisting the placement
acknowledgement. Confirm all workloads are stopped before storage removal;
keep the lease and storage journal after any failed unmount, deletion or sync.
Operator decommission can discharge an unavailable node's obligations under the
already-approved fencing contract.

Retire only owned paths. Plain directories, loop-backed ext4 and Btrfs each need
the appropriate checked teardown; a busy mount must refuse without lazy unmount
or deletion of live data. Remove sidecars and backing images only after their
mount is gone. Preserve every ordinary application volume and host-source mount.
Generated configuration and any supported test snapshots need explicit ownership
or admission refusal before claiming complete test-storage retirement.

Tests first: actual lease cleanup removes its volume while preserving another
namespace; Stop/redeploy preserves the marker; failed runtime stop preserves
storage; pre-existing data, symlink and malformed intent refuse. Reopen the
journal after interrupted preparation/cleanup. Privileged Linux checks real
loop and Btrfs teardown, busy mounts and process death. Real three-node lease
expiry and former-placement cleanup must confirm storage before releasing the
lease. Document the Rust state model in chapters 5 and 15 and update both
READMEs, the completion plan and progress ledger.

Implementation and evidence
---------------------------

The storage family is implemented with protocol 8/state 12. Lease schema 4 is
unchanged. Provisioning claims managed volumes and generated configuration before
creation; ready entries reopen without formatting, while incomplete/retiring
entries refuse reuse. Runtime retirement precedes storage removal. Local cleanup
and exact cluster lease retirements use the new command; ordinary Stop and
placement withdrawal preserve data. Test snapshots refuse before mutation and
are excluded from automatic discovery.

The initial running-agent test failed because lease cleanup left its data
behind. Two further regressions failed on duplicate journal entries and inline
configuration symlinks. All pass after the fixes. Full library checkpoints pass
3,378 tests on macOS (five explicit gates) and 3,430 on Linux (19 explicit gates),
followed by the final 20 volume tests and confirmed-runtime-retirement regression.
Strict all-target/all-feature Clippy and both actual-binary compatibility tests
pass on both platforms. Four privileged Linux tests pass: real Btrfs deletion,
busy ext4 refusal/retry, unexpected nested-mount refusal and physical provisioning
owner SIGKILL/reopen/retirement (0.49s together).

The three-node test scales a leased application down from three replicas, checks
former owners retain their data, then allows the lease to expire without a
client cleanup request. One worker's unexpected snapshot directory blocks its
retirement while other owners clean up. Repairing it allows the server reaper to
finish; ordinary data survives. The macOS run passes in 25.70s and the Linux run
in 25.91s. Two overlapping manual Linux invocations then competed for the same
fixed fixture ports; that bind failure was before any storage operation and is
followed by an isolated final rerun which passes in 25.58s.

The separate C34 runtime/discovery crash window before the first adoption record,
atomic process identity, complete process-tree retirement and registry leases
remain open. These tests do not close those gates or replace exact signed-release
qualification. Hosted CI is a further gate; the prior commit's macOS job-crash
cleanup timeout is being investigated independently.
