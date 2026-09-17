#!/usr/bin/env bash
# Reliaburger bootstrap. HTTPS authenticates the versioned installer; that
# installer pins the CLI checksum, and Relish verifies signed guest binaries.
set -euo pipefail
umask 077
fail() { printf 'reliaburger: %s\n' "$*" >&2; exit 1; }
command -v curl >/dev/null || fail 'curl is required'
version=${RELIABURGER_VERSION:-v0.1.0}
[[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]] || fail 'invalid RELIABURGER_VERSION'
staging=$(mktemp -d "${TMPDIR:-/tmp}/reliaburger-install.XXXXXXXX")
trap 'rm -rf "$staging"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP
url="https://github.com/reliaburger/reliaburger/releases/download/$version/install.sh"
if ! curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
  --tlsv1.2 --connect-timeout 15 --max-time 60 --retry 2 "$url" -o "$staging/install.sh"; then
  fail "could not download the installer for $version; check that the release has been published"
fi
bash "$staging/install.sh" "$@"
