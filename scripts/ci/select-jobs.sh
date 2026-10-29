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
#   macos        - run the portable suite on macOS (three shards)
#
# Pushes (to main and to every train), merge-queue groups, schedules and
# manual runs get everything; so does build.yml's call. Pull requests are
# judged by the files they change against their base:
#
#   - into a release's merge train (`release-*`): the light set. Lint, MSRV,
#     portable Linux, the demo app and the optimized contract boundaries
#     answer in about 15 minutes; the heavy suites and the benchmarks wait
#     for the push to the train, which runs everything on the merged result.
#     A `full-ci` label adds the heavy suites and the benchmarks.
#   - from a train (the gate PR into main): the light set too, since the
#     push to the train has just run everything on the same code (#1443).
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
# and accepts it, like the light set, on pull requests only.
#
# Retargeting a PR (GitHub does it when the branch under it merges) reruns
# this through ci-retarget.yml, so the new base's rules apply at once.
#
# Fixtures: scripts/ci/test_select_jobs.py.
#
# Inputs (environment): EVENT, BASE_REF, HEAD_REF, BASE_SHA, FULL_CI,
# RELEASE_GATE and MACOS_LABEL (each "true"/"false": whether the PR carries
# the full-ci, release-gate or macos label), and optionally HEAD_SHA
# (defaults to HEAD).
set -euo pipefail

output="${GITHUB_OUTPUT:-/dev/stdout}"

emit() {
    printf 'code=%s\ngossip=%s\nheavy=%s\ntask_arrays=%s\nmacos=%s\n' "$1" "$2" "$3" "$4" "$5" >>"$output"
    printf 'code=%s gossip=%s heavy=%s task_arrays=%s macos=%s\n' "$1" "$2" "$3" "$4" "$5" >&2
}

if [ "${EVENT}" != "pull_request" ]; then
    emit true true true true true
    exit 0
fi

# --no-renames lists both sides of a move: with rename detection Git names
# only the destination, so moving src/x.rs to docs/x.md looked docs-only.
changed=$(git diff --no-renames --name-only "${BASE_SHA}...${HEAD_SHA:-HEAD}")

# Documentation that neither the build nor any test reads. The manual is
# compiled into relish, and documentation_first_run checks snippets in the
# READMEs, the whitepaper, the relish design doc, the Linux servers guide and
# book chapters 2 and 4.
# tests/suite/website.rs parses the homepage tour's commands with relish and
# checks the site's installer, recordings and release cards against the
# roadmap, the quickstart and the release runbook.
is_docs_only() {
    case "$1" in
        docs/manual/* | README.md | docs/README.md | docs/whitepaper.md | \
            docs/design/cli-relish.md | docs/linux-servers.md | \
            docs/book/02-* | docs/book/04-* | \
            docs/website/* | docs/roadmap.md | docs/quickstart.md | docs/releasing.md)
            return 1 ;;
        docs/* | website/* | *.md) return 0 ;;
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
macos_code=false
while IFS= read -r file; do
    [ -z "$file" ] && continue
    is_docs_only "$file" || code=true
    if [ "$macos_code" = false ] && macos_code_file "$file"; then macos_code=true; fi
    case "$file" in
        src/mustard/* | benches/gossip* | benches/support/* | tests/gossip_*) gossip=true ;;
    esac
    # What benches/task_arrays.rs measures: range sets, the leader's state
    # machine, the node pool and the process runner; and the script that
    # gates on its numbers.
    case "$file" in
        src/bun/task_array* | src/bun/task_executor* | src/meat/task_array* | \
            src/meat/index_set* | benches/task_arrays.rs | \
            scripts/ci/bench_baseline.py) task_arrays=true ;;
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

if [ "$code" = false ]; then
    emit false "$gossip" false "$task_arrays" false
    exit 0
fi

macos=false
if [ "${MACOS_LABEL:-false}" = true ]; then
    macos=true
elif [ "$macos_code" = true ] && [ "$from_a_train" = false ] && [ "$dependabot" = false ]; then
    macos=true
fi

if [ "$from_a_train" = true ] && [ "${RELEASE_GATE:-false}" = true ]; then
    emit true true true true true
    exit 0
fi

# Into a train, or from one: the light set, or with full-ci the heavy suites
# and the benchmarks the change touches.
if [ "$into_a_train" = true ] || [ "$from_a_train" = true ]; then
    if [ "${FULL_CI}" = true ]; then
        emit true "$gossip" true "$task_arrays" "$macos"
    else
        emit true false false false "$macos"
    fi
    exit 0
fi

heavy=false
if [ "${BASE_REF}" = main ] || [ "${FULL_CI}" = true ]; then
    heavy=true
fi
emit true "$gossip" "$heavy" "$task_arrays" "$macos"
