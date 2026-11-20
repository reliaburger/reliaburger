#!/bin/bash
# seed-lab.sh [count]: the lab cluster and a seed for each of its nodes, made
# by relish as on real machines. `relish cluster create --bare-metal` makes
# the cluster's PKI on the Mac and writes work/cluster/stick/seeds/<mac>.seed
# per node: node 1's carries the cluster's keys, the others a join token
# each, so they enrol with node 1 on their first boot. rbnode.sh passes a
# node its seed. relish runs on the lab server (192.168.105.1), the operator,
# so the admin token, the root CA and an env.sh go there.
# OPERATOR_KEY="ed25519:..." adds the key cluster bun upgrades need.
set -euo pipefail; . "$(dirname "$0")/lab.env"
count=${1:-5}
c="$WORK/cluster"
[ -x "$WORK/relish" ] || { echo "no relish yet; run fetch-relish.sh first" >&2; exit 2; }
[ ! -e "$c" ] || { echo "$c already exists; move it aside to make a new cluster" >&2; exit 2; }
# The router reserves .101 up for 52:54:00:00:01:01 up (conf/router.conf).
machines=()
for n in $(seq 1 "$count"); do
    machines+=("52:54:00:00:01:$(printf %02x "$n")@192.168.105.$((100 + n))")
done
extra=()
[ -n "${OPERATOR_KEY:-}" ] && extra+=(--external-signing-key "$OPERATOR_KEY")
# RELIABURGER_HOME keeps the lab's relish context out of the Mac's own.
# --yes skips the master-key backup prompt: a lab cluster is thrown away.
RELIABURGER_HOME="$WORK/relish-home" "$WORK/relish" cluster create --bare-metal "$c" \
    --name lab --operator 192.168.105.1 --network 192.168.105.0/24 --yes \
    ${extra[@]+"${extra[@]}"} "${machines[@]}"
"$LAB/ssh.sh" 'mkdir -p ~/lab; umask 077; cat > ~/lab/admin.token' < "$c/secrets/admin.token"
"$LAB/ssh.sh" 'cat > ~/lab/root-ca.crt' < "$c/root-ca.crt"
"$LAB/ssh.sh" 'cat > ~/lab/env.sh' <<'ENV'
export RELIABURGER_ENDPOINT=https://192.168.105.101:9117 RELIABURGER_CA_CERT=$HOME/lab/root-ca.crt
export RELIABURGER_TOKEN=$(cat ~/lab/admin.token)
ENV
