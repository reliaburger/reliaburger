#!/bin/sh
# Reliaburger bootstrap. HTTPS authenticates the versioned installer; that
# installer pins the CLI checksum, and Relish verifies signed guest binaries.
# POSIX sh, so `curl -fsSL https://reliaburger.com/install.sh | sh` works with
# dash, bash, zsh and busybox alike. Pass installer options with `sh -s --`.
#
# Everything lives in main(), called on the last line: a piped shell reads the
# script as it arrives, and a truncated download must not run half a script.
set -eu

fail() { printf 'reliaburger: %s\n' "$*" >&2; exit 1; }

main() {
  umask 077
  command -v curl >/dev/null 2>&1 || fail 'curl is required'
  version=${RELIABURGER_VERSION:-v0.1.0}
  # The character gate rejects newlines, so the grep below sees exactly one line.
  case $version in
    ''|*[!A-Za-z0-9.-]*) fail 'invalid RELIABURGER_VERSION' ;;
  esac
  printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$' \
    || fail 'invalid RELIABURGER_VERSION'
  release_base=${RELIABURGER_RELEASE_BASE_URL:-https://github.com/reliaburger/reliaburger/releases/download/$version}
  case $release_base in
    https:///*|*[?#@\\[:space:]]*) fail 'release mirror must be an HTTPS URL without credentials, query or fragment' ;;
    https://?*) ;;
    *) fail 'release mirror must be an HTTPS URL without credentials, query or fragment' ;;
  esac
  release_base=${release_base%/}
  staging=$(mktemp -d "${TMPDIR:-/tmp}/reliaburger-install.XXXXXXXX") || fail 'could not create a staging directory'
  trap 'rm -rf "$staging"' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM HUP
  url="$release_base/install.sh"
  if ! curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
    --tlsv1.2 --connect-timeout 15 --max-time 60 --retry 2 "$url" -o "$staging/install.sh"; then
    fail "could not download the installer for $version; check that the release has been published"
  fi
  sh "$staging/install.sh" "$@"
}

main "$@"
