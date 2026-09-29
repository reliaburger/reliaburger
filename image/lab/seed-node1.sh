#!/bin/bash
# seed-node1.sh [count]: node 1's seed (work/cluster/seeds/node-01.tgz), from
# `relish init` output in work/cluster/init, with a hashed admin token added
# to the security bootstrap, as quickstart does. The plaintext token lands in
# work/cluster/admin.token (0600) and, with the root CA and an env.sh for
# relish, on the lab server.
set -euo pipefail; . "$(dirname "$0")/lab.env"
count=${1:-5}
c="$WORK/cluster"
[ -d "$c/init/identity" ] || { echo "run: work/relish init --cluster-name lab --node-id node-01 $c/init" >&2; exit 2; }
cargo build -q --manifest-path "$LAB/../tools/seed-admin/Cargo.toml" --bins
helpers="$LAB/../tools/seed-admin/target/debug"
mkdir -p "$c/seeds"
d=$(mktemp -d "$c/seeds/node-01.XXXXXX")
cp -R "$c/init/identity" "$d/identity"
cp "$c/init/lab-master.key" "$d/master.key"
cp "$c/init/lab-security-bootstrap.json" "$d/security-bootstrap.json"
(umask 077; "$helpers/seed-admin" "$d/security-bootstrap.json" > "$c/admin.token")
"$LAB/node-toml.py" 1 "$count" > "$d/node.toml"
"$helpers/check-config" "$d/node.toml"
COPYFILE_DISABLE=1 tar --format ustar -czf "$c/seeds/node-01.tgz" -C "$d" .
"$LAB/ssh.sh" 'mkdir -p ~/lab; umask 077; cat > ~/lab/admin.token' < "$c/admin.token"
"$LAB/ssh.sh" 'cat > ~/lab/root-ca.crt' < "$c/init/identity/root-ca.crt"
"$LAB/ssh.sh" 'cat > ~/lab/env.sh' <<'ENV'
export RELIABURGER_ENDPOINT=https://192.168.105.101:9117 RELIABURGER_CA_CERT=$HOME/lab/root-ca.crt
export RELIABURGER_TOKEN=$(cat ~/lab/admin.token)
ENV
echo "work/cluster/seeds/node-01.tgz: $(wc -c < "$c/seeds/node-01.tgz" | tr -d ' ') bytes"
