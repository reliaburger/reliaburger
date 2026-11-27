#!/usr/bin/env bash
# Decide which CI job groups a run needs and write them to $GITHUB_OUTPUT.
#
#   code         - anything the build or the tests read changed
#   gossip       - the gossip protocol or its benchmarks changed, or criterion's
#                  version did
#   heavy        - run the acceptance suites (cluster, upgrade, privileged Linux)
#   task_arrays  - the task-array leader, node pool, executor or their
#                  benchmarks changed, the script that gates on them, or
#                  criterion's version
#   macos        - run the portable suite on macOS (one job)
#   s3           - log export, council backups or the S3 client changed, or
#                  the script that gives `make test-s3` its server
#   portable     - run portable Linux: with `code`, or alone to save a train's
#                  dependency cache (below)
#   tested_by    - the merge-queue run that already tested a train push's
#                  commit, or empty
#   lint         - run lint and format: with `code`, and on its own for a
#                  draft or a 0.4.0 branch (below)
#
# Pushes (to main and to every train), schedules and manual runs get
# everything; so does build.yml's call. A merge-queue group gets everything
# but portable macOS (maintainer decision of 24 November 2026): it took 50
# minutes to over 2 hours there and was cancelled or failed three times that
# day, each time rebuilding the whole queue. macos-nightly.yml runs it on the
# newest train every night instead, and the release gate PR and the push to
# main still run it. Two more exceptions save runners (#1723):
#
#   - a merge group that changes only documentation runs no code job. Its
#     code is its base's, which the train's last run or the group ahead of
#     it is testing; if that group fails, the queue rebuilds this one.
#   - a push to a train whose commit the merge queue already passed, every
#     job on its first attempt (QUEUE_RUN, found by ci_reuse.py), runs
#     nothing: the queue pushes exactly the commit it tested. Portable Linux
#     still runs when the train's dependency graph differs from main's
#     (MAIN_REF), because only a train push saves the cache its PRs restore.
#
# A draft pull request, and one from a `0-4-0/` branch (work for a release
# whose train isn't open yet), runs only the cheap set: CI policy, select jobs
# and lint, whatever its labels say (maintainer decision of 24 November
# 2026). Marking a draft ready for review starts a run with the full set.
# So does one from a `0-3-4/` batch branch: 0.3.4's batches reach the train
# through integration PRs and never get a PR of their own.
#
# Other pull requests are judged by the files they change against their base:
#
#   - into a release's merge train (`release-*`): the light set. Lint, MSRV,
#     portable Linux, the source bundle build and the optimized contract
#     boundaries; the heavy suites and the benchmarks wait for the merge
#     queue, which runs everything on the merged result before it lands.
#     A `full-ci` label adds the heavy suites and the benchmarks.
#   - from a train (the gate PR into main): the light set too, since the
#     queue has just run everything on the same code (#1443).
#     A `release-gate` label, put on at code freeze, runs everything; a
#     `full-ci` label adds the heavy suites.
#   - into main: everything the change touches except macOS.
#   - other stacked PRs: the light set, plus the heavy suites with full-ci.
#
# macOS runs only where it adds something (#1443, maintainer decision of 28
# October 2026): on pushes and the nightly, on the release gate PR, on a PR
# that touches macOS-specific code (the Apple runtime, a Rust file with a
# `target_os = "macos"` cfg before or after the change, the installers, the
# demo scripts) and on any code PR labelled `macos`. A gate PR's diff holds
# the whole train, so only its labels give it macOS. Dependabot never gets
# it unless labelled; the nightly covers it. The evidence gate names a
# heavy selection without macOS `train-full` (scripts/ci/workflow_assembly.py)
# and accepts it on pull requests and merge groups only.
#
# Retargeting a PR (GitHub does it when the branch under it merges) reruns
# this through ci-retarget.yml, so the new base's rules apply at once.
#
# Fixtures: scripts/ci/test_select_jobs.py.
#
# Inputs (environment): EVENT, BASE_REF, HEAD_REF, BASE_SHA (a merge
# group's base for merge_group), FULL_CI, RELEASE_GATE and MACOS_LABEL (each
# "true"/"false": whether the PR carries the full-ci, release-gate or macos
# label), DRAFT ("true" for a draft pull request), and optionally HEAD_SHA
# (defaults to HEAD). For a push: GITHUB_REF,
# QUEUE_RUN and MAIN_REF (defaults to origin/main).
set -euo pipefail

