#!/bin/bash
# mkseed.sh: build the server's NoCloud seed (vfat, label CIDATA) with mtools.
# It fills seed/user-data.in with the lab's SSH key and the conf/ files, so
# the key never lands in the repo and conf/ is the one copy of each config.
set -euo pipefail; . "$(dirname "$0")/lab.env"
key=$(cat "$WORK/l2lab_key.pub")
awk -v key="$key" -v lab="$LAB" '
    /@FILE:[^@]*@/ {
        match($0, /@FILE:[^@]*@/); path = substr($0, RSTART + 6, RLENGTH - 7)
        while ((getline line < (lab "/" path)) > 0) print "      " line
        close(lab "/" path); next
    }
    { gsub(/@SSH_KEY@/, key); print }
' "$LAB/seed/user-data.in" > "$WORK/user-data"
rm -f "$WORK/seed.img"
dd if=/dev/zero of="$WORK/seed.img" bs=1m count=2 2>/dev/null
mformat -i "$WORK/seed.img" -v CIDATA ::
mcopy -i "$WORK/seed.img" "$WORK/user-data" "$LAB/seed/meta-data" "$LAB/seed/network-config" ::
mdir -i "$WORK/seed.img" ::
