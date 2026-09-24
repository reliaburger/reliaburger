#!/usr/bin/env bash
# shellcheck disable=SC2016  # backquotes in printf formats are Markdown
# Qualify a staged release candidate the way a user meets it: pipe the
# bootstrap to sh with RELIABURGER_RELEASE_BASE_URL pointing at the staged
# pre-release, run a short tour on the cluster it builds, then destroy the
# cluster and uninstall. Writes a Markdown record for docs/qualification/.
#
# Everything lives in a fresh, short RELIABURGER_HOME under /tmp (Lima's
# control sockets must fit in 104 bytes). The real ~/.reliaburger is never
# used, and the script fails if its top level or ~/.local/bin/relish changes.
#
# Usage:
#   scripts/release/qualify-staged-install.sh --base-url URL [options] [-- SETUP_ARGS...]
#
#   --base-url URL          staged directory, e.g. the staging pre-release's
#                           https://github.com/reliaburger/reliaburger/releases/download/TAG
#   --qualified-digest SHA  require candidate.json to have this SHA-256
#   --bootstrap URL|FILE    bootstrap to pipe to sh (default: the website's
#                           https://reliaburger.com/install.sh; a local file
#                           such as docs/website/install.sh is fetched as file://)
#   --manifest FILE|URL     tour manifest (default: examples/kubernetes/podinfo.yaml)
#   --record FILE           where to write the record (default: in the evidence directory)
#   --keep                  leave the cluster and the installation for debugging
#   SETUP_ARGS              extra `relish setup --quickstart` options, such as
#                           --api-port when another cluster holds the defaults
set -euo pipefail

usage() { sed -n '3,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
fail() { printf 'qualify: %s\n' "$*" >&2; exit 1; }

repository=$(cd "$(dirname "$0")/../.." && pwd)
base_url=
qualified_digest=
bootstrap=https://reliaburger.com/install.sh
manifest=$repository/examples/kubernetes/podinfo.yaml
record=
keep=false
setup_args=()
while [ "$#" -gt 0 ]; do
    case $1 in
        --base-url) base_url=${2:-}; shift 2 ;;
        --qualified-digest) qualified_digest=${2:-}; shift 2 ;;
        --bootstrap) bootstrap=${2:-}; shift 2 ;;
        --manifest) manifest=${2:-}; shift 2 ;;
        --record) record=${2:-}; shift 2 ;;
        --keep) keep=true; shift ;;
        -h|--help) usage; exit 0 ;;
        --) shift; setup_args=("$@"); break ;;
        *) usage >&2; fail "unknown argument: $1" ;;
    esac
done
[ -n "$base_url" ] || { usage >&2; fail '--base-url is required'; }
case $base_url in
    https://*[?#@\\[:space:]]*|https:///*) fail 'the base URL must be HTTPS without credentials, query or fragment' ;;
    https://?*) base_url=${base_url%/} ;;
    *) fail 'the base URL must be HTTPS' ;;
esac
if [ -n "$qualified_digest" ] && ! [[ $qualified_digest =~ ^[0-9a-f]{64}$ ]]; then
    fail '--qualified-digest must be a SHA-256 in lowercase hex'