output="${GITHUB_OUTPUT:-/dev/stdout}"

# emit code gossip heavy task_arrays macos s3 [portable [tested_by [lint]]]
emit() {
    local portable="${7:-$1}" tested_by="${8:-}" lint="${9:-$1}"
    printf 'code=%s\ngossip=%s\nheavy=%s\ntask_arrays=%s\nmacos=%s\ns3=%s\nportable=%s\ntested_by=%s\nlint=%s\n' \
        "$1" "$2" "$3" "$4" "$5" "$6" "$portable" "$tested_by" "$lint" >>"$output"
    printf 'code=%s gossip=%s heavy=%s task_arrays=%s macos=%s s3=%s portable=%s tested_by=%s lint=%s\n' \
        "$1" "$2" "$3" "$4" "$5" "$6" "$portable" "$tested_by" "$lint" >&2
}

case "${GITHUB_REF:-}" in
    refs/heads/release-*) train_push=true ;;
    *) train_push=false ;;
esac
if [ "${EVENT}" = push ] && [ "$train_push" = true ] && [[ "${QUEUE_RUN:-}" =~ ^[0-9]+$ ]]; then
    # What rust-cache keys on, apart from the toolchain and the workflow.
    portable=false
    if ! git diff --quiet "${MAIN_REF:-origin/main}" "${HEAD_SHA:-HEAD}" -- \
        Cargo.lock Cargo.toml .cargo/config.toml 2>/dev/null; then
        portable=true
    fi
    echo "merge-queue run ${QUEUE_RUN} already passed every job on this commit" >&2
    emit false false false false false false "$portable" "$QUEUE_RUN"
    exit 0
fi

if [ "${EVENT}" = merge_group ] && [ -z "${BASE_SHA:-}" ]; then
    emit true true true true false true
    exit 0
fi
if [ "${EVENT}" != pull_request ] && [ "${EVENT}" != merge_group ]; then
    emit true true true true true true
    exit 0
fi

# --no-renames lists both sides of a move: with rename detection Git names
# only the destination, so moving src/x.rs to docs/x.md looked docs-only.
changed=$(git diff --no-renames --name-only "${BASE_SHA}...${HEAD_SHA:-HEAD}")

