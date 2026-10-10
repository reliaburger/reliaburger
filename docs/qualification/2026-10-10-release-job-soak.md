# Mixed job release-soak checks, 10 October 2026

This is harness validation for the job feature in PR #654, based on commit
`3345dca1040a55bbc215d9e26279b6ed74ed620f`. It is not a staged V02 acceptance
record, a saturation benchmark or a 100-million-jobs-per-day qualification.

## Rig and binary

- Local Lima VM `reliaburger-test`, aarch64 Ubuntu 24.04, kernel
  `6.8.0-139-generic`, four vCPUs and 8 GiB RAM.
- Rootful runc and cgroup v2. One disposable Bun, with an app alongside the
  three job runtimes; private mount/network identity and separate data.
- Bun built from the exact feature head: `bun 0.1.6 (3345dca)`, debug binary
  SHA-256 `65f9adf1ca366c6662bc672c054defde01dd3ae474f9d1073b86be6a44b6c433`.
- Containers and host commands use BusyBox from pinned image
  `public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028`.
  Executable SHA-256:
  `f19470457088612bc3285404783d9f93533d917e869050aca13a4139b937c0a5`.

## Actual Linux execution

`sudo python3 scripts/release/job_soak_linux.py --bun /absolute/path/to/bun`
passed against the binary above. Its smaller test arrays exercised every mode:

- Independent effects for eight audited indexes per runtime: 24 execution
  attempts observed, with complete logical index coverage.
- Non-zero exits exhausting two attempts, deadlines with a descendant and
  committed start evidence, and the deliberate memory-limit kill.
- Cancellation of verified active work, two resource profiles, repeated
  commands, and actual private ownership/kernel inventory reconciliation.
- Minute schedules, publication-triggered singletons and `run_before` hooks
  beside a running app, including a real app revision change.
- Positive API drain followed by empty executor owner inventories. Live
  executors were observed before drain, so the check did not pass by running
  nothing.

The final disposable fixture was `/var/tmp/rb-release-job-smoke-2iix507e`.
It observed eight live owners and positive ownership evidence for all three
runtimes before drain. The first cold-image run also passed; repeated pulls
then hit ECR throttling. The final repeat used `--image-cache
/var/tmp/rb-release-job-smoke-jugx85ml/images`, copying only image content,
its catalogue and format stamp into a new fixture, preserving shifted rootfs
ownership. Job state and executor journals remained fresh.
Keep its detailed logs local for diagnosis; the repository records this compact
result rather than duplicating generated task JSON.

## Portable checks

`make ci` passed on the Linux VM: format, both Clippy feature configurations,
6247 Rust tests (162 owned skips), doctests, 372 CI-script tests (two Linux
skips), 213 release-script tests and ignored-test ownership checks. Shell syntax,
Python compilation and `git diff --check` also passed.

The portable run used one compiler job, a fresh `TMPDIR`, and a temporary PATH
shim making Buildah unavailable, as required by the build-delegation test. The
installed Buildah and shared root-owned temporary fixtures were left unchanged.
The first parallel compile hit the 8-GiB memory limit; the single-job build
completed. These are rig setup details, not suppressed test failures.

Python regression cases also reject stale/missing evidence, wrong owners,
unsafe journals, accounting regressions, changed resume identities, lost
admission/replay replies, expired drain deadlines and ordinary failures hidden
by final cancellation. An interrupted run without a drain timestamp renders
FAIL instead of crashing the reporter.

## Acceptance still required

The integrated candidate, including #669's review fixes, must run the unchanged
90-minute fast and eight-hour final staged tiers. The local fixture does not
inject the whole V02 fault schedule and cannot satisfy its required per-mode,
per-node, leader/follower/power-off and unknown-outcome coverage. Load caps need
calibration beside the full release app catalogue on that rig. Global retention
bounds and 24-hour throughput qualification remain issue #668.
