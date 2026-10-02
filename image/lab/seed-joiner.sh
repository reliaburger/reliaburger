#!/bin/bash
# seed-joiner.sh <n> [count]: enrol node n with node 1 (which must be up) and
# pack its seed (work/cluster/seeds/node-0<n>.tgz). relish runs on the lab
# server, which is on the nodes' network and in their operator_cidrs: it
# mints a join token bound to node-0<n>, then `relish join` makes the node's
# key and CSR there and fetches the signed identity, pinned to the root CA
# fingerprint from `relish init`.
set -euo pipefail; . "$(dirname "$0")/lab.env"
n=${1:?usage: seed-joiner.sh <n> [count]}; count=${2:-5}; id=$(printf node-%02d "$n")
c="$WORK/cluster"
cargo build -q --manifest-path "$LAB/../tools/seed-admin/Cargo.toml" --bins
fp=$(openssl x509 -in "$c/init/identity/root-ca.crt" -outform DER | shasum -a 256 | cut -d' ' -f1)
"$LAB/ssh.sh" bash -s "$id" "sha256:$fp" <<'REMOTE'
set -euo pipefail
id=$1 fp=$2
. ~/lab/env.sh
token=$(relish join-token create --node-id "$id" --ttl 10m)
work=$(mktemp -d ~/lab/$id.XXXXXX)
(umask 077; echo "$token" > "$work/join")
relish join https://192.168.105.101:9117 --node-id "$id" --token-file "$work/join" \
    --ca-fingerprint "$fp" --identity-dir "$work/identity" >&2
rm -f "$work/join"
ln -sfn "$work" ~/lab/$id
REMOTE
d=$(mktemp -d "$c/seeds/$id.XXXXXX")
"$LAB/ssh.sh" "tar -C ~/lab/$id/ -czf - identity" | tar -xzf - -C "$d"
cp "$c/init/lab-master.key" "$d/master.key"
"$LAB/node-toml.py" "$n" "$count" > "$d/node.toml"
"$LAB/../tools/seed-admin/target/debug/check-config" "$d/node.toml"
COPYFILE_DISABLE=1 tar --format ustar -czf "$c/seeds/$id.tgz" -C "$d" .
echo "work/cluster/seeds/$id.tgz: $(wc -c < "$c/seeds/$id.tgz" | tr -d ' ') bytes; identity: $(ls "$d/identity" | tr '\n' ' ')"