# Documentation that neither the build nor any test reads (#2485). Under
# docs/ that's only the plans, the talks, the research notes and the PDF
# build's configuration; everything else there is read by something:
# relish compiles the manual in and packs the book, the design docs, the
# release log, testing.md and flakes.md into its source bundle (whose size
# CI checks); tests/suite/website.rs reads the website, the roadmap, the
# quickstart, the runbook, the release log, the book and the qualification
# records; relish's concept-budget test reads docs/testing/; and
# documentation_first_run checks the READMEs' and the Linux servers guide's
# snippets. Outside docs/, the README, CLAUDE.md (in the source bundle), the
# policy files (tests/suite/issue_forms.rs), the skill under plugins/
# (tests/suite/agent_docs.rs) and any Markdown under src/ or tests/ count as
# code. CHANGELOG.md stays documentation: its test, test_changelog.py, runs
# in CI policy on every run. In a `case` pattern `*` matches `/` too, so
# docs/* is every file at any depth.
is_docs_only() {
    case "$1" in
        docs/plans/* | docs/talks/* | docs/research/* | docs/_quarto/*) return 0 ;;
        docs/*) return 1 ;;
        README.md | CLAUDE.md | CONTRIBUTING.md | SECURITY.md | AGENTS.md) return 1 ;;
        src/* | tests/* | plugins/* | .claude/*) return 1 ;;
        website/* | *.md) return 0 ;;
        *) return 1 ;;
    esac
}

# Code that only macOS builds or runs. A Rust file counts when either side of
# the change has a macOS cfg, so removing one counts too.
macos_code_file() {
    case "$1" in
        src/grill/apple* | docs/website/install.sh | scripts/release/install.sh.in | scripts/demo/*)
            return 0 ;;
        *.rs) ;;
        *) return 1 ;;
    esac
    local side content
    for side in "${HEAD_SHA:-HEAD}" "${BASE_SHA}"; do
        content=$(git show "$side:$1" 2>/dev/null || true)
        case "$content" in
            *'target_os = "macos"'*) return 0 ;;
        esac
    done
    return 1
}

code=false
gossip=false
task_arrays=false
s3=false
macos_code=false
while IFS= read -r file; do
    [ -z "$file" ] && continue
    is_docs_only "$file" || code=true
    if [ "$macos_code" = false ] && macos_code_file "$file"; then macos_code=true; fi
    # `gossip` selects the `benchmarks` job, which runs `make bench`: the
    # gossip benchmarks and the scheduler's planning pass and policy
    # evaluation beside them, so their benches and what they measure count
    # (#2493).
    case "$file" in
        src/mustard/* | benches/gossip* | benches/support/* | tests/gossip_*) gossip=true ;;
        benches/scheduler.rs | benches/policy.rs | src/cluster/orchestrate* | \
            src/meat/scheduler* | src/meat/filter* | src/meat/score* | src/meat/quota* | \
            src/meat/cluster_state* | src/policy/*) gossip=true ;;
    esac
    # What benches/task_arrays.rs measures: range sets, the leader's state
    # machine, the node pool and the process runner; and the script that
    # gates on its numbers.
    case "$file" in
        src/bun/task_array* | src/bun/task_executor* | src/meat/task_array* | \
            src/meat/index_set* | benches/task_arrays.rs | \
            scripts/ci/bench_baseline.py) task_arrays=true ;;
    esac
    # What `make test-s3` sends to an S3 server (#1534).
    case "$file" in
        src/ketchup/export.rs | src/council/backup.rs | src/object_storage.rs | \
            scripts/test-s3.sh) s3=true ;;
    esac
done <<<"$changed"

# Both benchmark jobs parse criterion's output and use its filter syntax, so a
# change to criterion's version runs them (#1151), in the manifest or only in
# the lockfile.
range="${BASE_SHA}...${HEAD_SHA:-HEAD}"
if git diff --no-renames "$range" -- Cargo.toml |
    grep -Eq '^[-+][[:space:]]*criterion[[:space:]]*='; then
    gossip=true
    task_arrays=true
fi
if git diff --no-renames -U3 "$range" -- Cargo.lock | awk '
    /^@@/ { package = 0; next }
    /^[ +-]name = "criterion"$/ { package = 1; next }
    /^[ +-]name = / { package = 0 }
    package && /^[-+]version = / { found = 1 }
    END { exit !found }'; then
    gossip=true
    task_arrays=true
fi

case "${HEAD_REF:-}" in
    release-*) from_a_train=true ;;
    *) from_a_train=false ;;
esac
case "${BASE_REF}" in
    release-*) into_a_train=true ;;
    *) into_a_train=false ;;
esac
case "${HEAD_REF:-}" in
    dependabot/*) dependabot=true ;;
    *) dependabot=false ;;
esac

if [ "${EVENT}" = merge_group ]; then
    if [ "$code" = false ]; then
        emit false false false false false false
    else
        emit true true true true false true
    fi
    exit 0
fi

# A draft, or a 0.4.0 branch: lint alone, when there's code to lint.
case "${HEAD_REF:-}" in
    0-4-0/* | 0-3-4/*) parked=true ;;
    *) parked=false ;;
esac
if [ "${DRAFT:-false}" = true ] || [ "$parked" = true ]; then
    echo "a draft, 0-4-0/ or 0-3-4/ pull request runs only the cheap set" >&2
    emit false false false false false false false "" "$code"
    exit 0
fi

if [ "$code" = false ]; then
    emit false "$gossip" false "$task_arrays" false "$s3"
    exit 0
fi

macos=false
if [ "${MACOS_LABEL:-false}" = true ]; then
    macos=true
elif [ "$macos_code" = true ] && [ "$from_a_train" = false ] && [ "$dependabot" = false ]; then
    macos=true
fi

if [ "$from_a_train" = true ] && [ "${RELEASE_GATE:-false}" = true ]; then
    emit true true true true true true
    exit 0
fi

# Into a train, or from one: the light set, or with full-ci the heavy suites
# and the benchmarks the change touches.
if [ "$into_a_train" = true ] || [ "$from_a_train" = true ]; then
    if [ "${FULL_CI}" = true ]; then
        emit true "$gossip" true "$task_arrays" "$macos" "$s3"
    else
        emit true false false false "$macos" false
    fi
    exit 0
fi

heavy=false
if [ "${BASE_REF}" = main ] || [ "${FULL_CI}" = true ]; then
    heavy=true
fi
emit true "$gossip" "$heavy" "$task_arrays" "$macos" "$s3"
