#!/usr/bin/env bash
# Actual OCI owner: one build, three isolated binary groups, strict completion.
set -euo pipefail
cd "$(dirname "$0")/../.."
export PYTHONDONTWRITEBYTECODE=1
if [[ "${GITHUB_ACTIONS:-}" == "true" ]]; then
  if (( $# != 0 )); then
    echo 'GitHub Actions uses current CI inputs; manual options are not allowed' >&2
    exit 1
  fi
  : "${EXPECTED_CHECKOUT_COMMIT:?current workflow checkout is required}"
  : "${GITHUB_RUN_ID:?current workflow run is required}"
  : "${GITHUB_RUN_ATTEMPT:?current workflow attempt is required}"
  : "${GITHUB_OUTPUT:?trusted owner output is required}"
  exec python3 scripts/ci/workflow_adapter.py produce --root . \
    --manifest tests/contracts/manifest.json --gate oci-interruptions --host linux \
    --directory target/contracts/linux/oci-interruptions \
    --commit "$EXPECTED_CHECKOUT_COMMIT" --run-id "$GITHUB_RUN_ID" \
    --attempt "$GITHUB_RUN_ATTEMPT" --github-output "$GITHUB_OUTPUT"
fi
# Ordinary disposable-host invocation observes the operator and creates a new
# manual session. Explicit owner/session options pass to the same strict driver.
exec python3 scripts/ci/oci_driver.py manual --root . "$@"
