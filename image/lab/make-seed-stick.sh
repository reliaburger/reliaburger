#!/bin/bash
# make-seed-stick.sh <image> <mac>=<seed.tgz>...: a FAT image labelled RBSEED
# holding seeds/<mac>.seed for each machine, the layout reliaburger-seed
# reads from a real USB stick. rbnode.sh and wyse.sh plug it in with
# STICK=<image>. On a real stick, copy the same seeds/ folder onto a FAT
# filesystem labelled RBSEED instead.
set -euo pipefail
out=${1:?usage: make-seed-stick.sh <image> <mac>=<seed.tgz>...}; shift
[ $# -ge 1 ] || { echo "name at least one <mac>=<seed.tgz>" >&2; exit 2; }
qemu-img create -q -f raw "$out" 64M
mformat -i "$out" -v RBSEED -F ::
mmd -i "$out" ::/seeds
for pair in "$@"; do
    mac=${pair%%=*} seed=${pair#*=}
    mcopy -i "$out" "$seed" "::/seeds/$(echo "$mac" | tr 'A-F:' 'a-f-').seed"
done
mdir -i "$out" ::/seeds