fi
case $bootstrap in
    https://*) ;;
    /*) [ -f "$bootstrap" ] || fail "no bootstrap at $bootstrap"; bootstrap=file://$bootstrap ;;
    *) [ -f "$bootstrap" ] || fail "no bootstrap at $bootstrap"; bootstrap=file://$PWD/$bootstrap ;;
esac
command -v curl >/dev/null || fail 'curl is required'

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# What must not change: the real home's top level and the PATH link.
real_home_state() {
    ls -1A "$HOME/.reliaburger" 2>/dev/null || true
    readlink "$HOME/.local/bin/relish" 2>/dev/null || true
}
real_before=$(real_home_state)

evidence=$(mktemp -d "${TMPDIR:-/tmp}/reliaburger-qualify.XXXXXX")
home=$(mktemp -d /tmp/rbq.XXXXXX)
case $home in
    "$HOME/.reliaburger"|"$HOME/.reliaburger/"*) fail "refusing to use $home" ;;
esac
[ -z "$(ls -A "$home")" ] || fail "$home is not empty"
[ -n "$record" ] || record=$evidence/record.md
[ ! -e "$record" ] || fail "$record already exists"
export RELIABURGER_HOME=$home
# No rc-file prompt and no ~/.local/bin link: an isolated home never gets one.
export RELIABURGER_NO_MODIFY_PATH=1
relish=$home/bin/relish
printf 'evidence: %s\nRELIABURGER_HOME: %s\n' "$evidence" "$home"

stage=preparing
result=FAIL
install_seconds=
tour_rows=
teardown=skipped

# Run one tour command, keeping its output and duration.
step() {
    local label=$1 started status=0
    shift
    stage=$label
    printf '\n$ relish %s\n' "$*" | tee -a "$evidence/tour.log"
    started=$(date +%s)
    "$relish" "$@" 2>&1 | tee -a "$evidence/tour.log" || status=$?
    tour_rows+="| \`relish $*\` | $(( $(date +%s) - started )) s | $status |"$'\n'
    return "$status"
}

# Every instance running, with at least one of each tour app.
apps_running() {
    "$relish" status 2>/dev/null | awk '
        NR > 1 && NF >= 5 { seen[$3] = 1; if ($5 != "running") pending = 1 }
        END { exit !(!pending && seen["frontend"] && seen["backend"] && seen["redis"] && seen["loadgen"]) }'
}

host_details() {
    printf -- '- Host: `%s`\n' "$(uname -srm)"
    if [ "$(uname -s)" = Darwin ]; then
        printf -- '- macOS %s, %s, %s GiB\n' "$(sw_vers -productVersion)" \
            "$(sysctl -n machdep.cpu.brand_string)" "$(( $(sysctl -n hw.memsize) / 1073741824 ))"
    else
        # shellcheck source=/dev/null
        printf -- '- %s, %s, %s GiB\n' "$(. /etc/os-release && printf '%s' "$PRETTY_NAME")" \
            "$(awk -F': ' '/^model name/ {print $2; exit}' /proc/cpuinfo)" \
            "$(awk '/^MemTotal/ {print int($2 / 1048576); exit}' /proc/meminfo)"
    fi
}

# Every downloaded file, and the candidate asset whose digest it has. Lima's
# tarball isn't ours; the CLI, the guest image and both Linux binaries must be.
cache_digests() {
    local path digest asset missing=0
    printf '| File | SHA-256 | Candidate asset |\n|---|---|---|\n'
    while IFS= read -r path; do
        digest=$(sha256 "$path")
        asset=$(awk -v d="$digest" '$1 == d {print $2; exit}' "$evidence/SHA256SUMS")
        printf '| `%s` | `%s` | %s |\n' "${path#"$home"/}" "$digest" "${asset:-not a release asset}"
        case ${path##*/} in
            relish|bun-*|relish-*|*.qcow2) [ -n "$asset" ] || missing=1 ;;
        esac
    done < <({ printf '%s\n' "$relish"; find "$home/cache" -type f ! -name '*.partial' 2>/dev/null; } | sort)
    return "$missing"
}

write_record() {
    {
        printf '# Staged install qualification: %s\n\n' "$result"
        printf '%s. `curl | sh` against a staged candidate, from empty caches in an\n' "$(date '+%-d %B %Y')"
        printf 'isolated `RELIABURGER_HOME`, then a short tour and teardown.\n\n'
        printf '## Setup\n\n'
        host_details
        printf -- '- Staged base URL: <%s>\n' "$base_url"
        printf -- '- Bootstrap: `%s` (SHA-256 `%s`)\n' "$bootstrap" "${bootstrap_digest:-not fetched}"
        printf -- '- `candidate.json` SHA-256: `%s`%s\n' "${candidate_digest:-not fetched}" \
            "${qualified_digest:+ (required: \`$qualified_digest\`)}"
        printf -- '- Command: `curl -fsSL %s | RELIABURGER_RELEASE_BASE_URL=%s sh -s -- --timings%s`\n' \
            "$bootstrap" "$base_url" "${setup_args[*]+ ${setup_args[*]}}"
        printf -- '- Relish: `%s`\n' "${relish_version:-not installed}"
        if [ "$result" = PASS ]; then printf -- '- Finished every step\n\n'; else printf -- '- Failed during: %s\n\n' "$stage"; fi
        printf '## Install\n\n'
        printf 'Wall time from the first `curl` to a ready cluster: %s s.\n\n' "${install_seconds:-n/a}"
        if [ -f "$evidence/install.log" ]; then
            printf '```\n'
            sed -n '/^where the time went/,$p' "$evidence/install.log"
            printf '```\n\n'
        fi
        if [ -f "$evidence/timings.json" ]; then
            printf '<details><summary>timings.json</summary>\n\n```json\n'
            cat "$evidence/timings.json"
            printf '```\n\n</details>\n\n'
        fi
        printf '## Downloaded bytes\n\n'
        cat "$evidence/digests.md" 2>/dev/null || printf 'Not collected.\n'
        printf '\n## Tour\n\n| Command | Time | Exit |\n|---|---|---|\n%s\n' "$tour_rows"
        printf '## Teardown\n\n%s\n\n' "$teardown"
        printf 'Evidence (logs, timings, candidate record): `%s`\n' "$evidence"
    } > "$record"
}

