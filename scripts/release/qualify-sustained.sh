#!/usr/bin/env bash
# shellcheck disable=SC2016  # single-quoted guest commands expand in the guest
# V02 sustained soak: build a three-node quickstart cluster from a staged
# candidate, load it with data-bearing workloads, then inject faults on a fixed
# schedule for hours while checking data, certificate, recovery and leak
# invariants. Writes a Markdown record for docs/qualification/.
#
# The plan, invariants and thresholds: docs/plans/2026-09-25-v02-sustained.md.
# The cluster lives in an isolated RELIABURGER_HOME made by
# qualify-staged-install.sh --keep; the real ~/.reliaburger and
# ~/.local/bin/relish are never used, and the run fails if they change.
#
# Usage:
#   scripts/release/qualify-sustained.sh --base-url URL --qualified-digest SHA [options]
#
#   --base-url URL          staged candidate directory (the staging pre-release)
#   --qualified-digest SHA  require candidate.json to have this SHA-256
#   --soak-bun PATH         signed soak build named bun-v0.1.0-soak.1 with PATH.sig
#                           (release signature) beside it; the harness adds the
#                           operator signature to a copy (OpenSSL 3 needed);
#                           without it the upgrade slots are skipped
#   --tier T                fast (compressed schedule, 90m: the iteration loop
#                           after each round of fixes) or final (full schedule,
#                           8h: the acceptance run on the final candidate)
#   --duration D            soak length: 40m, 8h, 2d (default: the tier's, or 24h)
#   --schedule S            full (hourly cycle) or compressed (10-minute cycle);
#                           default: the tier's, or full
#                           An explicit --duration or --schedule overrides the
#                           tier; a final tier shorter than 8h or not on the
#                           full schedule never counts as acceptance
#   --evidence DIR          evidence directory (default: a new
#                           /var/tmp/reliaburger-v02.XXXXXX)
#   --record FILE           where to write the record (default: in the evidence
#                           directory); refuses to overwrite
#   --home DIR              adopt an isolated home an earlier --keep left
#                           instead of bootstrapping one (never destroyed)
#   --resume                continue the run in --evidence after an interruption;
#                           the gap is recorded as a pause of the soak clock
#   --keep                  leave the cluster and the installation afterwards
set -euo pipefail

usage() { sed -n '3,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
fail() { printf 'sustained: %s\n' "$*" >&2; exit 1; }
say() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*"; }

# A long run must not sleep with the lid open or on idle.
if [ "$(uname -s)" = Darwin ] && [ -z "${RELIABURGER_SOAK_CAFFEINATED:-}" ] && command -v caffeinate >/dev/null; then
    RELIABURGER_SOAK_CAFFEINATED=1 exec caffeinate -dimsu "$0" "$@"
fi

repository=$(cd "$(dirname "$0")/../.." && pwd)
checker=$repository/scripts/release/sustained_check.py
workloads_template=$repository/scripts/release/sustained/soak-workloads.toml
podinfo=$repository/examples/kubernetes/podinfo.yaml
base_url=
qualified_digest=
soak_bun=
tier=
duration_text=
schedule=
evidence=
record=
home=
resume=false
keep=false
while [ "$#" -gt 0 ]; do
    case $1 in
        --base-url) base_url=${2:-}; shift 2 ;;
        --qualified-digest) qualified_digest=${2:-}; shift 2 ;;
        --soak-bun) soak_bun=${2:-}; shift 2 ;;
        --tier) tier=${2:-}; shift 2 ;;
        --duration) duration_text=${2:-}; shift 2 ;;
        --schedule) schedule=${2:-}; shift 2 ;;
        --evidence) evidence=${2:-}; shift 2 ;;
        --record) record=${2:-}; shift 2 ;;
        --home) home=${2:-}; shift 2 ;;
        --resume) resume=true; shift ;;
        --keep) keep=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown argument: $1" ;;
    esac
done

# A resumed run keeps the tier, schedule and length it started with.
if [ "$resume" = true ] && [ -f "${evidence:-.}/metadata.json" ]; then
    started_with() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2], ""))' "$evidence/metadata.json" "$1"; }
    [ -n "$tier" ] || tier=$(started_with tier)
    [ -n "$schedule" ] || schedule=$(started_with schedule)
    [ -n "$duration_text" ] || { duration_text=$(started_with duration_target); duration_text=${duration_text:+$(( duration_text / 60 ))m}; }
fi
# The two tiers (plan D3): fast for the fix-and-rerun loop, final for acceptance.
case $tier in
    fast) : "${schedule:=compressed}" "${duration_text:=90m}" ;;
    final) : "${schedule:=full}" "${duration_text:=8h}" ;;
    ''|custom) tier=custom; : "${schedule:=full}" "${duration_text:=24h}" ;;
    *) fail '--tier must be fast or final' ;;
esac

case $duration_text in
    *[0-9]m) duration=$(( ${duration_text%m} * 60 )) ;;
    *[0-9]h) duration=$(( ${duration_text%h} * 3600 )) ;;
    *[0-9]d) duration=$(( ${duration_text%d} * 86400 )) ;;
    *) fail '--duration must look like 40m, 24h or 2d' ;;
esac
# Schedule parameters: cycle length, power-off hold, TLS rotation period, how
# often the catalogue pulse, offline export and registry push run, and how many
# chaos kinds each chaos slot runs. Two a slot lets an 8-hour full run (four
# chaos slots with upgrade walks) reach all seven kinds. The run always
# completes min_cycles cycles, even when faults overrun their slots: every
# hour of a full run (so every special its length schedules), and cycles 0-3
# of a compressed one, which carry every fault kind and every special.
case $schedule in
    full) cycle=3600; hold_min=60; hold_max=180; rotation=900; kill_offset=20
          quorum_hold=300; pulse_every=1; export_every=6; push_every=1; chaos_per_slot=2
          min_cycles=$(( duration / cycle )) ;;
    compressed) cycle=600; hold_min=20; hold_max=40; rotation=300; kill_offset=10
          quorum_hold=60; pulse_every=0; export_every=2; push_every=1; chaos_per_slot=2
          min_cycles=4 ;;
    *) fail '--schedule must be full or compressed' ;;
esac
slot=$(( cycle / 6 ))
light_every=30
heavy_every=300
leaf_lifetime=3600

if [ "$resume" = true ]; then
    [ -f "${evidence:-.}/metadata.json" ] || fail '--resume needs --evidence with an earlier run'
else
    [ -n "$base_url" ] || [ -n "$home" ] || { usage >&2; fail '--base-url (or --home) is required'; }
fi
if [ -n "$base_url" ]; then
    case $base_url in
        https://*[?#@\\[:space:]]*|https:///*) fail 'the base URL must be HTTPS without credentials, query or fragment' ;;
        https://?*) base_url=${base_url%/} ;;
        *) fail 'the base URL must be HTTPS' ;;
    esac
fi
if [ -n "$qualified_digest" ] && ! [[ $qualified_digest =~ ^[0-9a-f]{64}$ ]]; then
    fail '--qualified-digest must be a SHA-256 in lowercase hex'
fi
if [ -n "$soak_bun" ]; then
    if [ ! -f "$soak_bun" ] || [ ! -f "$soak_bun.sig" ]; then fail "--soak-bun needs $soak_bun and $soak_bun.sig"; fi
    soak_bun=$(cd "$(dirname "$soak_bun")" && pwd)/${soak_bun##*/}
    case ${soak_bun##*/} in bun-v*) ;; *) fail 'the soak build must be named bun-vVERSION (the version comes from the name)' ;; esac
fi
for tool in curl openssl python3 perl; do
    command -v "$tool" >/dev/null || fail "$tool is required"
done

sha256() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}

real_home_state() {
    ls -1A "$HOME/.reliaburger" 2>/dev/null || true
    readlink "$HOME/.local/bin/relish" 2>/dev/null || true
}
real_before=$(real_home_state)

if [ -z "$evidence" ]; then
    evidence=$(mktemp -d /var/tmp/reliaburger-v02.XXXXXX)
elif [ "$resume" = false ]; then
    mkdir -p "$evidence"
    [ -z "$(ls -A "$evidence")" ] || fail "$evidence is not empty (use --resume to continue a run)"
fi
evidence=$(cd "$evidence" && pwd)
[ -n "$record" ] || record=$evidence/record.md
[ ! -e "$record" ] || fail "$record already exists"
mkdir -p "$evidence/snapshots" "$evidence/failures" "$evidence/tls" "$evidence/config"
chmod 700 "$evidence"
say "evidence: $evidence"

check() { python3 "$checker" "$@"; }
event() { check event "$evidence" "$@" || true; }
meta() { python3 - "$evidence/metadata.json" "$@" <<'PY'
import json, sys
path, key, value = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    data = json.load(open(path))
except FileNotFoundError:
    data = {}
if key.endswith("+"):
    data.setdefault(key[:-1], []).append(value)
elif value.startswith("json:"):
    data[key] = json.loads(value[5:])
else:
    data[key] = value
json.dump(data, open(path, "w"), indent=1, sort_keys=True)
PY
}
meta_get() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2], ""))' "$evidence/metadata.json" "$1" 2>/dev/null || true; }

