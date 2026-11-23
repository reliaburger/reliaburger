#!/bin/bash
# make-seed-stick.sh <image> <stick dir>: a FAT image labelled RBSEED holding
# the seeds/ folder of <stick dir>, which `relish cluster create --bare-metal`
# and `relish image seed` write as <cluster dir>/stick. That's the layout bun
# reads from a real USB stick; rbnode.sh and wyse.sh plug the image in with
# STICK=<image>. On real machines you copy the same seeds/ folder onto a FAT
# stick labelled RBSEED. relish writes the seeds but no filesystem image, so
# QEMU still needs this.
set -euo pipefail
usage="usage: make-seed-stick.sh <image> <stick dir>"
out=${1:?$usage}
stick=${2:?$usage}
seeds=("$stick"/seeds/*.seed)
[ -f "${seeds[0]}" ] || { echo "$stick/seeds holds no seeds" >&2; exit 2; }
qemu-img create -q -f raw "$out" 64M
mformat -i "$out" -v RBSEED -F ::
mmd -i "$out" ::/seeds
mcopy -i "$out" "${seeds[@]}" ::/seeds/
mdir -i "$out" ::/seeds
