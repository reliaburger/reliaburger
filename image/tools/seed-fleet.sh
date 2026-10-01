#!/bin/bash
# Seeds for a fleet of appliance nodes (preview; Phase 2's claim flow
# replaces it). Run it on the laptop you'll run relish from. It writes each
# node's seed as <dir>/stick/seeds/<mac>.seed: copy <dir>/stick/ onto a USB
# stick labelled RBSEED and each node picks its own seed on boot
# (reliaburger-seed.service).
#
#   seed-fleet.sh init <dir> --cluster NAME --operator IP [--network CIDR]
#                          [--ssh-key FILE] MAC@IP...
#       Creates the cluster (relish init, keys and CA stay in <dir>) and node
#       1's seed. List node 1 first. Each node's firewall lets only these
#       addresses reach the cluster ports before they've joined, unless
#       --network names the LAN that nodes added later will join from
#       (192.168.1.0/24, say); it needs a bun newer than 0.1.1. IP is each
#       node's reserved address; --operator is this laptop's address, the
#       one relish connects from.
#       --ssh-key puts a public key in every seed (spike only: root SSH, for
#       staging OS updates by hand until bun does it).
#   seed-fleet.sh join <dir>
#       Once node 1 is up: enrols every other node (a join token each, then
#       relish join) and writes their seeds. To add nodes later, append
#       "N MAC IP" lines to <dir>/fleet and run join again.
#   . <dir>/env.sh
#       Points relish at the cluster (endpoint, CA, admin token).
#
# Needs relish (or RELISH=/path), python3, openssl and cargo (the seed-admin
# helper, until relish can mint the first admin token itself).
set -euo pipefail

tools=$(cd "$(dirname "$0")" && pwd)
relish=${RELISH:-relish}

usage() {
    sed -n '2,/^set -euo pipefail/p' "$0" | sed -n 's/^# \{0,1\}//p' >&2
    exit 2
}

helpers() {
    # A debug build: it compiles the whole reliaburger crate, and release
    # takes several times longer for two small tools.
    cargo build -q --manifest-path "$tools/seed-admin/Cargo.toml" --bins
    echo "$tools/seed-admin/target/debug"
}

# check <node.toml>: load it the way bun does, where cargo is around (join
# can run on any machine that reaches node 1, with or without it).
check() {
    if command -v cargo >/dev/null; then
        "$(helpers)/check-config" "$1" >/dev/null
    fi
}

# pack <node dir> <mac>: the node's seed, named after its MAC.
pack() {
    local src=$1 mac=$2 out
    [ -f "$dir/authorized_keys" ] && cp "$dir/authorized_keys" "$src/authorized_keys"
    out="$dir/stick/seeds/$(echo "$mac" | tr 'A-F:' 'a-f-').seed"
    mkdir -p "$dir/stick/seeds"
    (umask 077; COPYFILE_DISABLE=1 tar --format ustar -czf "$out" -C "$src" .)
    # The unpacked copy holds the node's key; only the seed should remain.
    rm -r "$src"
    echo "  $out"
}

cmd=${1:-}; dir=${2:-}
[ -n "$cmd" ] && [ -n "$dir" ] || usage
shift 2