# Kill a command that outlives its budget (macOS has no timeout(1)).
with_timeout() { perl -e 'alarm shift; exec @ARGV or die "exec: $!\n"' "$@"; }

# --- the operator signature -----------------------------------------------------

# Network upgrades, and a cluster's `upgrade start --binary` is one (the nodes
# fetch it from the registry), need a second signature from the operator's key
# in [upgrades] external_signing_key. The soak plays the operator: a throwaway
# Ed25519 key, kept in the evidence so --resume reuses it, and a copy of the
# soak build's envelope with that key's signature added. The release signature
# is copied as it is; the release key is never needed. openssl rather than
# `relish dev countersign-binary`, so it works whatever relish the candidate has.
operator_public=
soak_sig=
countersign_soak_bun() {
    local key=$evidence/config/operator.key signature
    if [ ! -f "$key" ]; then
        [ "$resume" = false ] || fail '--soak-bun on --resume needs a run that started with --soak-bun'
        (umask 077 && openssl genpkey -algorithm ed25519 -outform DER -out "$key" 2>/dev/null) \
            || fail 'openssl cannot make an Ed25519 key (OpenSSL 3 is needed, not LibreSSL)'
    fi
    operator_public="ed25519:$(openssl pkey -inform DER -in "$key" -pubout -outform DER | tail -c 32 | base64 | tr -d '\n')"
    signature=$(openssl pkeyutl -sign -inkey "$key" -keyform DER -rawin -in "$soak_bun" | base64 | tr -d '\n') \
        || fail 'openssl cannot sign with Ed25519 (OpenSSL 3 is needed, not LibreSSL)'
    mkdir -p "$evidence/upgrade"
    soak_sig=$evidence/upgrade/${soak_bun##*/}.sig
    python3 - "$soak_bun.sig" "$soak_sig" "$(sha256 "$soak_bun")" "$signature" <<'PY' || fail "$soak_bun.sig does not belong to $soak_bun"
import json, sys
source, target, digest, signature = sys.argv[1:]
envelope = json.load(open(source))
if envelope.get("sha256", "").lower() != digest or not envelope.get("embedded"):
    sys.exit(1)
envelope["external"] = signature
json.dump(envelope, open(target, "w"), indent=2)
PY
}
[ -z "$soak_bun" ] || countersign_soak_bun

# --- the cluster --------------------------------------------------------------

api_port=29117
ingress_port=28080
registry_port=25050
bootstrapped=false
vm=()
address=()
down=(0 0 0 0)

bootstrap() {
    say 'bootstrapping the cluster with qualify-staged-install.sh --keep'
    local staged=("$repository/scripts/release/qualify-staged-install.sh" --base-url "$base_url" --keep
                  --record "$evidence/bootstrap-record.md")
    [ -z "$qualified_digest" ] || staged+=(--qualified-digest "$qualified_digest")
    staged+=(-- --api-port "$api_port" --ingress-port "$ingress_port" --registry-port "$registry_port")
    local result=0
    "${staged[@]}" > "$evidence/bootstrap.log" 2>&1 || result=$?
    home=$(sed -n 's/^RELIABURGER_HOME: //p' "$evidence/bootstrap.log" | head -n 1)
    [ -z "$home" ] || bootstrapped=true
    [ "$result" -ne 0 ] || return 0
    # A failed tour step after a finished install still leaves a cluster to
    # soak; the staged-install qualification owns that step, so note it.
    local stage
    stage=$(sed -n 's/^- Failed during: //p' "$evidence/bootstrap-record.md" 2>/dev/null)
    case $stage in
        status|path|metrics)
            meta notes+ "The bootstrap's staged-install tour failed at \`relish $stage\` (see bootstrap-record.md); the install had finished, so the soak went ahead"
            say "bootstrap tour failed at $stage; the cluster is installed, continuing" ;;
        *) fail "bootstrap failed during ${stage:-an unknown stage} (exit $result); see $evidence/bootstrap.log" ;;
    esac
}

discover() {
    case $home in
        ''|"$HOME/.reliaburger"|"$HOME/.reliaburger/"*) fail "refusing to use RELIABURGER_HOME=$home" ;;
    esac
    [ -f "$home/clusters/laptop/state.json" ] || fail "no quickstart cluster in $home"
    export RELIABURGER_HOME=$home RELIABURGER_NO_MODIFY_PATH=1
    export LIMA_HOME=$home/lima
    relish=$home/bin/relish
    limactl=$home/tools/lima-2.1.0/bin/limactl
    if [ ! -x "$relish" ] || [ ! -x "$limactl" ]; then fail "no relish or limactl in $home"; fi
    local state=$home/clusters/laptop/state.json index name ip
    read -r api_port ingress_port registry_port < <(python3 -c '
import json, sys
spec = json.load(open(sys.argv[1]))["spec"]
print(spec["api_port"], spec["ingress_port"], spec["registry_port"])' "$state")
    vm=(none)
    address=(none)
    while read -r index name ip; do
        vm[index]=$name
        address[index]=$ip
    done < <(python3 -c '
import json, sys
for index, node in enumerate(json.load(open(sys.argv[1]))["nodes"], 1):
    print(index, node["name"], node.get("address") or "-")' "$state")
    [ "${#vm[@]}" -eq 4 ] || fail 'the soak needs a three-node quickstart cluster'
    ca=$home/clusters/laptop/security/identity/root-ca.crt
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["token"])' "$home/context.json" > "$evidence/.token"
    chmod 600 "$evidence/.token"
    printf 'Authorization: Bearer %s\n' "$(cat "$evidence/.token")" > "$evidence/.auth-header"
    chmod 600 "$evidence/.auth-header"
    say "cluster: ${vm[1]} ${vm[2]} ${vm[3]} (API ${api_port}-$(( api_port + 2 )), ingress $ingress_port)"
}

node_names() { printf '%s\n' "${vm[1]}" "${vm[2]}" "${vm[3]}"; }
node_index() { local i; for i in 1 2 3; do [ "${vm[i]}" != "$1" ] || { echo "$i"; return 0; }; done; return 1; }

# Run a command in a guest as root; `gsh_in` reads a script from stdin.
gsh() { local node=$1; shift; with_timeout 120 "$limactl" shell --workdir / "${vm[node]}" sudo bash -c "$*"; }
gsh_in() { local node=$1; shift; with_timeout 180 "$limactl" shell --workdir / "${vm[node]}" sudo bash -s -- "$@"; }

# Relish through one node's forwarded API. `rel` picks a node that is up.
# The first node that is up, not the fault's target and answering /v1/health
# (a crash-looping bun is up as far as the harness knows).
api_node() {
    local i
    for i in 1 2 3; do
        if [ "${down[i]}" = 1 ] || [ "${avoid:-0}" = "$i" ]; then continue; fi
        if curl -fsS --max-time 3 --cacert "$ca" --connect-to "${vm[i]}:9117:127.0.0.1:$(( api_port + i - 1 ))" \
            "https://${vm[i]}:9117/v1/health" > /dev/null 2>&1; then
            echo "$i"
            return
        fi
    done
    for i in 1 2 3; do [ "${down[i]}" = 1 ] || { echo "$i"; return; }; done
    echo 1
}
# Relish gets 330 s (exec's own limit is 300 s); `relish test` runs set
# rel_budget for the call.
rel_budget=330
rel_on() {
    local node=$1; shift
    RELIABURGER_TOKEN=$(cat "$evidence/.token") with_timeout "$rel_budget" "$relish" \
        --endpoint "https://127.0.0.1:$(( api_port + node - 1 ))" --ca-cert "$ca" "$@"
}
rel() { rel_on "$(api_node)" "$@"; }
api_get() {
    local node=$1 path=$2
    curl -fsS --max-time 20 --cacert "$ca" -H @"$evidence/.auth-header" \
        --connect-to "${vm[node]}:9117:127.0.0.1:$(( api_port + node - 1 ))" "https://${vm[node]}:9117$path"
}
kill_bun() {
    local node=$1
    check expect "$evidence" restart "${vm[node]}"
    gsh "$node" 'kill -9 "$(systemctl show -p MainPID --value reliaburger.service)"'
}

# --- the soak CA and operator ingress certificates ------------------------------

tls=$evidence/tls
ingress_rotations=0

soak_ca_init() {
    [ -f "$tls/ca.crt" ] && return 0
    mkdir -p "$tls/issued"
    : > "$tls/index.txt"
    echo 1000 > "$tls/serial"
    cat > "$tls/ca.cnf" <<EOF
[ca]
default_ca = soak
[soak]
database = $tls/index.txt
new_certs_dir = $tls/issued
serial = $tls/serial
certificate = $tls/ca.crt
private_key = $tls/ca.key
default_md = sha256
policy = anything
unique_subject = no
x509_extensions = leaf
[anything]
commonName = supplied
[leaf]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = serverAuth
subjectAltName = DNS:podinfo.localhost, DNS:localhost
[req]
distinguished_name = name
[name]
[ca_ext]
basicConstraints = critical, CA:TRUE
keyUsage = critical, keyCertSign, cRLSign
EOF
    openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$tls/ca.key" 2>/dev/null
    openssl req -x509 -new -key "$tls/ca.key" -subj '/CN=Reliaburger V02 soak CA' -days 7 \
        -config "$tls/ca.cnf" -extensions ca_ext -out "$tls/ca.crt"
}

