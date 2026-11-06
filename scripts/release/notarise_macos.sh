#!/usr/bin/env bash
# Developer ID sign and notarise the macOS relish, in place (#1583).
#
#   scripts/release/notarise_macos.sh dist/relish-macos-aarch64
#
# build.yml runs this between building the binary and recording the
# candidate, because codesign rewrites the file: candidate.json, the .sig
# envelope, SHA256SUMS and the installer's pinned digest must all cover the
# notarised bytes, so nothing downstream changes.
#
# It reads five secrets of the `release` GitHub Environment:
#
#   APPLE_DEVELOPER_ID_P12           base64 of a .p12 holding the "Developer ID
#                                    Application" certificate and its key
#   APPLE_DEVELOPER_ID_P12_PASSWORD  the password that .p12 was exported with
#   APPLE_NOTARY_KEY                 base64 of an App Store Connect API key (.p8)
#   APPLE_NOTARY_KEY_ID              that key's ID
#   APPLE_NOTARY_ISSUER_ID           the issuer ID of the team that owns it
#
# With none of them set it says so, records notarised=false and succeeds,
# leaving the binary exactly as built: that's every build until the
# maintainer adds them, and every nightly build. With only some set it fails,
# because half a configuration is a mistake, not a choice.
#
# A bare Mach-O can't carry a stapled ticket, so Gatekeeper checks the
# notarisation online the first time the binary runs. Needs macOS with
# security, codesign, ditto and xcrun notarytool.
set -euo pipefail

SECRETS=(APPLE_DEVELOPER_ID_P12 APPLE_DEVELOPER_ID_P12_PASSWORD APPLE_NOTARY_KEY
         APPLE_NOTARY_KEY_ID APPLE_NOTARY_ISSUER_ID)
IDENTIFIER=com.reliaburger.relish

[ $# -eq 1 ] || { echo "usage: $0 BINARY" >&2; exit 2; }
binary=$1
[ -f "$binary" ] || { echo "notarise: no such binary: $binary" >&2; exit 1; }

output() { [ -z "${GITHUB_OUTPUT:-}" ] || echo "$1" >> "$GITHUB_OUTPUT"; }
summary() { [ -z "${GITHUB_STEP_SUMMARY:-}" ] || echo "$1" >> "$GITHUB_STEP_SUMMARY"; }

# Strings, not arrays: macOS's own bash 3.2 calls an empty array unbound.
present="" missing=""
for name in "${SECRETS[@]}"; do
  if [ -n "${!name:-}" ]; then present="$present $name"; else missing="$missing $name"; fi
done
if [ -z "$present" ]; then
  message="relish-macos-aarch64 is NOT notarised: none of ${SECRETS[*]} is set in this job's environment. See docs/releasing.md#macos-notarisation."
  echo "::notice title=macOS notarisation skipped::$message"
  summary "$message"
  output notarised=false
  exit 0
fi
if [ -n "$missing" ]; then
  echo "notarise:$present set but$missing missing; set all five or none" >&2
  exit 1
fi

work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/reliaburger-notarise.XXXXXXXX")
keychain="$work/signing.keychain-db"
cleanup() {
  security delete-keychain "$keychain" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

# A throwaway keychain with a random password, so the identity never sits in
# the runner's login keychain.
keychain_password=$(openssl rand -hex 24)
printf '%s' "$APPLE_DEVELOPER_ID_P12" | base64 --decode > "$work/identity.p12"
printf '%s' "$APPLE_NOTARY_KEY" | base64 --decode > "$work/notary.p8"
security create-keychain -p "$keychain_password" "$keychain"
security set-keychain-settings -lut 3600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"
security import "$work/identity.p12" -k "$keychain" -P "$APPLE_DEVELOPER_ID_P12_PASSWORD" \
  -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" >/dev/null
# codesign finds identities only in the search list; keep what's there.
search_list=$(security list-keychains -d user | tr -d '"')
# shellcheck disable=SC2086 # one path per word, and keychain paths have no spaces
security list-keychains -d user -s "$keychain" $search_list

identities=$(security find-identity -v -p codesigning "$keychain" | awk '/"Developer ID Application: / { print $2 }')
count=$(printf '%s' "$identities" | grep -c . || true)
[ "$count" -eq 1 ] || { echo "notarise: the .p12 must hold exactly one Developer ID Application identity, found $count" >&2; exit 1; }

# The hardened runtime and a secure timestamp are what notarisation requires.
codesign --force --sign "$identities" --keychain "$keychain" --identifier "$IDENTIFIER" \
  --options runtime --timestamp "$binary"
codesign --verify --strict --verbose=2 "$binary"

ditto -c -k --keepParent "$binary" "$work/relish.zip"
xcrun notarytool submit "$work/relish.zip" --key "$work/notary.p8" --key-id "$APPLE_NOTARY_KEY_ID" \
  --issuer "$APPLE_NOTARY_ISSUER_ID" --wait --timeout 30m --output-format json > "$work/result.json"
read -r status submission < <(python3 -c '
import json, sys
result = json.load(open(sys.argv[1]))
print(result.get("status", "unknown"), result.get("id", "unknown"))
' "$work/result.json")
if [ "$status" != Accepted ]; then
  echo "notarise: Apple answered $status for submission $submission" >&2
  xcrun notarytool log "$submission" --key "$work/notary.p8" --key-id "$APPLE_NOTARY_KEY_ID" \
    --issuer "$APPLE_NOTARY_ISSUER_ID" >&2 || true
  exit 1
fi

# Signing must not have broken it.
"$binary" --version
digest=$(shasum -a 256 "$binary" | awk '{ print $1 }')
echo "notarised $binary as submission $submission, SHA-256 $digest"
summary "relish-macos-aarch64 is Developer ID signed and notarised: submission \`$submission\`, SHA-256 \`$digest\`."
output notarised=true