teardown_cluster() {
    local cluster name leftover
    if [ ! -x "$relish" ]; then
        teardown='nothing was installed'
        rm -rf "$home"
        return 0
    fi
    for cluster in "$home"/clusters/*/; do
        [ -d "$cluster" ] || continue
        name=$(basename "$cluster")
        "$relish" local destroy --name "$name" --yes >>"$evidence/teardown.log" 2>&1 \
            || { teardown="\`relish local destroy --name $name --yes\` failed; $home is left in place"; return 1; }
    done
    "$relish" uninstall --yes >>"$evidence/teardown.log" 2>&1 \
        || { teardown="\`relish uninstall --yes\` failed; $home is left in place"; return 1; }
    # Uninstall keeps what it didn't create, such as the saved context.
    leftover=$(find "$home" -mindepth 1 -maxdepth 1 -exec basename {} \; 2>/dev/null | tr '\n' ' ')
    teardown="\`relish local destroy --yes\` and \`relish uninstall --yes\` succeeded${leftover:+; they left ${leftover% }}"
    rm -rf "$home"
}

finish() {
    local status=$?
    trap - EXIT
    if [ "$keep" = true ]; then
        teardown="kept for debugging: RELIABURGER_HOME=$home"
    else
        if ! teardown_cluster; then
            status=1
            [ "$result" != PASS ] || stage=teardown
        fi
    fi
    if [ "$(real_home_state)" != "$real_before" ]; then
        printf 'qualify: ~/.reliaburger or ~/.local/bin/relish changed during the run\n' >&2
        teardown="$teardown. **The real ~/.reliaburger or ~/.local/bin/relish changed during the run.**"
        result=FAIL
        status=1
    fi
    [ "$status" -eq 0 ] || result=FAIL
    write_record
    printf '\n%s: record written to %s\n' "$result" "$record"
    exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

stage='fetching the candidate record'
curl -fsSL --proto '=https' "$base_url/candidate.json" -o "$evidence/candidate.json"
curl -fsSL --proto '=https' "$base_url/SHA256SUMS" -o "$evidence/SHA256SUMS"
candidate_digest=$(sha256 "$evidence/candidate.json")
if [ -n "$qualified_digest" ] && [ "$candidate_digest" != "$qualified_digest" ]; then
    fail "candidate.json has SHA-256 $candidate_digest, not the qualified $qualified_digest"
fi
# candidate.json is indented, sorted JSON: the asset's sha256 is the next line.
sums_expected=$(grep -A1 '"SHA256SUMS": {' "$evidence/candidate.json" | sed -n 's/.*"sha256": "\([0-9a-f]*\)".*/\1/p')
[ "$(sha256 "$evidence/SHA256SUMS")" = "$sums_expected" ] || fail 'SHA256SUMS differs from candidate.json'
curl -fsSL "$bootstrap" -o "$evidence/bootstrap.sh"
bootstrap_digest=$(sha256 "$evidence/bootstrap.sh")

stage='install (curl | sh)'
started=$(date +%s)
# The same pipeline a user types; the bootstrap passes --timings to setup.
# (`${a[@]+...}` because macOS's bash 3.2 calls an empty array unbound.)
curl -fsSL "$bootstrap" | RELIABURGER_RELEASE_BASE_URL=$base_url \
    sh -s -- --timings ${setup_args[@]+"${setup_args[@]}"} 2>&1 | tee "$evidence/install.log"
install_seconds=$(( $(date +%s) - started ))
relish_version=$("$relish" --version)
grep -q 'using an explicit release mirror' "$evidence/install.log" \
    || fail 'setup did not use the staged mirror'
for timings in "$home"/clusters/*/timings.json; do
    if [ -f "$timings" ]; then cp "$timings" "$evidence/timings.json"; fi
done
stage='checking downloaded bytes'
cache_digests > "$evidence/digests.md" \
    || fail 'a downloaded CLI, agent or guest image is not a candidate asset'

step 'apply the tour manifest' apply -f "$manifest"
stage='waiting for the tour apps'
deadline=$(( $(date +%s) + 300 ))
until apps_running; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
        "$relish" status >>"$evidence/tour.log" 2>&1 || true
        fail 'tour apps not running after 300 s'
    fi
    sleep 5
done
step 'status' status
step 'path' path frontend --to redis
sleep 10
step 'metrics' metrics frontend
result=PASS