# Issue a leaf valid from START to END (epoch seconds) with a new key into DIR.
issue_leaf() {
    local directory=$1 start=$2 end=$3
    mkdir -p "$directory"
    openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$directory/key.pem" 2>/dev/null
    openssl req -new -key "$directory/key.pem" -subj '/CN=podinfo.localhost' -config "$tls/ca.cnf" -out "$directory/csr.pem"
    openssl ca -batch -notext -config "$tls/ca.cnf" -in "$directory/csr.pem" -out "$directory/cert.pem" \
        -startdate "$(check utc-stamp "$start")" -enddate "$(check utc-stamp "$end")" 2>/dev/null
    openssl x509 -noout -serial -in "$directory/cert.pem" | sed 's/^serial=//'
}

# Install a certificate and key as a node's operator ingress pair.
push_pair() {
    local node=$1 cert=$2 key=$3
    { cat "$key"; printf '@@\n'; cat "$cert"; } | gsh "$node" '
        set -e
        umask 077
        mkdir -p /etc/reliaburger/soak-tls
        awk -v key=/etc/reliaburger/soak-tls/ingress.key.new -v cert=/etc/reliaburger/soak-tls/ingress.crt.new \
            '"'"'/^@@$/ {part = 1; next} {print > (part ? cert : key)}'"'"'
        mv /etc/reliaburger/soak-tls/ingress.key.new /etc/reliaburger/soak-tls/ingress.key
        mv /etc/reliaburger/soak-tls/ingress.crt.new /etc/reliaburger/soak-tls/ingress.crt'
}

served_ingress_serial() {
    gsh "$1" 'echo | timeout 5 openssl s_client -connect 127.0.0.1:443 -servername podinfo.localhost 2>/dev/null | openssl x509 -noout -serial' \
        2>/dev/null | sed -n 's/^serial=//p'
}

# Rotate every live node's operator ingress pair. Every fourth rotation first
# offers an invalid pair (torn or expired), which must be refused while the
# last good pair keeps serving.
rotate_ingress() {
    local now serial previous kind node started served elapsed directory
    now=$(date +%s)
    ingress_rotations=$(( ingress_rotations + 1 ))
    previous=$(cat "$tls/current/serial" 2>/dev/null || true)
    if [ $(( ingress_rotations % 4 )) -eq 0 ] && [ -n "$previous" ]; then
        if [ $(( ingress_rotations % 8 )) -eq 0 ]; then kind=expired; else kind=torn; fi
        directory=$tls/invalid-$ingress_rotations
        if [ "$kind" = expired ]; then
            issue_leaf "$directory" $(( now - 7200 )) $(( now - 3600 )) > /dev/null
            cp "$directory/key.pem" "$directory/offered-key.pem"
        else
            issue_leaf "$directory" $(( now - 60 )) $(( now + 2700 )) > /dev/null
            cp "$tls/current/key.pem" "$directory/offered-key.pem"
        fi
        for node in 1 2 3; do
            [ "${down[node]}" = 0 ] || continue
            push_pair "$node" "$directory/cert.pem" "$directory/offered-key.pem" || true
        done
        sleep 3
        for node in 1 2 3; do
            [ "${down[node]}" = 0 ] || continue
            served=$(served_ingress_serial "$node" || true)
            if [ "$served" = "$previous" ]; then
                event --phase "tls:invalid-$kind" --target "${vm[node]}" --verdict ok --detail "refused; still serving $served"
            else
                event --phase "tls:invalid-$kind" --target "${vm[node]}" --verdict fail --detail "served ${served:-nothing}, expected $previous"
                record_failure "ingress-invalid-pair" "${vm[node]} served ${served:-nothing} after a $kind pair; expected the last good $previous" '' "ingress-invalid-pair|${vm[node]}"
            fi
        done
    fi
    directory=$tls/leaf-$ingress_rotations
    serial=$(issue_leaf "$directory" $(( now - 60 )) $(( now + 2700 )))
    printf '%s\n' "$serial" > "$directory/serial"
    for node in 1 2 3; do
        [ "${down[node]}" = 0 ] || continue
        started=$(date +%s)
        push_pair "$node" "$directory/cert.pem" "$directory/key.pem" || { event --phase tls:reload --target "${vm[node]}" --verdict skipped --detail 'push failed'; continue; }
        check ingress-expect "$evidence" "${vm[node]}" "$serial"
        served=
        while :; do
            served=$(served_ingress_serial "$node" || true)
            elapsed=$(( $(date +%s) - started ))
            if [ "$served" = "$serial" ] || [ "$elapsed" -ge 15 ]; then break; fi
            sleep 1
        done
        if [ "$served" = "$serial" ] && [ "$elapsed" -le 5 ]; then
            check count "$evidence" ingress-reload
            event --phase tls:reload --target "${vm[node]}" --verdict ok --duration "$elapsed" --detail "serving $serial"
        else
            event --phase tls:reload --target "${vm[node]}" --verdict fail --duration "$elapsed" --detail "served ${served:-nothing}"
            record_failure "ingress-reload" "${vm[node]} served ${served:-nothing} ${elapsed} s after the $serial pair landed" '' "ingress-reload|${vm[node]}"
        fi
    done
    rm -rf "$tls/current"
    cp -R "$directory" "$tls/current"
    next_rotation=$(( $(date +%s) + rotation ))
}

# --- configuration --------------------------------------------------------------

leaf_supported=unknown

probe_leaf_lifetime() {
    local node=1 probe=$evidence/config/probe.toml output
    cp "$evidence/config/node-1.orig.toml" "$probe"
    # A half-set ingress pair fails validation right after parsing, so bun
    # exits before it starts anything: an unknown-field error means the key
    # is not supported, the ingress error means the file parsed.
    check toml-set "$probe" security.leaf_lifetime_override_secs=$leaf_lifetime 'ingress.tls_cert="/nonexistent/soak-probe.crt"'
    output=$(gsh_in "$node" < <(printf 'umask 077\ncat > /var/tmp/soak-probe.toml <<"TOML"\n%s\nTOML\ntimeout 20 /usr/local/bin/bun --cluster --runtime runc --config /var/tmp/soak-probe.toml --listen 127.0.0.1:1 2>&1 || true\nrm -f /var/tmp/soak-probe.toml\n' "$(cat "$probe")") || true)
    printf '%s\n' "$output" > "$evidence/config/leaf-probe.log"
    case $output in
        *leaf_lifetime_override_secs*) leaf_supported=no ;;
        *tls_cert/tls_key*) leaf_supported=yes ;;
        *) leaf_supported=unknown ;;
    esac
    say "leaf_lifetime_override_secs on this build: $leaf_supported"
}

wait_node_ready() {
    local node=$1 deadline=$(( $(date +%s) + ${2:-180} ))
    until api_get "$node" /v1/readiness 2>/dev/null | grep -q '"ready":true'; do
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 3
    done
}

