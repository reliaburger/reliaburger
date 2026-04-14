#!/usr/bin/env bash
#
# Config tooling demo: shows relish lint, fmt, compile, and diff.
#
# No daemon needed — all commands are local.
#
# Usage:
#   ./scripts/relish-toml-demo.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
TMPDIR="$(mktemp -d)"

# Colours (disabled when piped)
if [[ -t 1 ]]; then
    BOLD='\033[1m'
    CYAN='\033[36m'
    GREEN='\033[32m'
    YELLOW='\033[33m'
    RED='\033[31m'
    DIM='\033[2m'
    RESET='\033[0m'
else
    BOLD='' CYAN='' GREEN='' YELLOW='' RED='' DIM='' RESET=''
fi

section() { echo -e "\n${BOLD}${CYAN}=== $1 ===${RESET}\n"; }
cmd()     { echo -e "${GREEN}--- $1 ---${RESET}"; }
note()    { echo -e "${DIM}$1${RESET}"; }
warn()    { echo -e "${YELLOW}$1${RESET}"; }

cleanup() {
    rm -rf "${TMPDIR}"
}
trap cleanup EXIT

section "Reliaburger Config Tooling Demo"

cmd "building relish"
cargo build --bin relish --manifest-path "${REPO_DIR}/Cargo.toml" --quiet
RELISH="${REPO_DIR}/target/debug/relish"

# -----------------------------------------------------------------------
# 1. relish lint — validate config files
# -----------------------------------------------------------------------

section "1. relish lint — Validate Configs"

cat > "${TMPDIR}/good.toml" << 'TOML'
[app.web]
image = "myapp:v1"
replicas = 3
port = 8080

[app.web.health]
path = "/healthz"

[job.migrate]
image = "myapp:v1"
command = ["npm", "run", "migrate"]
run_before = ["app.web"]
TOML

note "Valid config:"
cat "${TMPDIR}/good.toml"
echo ""
cmd "relish lint good.toml"
"${RELISH}" lint "${TMPDIR}/good.toml"
echo ""

cat > "${TMPDIR}/bad.toml" << 'TOML'
[app.broken]
replicas = 3
port = 8080
TOML

warn "Invalid config (missing image):"
cat "${TMPDIR}/bad.toml"
echo ""
cmd "relish lint bad.toml"
"${RELISH}" lint "${TMPDIR}/bad.toml" 2>&1 || true
echo ""

# -----------------------------------------------------------------------
# 2. relish fmt — format config files
# -----------------------------------------------------------------------

section "2. relish fmt — Canonical Formatting"

cat > "${TMPDIR}/messy.toml" << 'TOML'
[job.cleanup]
image = "cleanup:latest"
schedule = "0 3 * * *"

[build.myapp]
context = "."
dockerfile = "Dockerfile"

[app.web]
image = "myapp:v1"
replicas = 3
port = 8080

[namespace.backend]
cpu = "8000m"
memory = "16Gi"

[permission.deployer]
actions = ["deploy"]
apps = ["web"]
TOML

warn "Messy config (sections in wrong order):"
note "  Sections: job → build → app → namespace → permission"
echo ""
cmd "relish fmt --check messy.toml"
"${RELISH}" fmt "${TMPDIR}/messy.toml" --check 2>&1 || true
echo ""

cmd "relish fmt messy.toml"
"${RELISH}" fmt "${TMPDIR}/messy.toml"
echo ""
note "Formatted config (canonical order: namespace → permission → app → job → build):"
cat "${TMPDIR}/messy.toml"
echo ""

cmd "relish fmt --check messy.toml (again)"
"${RELISH}" fmt "${TMPDIR}/messy.toml" --check
note "(idempotent — already formatted)"

# -----------------------------------------------------------------------
# 3. relish compile — merge config directories
# -----------------------------------------------------------------------

section "3. relish compile — Merge Config Directories"

# Create a directory structure with defaults
mkdir -p "${TMPDIR}/configs/backend"

cat > "${TMPDIR}/configs/_defaults.toml" << 'TOML'
image = "myorg/base:v3"
TOML

cat > "${TMPDIR}/configs/web.toml" << 'TOML'
[app.web]
image = "myorg/web:v2"
replicas = 3
port = 8080
TOML

cat > "${TMPDIR}/configs/backend/api.toml" << 'TOML'
[app.api]
replicas = 2
port = 9090
TOML

cat > "${TMPDIR}/configs/backend/worker.toml" << 'TOML'
[app.worker]
replicas = 1
TOML

note "Directory structure:"
echo "  configs/"
echo "    _defaults.toml        # image = \"myorg/base:v3\""
echo "    web.toml              # [app.web] image = \"myorg/web:v2\" (overrides default)"
echo "    backend/"
echo "      api.toml            # [app.api] (inherits default image + backend namespace)"
echo "      worker.toml         # [app.worker] (inherits default image + backend namespace)"
echo ""

cmd "relish compile configs/"
"${RELISH}" compile "${TMPDIR}/configs/"

# -----------------------------------------------------------------------
# 4. relish diff — structural comparison
# -----------------------------------------------------------------------

section "4. relish diff — Structural Comparison"

cat > "${TMPDIR}/v1.toml" << 'TOML'
[app.web]
image = "myapp:v1"
replicas = 3
port = 8080

[app.redis]
image = "redis:7-alpine"
port = 6379

[job.cleanup]
image = "cleanup:v1"
schedule = "0 3 * * *"
TOML

cat > "${TMPDIR}/v2.toml" << 'TOML'
[app.web]
image = "myapp:v2"
replicas = 5
port = 8080

[app.api]
image = "api:v1"
port = 9090

[job.cleanup]
image = "cleanup:v2"
schedule = "0 3 * * *"
TOML

note "v1: web + redis + cleanup"
note "v2: web (updated image + replicas) + api (new) + cleanup (updated) — redis removed"
echo ""
cmd "relish diff v1.toml v2.toml"
"${RELISH}" diff "${TMPDIR}/v1.toml" "${TMPDIR}/v2.toml"

section "Demo Complete"

echo "  - relish lint: validates configs, catches missing required fields"
echo "  - relish fmt: canonical section ordering, idempotent"
echo "  - relish compile: merges directories, applies _defaults.toml, derives namespaces"
echo "  - relish diff: structural field-by-field comparison (add/modify/remove)"
echo ""
