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

# A byte count as people read it: 812 B, 6.9 KiB, 37.2 MiB.
human_size() {
  awk -v bytes="$1" 'BEGIN {
    if (bytes >= 1048576) printf "%.1f MiB", bytes / 1048576
    else if (bytes >= 1024) printf "%.1f KiB", bytes / 1024
    else printf "%d B", bytes
  }'
}

file_size() {
  if [ -f "$1" ]; then wc -c <"$1" | tr -d ' '; else printf '0\n'; fi
}

now() { date +%s 2>/dev/null || printf '0\n'; }

# Fetch $1 into $2, retrying a dropped connection; $3 says what it is and $4
# its size in bytes, when known. The same helpers as the versioned
# installer's, word for word (test_package.py checks), so see
# scripts/release/install.sh.in for why it isn't curl's --retry-all-errors.
download() {
  if [ -n "${4:-}" ]; then
    printf 'Downloading %s (%s)...\n' "$3" "$(human_size "$4")" >&2
  else
    printf 'Downloading %s...\n' "$3" >&2
  fi
  # A bar on a terminal. A log gets the lines around it instead, because the
  # bar redraws itself with carriage returns.
  meter=--silent
  [ ! -t 2 ] || meter=--progress-bar
  started=$(now)
  attempt=1
  while :; do
    status=0
    curl --fail "$meter" --show-error --location --proto '=https' --proto-redir '=https' \
      --tlsv1.2 --connect-timeout 15 --speed-limit 1 --speed-time 30 \
      --continue-at - "$1" -o "$2" || status=$?
    if [ "$status" -eq 0 ]; then
      fetched=$(file_size "$2")
      readable=
      [ "$fetched" -lt 1024 ] || readable=" ($(human_size "$fetched"))"
      printf 'Downloaded %s bytes%s in %s s\n' "$fetched" "$readable" "$(($(now) - started))" >&2
      return 0
    fi
    have=$(file_size "$2")
    next=resuming
    # 33: the server can't resume, so the next attempt starts from scratch.
    if [ "$status" -eq 33 ]; then
      rm -f "$2"
      next='starting again'
    fi
    [ "$attempt" -lt 5 ] || return "$status"
    attempt=$((attempt + 1))
    printf 'reliaburger: download interrupted (curl exit %s) after %s; %s, attempt %s of 5\n' \
      "$status" "$(human_size "$have")" "$next" "$attempt" >&2
    sleep 2
  done
}

main() {
  umask 077
  command -v curl >/dev/null 2>&1 || fail 'curl is required'
  version=${RELIABURGER_VERSION:-v0.1.5}
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
  what="the Reliaburger $version installer"
  [ -z "${RELIABURGER_RELEASE_BASE_URL:-}" ] || what="$what from $release_base"
  # No size: it's a few kilobytes, and asking first would cost a round trip.
  if ! download "$url" "$staging/install.sh" "$what"; then
    fail "could not download the installer for $version; check that the release has been published"
  fi
  sh "$staging/install.sh" "$@"
}

main "$@"