configure_nodes() {
    local node label settings
    soak_ca_init
    mkdir -p "$tls/current"
    printf '%s\n' "$(issue_leaf "$tls/current" $(( $(date +%s) - 60 )) $(( $(date +%s) + 2700 )))" > "$tls/current/serial"
    for node in 1 2 3; do
        gsh "$node" '[ -f /etc/reliaburger/node.toml.pre-soak ] || cp /etc/reliaburger/node.toml /etc/reliaburger/node.toml.pre-soak; cat /etc/reliaburger/node.toml.pre-soak' \
            > "$evidence/config/node-$node.orig.toml"
    done
    probe_leaf_lifetime
    meta deviations+ 'node.toml `[testing] allowed_operations` adds `provision_isolated_workloads` (quickstart allows only inject_workload_faults and alter_node_state), so the catalogue pulse can lease test namespaces'
    meta deviations+ 'node.toml `[logs]`/`[metrics]` export to file:///var/lib/reliaburger/soak-export every 60 s with max_storage_mb = 8, `[storage.snapshots]` every 900 s (compressed: 120 s), retain 4, uploaded to the same directory'
    meta deviations+ 'node.toml `[ingress] tls_cert/tls_key` point at an operator pair from a soak CA, 45-minute leaves rotated by the harness'
    meta deviations+ 'node.toml `[node.labels] soak-volume` = "writer" on node 2 and "redis" on node 3, to pin the volume apps'
    if [ -n "$operator_public" ]; then
        meta deviations+ "node.toml \`[upgrades] external_signing_key\` = a throwaway operator key the harness made for this run (\`$operator_public\`); the soak build's \`.sig\` gets that key's external signature beside the release one (D2)"
    fi
    if [ "$leaf_supported" = yes ]; then
        meta deviations+ "node.toml \`[security] leaf_lifetime_override_secs = $leaf_lifetime\` (D1)"
        meta leaf_lifetime_secs "json:$leaf_lifetime"
    else
        meta notes+ "This build does not accept \`[security] leaf_lifetime_override_secs\` ($leaf_supported); node leaves keep their one-year lifetime and node renewals are not measured"
    fi
    for node in 3 2 1; do
        label=
        [ "$node" -ne 2 ] || label=writer
        [ "$node" -ne 3 ] || label=redis
        cp "$evidence/config/node-$node.orig.toml" "$evidence/config/node-$node.soak.toml"
        settings=(
            'logs.export_path="file:///var/lib/reliaburger/soak-export/logs"'
            logs.export_interval_secs=60
            logs.max_storage_mb=8
            'metrics.export_path="file:///var/lib/reliaburger/soak-export/metrics"'
            metrics.max_storage_mb=8
            "storage.snapshots.interval_secs=$([ "$schedule" = full ] && echo 900 || echo 120)"
            storage.snapshots.retain=4
            'storage.snapshots.upload_url="file:///var/lib/reliaburger/soak-export/snapshots"'
            'testing.allowed_operations=["inject_workload_faults", "alter_node_state", "provision_isolated_workloads"]'
            'ingress.tls_cert="/etc/reliaburger/soak-tls/ingress.crt"'
            'ingress.tls_key="/etc/reliaburger/soak-tls/ingress.key"'
        )
        [ -z "$label" ] || settings+=("node.labels.soak-volume=\"$label\"")
        [ "$leaf_supported" != yes ] || settings+=("security.leaf_lifetime_override_secs=$leaf_lifetime")
        [ -z "$operator_public" ] || settings+=("upgrades.external_signing_key=\"$operator_public\"")
        check toml-set "$evidence/config/node-$node.soak.toml" "${settings[@]}"
        push_pair "$node" "$tls/current/cert.pem" "$tls/current/key.pem"
        gsh_in "$node" < <(printf 'set -e\numask 077\nmkdir -p /var/lib/reliaburger/soak-export\ncat > /etc/reliaburger/node.toml.new <<"TOML"\n%s\nTOML\nmv /etc/reliaburger/node.toml.new /etc/reliaburger/node.toml\n' "$(cat "$evidence/config/node-$node.soak.toml")")
        # SIGKILL, not a graceful restart: systemd restarts bun with the new
        # file. A graceful `systemctl restart` currently wedges a node whose
        # retirement it interrupts (see the plan's product findings).
        avoid=$node
        kill_bun "$node"
        wait_node_ready "$node" 180 || setup_fail "${vm[node]} not ready after applying the soak node.toml"
        wait_cluster 300 || setup_fail "cluster not healthy after reconfiguring ${vm[node]}"
        avoid=0
        event --phase configure --target "${vm[node]}" --verdict ok
    done
    check ingress-expect "$evidence" "${vm[1]}" "$(cat "$tls/current/serial")"
    check ingress-expect "$evidence" "${vm[2]}" "$(cat "$tls/current/serial")"
    check ingress-expect "$evidence" "${vm[3]}" "$(cat "$tls/current/serial")"
    next_rotation=$(( $(date +%s) + rotation ))
    wait_labels
}

# The pinned volume apps can't be placed until the cluster sees the labels.
# On the 0.1.0 candidate a restarted node sometimes never republishes its
# node.toml labels; that is a failure row, and one more restart is tried.
# Print the nodes (2, 3) whose soak-volume label the cluster doesn't show yet.
missing_labels() {
    rel --output json nodes > "$evidence/.nodes.json" 2>/dev/null || { echo 2 3; return; }
    python3 - "$evidence/.nodes.json" "${vm[2]}" "${vm[3]}" <<'PY'
import json, sys
labels = {node["node_id"]: node.get("labels", {}).get("soak-volume") for node in json.load(open(sys.argv[1]))}
print(" ".join(index for index, name, want in (("2", sys.argv[2], "writer"), ("3", sys.argv[3], "redis"))
               if labels.get(name) != want))
PY
}

wait_labels() {
    local attempt deadline node missing
    for attempt in 1 2; do
        deadline=$(( $(date +%s) + 120 ))
        while [ "$(date +%s)" -lt "$deadline" ]; do
            missing=$(missing_labels)
            [ -n "$missing" ] || return 0
            sleep 5
        done
        [ "$attempt" -eq 1 ] || setup_fail 'the soak-volume node labels never reached the cluster'
        record_failure node-labels "node.toml labels not visible in relish nodes 120 s after the restart: $(tr -d '\n ' < "$evidence/.nodes.json" | head -c 400)"
        for node in $missing; do
            avoid=$node
            kill_bun "$node"
            wait_node_ready "$node" 180 || setup_fail "${vm[node]} not ready after a restart for its labels"
            wait_cluster 300 || setup_fail "cluster not healthy after restarting ${vm[node]} for its labels"
        done
        avoid=0
    done
}

# One leader, three live voters.
wait_cluster() {
    local deadline=$(( $(date +%s) + $1 )) file=$evidence/.nodes.json
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if rel --output json nodes > "$file" 2>/dev/null \
            && [ "$(check nodes-json "$file" council | wc -l)" -eq 3 ] \
            && [ "$(check nodes-json "$file" alive | wc -l)" -eq 3 ] \
            && [ "$(check nodes-json "$file" leader | wc -l)" -eq 1 ]; then
            return 0
        fi
        sleep 5
    done
    return 1
}

generation=0
write_workloads() {
    sed "s/SOAK_GENERATION = \"[0-9]*\"/SOAK_GENERATION = \"$generation\"/" "$workloads_template" > "$evidence/workloads.toml"
}

apply_workloads() {
    write_workloads
    rel apply -f "$podinfo" > "$evidence/config/apply-podinfo.log" 2>&1 || fail "applying $podinfo failed"
    rel apply -f "$evidence/workloads.toml" > "$evidence/config/apply-workloads.log" 2>&1 || fail 'applying the soak workloads failed'
    local deadline=$(( $(date +%s) + 600 )) file=$evidence/.status.json
    until rel --output json status > "$file" 2>/dev/null && python3 - "$file" <<'PY'
import json, sys
want = {"soak-writer": 1, "soak-redis": 1, "soak-redis-client": 1, "soak-spammer": 1, "soak-identity": 3,
        "frontend": 3, "backend": 1, "redis": 1, "loadgen": 1}
running = {}
for instance in json.load(open(sys.argv[1])):
    if instance["state"] == "running":
        running[instance["app_name"]] = running.get(instance["app_name"], 0) + 1
sys.exit(0 if all(running.get(app, 0) >= n for app, n in want.items()) else 1)
PY
    do
        [ "$(date +%s)" -lt "$deadline" ] || setup_fail 'soak workloads not running after 600 s'
        sleep 10
    done
    event --phase configure --target workloads --verdict ok --detail "generation $generation"
}

# --- evidence collection ---------------------------------------------------------

