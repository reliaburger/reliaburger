#!/usr/bin/env bash
#
# Workload identity demo: shows the full SPIFFE certificate lifecycle.
# Initialises a cluster, generates a CSR, signs it, mints an OIDC JWT,
# writes identity files, and demonstrates rotation states.
#
# No daemon needed — exercises sesame APIs directly via a test binary.
#
# Usage:
#   ./scripts/workload-identity-demo.sh

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Colours (disabled when piped)
if [[ -t 1 ]]; then
    BOLD='\033[1m'
    CYAN='\033[36m'
    GREEN='\033[32m'
    DIM='\033[2m'
    RESET='\033[0m'
else
    BOLD='' CYAN='' GREEN='' DIM='' RESET=''
fi

section() { echo -e "\n${BOLD}${CYAN}=== $1 ===${RESET}\n"; }
cmd()     { echo -e "${GREEN}--- $1 ---${RESET}"; }
note()    { echo -e "${DIM}$1${RESET}"; }

section "Reliaburger Workload Identity Demo"
echo "This demo exercises the full SPIFFE certificate lifecycle:"
echo "  1. Cluster initialisation (CA hierarchy + OIDC keypair)"
echo "  2. CSR generation + signing (worker/council)"
echo "  3. OIDC JWT minting + verification + JWKS"
echo "  4. Identity bundle + tmpfs delivery"
echo "  5. Rotation state machine"
echo ""

cmd "building"
cargo build --manifest-path "${REPO_DIR}/Cargo.toml" --quiet 2>&1

cmd "running identity demo"
cargo test --manifest-path "${REPO_DIR}/Cargo.toml" \
    --test identity_demo -- --nocapture --test-threads=1 2>&1

section "Demo Complete"
echo "  All workload identity operations demonstrated successfully."
echo ""
echo "  Every workload gets:"
echo "    cert.pem     — SPIFFE X.509 certificate"
echo "    key.pem      — private key (never leaves worker)"
echo "    ca.pem       — Workload CA + Root CA chain"
echo "    bundle.pem   — cert + CA chain"
echo "    token        — OIDC JWT for cloud federation"
echo ""
echo "  Certificates rotate every 30 minutes."
echo "  Grace period: 4 hours if council is unreachable."
echo ""
