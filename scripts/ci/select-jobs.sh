#!/usr/bin/env bash
# Decide which CI job groups a run needs and write them to $GITHUB_OUTPUT.
#
#   code         - anything the build or the tests read changed
#   gossip       - the gossip protocol or its benchmarks changed
#   heavy        - run the acceptance suites (cluster, upgrade, privileged Linux)
#   task_arrays  - the task-array leader, node pool, executor or their
#                  benchmarks changed
#   macos        - run the portable suite on macOS (three shards)
#
# Pushes, merge-queue groups, schedules and manual runs get everything. Pull
# requests are judged by the files they change against their base:
#
#   - into a release's merge train (`release-*`): the light set. Lint, MSRV,
#     portable Linux, the demo app and the optimized contract boundaries
#     answer in about 15 minutes; macOS, the heavy suites and the benchmarks
#     wait for the merge queue and the push to the train, which run
#     everything on the merged result. A `full-ci` label adds the heavy suites
#     and the benchmarks but still not macOS: three macOS runners per PR
#     queued for over an hour. A head that is itself a train gets everything.
#   - into main, or from a train: everything the change touches.
#   The evidence gate names the full-ci train selection `train-full`
#   (scripts/ci/workflow_assembly.py) and accepts it on pull requests only.
#   - other stacked PRs: macOS but not the heavy suites, unless labelled
#     full-ci; they run in full once the PR targets main.
#
# Retargeting a PR (GitHub does it when the branch under it merges) reruns
# this through ci-retarget.yml, so the new base's rules apply at once.
#
# Fixtures: scripts/ci/test_select_jobs.py.
#
# Inputs (environment): EVENT, BASE_REF, HEAD_REF, BASE_SHA, FULL_CI
# ("true"/"false"), and optionally HEAD_SHA (defaults to HEAD).
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

code=false
gossip=false
task_arrays=false
while IFS= read -r file; do
    [ -z "$file" ] && continue
    is_docs_only "$file" || code=true
    case "$file" in
        src/mustard/* | benches/gossip* | benches/support/* | tests/gossip_*) gossip=true ;;
    esac
    # What benches/task_arrays.rs measures: range sets, the leader's state
    # machine, the node pool and the process runner.
    case "$file" in
        src/bun/task_array* | src/bun/task_executor* | src/meat/task_array* | \
            src/meat/index_set* | benches/task_arrays.rs) task_arrays=true ;;
    esac
done <<<"$changed"

case "${HEAD_REF:-}" in
    release-*) from_a_train=true ;;
    *) from_a_train=false ;;
esac
case "${BASE_REF}" in
    release-*) into_a_train=true ;;
    *) into_a_train=false ;;
esac

if [ "$code" = false ]; then
    emit false "$gossip" false "$task_arrays" false
    exit 0
fi

if [ "$into_a_train" = true ] && [ "$from_a_train" = false ] && [ "${FULL_CI}" != true ]; then
    emit true false false false false
    exit 0
fi

if [ "$into_a_train" = true ] && [ "$from_a_train" = false ]; then
    emit true "$gossip" true "$task_arrays" false
    exit 0
fi

heavy=false
if [ "${BASE_REF}" = main ] || [ "$into_a_train" = true ] || [ "$from_a_train" = true ] || [ "${FULL_CI}" = true ]; then
    heavy=true
fi
emit true "$gossip" "$heavy" "$task_arrays" true