# Everything one guest reports in a single shell: leak inventory, served
# certificates, exporter source/destination and snapshot archives.
guest_report() {
    cat <<'GUEST'
section() { printf '@@ %s\n' "$1"; }
section inventory
pid=$(systemctl show -p MainPID --value reliaburger.service)
echo "boot $(cat /proc/sys/kernel/random/boot_id)"
echo "nrestarts $(systemctl show -p NRestarts --value reliaburger.service)"
echo "bun_pid $pid"
if [ "${pid:-0}" -gt 0 ]; then
    echo "bun_fd $(ls /proc/"$pid"/fd | wc -l)"
    echo "bun_rss_kb $(awk '/^VmRSS/ {print $2}' /proc/"$pid"/status)"
fi
runc --root /var/lib/reliaburger/data/instances/runc/state list -q 2>/dev/null | sed 's/^/runc /'
ls /run/netns 2>/dev/null | sed 's/^/netns /'
ip -o link show type veth | awk -F': ' '{split($2, name, "@"); print "veth " name[1]}'
find /sys/fs/cgroup/reliaburger -mindepth 3 -maxdepth 3 -type d 2>/dev/null | sed 's|^/sys/fs/cgroup/reliaburger/|cgroup |'
find /sys/fs/bpf -mindepth 1 2>/dev/null | sed 's|^/sys/fs/bpf/|bpf |'
ss -ltnH | awk '{print "listen " $4}'
leases=/var/lib/reliaburger/data/instances/runc/bundles/.network-leases.json
[ ! -f "$leases" ] || python3 -c 'import json, sys; [print("lease", key) for key in json.load(open(sys.argv[1])).get("allocations", {})]' "$leases"
for directory in data images logs metrics volumes soak-export; do
    echo "disk $directory $(du -sk /var/lib/reliaburger/$directory 2>/dev/null | cut -f1)"
done
echo "panics $(journalctl -u reliaburger --no-pager -q --cursor-file=/var/tmp/soak-journal.cursor 2>/dev/null | grep -c 'panicked at')"
section ingress
echo | timeout 5 openssl s_client -connect 127.0.0.1:443 -servername podinfo.localhost 2>/dev/null | openssl x509 -noout -serial -enddate 2>/dev/null
section api
echo | timeout 5 openssl s_client -connect 127.0.0.1:9117 2>/dev/null | openssl x509 -noout -serial -enddate 2>/dev/null
section export
for kind in logs metrics; do
    source=/var/lib/reliaburger/$kind
    [ "$kind" = metrics ] || source=$source/parquet
    find "$source" -maxdepth 1 -name '*.parquet' -type f 2>/dev/null | while read -r file; do
        echo "src $kind ${file##*/} $(sha256sum "$file" | cut -d' ' -f1)"
    done
    destination=/var/lib/reliaburger/soak-export/$kind
    verified=$destination/.soak-verified
    find "$destination" -mindepth 2 -maxdepth 2 -name '*.parquet' -type f 2>/dev/null | while read -r file; do
        name=${file#"$destination"/}
        if grep -qxF "$name" "$verified" 2>/dev/null; then
            echo "dest $kind $name verified"
        else
            digest=$(sha256sum "$file" | cut -d' ' -f1)
            echo "dest $kind $name $digest"
            case ${file##*/} in "$digest"-*) echo "$name" >> "$verified" ;; esac
        fi
    done
done
find /var/lib/reliaburger/soak-export/snapshots -name '*.tar.gz' -type f 2>/dev/null | while read -r file; do
    if gzip -t "$file" 2>/dev/null; then state=ok; else state=bad; fi
    echo "snap ${file#/var/lib/reliaburger/soak-export/snapshots/} $state"
done
GUEST
}

split_report() {
    local directory=$1 name=$2
    awk -v directory="$directory" -v name="$name" '
        /^@@ / {file = directory "/" $2 "-" name ".txt"; next}
        file {print > file}'
}

snapshot_count=0
snapshot_dir=
new_snapshot() {
    snapshot_count=$(( snapshot_count + 1 ))
    snapshot_dir=$evidence/snapshots/$(date -u +%Y%m%dT%H%M%S)-$1-$snapshot_count
    mkdir -p "$snapshot_dir"
    printf '{"ts": %s, "kind": "%s", "slot": "%s"}\n' "$(date +%s)" "$1" "${current_slot:-}" > "$snapshot_dir/meta.json"
}

collect_light() {
    local directory=$1
    curl -s -o /dev/null -w '%{http_code} %{time_total}\n' --max-time 5 -H 'Host: podinfo.localhost' \
        "http://127.0.0.1:$ingress_port/" > "$directory/http.txt" 2>/dev/null || true
    rel logs soak-writer --tail 20 > "$directory/writer-log.txt" 2>&1 || true
    rel logs soak-redis-client --tail 20 > "$directory/redis-log.txt" 2>&1 || true
}

# settle: what recovery needs; heavy: everything.
collect() {
    local kind=$1 directory=$2 node exit_code=0
    collect_light "$directory"
    rel --output json wtf > "$directory/wtf.json" 2>"$directory/wtf.err" || exit_code=$?
    echo "$exit_code" > "$directory/wtf.exit"
    rel --output json nodes > "$directory/nodes.json" 2>/dev/null || rm -f "$directory/nodes.json"
    rel --output json status > "$directory/status.json" 2>/dev/null || rm -f "$directory/status.json"
    for node in 1 2 3; do
        [ "${down[node]}" = 0 ] || continue
        if [ "$kind" = heavy ]; then
            api_get "$node" /v1/diagnostics > "$directory/diagnostics-${vm[node]}.json" 2>/dev/null || true
            guest_report | gsh_in "$node" 2>/dev/null | split_report "$directory" "${vm[node]}" || true
            rel_on "$node" exec soak-identity cat /run/reliaburger/identity/cert.pem 2>/dev/null \
                | openssl x509 -noout -serial -enddate > "$directory/identity-${vm[node]}.txt" 2>/dev/null || true
        else
            guest_report | sed '/^section ingress/,$d' | gsh_in "$node" 2>/dev/null | split_report "$directory" "${vm[node]}" || true
        fi
    done
    if [ "$kind" = heavy ]; then
        # The writer's volume lives on node 2; exec reaches the local instance.
        [ "${down[2]}" = 1 ] || rel_on 2 exec soak-writer awk '$0 != NR { print "BAD " NR ": " $0; exit } END { print "LAST " NR }' /data/seq \
            > "$directory/writer-file.txt" 2>/dev/null || rm -f "$directory/writer-file.txt"
        [ "${down[1]}" = 1 ] || registry verify > "$directory/registry.json" 2>/dev/null || true
    fi
}

registry() {
    check registry "$evidence" "$1" --port "$registry_port" --server-name "${vm[1]}" --ca "$ca" \
        --token-file "$evidence/.token" "${@:2}"
}

failure_count=0
record_failure() {
    local check_name=$1 detail=$2 snapshot=${3:-} key=${4:-} directory node
    # A keyed failure that keeps recurring gets one bundle per half hour.
    if [ -n "$key" ] && check seen "$evidence" "$key"; then
        event --phase failure --target "$check_name" --verdict repeat --detail "$detail"
        return 0
    fi
    failure_count=$(( $(find "$evidence/failures" -mindepth 1 -maxdepth 1 -type d | wc -l) + 1 ))
    directory=$evidence/failures/$failure_count
    mkdir -p "$directory"
    python3 - "$directory/summary.json" "$check_name" "$detail" "${current_slot:-}" "${snapshot:-}" <<'PY'
import json, sys, time
path, check, detail, slot, snapshot = sys.argv[1:]
json.dump({"ts": int(time.time()), "check": check, "detail": detail, "slot": slot, "snapshot": snapshot,
           "class": "unclassified", "cause": "open", "action": "open", "disposition": "open"},
          open(path, "w"), indent=1)
PY
    [ -z "$snapshot" ] || cp -R "$snapshot" "$directory/snapshot"
    for node in 1 2 3; do
        [ "${down[node]}" = 0 ] || continue
        gsh "$node" 'journalctl -u reliaburger --since "-15min" --no-pager' > "$directory/journal-${vm[node]}.txt" 2>&1 || true
        api_get "$node" /v1/diagnostics > "$directory/diagnostics-${vm[node]}.json" 2>/dev/null || true
    done
    rel --output json status > "$directory/status.json" 2>&1 || true
    rel --output json wtf > "$directory/wtf.json" 2>&1 || true
    cp "$evidence/state.json" "$directory/state.json" 2>/dev/null || true
    printf 'slot %s\nwindow %s\ndown %s %s %s\n' "${current_slot:-}" "${window_label:-}" "${down[1]}" "${down[2]}" "${down[3]}" > "$directory/context.txt"
    event --phase failure --target "$check_name" --verdict fail --detail "$detail"
    say "FAILURE $failure_count: $check_name: $detail"
}

# A cluster that can't be set up can't be soaked: bundle the evidence and stop.
setup_fail() {
    record_failure setup "$1" "${last_snapshot:-}"
    fail "$1"
}

# Collect and evaluate one snapshot; failures get a bundle. Returns 0 when the
# cluster looks settled, 1 otherwise.
observe() {
    local kind=$1 directory result=0 output
    new_snapshot "$kind"
    directory=$snapshot_dir
    if [ "$kind" = light ]; then collect_light "$directory"; else collect "$kind" "$directory"; fi
    output=$(check evaluate "$evidence" "$directory") || result=$?
    [ -z "$output" ] || printf '%s\n' "$output" > "$directory/findings.txt"
    if [ "$result" -eq 1 ]; then
        record_failure "$kind-check" "$(grep '^fail' <<<"$output" | head -n 5 | tr '\n' ';')" "$directory"
    fi
    last_snapshot=$directory
    [ "$result" -eq 0 ]
}

# --- slots --------------------------------------------------------------------

window_label=
next_light=0
next_heavy=$(( $(date +%s) + heavy_every ))
next_rotation=$(( $(date +%s) + rotation ))
open_window() { window_label=$1; check window "$evidence" open "$1" --down "${2:-0}"; }
close_window() { check window "$evidence" "${1:-close}"; window_label=; }

# Wait for two consecutive clean settle checks after a fault.
settle() {
    local label=$1 deadline=$2 started streak=0 elapsed
    started=$(date +%s)
    while :; do
        elapsed=$(( $(date +%s) - started ))
        if [ "$elapsed" -ge "$deadline" ]; then
            close_window close
            event --phase "$label" --verdict fail --settle-seconds "$elapsed" --detail "not settled within $deadline s"
            record_failure settle "$label did not settle within $deadline s" "${last_snapshot:-}"
            return 1
        fi
        # Heavy checks and rotations keep their cadence through long settles.
        local kind=settle
        if [ "$(date +%s)" -ge "$next_heavy" ]; then
            kind=heavy
            next_heavy=$(( $(date +%s) + heavy_every ))
        fi
        if observe "$kind"; then streak=$(( streak + 1 )); else streak=0; fi
        if [ "$(date +%s)" -ge "$next_rotation" ]; then
            run_slot rotate_ingress
            next_rotation=$(( $(date +%s) + rotation ))
        fi
        if [ "$streak" -ge 2 ]; then
            close_window settled
            event --phase "$label" --verdict settled --settle-seconds "$(( $(date +%s) - started ))"
            say "$label settled in $(( $(date +%s) - started )) s"
            return 0
        fi
        sleep 10
    done
}

random_between() { echo $(( $1 + RANDOM % ($2 - $1 + 1) )); }

current_leader() {
    rel --output json nodes > "$evidence/.nodes.json" 2>/dev/null || return 1
    node_index "$(check nodes-json "$evidence/.nodes.json" leader | head -n 1)"
}
random_follower() {
    local leader candidates=() i
    leader=$(current_leader) || leader=0
    for i in 1 2 3; do [ "$i" = "$leader" ] || [ "${down[i]}" = 1 ] || candidates+=("$i"); done
    echo "${candidates[RANDOM % ${#candidates[@]}]}"
}

slot_bun_kill() {
    local role=$1 node
    if [ "$role" = leader ]; then node=$(current_leader) || node=1; else node=$(random_follower); fi
    say "fault: SIGKILL bun on the $role ${vm[node]}"
    open_window "bun-kill-$role"
    avoid=$node
    kill_bun "$node"
    event --phase "fault:bun-kill-$role" --target "${vm[node]}" --command 'kill -9 MainPID'
    settle "fault:bun-kill-$role" 300 || true
    avoid=0
}

slot_deploy_kill() {
    local node offset
    generation=$(( generation + 1 ))
    meta generation "$generation"
    write_workloads
    node=$(random_between 1 3)
    offset=$(random_between 0 "$kill_offset")
    say "fault: rolling deploy (generation $generation), SIGKILL bun on ${vm[node]} after $offset s"
    open_window deploy-kill
    rel apply -f "$evidence/workloads.toml" > "$evidence/snapshots/apply-$generation.log" 2>&1 || true
    sleep "$offset"
    avoid=$node
    kill_bun "$node"
    event --phase fault:deploy-kill --target "${vm[node]}" --detail "generation $generation, kill after $offset s"
    settle fault:deploy-kill 300 || true
    avoid=0
}

power_offs=0
power_off() {
    local node=$1
    down[node]=1
    check power-cut "$evidence"
    "$limactl" stop --force "${vm[node]}" > "$evidence/snapshots/power-off-$node-$(date +%s).log" 2>&1 || true
}
power_on() {
    local node=$1 result=0
    "$relish" local start "$node" > "$evidence/snapshots/power-on-$node-$(date +%s).log" 2>&1 || result=$?
    down[node]=0
    # A node that missed rotations must serve the current pair.
    push_pair "$node" "$tls/current/cert.pem" "$tls/current/key.pem" || true
    check ingress-expect "$evidence" "${vm[node]}" "$(cat "$tls/current/serial")"
    return "$result"
}

slot_power_off() {
    local node hold
    power_offs=$(( power_offs + 1 ))
    if [ $(( power_offs % 4 )) -eq 0 ]; then node=1; else node=$(random_between 2 3); fi
    hold=$(random_between "$hold_min" "$hold_max")
    say "fault: power off ${vm[node]} for $hold s"
    open_window power-off 1
    power_off "$node"
    event --phase fault:power-off --target "${vm[node]}" --command 'limactl stop --force' --detail "held $hold s"
    wait_until $(( $(date +%s) + hold ))
    power_on "$node" || event --phase fault:power-off --target "${vm[node]}" --verdict fail --detail 'relish local start failed'
    settle fault:power-off 480 || true
}

# Every chaos kind against the rails: kill and pause meet the replica minimum,
# so they aim at the three-replica frontend, one replica at a time; delay,
# drop, partition and dns are network faults, which no replica rail guards;
# the scenario runs its own checks.
# Print one running instance of APP, or nothing unless at least two run.
one_of_several() {
    rel --output json status > "$evidence/.chaos-status.json" 2>/dev/null || return 0
    python3 - "$evidence/.chaos-status.json" "$1" <<'PY'
import json, sys
ids = sorted(i["id"] for i in json.load(open(sys.argv[1])) if i["app_name"] == sys.argv[2] and i["state"] == "running")
print(ids[0] if len(ids) >= 2 else "")
PY
}

chaos_index=0
slot_chaos() {
    local round
    for (( round = 0; round < chaos_per_slot; round++ )); do chaos_one; done
}

chaos_one() {
    local kinds=(kill delay partition scenario pause drop dns) kind instance result=0 output=$evidence/snapshots/chaos-$chaos_index.log
    kind=${kinds[chaos_index % ${#kinds[@]}]}
    chaos_index=$(( chaos_index + 1 ))
    say "fault: chaos $kind"
    open_window "chaos-$kind"
    case $kind in
        # One of three frontends: the replica-minimum rail refuses a kill that leaves none.
        kill) rel fault kill frontend --count 1 --acknowledge --reason v02 > "$output" 2>&1 || result=$? ;;
        delay) rel fault delay frontend 200ms --duration 60s --acknowledge --reason v02 > "$output" 2>&1 || result=$? ;;
        partition) rel fault partition soak-redis --from soak-redis-client --duration 60s --acknowledge --reason v02 > "$output" 2>&1 || result=$? ;;
        # One frontend instance: the replica-minimum rail counts an unscoped
        # pause as freezing every replica, and refuses it whatever the count.
        pause)
            instance=$(one_of_several frontend)
            if [ -n "$instance" ]; then
                rel fault pause frontend --instance "$instance" --duration 60s --acknowledge --reason v02 > "$output" 2>&1 || result=$?
            else
                printf 'fewer than two running frontend instances to pause one of\n' > "$output"; result=1
            fi ;;
        drop) rel fault drop frontend 10% --duration 60s --acknowledge --reason v02 > "$output" 2>&1 || result=$? ;;
        dns) rel fault dns soak-redis nxdomain --duration 60s --acknowledge --reason v02 > "$output" 2>&1 || result=$? ;;
        scenario)
            rel_budget=900 rel --output json test --chaos --yes --filter dead_worker_node_has_workloads_rescheduled \
                --timeout 600s > "$output" 2>&1 || result=$? ;;
    esac
    event --phase "fault:chaos-$kind" --exit "$result" --command "$kind" --verdict "$([ "$result" -eq 0 ] && echo ok || echo fail)"
    [ "$result" -eq 0 ] || record_failure "chaos-$kind" "$(tail -n 3 "$output" | tr '\n' ' ')"
    case $kind in kill|scenario) ;; *) wait_until $(( $(date +%s) + 65 )) ;; esac
    rel fault clear > /dev/null 2>&1 || true
    settle "fault:chaos-$kind" 300 || true
}

