#!/usr/bin/env bash
# Decide which CI job groups a run needs and write them to $GITHUB_OUTPUT.
#
#   code    - anything the build or the tests read changed
#   gossip  - the gossip protocol or its benchmarks changed
#   heavy   - run the acceptance suites (cluster, upgrade, privileged Linux)
#
# Pushes, schedules and manual runs get everything. Pull requests are judged
# by the files they change against their base. Stacked PRs (base other than
# main) skip the heavy suites unless labelled full-ci; they run in full once
# the PR targets main.
#
# Inputs (environment): EVENT, BASE_REF, BASE_SHA, FULL_CI ("true"/"false"),
# and optionally HEAD_SHA (defaults to HEAD).
set -euo pipefail

output="${GITHUB_OUTPUT:-/dev/stdout}"

emit() {
    printf 'code=%s\ngossip=%s\nheavy=%s\n' "$1" "$2" "$3" >>"$output"
    printf 'code=%s gossip=%s heavy=%s\n' "$1" "$2" "$3" >&2
}

if [ "${EVENT}" != "pull_request" ]; then
    emit true true true
    exit 0
fi

changed=$(git diff --name-only "${BASE_SHA}...${HEAD_SHA:-HEAD}")

# Documentation that neither the build nor any test reads. The manual is
# compiled into relish, and documentation_first_run checks snippets in the
# READMEs, the whitepaper, the relish design doc and book chapters 2 and 4.
# tests/suite/website.rs parses the homepage tour's commands with relish.
is_docs_only() {
    case "$1" in
        docs/manual/* | README.md | docs/README.md | docs/whitepaper.md | \
            docs/design/cli-relish.md | docs/book/02-* | docs/book/04-* | \
            docs/website/index.html)
            return 1 ;;
        docs/* | website/* | *.md) return 0 ;;
        *) return 1 ;;
    esac
}

code=false
gossip=false
while IFS= read -r file; do
    [ -z "$file" ] && continue
    is_docs_only "$file" || code=true
    case "$file" in
        src/mustard/* | benches/* | tests/gossip_*) gossip=true ;;
    esac
done <<<"$changed"

heavy=false
if [ "$code" = true ] && { [ "${BASE_REF}" = main ] || [ "${FULL_CI}" = true ]; }; then
    heavy=true
fi

emit "$code" "$gossip" "$heavy"
