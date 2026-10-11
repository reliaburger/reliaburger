#!/usr/bin/env bash
# Decide which CI job groups a run needs and write them to $GITHUB_OUTPUT.
#
#   code    - anything the build or the tests read changed
#   gossip       - the gossip protocol or its benchmarks changed
#   heavy        - run the acceptance suites (cluster, upgrade, privileged Linux)
#   task_arrays  - the task-array leader, node pool, executor or their
#                  benchmarks changed
#
# Pushes, schedules and manual runs get everything. Pull requests are judged
# by the files they change against their base. A PR into main or into a
# release's merge-train branch (`release-*`) gets the heavy suites; other
# stacked PRs skip them unless labelled full-ci, and run in full once the PR
# targets main. Retargeting a PR (GitHub does it when the branch under
# it merges) reruns this through ci-retarget.yml, so the heavy suites don't
# wait for the next push.
#
# Fixtures: scripts/ci/test_select_jobs.py.
#
# Inputs (environment): EVENT, BASE_REF, BASE_SHA, FULL_CI ("true"/"false"),
# and optionally HEAD_SHA (defaults to HEAD).
set -euo pipefail

output="${GITHUB_OUTPUT:-/dev/stdout}"

emit() {
    printf 'code=%s\ngossip=%s\nheavy=%s\ntask_arrays=%s\n' "$1" "$2" "$3" "$4" >>"$output"
    printf 'code=%s gossip=%s heavy=%s task_arrays=%s\n' "$1" "$2" "$3" "$4" >&2
}

if [ "${EVENT}" != "pull_request" ]; then
    emit true true true true
    exit 0
fi

# --no-renames lists both sides of a move: with rename detection Git names
# only the destination, so moving src/x.rs to docs/x.md looked docs-only.
changed=$(git diff --no-renames --name-only "${BASE_SHA}...${HEAD_SHA:-HEAD}")

# Documentation that neither the build nor any test reads. The manual is
# compiled into relish, and documentation_first_run checks snippets in the
# READMEs, the whitepaper, the relish design doc, the Linux servers guide and
# book chapters 2 and 4.
# tests/suite/website.rs parses the homepage tour's commands with relish.
is_docs_only() {
    case "$1" in
        docs/manual/* | README.md | docs/README.md | docs/whitepaper.md | \
            docs/design/cli-relish.md | docs/linux-servers.md | \
            docs/book/02-* | docs/book/04-* | \
            docs/website/index.html)
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

heavy=false
case "${BASE_REF}" in
    main | release-*) targets_a_release=true ;;
    *) targets_a_release=false ;;
esac
if [ "$code" = true ] && { [ "$targets_a_release" = true ] || [ "${FULL_CI}" = true ]; }; then
    heavy=true
fi

emit "$code" "$gossip" "$heavy" "$task_arrays"