# Relish test against the cluster, as users run it.
catalogue_pulse() {
    local result=0 output
    output=$evidence/snapshots/pulse-$(date +%s).json
    say 'catalogue pulse'
    open_window pulse
    # Five groups, four at a time, 300 s each at most.
    rel_budget=1800 rel --output json test --profile full-runc --filter volumes,image-registry,workload-identity,ingress,deployments \
        --timeout 300s > "$output" 2>"$output.err" || result=$?
    event --phase pulse --exit "$result" --verdict "$([ "$result" -eq 0 ] && echo ok || echo fail)" --detail "${output##*/}"
    [ "$result" -eq 0 ] || record_failure pulse "relish test exited $result; see ${output##*/}"
    settle pulse:settle 300 || true
}

# An offline export from another process contends for the checkpoint lock;
# both "exported" and "busy" are fine, and the archive must stay searchable.
offline_export() {
    local node output result=0
    output=$evidence/snapshots/offline-export-$(date +%s).log
    node=$(random_between 2 3)
    [ "${down[node]}" = 0 ] || node=1
    gsh "$node" 'relish logs-export --source /var/lib/reliaburger/logs/parquet --dest /var/lib/reliaburger/soak-export/offline --node-id offline 2>&1; echo "export exit $?"
        relish logs-search /var/lib/reliaburger/soak-export/logs/'"${vm[node]}"' "SELECT count(*) AS n FROM logs" 2>&1; echo "search exit $?"' \
        > "$output" 2>&1 || result=$?
    if grep -q '^search exit 0' "$output" && { grep -q '^export exit 0' "$output" || grep -q 'busy' "$output"; }; then
        event --phase storage:offline-export --target "${vm[node]}" --verdict ok
    else
        event --phase storage:offline-export --target "${vm[node]}" --verdict fail
        record_failure offline-export "offline logs-export or logs-search failed on ${vm[node]}; see ${output##*/}"
    fi
}

special_graceful() {
    say 'special: graceful whole-cluster stop and start'
    open_window graceful-stop 3
    down=(0 1 1 1)
    "$relish" local stop > "$evidence/snapshots/graceful-stop.log" 2>&1 || true
    event --phase special:graceful-stop
    local result=0
    "$relish" local start > "$evidence/snapshots/graceful-start.log" 2>&1 || result=$?
    down=(0 0 0 0)
    [ "$result" -eq 0 ] || record_failure graceful-start "relish local start exited $result"
    settle special:graceful-stop 600 || true
}

