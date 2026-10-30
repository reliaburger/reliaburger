#!/bin/bash
# tunnel.sh <node-ip> [local-port] [remote-port]: forward 127.0.0.1:<local-port>
# on the Mac to <node-ip>:<remote-port> (bun's API, 9117, by default) through
# the server, in the background. The lab runs relish on the server instead
# (its certificate checks and operator_cidrs expect the node's own address),
# so this is for poking at a node from the Mac.
. "$(dirname "$0")/lab.env"
ip=${1:?usage: tunnel.sh <node-ip> [local-port] [remote-port]}; lp=${2:-19117}; rp=${3:-9117}
exec ssh -F "$WORK/ssh_config" -f -N -o ExitOnForwardFailure=yes -L "127.0.0.1:$lp:$ip:$rp" l2lab-server
