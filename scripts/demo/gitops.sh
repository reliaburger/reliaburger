#!/usr/bin/env bash
#
# GitOps on a one-node cluster, with a file:// repository.
#
# Pushes examples/gitops/repository to a local bare repository, deploys an
# app by hand with `relish apply`, then turns GitOps on with
# examples/node/gitops.toml. The sync creates the repository's apps in the
# namespaces its directories name and leaves the hand-applied app alone,
# because GitOps owns only what it wrote. Deleting an app from the repository
# deletes it from the cluster; `relish delete` of an app GitOps owns is
# refused. Each step checks the cluster and fails the demo if it's wrong.
#
# Usage: scripts/demo/gitops.sh
# Environment: see scripts/demo/lib.sh (RELISH, BUN, TESTAPP, DEMO_PORT_BASE).
# Needs git. tests/demos.rs runs it under `make test-cluster`.

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "$0")" && pwd)"
. "${DEMO_DIR}/lib.sh"

REPOSITORY="examples/gitops/repository"
GITOPS_CONFIG="examples/node/gitops.toml"
BY_HAND="examples/phase-1/proc-rollout.toml"

demo_start

# Git with a fixed identity, so commits work on a machine with none set.
demo_git() {
    git -C "${DEMO_WORK}/checkout" -c user.name=demo -c user.email=demo@example.com "$@"
}

# The example's files, pointed at this build's testapp and the demo's ports.
point_at_demo() {
    sed -e "s|\"target/debug/testapp\"|\"${TESTAPP}\"|" \
        -e "s|8080|${DEMO_THIRD_APP_PORT}|g" \
        -e "s|8081|${DEMO_APP_PORT}|g" \
        -e "s|8082|${DEMO_SECOND_APP_PORT}|g" \
        "$1" >"$2"
}

# The STATE column of every instance `relish status` lists for app $1 in
# namespace $2, one per line. Fails when relish does, so a cluster that
# doesn't answer never looks like one without the app.
app_states() {
    local listing node instance app namespace state rest
    listing="$("${RELISH}" status)" || return 1
    while read -r node instance app namespace state rest; do
        if [[ "${app}" == "$1" && "${namespace}" == "$2" ]]; then
            echo "${state}"
        fi
    done <<<"${listing}"
}

# Whether app $1 in namespace $2 has a running instance.
app_runs() {
    local states
    states="$(app_states "$1" "$2")" || return 1
    [[ $'\n'"${states}"$'\n' == *$'\n'running$'\n'* ]]
}

# Whether `relish status` no longer lists app $1 in namespace $2 at all.
app_is_gone() {
    local states
    states="$(app_states "$1" "$2")" || return 1
    [[ -z "${states}" ]]
}

demo_step "Push ${REPOSITORY} to a bare repository"
git init --quiet --bare --initial-branch=main "${DEMO_WORK}/infra.git"
git clone --quiet "${DEMO_WORK}/infra.git" "${DEMO_WORK}/checkout" 2>/dev/null
mkdir -p "${DEMO_WORK}/checkout/shop" "${DEMO_WORK}/checkout/ops"
for file in _defaults.toml shop/storefront.toml ops/status-page.toml; do
    point_at_demo "${REPO_DIR}/${REPOSITORY}/${file}" "${DEMO_WORK}/checkout/${file}"
done
demo_git symbolic-ref HEAD refs/heads/main
demo_git add .
demo_git commit --quiet -m "storefront and status page"
demo_git push --quiet origin main
demo_run git -C "${DEMO_WORK}/checkout" log --oneline

demo_step "Deploy an app by hand, before GitOps is on (${BY_HAND})"
point_at_demo "${REPO_DIR}/${BY_HAND}" "${DEMO_WORK}/web.toml"
demo_run "${RELISH}" apply "${DEMO_WORK}/web.toml"
demo_wait_for "web to run" 60 app_runs web default

demo_step "Turn GitOps on (${GITOPS_CONFIG}, pointed at the bare repository)"
sed -e "s|^repo = .*|repo = \"file://${DEMO_WORK}/infra.git\"|" \
    -e "s|^poll_interval_secs = .*|poll_interval_secs = 2|" \
    "${REPO_DIR}/${GITOPS_CONFIG}" >"${DEMO_WORK}/gitops.toml"
demo_run "${RELISH}" gitops set "${DEMO_WORK}/gitops.toml"
demo_wait_for "storefront to run in namespace shop" 90 app_runs storefront shop
demo_wait_for "status-page to run in namespace ops" 90 app_runs status-page ops
demo_wait_for "storefront to answer" 60 demo_serves "${DEMO_APP_PORT}" storefront-v1
demo_run "${RELISH}" status
demo_run "${RELISH}" gitops status
app_runs web default || demo_fail "the first sync removed web, which GitOps never wrote"
echo "web, applied by hand, survived the first sync"

demo_step "Delete storefront by hand: refused, because the next sync would undo it"
if "${RELISH}" delete storefront --namespace shop; then
    demo_fail "relish delete of a GitOps-managed app succeeded"
fi
echo "(refused, as it should be)"

demo_step "Delete storefront from the repository instead"
demo_git rm --quiet shop/storefront.toml
demo_git commit --quiet -m "retire the storefront"
demo_git push --quiet origin main
demo_wait_for "GitOps to remove storefront" 90 app_is_gone storefront shop
echo "GitOps removed storefront"
demo_run "${RELISH}" status
app_runs status-page ops || demo_fail "status-page went away with storefront"
app_runs web default || demo_fail "a sync removed web, which GitOps never wrote"
echo "web, applied by hand, survived every sync"

echo ""
echo "gitops demo complete"