special_quorum_loss() {
    say 'special: quorum loss (nodes 2 and 3 powered off)'
    open_window quorum-loss 2
    power_off 2
    power_off 3
    event --phase special:quorum-loss --detail "held $quorum_hold s"
    wait_until $(( $(date +%s) + quorum_hold ))
    power_on 2 || true
    power_on 3 || true
    local returned deadline
    returned=$(date +%s)
    deadline=$(( returned + 120 ))
    if wait_cluster 120; then
        event --phase special:quorum-back --verdict ok --duration $(( $(date +%s) - returned ))
    else
        record_failure quorum "no quorum within 120 s of the second node returning"
    fi
    settle special:quorum-loss 600 || true
}

special_all_off() {
    say 'special: every VM powered off at once'
    open_window all-off 3
    power_off 1
    power_off 2
    power_off 3
    event --phase special:all-off
    wait_until $(( $(date +%s) + hold_min ))
    local result=0
    "$relish" local start > "$evidence/snapshots/all-off-start.log" 2>&1 || result=$?
    down=(0 0 0 0)
    local node
    for node in 1 2 3; do
        push_pair "$node" "$tls/current/cert.pem" "$tls/current/key.pem" || true
    done
    [ "$result" -eq 0 ] || record_failure all-off-start "relish local start exited $result"
    settle special:all-off 600 || true
}

# v0.1.0 -> soak build -> v0.1.0, from node 1 against its own registry (the
# host forward can't reach the registry address nodes are told to use).
upgrade_walks=0
slot_upgrade() {
    upgrade_walks=$(( upgrade_walks + 1 ))
    local target name inject started
    name=${soak_bun##*/}
    target=${name#bun-v}
    inject=$(( upgrade_walks % 2 ))
    # Compressed runs see two walks at most, so both carry a fault: a leader
    # SIGKILL on the first and a follower power-off on the second.
    [ "$schedule" = full ] || inject=1
    say "upgrade walk $upgrade_walks to $target$([ "$inject" -eq 1 ] && echo ', with a fault mid-walk')"
    open_window upgrade
    # The countersigned envelope, under the name upgrade start looks for.
    with_timeout 300 "$limactl" copy "$soak_bun" "$soak_sig" "${vm[1]}:/var/tmp/" > /dev/null
    gsh 1 "install -m 600 /dev/null /root/.soak-token && cat > /root/.soak-token" < "$evidence/.token"
    started=$(date +%s)
    gsh 1 "RELIABURGER_TOKEN=\$(cat /root/.soak-token) relish --endpoint https://127.0.0.1:9117 --ca-cert /etc/reliaburger/identity/root-ca.crt upgrade start --binary /var/tmp/$name --registry ${address[1]}:5050 --allow-downgrade" \
        > "$evidence/snapshots/upgrade-$upgrade_walks-start.log" 2>&1 || record_failure upgrade "upgrade start failed; see upgrade-$upgrade_walks-start.log"
    if [ "$inject" -eq 1 ]; then
        sleep "$(random_between 10 60)"
        if [ $(( upgrade_walks % 4 )) -eq 1 ]; then kill_bun "$(current_leader || echo 1)"; else
            local node; node=$(random_between 2 3); power_off "$node"; sleep "$hold_min"; power_on "$node" || true
        fi
    fi
    # Every node reporting the target isn't the end of the run: the leader
    # marks it Completed on a later tick, and a rollback before that is
    # refused as "already in progress". A paused run needs no wait, since
    # rollback replaces it.
    if wait_versions "$target" 600 "$started" upgrade; then wait_upgrade_idle 120 || true; fi
    started=$(date +%s)
    gsh 1 "RELIABURGER_TOKEN=\$(cat /root/.soak-token) relish --endpoint https://127.0.0.1:9117 --ca-cert /etc/reliaburger/identity/root-ca.crt upgrade rollback v0.1.0" \
        > "$evidence/snapshots/upgrade-$upgrade_walks-rollback.log" 2>&1 || record_failure upgrade "upgrade rollback failed"
    wait_versions 0.1.0 600 "$started" rollback || true
    settle upgrade:settle 600 || true
    return 0
}

wait_upgrade_idle() {
    local budget=$1 started status
    started=$(date +%s)
    while :; do
        status=$(gsh 1 "RELIABURGER_TOKEN=\$(cat /root/.soak-token) relish --endpoint https://127.0.0.1:9117 --ca-cert /etc/reliaburger/identity/root-ca.crt upgrade status" 2>&1 || true)
        case $status in *[Nn]'o upgrade in progress'*) return 0 ;; esac
        if [ $(( $(date +%s) - started )) -ge "$budget" ]; then
            record_failure upgrade-complete "run still in progress $budget s after every node reported the target: $(printf '%s' "$status" | head -n 3 | tr '\n' ' ')"
            return 1
        fi
        sleep 5
    done
}

wait_versions() {
    local want=$1 budget=$2 started=$3 label=$4 node versions
    while :; do
        versions=
        for node in 1 2 3; do
            versions+="$(curl -fsS --max-time 5 --cacert "$ca" --connect-to "${vm[node]}:9117:127.0.0.1:$(( api_port + node - 1 ))" \
                "https://${vm[node]}:9117/v1/version" 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"].lstrip("v"))' 2>/dev/null || echo '?') "
        done
        if [ "$versions" = "$want $want $want " ]; then
            event --phase "upgrade:$label" --verdict ok --duration $(( $(date +%s) - started )) --detail "$versions"
            return 0
        fi
        if [ $(( $(date +%s) - started )) -ge "$budget" ]; then
            event --phase "upgrade:$label" --verdict fail --duration "$budget" --detail "$versions"
            record_failure "upgrade-$label" "versions after $budget s: $versions (wanted $want)"
            return 1
        fi
        sleep 10
    done
}

# Wait until T, running light, heavy and rotation work when due.
wait_until() {
    local until=$1 now
    while :; do
        now=$(date +%s)
        [ "$now" -lt "$until" ] || return 0
        if [ "$now" -ge "$next_rotation" ]; then
            run_slot rotate_ingress
            next_rotation=$(( $(date +%s) + rotation ))
        elif [ "$now" -ge "$next_heavy" ]; then
            next_heavy=$(( now + heavy_every ))
            observe heavy || true
        elif [ "$now" -ge "$next_light" ]; then
            next_light=$(( now + light_every ))
            observe light || true
        else
            sleep "$(( until - now < 5 ? until - now : 5 ))"
        fi
    done
}

# A slot that trips over a harness error must not end a day-long run: record
# it and carry on. (Inside `||`, bash also suspends `set -e` for the slot.)
run_slot() {
    local result=0
    "$@" || result=$?
    if [ "$result" -ne 0 ]; then
        event --phase harness-error --target "$1" --exit "$result" --detail "${current_slot:-}"
        say "harness error in $1 (exit $result); continuing"
        [ -z "$window_label" ] || close_window close
        avoid=0
    fi
    return 0
}

run_cycle() {
    local index=$1 start=$2
    say "cycle $index"
    current_slot="$index:follower-kill"; wait_until "$start"; run_slot slot_bun_kill follower
    current_slot="$index:leader-kill"; wait_until $(( start + slot )); run_slot slot_bun_kill leader
    if [ $(( index % 2 )) -eq 1 ] && [ -n "$soak_bun" ]; then
        current_slot="$index:upgrade"; wait_until $(( start + 2 * slot )); run_slot slot_upgrade
        # Compressed runs chaos after the walk too, so cycles 0-3 cover all seven kinds.
        if [ "$schedule" = compressed ]; then
            current_slot="$index:chaos"; wait_until $(( start + 4 * slot )); run_slot slot_chaos
        fi
    else
        [ $(( index % 2 )) -eq 0 ] || event --phase upgrade --verdict skipped --detail 'no --soak-bun'
        current_slot="$index:deploy-kill"; wait_until $(( start + 2 * slot )); run_slot slot_deploy_kill
        current_slot="$index:power-off"; wait_until $(( start + 3 * slot )); run_slot slot_power_off
        current_slot="$index:chaos"; wait_until $(( start + 4 * slot )); run_slot slot_chaos
    fi
    current_slot="$index:settle"; wait_until $(( start + 5 * slot ))
    observe heavy || true
    next_heavy=$(( $(date +%s) + heavy_every ))
    if [ $(( index % push_every )) -eq 0 ] && [ "${down[1]}" = 0 ]; then
        registry push --tag "c$index-$(date +%s)" > "$evidence/snapshots/registry-push-$index.json" 2>&1 || true
        event --phase storage:registry-push --detail "$(cat "$evidence/snapshots/registry-push-$index.json")"
    fi
    if [ $(( index % export_every )) -eq $(( export_every - 1 )) ]; then run_slot offline_export; fi
    if { [ "$pulse_every" -gt 0 ] && [ $(( index % pulse_every )) -eq 0 ]; } || { [ "$schedule" = compressed ] && [ "$index" -eq 0 ]; }; then
        run_slot catalogue_pulse
    fi
    run_slot special_for_cycle "$index"
    current_slot=
}

# Specials scale with the run: graceful stop/start at each half, quorum loss
# at 40% and 85% of the hours, every VM off at 70%. An 8 h final-tier run gets
# graceful at 4 and 8, quorum loss at 3 and 7, all off at 6.
# The compressed schedule runs one of each on cycles 1-3 instead.
# min_cycles makes sure every scheduled one gets its cycle.
special_for_cycle() {
    local index=$1 hour
    if [ "$schedule" = compressed ]; then
        case $index in
            1) special_graceful ;;
            2) special_quorum_loss ;;
            3) special_all_off ;;
        esac
        return 0
    fi
    hour=$(( index + 1 ))
    local hours=$(( duration / 3600 )) half
    half=$(( hours / 2 )); [ "$half" -ge 1 ] || half=1
    if [ $(( hour % half )) -eq 0 ]; then special_graceful; fi
    if [ "$hour" -eq $(( (hours * 40 + 50) / 100 )) ] || [ "$hour" -eq $(( (hours * 85 + 50) / 100 )) ]; then
        special_quorum_loss
    fi
    if [ "$hour" -eq $(( (hours * 70 + 50) / 100 )) ]; then special_all_off; fi
}

