#!/bin/bash
# fetch-ovmf.sh: copy Ubuntu's x86-64 OVMF (code and vars) from the server
# into work/, for wyse.sh. Homebrew's QEMU ships no x86-64 vars template.
set -euo pipefail; . "$(dirname "$0")/lab.env"
"$LAB/ssh.sh" 'dpkg -s ovmf >/dev/null 2>&1 || sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ovmf >/dev/null'
for f in OVMF_CODE_4M.fd OVMF_VARS_4M.fd; do
    "$LAB/ssh.sh" "cat /usr/share/OVMF/$f" > "$WORK/$f"
done
ls -l "$WORK"/OVMF_*.fd
