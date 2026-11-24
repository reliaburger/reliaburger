# Plan: mount isolation for host workloads

[#714](https://github.com/reliaburger/reliaburger/issues/714), and the wider
gap behind it. Linux turns `[process_workloads] mount_isolation` on by
default, and nothing implements it. The issue only talks about task arrays,
and it says ordinary process workloads already run in a private mount
namespace. They don't. Every host workload is refused under isolation: apps,
single jobs and task arrays. This plan decides what the isolated view is,
where it's built, which refusals go, and which tests prove it. It's a plan,
not code, and it ends with the questions the maintainer needs to answer.

Line references are to `main` at
[`b78fc184`](https://github.com/reliaburger/reliaburger/tree/b78fc18419462921c0c2461b07c5ed21ea8d2ce9)
on 24 November 2026.

## What's there today

The setting exists, and the default is on wherever it could matter:
`ProcessWorkloadsConfig::default` in
[`src/config/process_workloads.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/config/process_workloads.rs)
sets `mount_isolation: cfg!(target_os = "linux")`. Its doc comment says what
happens next: "Not implemented yet (#360). When true, Bun refuses `exec` and
`script` workloads rather than run them without the isolation, and doesn't
advertise host workloads to the scheduler."

That refusal lives in four places:

- **Apps.** `admit_app` in
  [`src/bun/supervisor.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/supervisor.rs#L506-L527)
  refuses an `exec` or `script` app when `mount_isolation` is on, with a
  comment that says plainly the namespace is "NOT implemented: the process is
  spawned with no namespace of its own".
- **Jobs and arrays.** `capability_refusal` in
  [`src/bun/task_array_node.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/task_array_node.rs#L231-L240)
  returns "host jobs don't run under [process_workloads] mount_isolation yet",
  with `TODO(#714)`. The leader runs the same function against each node's
  gossiped `JobCapabilities`
  ([`src/meat/job_capability.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/meat/job_capability.rs#L37-L39)),
  so it refuses a run at the door when no node can take it.
- **Advertising.** `host_workloads_enabled` in
  [`src/bin/bun.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bin/bun.rs#L4255-L4264)
  and `process_capability` in
  [`src/bun/supervisor.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/supervisor.rs#L248-L259)
  both say "no host work" when isolation is on. The first feeds
  `statics.process_workloads` in
  [`src/bun/capabilities.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/capabilities.rs#L694-L699),
  which is what makes `relish test` skip its host `process-workloads` and
  `jobs` cases. The second becomes `ProcessWorkloadCapability.enabled`
  ([`src/meat/cluster_state.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/meat/cluster_state.rs#L57-L69)),
  which the scheduler's filter reads before it places an `exec` or `script`
  app.
- **Messages.** The leader's own wording for a run no node can take, in
  `job_events.rs`, `task_array_leader.rs` and `task_array_store.rs`, and the
  CLI test that pins it in `src/relish/commands.rs`.

Nothing builds a namespace on the spawn side either:

- **The process owner's gate.** Every production host launch goes through
  `ProcessGrill::with_owner` (`src/grill/mod.rs` and `src/bin/bun.rs`): a
  durable `__process-owner` helper whose child runs `run_execution_gate` in
  [`src/grill/process_owner.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/grill/process_owner.rs#L548-L612).
  The gate waits for activation, checks the durable record, joins the
  workload cgroup, raises `oom_score_adj` and `exec`s the command. No
  `unshare`, no mounts.
- **The native host executor.** `child_setup` in
  [`src/bun/reusable_executor/helper.c`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/reusable_executor/helper.c#L185-L222)
  has a private-scratch block (`unshare(CLONE_NEWNS | CLONE_NEWIPC)`, a
  recursive `MS_PRIVATE` on `/`, a 16 MiB tmpfs on `/tmp` and `/dev/shm`, a
  read-only `/dev`) that only the container build runs. `RB_EXECUTOR_HOST`
  skips it, and the host helper drops `CAP_SYS_ADMIN` on purpose in `main`:
  "On the host these would be real initial-namespace privileges".
- **`NodeRunner::Process`.** `ProcessRunner` in
  [`src/bun/task_executor.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/task_executor.rs#L276-L340)
  spawns each attempt straight from Tokio. Production never builds it (Bun
  always uses `NodeRunner::Owned`); tests, `benches/task_arrays.rs` and
  `examples/job-throughput.rs` do.

So on a default Linux node, no host workload runs. Nodes that
`relish setup --ssh` installs get the default, but they're off for two more
reasons too: the unit it writes
([`provision::SERVICE`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/relish/quickstart/provision.rs#L204))
starts `bun --runtime runc`, which is container-only, and the `node.toml` it
writes has no `allowed_binaries`. Turning isolation off wouldn't make
host work run there. That matters when we get to the defaults.

The investigation on 24 November estimated 3–5 days. This plan agrees,
for the view below without the optional parts. Each optional part costs
about a day more.

## Who this is for

Host workloads exist for work that has to see the host: a log shipper that
reads `/var/log`, a backup agent, a build tool that uses the host's
compilers, a GPU job before 0.6.0, and the throughput story's host batches
(1.81 billion tasks in a day in 0.2.3). The operator has already said they
trust the binary, by allowlisting its absolute path. What they haven't said
is that it should see Bun's own keys, every other workload's environment and
temp files, and the volumes of the containers next to it. Today, with
isolation off, it does.

## 1. What the isolated view is

### The threat, honestly

A host workload runs as root with Bun's full capability set. That's the
"existing Bun-user privilege contract" the helper's comments name. A mount
namespace changes what paths resolve to. It doesn't change who you are. So
we should be clear about what each part of this plan stops.

**What it stops** (a trusted binary that's buggy or careless, or a
compromised one that only uses paths):

- reading Bun's identity key, the node's `node.toml`, the council's data or
  the images by path;
- reading another host workload's `owner.json`, which holds that workload's
  complete resolved environment, decrypted secrets included, under
  `{storage.data}/instances`;
- reading or writing a container's managed volume under `storage.volumes`;
- one workload's `/tmp` files colliding with, or being read by, another's,
  and a stray `rm -rf /tmp/*` taking out its neighbours;
- writing into Bun's sockets under `/run/reliaburger` and the pinned eBPF
  maps under `/sys/fs/bpf`.

**What it doesn't stop**, with the view alone:

- `umount` of any mask. Root holds `CAP_SYS_ADMIN` in the namespace's owning
  user namespace (the initial one), so the masks aren't
  [locked](https://man7.org/linux/man-pages/man7/mount_namespaces.7.html)
  and come off with one system call;
- `setns` into `/proc/1/ns/mnt`, or reading `/proc/<pid>/root` and
  `/proc/<pid>/environ` of Bun or any other process. There's no PID
  namespace, and root with `CAP_SYS_PTRACE` passes the
  [ptrace access check](https://man7.org/linux/man-pages/man2/ptrace.2.html)
  for every process;
- opening the raw disk under `/dev` and reading the filesystem underneath;
- the network: Bun's API on loopback, other nodes, the internet. There's no
  network namespace for host processes, and `allow_from` and egress rules
  don't apply to them (manual 13 already says so);
- resource abuse beyond the limits native executors enforce.

That's why the plan offers a second, small step (option C below): drop a
short list of capabilities from the bounding set before `exec`. It closes the
first two escapes, because `umount` and `setns` need `CAP_SYS_ADMIN`, and
`/proc/<pid>/root` of a process with more capabilities than you needs
`CAP_SYS_PTRACE`
([capabilities(7)](https://man7.org/linux/man-pages/man7/capabilities.7.html),
[proc(5)](https://man7.org/linux/man-pages/man5/proc.5.html)). It leaves raw
disks, same-capability peers in `/proc` and the network. Even with it, this
isn't a sandbox. The manual should keep saying what it says now: if you don't
trust the binary, give the workload an image.

### Options compared

**Whose view: the host's root minus a denylist, or an allowlisted view?**

| | A. Host root, minus Bun's paths (recommended) | B. Allowlisted view |
|---|---|---|
| What the workload sees | Everything on the host except a list Bun computes | Only `/usr`, `/lib`, `/etc/ssl` and similar, read-only, plus what the operator grants |
| Who it suits | The reasons host workloads exist: they need the host | Workloads that could be containers |
| Breaks | Nothing that didn't already touch Bun's paths | Most real host tools (they read `/etc`, `/var`, `/home`, `/opt`) |
| New config | None | An `allow_paths` list per node or workload, which [agent-bun.md §5.2](../design/agent-bun.md#52-process-workload-lifecycle) step 8 sketches |
| Precedent | systemd's [`InaccessiblePaths=`, `PrivateTmp=`, `TemporaryFileSystem=`](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html) | Nomad's [`exec` driver](https://developer.hashicorp.com/nomad/docs/drivers/exec), a chroot built from `chroot_env`; [bubblewrap](https://github.com/containers/bubblewrap) |

B is the design doc's target, and it's a real sandbox's filesystem half. But
a workload that's happy in B is happy in a container, and we already have
containers. A is what an operator who allowlisted `/usr/bin/restic` actually
wants: the host, without Bun's secrets. Nomad draws the same line between
`exec` and [`raw_exec`](https://developer.hashicorp.com/nomad/docs/drivers/raw_exec),
and A sits between them.

**Propagation: `MS_REC|MS_PRIVATE` or `MS_REC|MS_SLAVE`?**

The container helper uses `MS_PRIVATE`, and the obvious move is to copy it.
It's the wrong one for the host's root. A private namespace gets a copy of
every mount that exists when it's made, and nothing the host does later
reaches it ([mount_namespaces(7)](https://man7.org/linux/man-pages/man7/mount_namespaces.7.html),
[shared subtrees](https://docs.kernel.org/filesystems/sharedsubtree.html)).
On a `runc+process` node Bun keeps mounting and unmounting container root
filesystems and managed volumes. A long-running host app would keep a private
copy of every one that existed when it started, the unmount in Bun's
namespace wouldn't reach it, and the filesystem underneath would stay busy:
a Btrfs subvolume that won't delete, a volume whose `size` can't be
re-applied. `MS_SLAVE` lets the host's mounts and unmounts flow in and nothing
flow out, which is what systemd does for its sandboxed units (`MountFlags=`
applies `slave` first, then `shared`).

Doesn't `slave` show the workload a new volume Bun mounts under a hidden
directory? No. The propagated mount lands on the namespace's copy of the
parent mount, beneath our mask. Since a 2017 kernel change, a mount propagated onto a
busy mountpoint is tucked under the mount already there
([commit 1064f874](https://github.com/torvalds/linux/commit/1064f874abc0d05eeed8993815f584d847b72486)),
so the mask stays on top. That's a claim we test, not one we trust (see
"Tests").

Recommended: `MS_REC|MS_SLAVE` on `/`.

**Private `/tmp` and `/dev/shm`.** Yes: a fresh tmpfs on `/tmp`, `/var/tmp`
and `/dev/shm`, `nosuid,nodev`, and `noexec` on `/dev/shm`, as the container
helper does. `/var/tmp` is in the list because systemd's `PrivateTmp=`
includes it and tools use it for exactly the files we're trying to keep apart.
Each tmpfs is per launch, so it's empty at start and gone at exit. Its pages
are charged to the memory cgroup of whoever writes them, so a workload with a
memory limit pays for its own temp files. Sizes are a question for the
maintainer (Q3). The container helper's 16 MiB is too small for a host app.

**Hiding paths: tmpfs over each, or bind-mask?**

- *tmpfs over a directory*: mount an empty `ro,nosuid,nodev,noexec,mode=000`
  tmpfs on it. No source on disk, nothing to keep safe, works for directories
  only. This is `TemporaryFileSystem=/path:ro`.
- *Bind-mask*: bind an empty, root-owned, mode-000 directory (or an empty
  file, for a file) over it. Works for files and directories alike, but the
  source has to exist outside everything we hide, and be one nobody can write
  to.

Recommended: tmpfs for directories, and a bind of `/dev/null` for the one
case that's a file (a `--config` outside `/etc/reliaburger`). Every path we
hide is a directory in the default layout.

**The hidden list**, computed by Bun from its own configuration, not typed by
the operator:

- `storage.data`, `images`, `logs`, `metrics` and `volumes` (by default under
  `/var/lib/reliaburger`; when they share a parent, the parent);
- `security.identity_dir` and the bootstrap file, when they're outside those;
- the directory of the `--config` file (`/etc/reliaburger` by default);
- `/run/reliaburger` (Bun's sockets, the host executors' control sockets);
- `/sys/fs/bpf` (the pinned Onion and Smoker maps);
- `/sys/fs/cgroup/reliaburger` (every other workload's cgroup, and its
  `cgroup.kill`).

Each path is resolved with symlinks followed before it's recorded, the same
way `allowed_host_paths` resolves its prefixes, so a symlinked data directory
can't dodge the list.

**Binding back the workload's own paths.** Some of what a workload needs is
under the hidden list:

- its own cgroup, under `/sys/fs/cgroup/reliaburger` (the native helper
  writes its PID into `helper/cgroup.procs` at start, and tools read their
  own limits there);
- for the native host helper, its executor directory (the helper binary
  itself is installed there by `start_host` in
  [`pool.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/reusable_executor/pool.rs#L604-L737))
  and its control socket in `/run/reliaburger/host-executors`.

There's a chicken-and-egg problem: once the mask is up, the source is hidden
too. So the gate takes a detached copy of each one *before* masking, with
`open_tree(OPEN_TREE_CLONE)`
([open_tree(2)](https://man7.org/linux/man-pages/man2/open_tree.2.html),
Linux 5.2, under our 5.8 floor), and attaches it back with `move_mount`
afterwards. A bind can be read-only (remounted `MS_RDONLY` once attached) or
read-write; the helper's directory is read-only, the cgroup read-write.

Host workloads have no volumes or working directory to bind back today.
`relish apply` refuses `volumes`, `config_file` and `working_dir` on an
`exec` or `script` app (manual 01, "What a field can't do"), and host jobs run in
`/`. This plan doesn't change that. The bind-back mechanism is the one a later
"host volumes" issue would use, which is a good reason to build it once,
generally. A host job's `cwd` that falls under a hidden path is refused at
admission, so the vfork child's `chdir` never meets one.

**The host's root: read-only or read-write?** Read-write (recommended). A
read-only root (`ProtectSystem=strict`) breaks the backup agent writing to
`/srv/backups` and the log shipper's checkpoint in `/var/lib/shipper`, and
the fix is a per-workload writable list: config we don't have and a second
design. The thing we're protecting is Bun's paths, and those are hidden, not
read-only.

### The view, in one place

Built by the gate, in this order, in a single-threaded process:

1. `unshare(CLONE_NEWNS)`.
2. `mount(NULL, "/", NULL, MS_REC | MS_SLAVE, NULL)`.
3. `open_tree(OPEN_TREE_CLONE)` each bind-back source.
4. tmpfs on `/tmp`, `/var/tmp` and `/dev/shm`.
5. A read-only mode-000 tmpfs on each hidden directory, `/dev/null` on each
   hidden file.
6. `move_mount` each bind-back to its path; remount the read-only ones.
7. Option C: drop the listed capabilities from the bounding set.
8. `exec`.

It runs after the gate joins the workload's cgroup, so tmpfs pages are
charged to the workload from the first byte.

## 2. The spawn paths to change

**One place builds the view: the process owner's gate.** Every host launch
in production already goes through it:

- host apps, and their image-less init containers (`ProcessGrill::start` →
  `ProcessControl::start` →
  [`__process-owner`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/grill/process_control.rs#L317-L412)
  → `run_execution_gate`);
- host jobs on the fresh path (ordinary `[job]` host commands, and process
  batches on kernels without executor support), each attempt a ProcessGrill
  instance of its own (`OwnedRunner` in `src/bun/task_runtime.rs`);
- the native host helper itself: `start_host` launches it through the host
  runtime's `launch`, so it's a ProcessGrill instance too.

That last one is the key. The helper's tasks are its `CLONE_VM|CLONE_VFORK`
children, and a child inherits its parent's mount namespace. If the gate
builds the view before it `exec`s the helper, every task the helper starts
runs in that view, and `task_child` doesn't change at all. It still only
`dup2`s, drops credentials, `chdir`s and `execvp`s.

Why not do it in `helper.c`'s `child_setup`, next to the container code?
Three reasons:

1. **Privilege.** `unshare(CLONE_NEWNS)` needs `CAP_SYS_ADMIN` in the
   initial user namespace for a host helper. `main` drops it on purpose.
   Giving it back would hand a long-lived process that talks to a socket a
   real host privilege, to save work done once per executor.
2. **The vfork child.** `unshare` and `mount` are plain system calls, so
   they'd be legal there, but computing the hidden list, resolving paths and
   handling errors in an async-signal-safe C child sharing the helper's
   memory is the hardest place in the codebase to get right.
3. **Throughput.** A namespace and a dozen mounts per task would cost every
   task; per executor, they cost once. The 0.2.3 day run averaged about
   21,000 tasks a second across three nodes.

The price is that tasks of one executor share one `/tmp`. An executor only
ever serves one job (`ExecutorKey::for_job`, and the test
`tasks_of_different_jobs_never_share_an_executor`), so that's tasks of the
same job, at the same trust. The manual says so.

**What changes, by file:**

- `src/grill/process_owner.rs`: `ProcessLaunch` gains a `mount_view:
  Option<MountView>`, with the hidden paths, the bind-backs and the tmpfs
  sizes. `activate_execution` builds it after `raise_oom_score`, before
  returning the record to `run_execution_gate`. `None` means isolation off,
  and the gate does exactly what it does today. The record's `schema` goes
  from 3 to 4. A failure is a `GateReport::Failed` with the step that failed,
  so `relish inspect` says "mount isolation: cannot hide /var/lib/reliaburger:
  …" rather than a bare exit code.
- New `src/grill/mount_view.rs`: the `MountView` type, its construction from
  `NodeConfig` (`MountView::for_node`), path resolution and the Linux system
  calls behind `#[cfg(target_os = "linux")]`. It's a module of plain
  functions over `nix`. There's one implementation, so no trait.
- `src/grill/process_control.rs`: `prepare_once` writes the view into the
  record. `ProcessGrill::with_owner` takes the node's `MountView`, or `None`,
  when Bun builds it in `src/bin/bun.rs`.
- `src/bun/reusable_executor/pool.rs`: `start_host` adds the executor
  directory, its control socket and its cgroup base to the view's bind-backs
  for that one launch. Nothing else in the pool changes.
- `src/grill/process_owner/exec.rs`: `relish exec` into a host app starts a
  nested owner whose gate would build a *fresh* view from the recorded spec,
  with its own empty `/tmp`. That's not what `docker exec` does. The nested
  gate instead joins the running workload's namespace: `pidfd_open` on the
  workload's root, then `setns(pidfd, CLONE_NEWNS)`, and skips the build.
  Stop hooks that run through exec get the same view as their app this way.
- `src/bun/reusable_executor/helper.c`: no functional change. The comment on
  `child_setup`'s host branch says where the host view comes from, and the
  host helper's `main` checks it's in a namespace other than PID 1's when its
  launch asked for isolation (a cheap guard against a gate regression).
- `NodeRunner::Process` (`ProcessRunner`): stays unisolated and keeps
  refusing under isolation. It's a test and benchmark runner; its
  `JobCapabilities` says isolation is unavailable.

The gate must be single-threaded for `unshare(CLONE_NEWNS)`, which fails with
`EINVAL` in a process whose threads share a filesystem context
([unshare(2)](https://man7.org/linux/man-pages/man2/unshare.2.html)). It is:
`__process-owner` runs before Tokio starts. A unit test pins that the gate
path never starts a thread.

## 3. Refusals to remove, and advertising to restore

The idea is one function, `mount_isolation_available()`, in
`src/grill/mount_view.rs`: Linux, Bun running as root, and the owned process
runtime. Every refusal turns from "isolation is on" into "isolation is on and
this node can't provide it".

| Where | Today | After |
|---|---|---|
| `admit_app` (`supervisor.rs`) | refuses every host app when isolation is on | refuses only when it's on and unavailable, saying why ("mount isolation needs Linux mount namespaces and a root Bun") |
| `capability_refusal` (`task_array_node.rs`) | refuses on `capabilities.mount_isolation` | refuses on `capabilities.mount_isolation_unavailable`; the `TODO(#714)` goes |
| `JobCapabilities.mount_isolation` (`meat/job_capability.rs`) | the node's setting | renamed `mount_isolation_unavailable`: set when isolation is on and the runner can't build it (macOS, `NodeRunner::Process`, a non-root Bun) |
| `job_capabilities` (`task_array_node.rs`) | copies the setting for every runner | `Owned` reports unavailable only when `mount_isolation_available()` is false; `Process` reports it whenever isolation is on |
| `host_workloads_enabled` (`bin/bun.rs`) | false under isolation | false only when isolation is on and unavailable |
| `process_capability` (`supervisor.rs`) and `ProcessWorkloadCapability.enabled` | false under isolation | the same rule, so the scheduler's `filter` places host apps on isolated nodes again |
| `statics.process_workloads` (`capabilities.rs`) | "allowlist is non-empty and mount_isolation is off" | "allowlist is non-empty and the node can honour its mount isolation setting" |
| `relish test` | skips host cases under isolation | runs them; skips only where isolation is unavailable |
| Leader messages (`job_events.rs`, `task_array_leader.rs`, `task_array_store.rs`, `relish/commands.rs` test) | "don't run under mount_isolation yet" | "this node can't provide [process_workloads] mount_isolation (it needs Linux and a root Bun)" |

Renaming a gossiped field is a wire change, and the record's `schema` moves
with the durable owner records, so `CURRENT` in `src/compatibility.rs` bumps
both `protocol` (61 to 62) and `state` (76 to 77). A cluster starts afresh,
as the pre-1.0 policy says. Bumping both in one release keeps it to one fresh
start.

## 4. Tests first

Every test below is written and seen failing before the code it covers.

### Unit tests (portable)

- `mount_view_hides_every_storage_directory_and_the_config_directory`:
  `MountView::for_node` on a `NodeConfig` with storage spread over three
  parents lists each, plus `/run/reliaburger`, `/sys/fs/bpf` and
  `/sys/fs/cgroup/reliaburger`.
- `mount_view_resolves_symlinks_before_it_records_a_path`, and
  `mount_view_collapses_paths_under_a_hidden_parent`.
- `a_config_file_outside_etc_reliaburger_is_hidden_as_a_file`.
- `a_job_cwd_under_a_hidden_path_is_refused_at_admission`.
- `owner_record_round_trips_its_mount_view`, and
  `an_owner_record_without_a_view_launches_as_before`.
- `capability_refusal_runs_host_tasks_under_available_isolation` and
  `capability_refusal_refuses_when_isolation_is_unavailable`, replacing
  `mount_isolation_keeps_task_arrays_off_the_node`.
- `admit_app` tests: `host_workload_requesting_mount_isolation_is_refused`
  becomes `host_workload_runs_under_available_mount_isolation` and
  `host_workload_is_refused_where_mount_isolation_is_unavailable`.
- `host_workloads_are_not_advertised_under_mount_isolation` in `bin/bun.rs`
  becomes `host_workloads_are_advertised_under_available_mount_isolation`.
- Gossip: the `protocol.rs` and `directory.rs` round-trip tests pick up the
  renamed field.
- `the_execution_gate_never_starts_a_thread`.

### Gated Linux tests (`make test-linux`)

A new `tests/host_mount_isolation.rs` binary, `#[ignore = "requires root and
cgroup v2; run with make test-linux"]`, with `binary(host_mount_isolation)`
added to the target's filter. Each test plants a file in a fake storage
directory and a marker in a second workload's `/tmp`, then checks what the
workload can see:

- `a_host_app_cannot_see_buns_storage_or_another_workloads_tmp`: an `exec`
  app of `/bin/sh` that `test -e`s each hidden path and the marker, and
  exits 0 only if none is there.
- `a_fresh_host_job_cannot_see_a_hidden_path`.
- `a_native_host_task_cannot_see_a_hidden_path` (a process batch, so it goes
  through the helper), and
  `tasks_of_one_native_executor_share_one_tmp_and_another_jobs_do_not`.
- `relish_exec_into_a_host_app_sees_the_apps_tmp`: the `setns` path.
- `a_volume_bun_mounts_after_the_workload_starts_stays_hidden`: the
  mount-tucking claim.
- `bun_can_unmount_a_volume_while_a_host_workload_runs`: the reason for
  `MS_SLAVE`; under `MS_PRIVATE` this one fails, and we should see it fail
  once.
- `a_workload_writes_to_the_host_root_outside_the_hidden_list`: read-write
  root.
- With option C: `an_isolated_root_workload_cannot_unmount_a_mask_or_setns`.

The cluster suites (`tests/cluster_task_arrays.rs`,
`tests/owned_task_arrays.rs`) set `mount_isolation: false` in about a dozen
places. Each one is either the point of the test (keep it) or there only
because isolation didn't work (drop it). One of the cluster tests flips:
a host array with isolation on, on three nodes, runs to the end.

`make test-cluster` runs those. `make test-upgrade` matters because of the
`state` bump. The jobs fast soak runs host batches with isolation on in its
next gate, which is where we learn what the view costs an executor start.

### macOS

There are no mount namespaces on macOS. `sandbox-exec` (Seatbelt) can deny
file reads by path, but Apple deprecated it years ago and it isn't a
supported interface. The default is already off there, and that's what stays.
An operator who sets `mount_isolation = true` on a Mac gets the refusal they
get today, worded as unavailability rather than "not implemented yet":
`host_workload_is_refused_where_mount_isolation_is_unavailable` runs on
every platform. `make test-apple` needs nothing new. Manual 13 says it in one
line.

## 5. Docs to update

In the same change, as `CLAUDE.md` asks:

- **Manual 01** ("Deploy an app"): the placement paragraph that says
  `mount_isolation` must be off, and the batch summary near the end. Add what
  an isolated host app sees, in four lines.
- **Manual 07** ("Diagnostics"): the `relish test` paragraph that says host
  cases skip under isolation, and its TOML snippet.
- **Manual 13** ("Reference"): the `[process_workloads]` row and "Host
  workloads require an explicit allowlist and `mount_isolation = false`".
- **Manual 14** ("Batch jobs"): the "Linux turns mount_isolation on" section
  and its example refusal; add the shared-`/tmp` note for native executors.
- **Design**:
  [agent-bun.md §5.2](../design/agent-bun.md#52-process-workload-lifecycle),
  its status note, step 3 and step 8 (the denylist view replaces the
  allowlisted one as what ships, with the allowlisted view recorded as the
  option we didn't take), and the threat table near the end;
  [scheduler-meat.md](../design/scheduler-meat.md) on
  `NodeCapabilities::process` and the array-loss detection paragraph.
- **Book chapter 17** ("A Million Jobs"): "A run nobody can take" uses
  `mount_isolation` as its example of a configuration refusal. It stays a
  good story, with a coda: the refusal became rare, and the code that built
  the view belongs in the chapter. That's where `open_tree`, `move_mount` and
  `setns` via a pidfd get explained for a reader who knows C, and where the
  `MS_PRIVATE` trap goes in "Lessons learned". Chapter 8 ("Breaking Things on
  Purpose"), which introduced process workloads, gets a short pointer.
- `src/config/process_workloads.rs`: the field's doc comment.
- `README.md`, `docs/README.md`, `docs/features.toml` and the website's
  feature pages, where they say host workloads need isolation off. The doc
  snippet checks (`tests/suite/doc_snippets.rs`) cover the TOML that changes.

## 6. Defaults

**Should `relish setup --ssh` keep isolation on?** Yes, once this lands. On
is the safe default, and after this plan it's also a default that works. The
honest picture, though, is that `--ssh` nodes run no host workloads for two
other reasons: `--runtime runc` and an empty allowlist. Both are right for a
cluster that someone set up in one command, and neither is this plan's
business. If the maintainer wants `--ssh` clusters to run host work out of
the box, that's a separate question (Q5): `--runtime runc+process` and an
allowlist would make host execution a default, which deny-by-default says
not to.

The same goes for every other Linux node: keep the default on. The
`mount_isolation = false` lines in `examples/node/process-runtime.toml`,
`examples/phase-8/`, the demo scripts, the soak and the gym were there to get
past the refusal. Most can go, so the examples show the default. The
throughput benchmark keeps `false` until the soak shows what the view costs.

## 7. Milestone

**Recommended: 0.3.3**, as its own PR on that train. The
[0.3.3 milestone](https://github.com/reliaburger/reliaburger/milestone/36) is
"loose ends before outside testers: tester-visible bugs", and this is one: a
tester who follows manual 14 on a default Linux node gets a refusal that
says "yet". It's a patch-sized change (one new module, one gate, no new
config) with a wire and state bump, and 0.3.2 already moved the wire in a
patch release.

**The alternative is 0.4.0**, if 0.3.3 is close to its freeze or its 36 open
issues are already more than a patch. 0.4.0's headline is planned
maintenance, and this doesn't belong to that story, so it would be an
"alongside" item. It shouldn't wait for "Stronger walls" (0.4.4 and 0.4.5):
those are VMs, a different boundary.

F02 ([#360](https://github.com/reliaburger/reliaburger/issues/360)) keeps the
rest of process isolation: PID, network and UTS namespaces, seccomp, the
`burger` user. This plan is the mount half of it, and #360 should say so
when this merges. The field's doc comment cites #360 today; it would cite
nothing afterwards.

## 8. Risks and open questions

### Risks

- **Mount leaks on busy nodes.** The reason for `MS_SLAVE`, and the test
  that pins it. If a kernel we support turns out to behave differently, the
  symptom is a volume or rootfs that won't go away while a host app runs.
- **Executor start time.** A namespace, three tmpfs and six or so masks per
  executor is a few hundred microseconds, against an executor that lives for
  thousands of tasks. The fresh path pays it per attempt, which matters only
  for non-native host jobs. The jobs fast soak measures both.
- **Paths we didn't think of.** The hidden list is Bun's own layout. An
  operator who points `storage.data` at a shared directory, or keeps a token
  in `/root`, gets no help from it. The manual says the list is what's
  hidden, and nothing else.
- **Workloads that relied on seeing Bun's paths.** A host log shipper that
  read `/var/lib/reliaburger/logs` directly breaks. That's the point, but it
  should be in the release notes.
- **A false sense of safety.** The word "isolation" promises more than this
  delivers. The manual's wording carries the threat section's two lists, and
  `relish wtf` keeps its `namespace-isolation` warning for host workloads.

### Questions for the maintainer

1. **Q1. Denylist view (A) or allowlisted view (B)?** Recommended A. B is
   the design doc's target, but it's the filesystem half of a sandbox, and a
   workload that wants it wants a container.
2. **Q2. Option C, the bounding-set drop?** Recommended yes:
   `CAP_SYS_ADMIN`, `CAP_SYS_PTRACE`, `CAP_SYS_MODULE`, `CAP_BPF` and
   `CAP_SYS_BOOT`, dropped in the gate after the view is built. Without it a
   mask is one `umount` from gone. With it, a host workload that mounts
   things, traces other processes or loads BPF needs `mount_isolation =
   false`, and the manual says so. About a day.
3. **Q3. Temp sizes.** Recommended 256 MiB for `/tmp` and `/var/tmp`
   together and 64 MiB for `/dev/shm`, as constants, charged to the
   workload's memory cgroup. Or sized from the workload's memory limit, when
   it has one.
4. **Q4. 0.3.3 or 0.4.0?** Recommended 0.3.3.
5. **Q5. Host work on `--ssh` clusters by default?** Recommended no: keep
   `--runtime runc` and an empty allowlist, and document the two lines an
   operator adds.
6. **Q6. Close #714 with this, and narrow #360?** Recommended: #714 closes
   (its "done when" is met, apps included), and #360's body names what's
   left.

## Sources

- Issues: [#714](https://github.com/reliaburger/reliaburger/issues/714)
  (host task arrays under isolation), [#360](https://github.com/reliaburger/reliaburger/issues/360)
  (F02, process-workload isolation), and the
  [0.3.3 milestone](https://github.com/reliaburger/reliaburger/milestone/36)
- Linux: [mount_namespaces(7)](https://man7.org/linux/man-pages/man7/mount_namespaces.7.html),
  [unshare(2)](https://man7.org/linux/man-pages/man2/unshare.2.html),
  [open_tree(2)](https://man7.org/linux/man-pages/man2/open_tree.2.html),
  [capabilities(7)](https://man7.org/linux/man-pages/man7/capabilities.7.html),
  [ptrace(2)](https://man7.org/linux/man-pages/man2/ptrace.2.html) (access
  mode checks), [proc(5)](https://man7.org/linux/man-pages/man5/proc.5.html),
  [Shared subtrees](https://docs.kernel.org/filesystems/sharedsubtree.html)
- Linux commit [1064f874](https://github.com/torvalds/linux/commit/1064f874abc0d05eeed8993815f584d847b72486),
  "mnt: Tuck mounts under others instead of creating shadow/side mounts"
- systemd: [systemd.exec(5)](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)
  (`PrivateTmp=`, `InaccessiblePaths=`, `TemporaryFileSystem=`, `MountFlags=`)
- Nomad: [`exec` driver](https://developer.hashicorp.com/nomad/docs/drivers/exec),
  [`raw_exec` driver](https://developer.hashicorp.com/nomad/docs/drivers/raw_exec)
- [bubblewrap](https://github.com/containers/bubblewrap)
- Reliaburger, at `main` on 24 November 2026
  ([`b78fc184`](https://github.com/reliaburger/reliaburger/tree/b78fc18419462921c0c2461b07c5ed21ea8d2ce9)):
  [`src/config/process_workloads.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/config/process_workloads.rs),
  [`src/bun/supervisor.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/supervisor.rs),
  [`src/bun/task_array_node.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/task_array_node.rs),
  [`src/bin/bun.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bin/bun.rs),
  [`src/bun/capabilities.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/capabilities.rs),
  [`src/meat/job_capability.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/meat/job_capability.rs),
  [`src/meat/cluster_state.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/meat/cluster_state.rs),
  [`src/grill/process_owner.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/grill/process_owner.rs),
  [`src/grill/process_owner/exec.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/grill/process_owner/exec.rs),
  [`src/grill/process_control.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/grill/process_control.rs),
  [`src/bun/reusable_executor/helper.c`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/reusable_executor/helper.c),
  [`src/bun/reusable_executor/pool.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/reusable_executor/pool.rs),
  [`src/bun/task_executor.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/bun/task_executor.rs),
  [`src/relish/quickstart/provision.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/relish/quickstart/provision.rs),
  [`src/compatibility.rs`](https://github.com/reliaburger/reliaburger/blob/b78fc18419462921c0c2461b07c5ed21ea8d2ce9/src/compatibility.rs)
- Reliaburger docs: [agent-bun.md §5.2](../design/agent-bun.md#52-process-workload-lifecycle),
  [scheduler-meat.md](../design/scheduler-meat.md),
  [manual 14, batch jobs](../manual/14_batch-jobs.md),
  [book chapter 17](../book/17-a-million-jobs.md)
