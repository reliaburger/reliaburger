#!/bin/sh
# Runs on the lab server (l2lab-net.service). The netboot server gets its own
# network namespace "nb" with a veth into br0 at 192.168.105.2, so it's a
# separate host on the wire from the router at .1 and the two dnsmasq
# instances don't fight over UDP 67. The router NATs the lab out of "wan".
set -eu
sysctl -qw net.ipv4.ip_forward=1
ip netns add nb 2>/dev/null || true
if ! ip link show veth-nb >/dev/null 2>&1; then
  ip link add veth-nb type veth peer name eth0 netns nb
fi
ip link set veth-nb master br0 up
ip -n nb link set lo up
ip -n nb addr replace 192.168.105.2/24 dev eth0
ip -n nb link set eth0 up
ip -n nb route replace default via 192.168.105.1
nft -f - <<'NFT'
table ip l2lab
delete table ip l2lab
table ip l2lab {
  chain post { type nat hook postrouting priority srcnat; oifname "wan" ip saddr 192.168.105.0/24 masquerade; }
}
NFT