case $cmd in
init)
    cluster= operator= ssh_key= network=
    nodes=()
    while [ $# -gt 0 ]; do
        case $1 in
            --cluster) cluster=$2; shift 2 ;;
            --operator) operator=$2; shift 2 ;;
            --network) network=$2; shift 2 ;;
            --ssh-key) ssh_key=$2; shift 2 ;;
            *@*) nodes+=("$1"); shift ;;
            *) usage ;;
        esac
    done
    [ -n "$cluster" ] && [ -n "$operator" ] && [ ${#nodes[@]} -ge 1 ] || usage
    [ ! -e "$dir/fleet" ] || { echo "$dir already holds a fleet" >&2; exit 1; }
    mkdir -p "$dir"
    chmod 0700 "$dir"
    dir=$(cd "$dir" && pwd)
    addresses=$(for node in "${nodes[@]}"; do echo "${node#*@}"; done | paste -sd, -)
    i=0
    for node in "${nodes[@]}"; do
        i=$((i + 1))
        echo "$i ${node%@*} ${node#*@}"
    done > "$dir/fleet"
    echo "$cluster $operator $addresses $network" > "$dir/cluster"
    if [ -n "$ssh_key" ]; then
        cp "$ssh_key" "$dir/authorized_keys"
    fi

    "$relish" init --cluster-name "$cluster" --node-id node-01 "$dir/init" >/dev/null
    h=$(helpers)
    node=$(mktemp -d "$dir/.node-01.XXXXXX")
    cp -R "$dir/init/identity" "$node/identity"
    cp "$dir/init/$cluster-master.key" "$node/master.key"
    cp "$dir/init/$cluster-security-bootstrap.json" "$node/security-bootstrap.json"
    (umask 077; "$h/seed-admin" "$node/security-bootstrap.json" > "$dir/admin.token")
    "$tools/node-toml.py" --cluster "$cluster" --addresses "$addresses" --index 1 \
        --operator "$operator" ${network:+--network "$network"} \
        ${OPERATOR_KEY:+--operator-key "$OPERATOR_KEY"} > "$node/node.toml"
    check "$node/node.toml"
    first=$(head -n 1 "$dir/fleet")
    echo "seed for node-01 ($(echo "$first" | cut -d' ' -f2)):"
    pack "$node" "$(echo "$first" | cut -d' ' -f2)"
    cat > "$dir/env.sh" <<ENV
export RELIABURGER_ENDPOINT=https://$(echo "$first" | cut -d' ' -f3):9117
export RELIABURGER_CA_CERT=$dir/init/identity/root-ca.crt
export RELIABURGER_TOKEN=\$(cat $dir/admin.token)
ENV
    fingerprint=$(openssl x509 -in "$dir/init/identity/root-ca.crt" -outform DER | openssl dgst -sha256 -r | cut -d' ' -f1)
    echo "sha256:$fingerprint" > "$dir/ca-fingerprint"
    echo "root CA sha256:$fingerprint; back up $dir/init (master key and sealed CA key)"
    ;;
join)
    [ -f "$dir/fleet" ] || { echo "$dir holds no fleet; run init first" >&2; exit 1; }
    dir=$(cd "$dir" && pwd)
    # Without --network (and in fleets made before it) nodes admit only the
    # addresses listed at init.
    read -r cluster operator addresses network < "$dir/cluster"
    addresses=$(cut -d' ' -f3 "$dir/fleet" | paste -sd, -)
    # shellcheck disable=SC1091
    . "$dir/env.sh"
    first_ip=$(head -n 1 "$dir/fleet" | cut -d' ' -f3)
    tail -n +2 "$dir/fleet" | while read -r i mac ip; do
        id=$(printf node-%02d "$i")
        if [ -f "$dir/stick/seeds/$(echo "$mac" | tr 'A-F:' 'a-f-').seed" ]; then
            echo "$id: already seeded"
            continue
        fi
        node=$(mktemp -d "$dir/.$id.XXXXXX")
        (umask 077; "$relish" join-token create --node-id "$id" --ttl 15m > "$node/join-token")
        "$relish" join "https://$first_ip:9117" --node-id "$id" --token-file "$node/join-token" \
            --ca-fingerprint "$(cat "$dir/ca-fingerprint")" --identity-dir "$node/identity" >/dev/null
        rm -f "$node/join-token"
        cp "$dir/init/$cluster-master.key" "$node/master.key"
        "$tools/node-toml.py" --cluster "$cluster" --addresses "$addresses" --index "$i" \
            --operator "$operator" ${network:+--network "$network"} \
            ${OPERATOR_KEY:+--operator-key "$OPERATOR_KEY"} > "$node/node.toml"
        check "$node/node.toml"
        echo "seed for $id ($mac):"
        pack "$node" "$mac"
    done
    ;;
*)
    usage
    ;;
esac
