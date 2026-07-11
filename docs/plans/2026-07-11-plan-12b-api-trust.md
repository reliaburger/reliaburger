# Phase 12b.1 — Internal API trust boundary (remainder)

Theme: `docs/progress.md` §12b.1 "Internal API trust boundary".
Findings: JOB2 residual (server-owned registry destination), JOB6 build-context
subset (bounded/sandboxed/cleaned extraction, Dockerfile path escape, Buildah
timeout kill, shared temp dirs), H4/D8 groundwork (central route→role matrix).
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Already landed (do not redo): `require_system` on `/v1/batch/run|report` and
`/v1/build/run`, `authorize(Deployer)` on submit endpoints, callbacks bounded to
known members, service token no longer forwarded, `context_digest` validated as
a well-formed OCI digest (JOB1 + the traversal half of JOB2).

Out of scope (later themes): scope *enforcement* in authorize (AUTH1, 12b.3),
per-node capabilities replacing the service token (AUTH2/AUTH4, 12b.3), batch/
build durability and delegation retry (JOB3–JOB7, 12b.2).

## Ground truth

- `src/bun/build_runner.rs` (~524 lines): `BuildRunRequest` still carries
  `registry_port: u16` (line ~29) chosen by the caller; `run_build` fetches
  `context_download_url(request.registry_port, …)` — a caller can point a
  privileged Bun at an arbitrary localhost port (JOB2 residual).
- Extraction (~lines 154–162): the whole context body is buffered in memory,
  then `tar::Archive::new(&context[..]).unpack(&extract_dir)` with no size,
  entry-count or entry-type bounds. `build_dir` is digest-derived, so two
  concurrent builds of the same digest share a directory, and failure paths
  don't reliably clean it (JOB6).
- Buildah runs under `tokio::time::timeout(timeout, output)` (~line 173). On
  timeout the future is dropped but the child is not necessarily killed, and
  Buildah's own children are never killed (no process group) (JOB6).
- Dockerfile path: `pickle::build::resolve_dockerfile`/`buildah_build_args`
  join `spec.dockerfile` onto the context dir with no check that the result
  stays inside it — `../../etc/x` escapes (JOB6).
- Roles are enforced ad hoc per handler; there is no single place that says
  which route requires which principal (H4/D8 groundwork).

## Implementation steps (tests first for each)

### 1. Server-owned registry destination

Remove `registry_port` from `BuildRunRequest` (and any batch/build request that
carries a destination the server already knows). The handler derives the port
from its own `[images]` node config / `ApiState`. Delegated builds (`/v1/build/run`)
derive the *entry node's* registry address from cluster membership, never from
the request body. Test: a request body smuggling an extra `registry_port` field
is ignored (serde `deny_unknown_fields` on the request struct is acceptable and
preferable — assert 400).

### 2. Bounded, sandboxed, cleaned context extraction

- New `[images] max_context_bytes` config (default 256 MiB, documented).
- Stream the context download to disk with a hard byte cap instead of buffering
  the body; abort past the cap.
- Extract via a hardened helper (new `pickle::build::unpack_context` or module
  in `build_runner.rs`): reject absolute paths and `..` components, reject
  symlinks/hardlinks/devices/FIFOs (regular files + dirs only), cap cumulative
  unpacked bytes (guards sparse-file bombs — count written bytes, not header
  sizes) and entry count, strip setuid/setgid bits.
- Per-build unique temp dir: `<builds_dir>/<build_id>-<random>` instead of the
  digest-derived path; an RAII guard removes it on *every* exit path (success,
  failure, timeout, panic).

Tests: oversized body rejected at the cap; tar with `../evil` entry rejected;
tar with a symlink entry rejected; sparse bomb stops at the unpacked-bytes cap;
two concurrent builds of the same digest get distinct dirs; temp dir gone after
success and after failure.

### 3. Dockerfile path confinement

Validate `spec.dockerfile` at both `validate_build` and `run_build` time: must
be relative, no `..`/root components; after extraction, canonicalise and assert
the resolved path is inside the context dir before invoking Buildah. Test: a
build spec with `dockerfile = "../../outside"` is rejected with a clear error,
and (integration) a crafted `/v1/build/run` body cannot make Buildah read
outside the extract dir.

### 4. Kill Buildah on timeout

Spawn Buildah in its own process group (Unix `process_group(0)`), with
`kill_on_drop(true)` as a backstop; on `tokio::time::timeout` elapse, send
SIGKILL to the group, reap the child, then clean the temp dir. Test: a fake
`buildah` shim in PATH that sleeps + spawns a sleeping child; assert both PIDs
are gone after the timeout fires (poll `kill -0` with a bound). Gate it behind
Unix; keep the existing timeout error message.

### 5. Central route→role matrix

One table, one place — a `const`-style declaration (e.g. `src/bun/authz.rs`)
mapping every mounted route (path pattern + method) to its required principal:
`Public | AnyToken | Deployer | Admin | System`. The router and the middleware
both consume it (or the middleware consults it and a test asserts the mounted
router's paths ⊆ matrix). This does not add scope enforcement (12b.3); it makes
the current role requirements auditable and closes the "no central matrix" gap.
Test: enumerating the mounted routes finds no route missing from the matrix,
and the existing role tests still pass (representative 401/403 assertions for
one route per principal class).

## Acceptance

- All new tests + full `make ci` green (fmt, clippy `-D warnings`, tests).
- Buildah-gated Lima behaviour unchanged (`RELIABURGER_BUILDAH_TESTS` suite
  still passes — flag anything you cannot run on macOS in the final report).
- `docs/progress.md`: tick the remaining nested items under the theme (add
  nested `- [x]` lines mirroring the existing style; check the theme box —
  after this the theme is complete).
- Book: extend chapter 12 (`docs/book/12-squeezing-every-drop.md`) build
  section with the hardening narrative — why caller-controlled destinations
  and unbounded tar extraction were the exact JOB2/JOB6 shape, how process
  groups fix the orphaned-Buildah problem. Explain any new Rust syntax on
  first use (e.g. RAII drop guards, `process_group`). British English, follow
  the style guide in CLAUDE.md.

## Constraints

- Do not touch `src/bun/agent.rs` beyond what handler plumbing strictly needs
  (other in-flight themes edit it heavily).
- No new dependencies unless genuinely needed (tempfile is already available;
  prefer std/tokio).
- Error messages lowercase, no trailing full stop; thiserror in library code.