# --- teardown and record -----------------------------------------------------

teardown=skipped
teardown_cluster() {
    if [ "$bootstrapped" != true ] || [ -z "$home" ] || [ ! -x "${relish:-}" ]; then
        teardown="not bootstrapped by this run; left in place: ${home:-none}"
        return 0
    fi
    "$relish" local destroy --name laptop --yes >> "$evidence/teardown.log" 2>&1 \
        || { teardown="\`relish local destroy --yes\` failed; $home is left in place"; return 1; }
    "$relish" uninstall --yes >> "$evidence/teardown.log" 2>&1 \
        || { teardown="\`relish uninstall --yes\` failed; $home is left in place"; return 1; }
    rm -rf "$home"
    teardown='`relish local destroy --yes` and `relish uninstall --yes` succeeded'
}

finish() {
    local result=$?
    trap - EXIT
    [ -z "${background_pid:-}" ] || kill "$background_pid" 2>/dev/null || true
    [ -f "$evidence/metadata.json" ] || { printf 'sustained: stopped before the soak started\n' >&2; exit "$result"; }
    meta finished_at "json:$(date +%s)"
    if [ "$keep" = true ]; then
        teardown="kept: RELIABURGER_HOME=${home:-none}"
    elif ! teardown_cluster; then
        result=1
    fi
    meta teardown "$teardown"
    if [ "$(real_home_state)" != "$real_before" ]; then
        meta notes+ '**The real ~/.reliaburger or ~/.local/bin/relish changed during the run.**'
        meta result FAIL
        result=1
    fi
    if [ "$result" -ne 0 ] && [ -z "$(meta_get result)" ]; then
        meta notes+ "The harness stopped early (exit $result) during ${current_slot:-setup}"
        meta result FAIL
    fi
    local rendered=0
    check render "$evidence" "$record" || rendered=$?
    printf '\nrecord: %s\n' "$record"
    [ "$result" -ne 0 ] || result=$rendered
    exit "$result"
}

# --- main ------------------------------------------------------------------------

start_run() {
    meta tier "$tier"
    meta schedule "$schedule"
    meta evidence "\`$evidence\`"
    meta started_at "json:$(date +%s)"
    meta duration_target "json:$duration"
    meta ingress_rotation_secs "json:$rotation"
    [ -z "$base_url" ] || meta base_url "<$base_url>"
    meta candidate_digest "\`${qualified_digest:-not checked}\`"
    if [ -n "$soak_bun" ]; then
        meta soak_bun "\`${soak_bun##*/}\`, SHA-256 \`$(sha256 "$soak_bun")\`"
    else
        meta soak_bun 'not supplied: upgrade slots skipped'
    fi
    if [ "$(uname -s)" = Darwin ]; then
        meta host "macOS $(sw_vers -productVersion), $(sysctl -n machdep.cpu.brand_string), $(( $(sysctl -n hw.memsize) / 1073741824 )) GiB"
    else
        # shellcheck source=/dev/null
        meta host "$(. /etc/os-release && printf '%s' "$PRETTY_NAME"), $(uname -srm), $(awk -F': ' '/^model name/ {print $2; exit}' /proc/cpuinfo), $(nproc) CPUs, $(awk '/^MemTotal/ {print int($2 / 1048576); exit}' /proc/meminfo) GiB"
    fi
}

trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

if [ "$resume" = true ]; then
    home=$(meta_get home_path)
    [ "$(meta_get bootstrapped)" != true ] || bootstrapped=true
    discover
    generation=$(meta_get generation); generation=${generation:-0}
    first_cycle=$(meta_get cycles); first_cycle=${first_cycle:-0}
    started_at=$(meta_get started_at)
    last_event=$(tail -n 1 "$evidence/events.jsonl" | python3 -c 'import json,sys; print(json.load(sys.stdin)["ts"])')
    gap=$(( $(date +%s) - last_event ))
    event --phase pause --duration "$gap" --detail 'resumed after an interruption'
    # Carry on the chaos rotation and the walk count where the run left off.
    chaos_index=$(python3 -c 'import json,sys; print(sum(1 for e in map(json.loads, open(sys.argv[1])) if e.get("phase", "").startswith("fault:chaos-") and "exit" in e))' "$evidence/events.jsonl")
    upgrade_walks=$(python3 -c 'import json,sys; print(sum(1 for e in map(json.loads, open(sys.argv[1])) if e.get("phase") == "upgrade:upgrade"))' "$evidence/events.jsonl")
    ingress_rotations=$(find "$tls" -maxdepth 1 -name 'leaf-*' | wc -l | tr -d ' ')
    say "resuming at cycle $first_cycle after a $gap s gap"
    paused=$(python3 -c 'import json,sys; print(sum(e.get("duration", 0) for e in map(json.loads, open(sys.argv[1])) if e.get("phase") == "pause"))' "$evidence/events.jsonl")
    end=$(( started_at + duration + paused ))
    wait_cluster 600 || fail 'the cluster is not healthy; fix it before resuming'
    next_rotation=$(date +%s)
else
    start_run
    if [ -n "$home" ]; then
        home=$(cd "$home" && pwd)
        meta notes+ "Adopted the existing home $home instead of bootstrapping"
    else
        bootstrap
    fi
    meta home_path "$home"
    meta bootstrapped "$bootstrapped"
    meta home "\`$home\`"
    discover
    meta lima "$("$limactl" --version)"
    meta guest "$(gsh 1 '. /etc/os-release; printf "%s, kernel %s" "$PRETTY_NAME" "$(uname -r)"')"
    python3 - "$evidence/state.json" "${vm[1]}" "${vm[2]}" "${vm[3]}" <<'PY'
import json, sys
json.dump({"nodes": sys.argv[2:]}, open(sys.argv[1], "w"))
PY
    configure_nodes
    apply_workloads
    meta versions "$(for node in 1 2 3; do curl -fsS --max-time 5 --cacert "$ca" --connect-to "${vm[node]}:9117:127.0.0.1:$(( api_port + node - 1 ))" "https://${vm[node]}:9117/v1/version" 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["version"])' || echo '?'; done | sort | uniq -c | awk '{printf "%s%s x%s", sep, $2, $1; sep=", "}')"
    say 'waiting for the baseline to settle'
    open_window baseline
    settle baseline 600 || setup_fail 'the cluster did not settle for the baseline'
    check baseline "$evidence" "$last_snapshot"
    event --phase baseline --verdict ok --detail "${last_snapshot##*/}"
    registry push --tag baseline > "$evidence/snapshots/registry-push-baseline.json" 2>&1 || true
    # A full check of the healthy cluster, before the first fault.
    observe heavy || true
    first_cycle=0
    started_at=$(date +%s)
    meta started_at "json:$started_at"
    end=$(( started_at + duration ))
    next_rotation=$(( started_at + rotation ))
fi

next_light=$(date +%s)
next_heavy=$(( $(date +%s) + heavy_every ))
index=$first_cycle
while :; do
    cycle_start=$(( started_at + index * cycle ))
    # A cycle that starts finishes; none starts once the soak time is up,
    # unless the run hasn't yet reached min_cycles.
    if [ "$index" -ge "$min_cycles" ] && { [ "$cycle_start" -ge "$end" ] || [ "$(date +%s)" -ge "$end" ]; }; then break; fi
    [ "$cycle_start" -ge "$(date +%s)" ] || cycle_start=$(date +%s)
    run_cycle "$index" "$cycle_start"
    index=$(( index + 1 ))
    meta cycles "json:$index"
done
current_slot=final
wait_until "$end"
observe heavy || true
say "soak finished after $index cycles"
