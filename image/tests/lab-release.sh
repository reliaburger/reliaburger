#!/bin/bash
# Lay out a CI lab build's next version the way GitHub lays out a release,
# so any web server can play GitHub for `relish os upgrade`:
#
#   lab-release.sh <next dir> <next version> <tree>
#
#   <tree>/releases/download/os-channel/os-channel.json{,.sig}
#   <tree>/releases/download/os-<next version>-x86_64/reliaburger-os_<v>.efi,
#       .usr.*.raw.zst, .usr-verity.*.raw.zst, .SHA256SUMS{,.sig}
#   <tree>/lab-signing-key.pub.pem
#
# <next dir> is the second build's output, with its SHA256SUMS signed and
# os-channel.json written (os_release.py lab-channel) by the run's throwaway
# key, whose public half is spike-signing-key.pub.pem. appliance.yml uploads
# <tree> as appliance-x86_64-next, and image/tests/os-update.sh serves it.
# Files are hard-linked where they can be, else copied.
set -euo pipefail
usage="usage: lab-release.sh <next dir> <next version> <tree>"
next_dir=$(cd "${1:?$usage}" && pwd)
next=${2:?$usage}
tree=${3:?$usage}
download="$tree/releases/download"
release="$download/os-$next-x86_64"
mkdir -p "$release" "$download/os-channel"

place() {
    [ -f "$1" ] || { echo "lab-release.sh: $1 is missing" >&2; exit 1; }
    ln -f "$1" "$2" 2>/dev/null || cp -f "$1" "$2"
}
shopt -s nullglob
images=("$next_dir"/reliaburger-os_"$next".usr*.raw.zst)
[ "${#images[@]}" -eq 2 ] || {
    echo "lab-release.sh: expected the /usr image and its verity for $next in $next_dir, found ${#images[@]}" >&2
    exit 1
}
for f in "$next_dir"/reliaburger-os_"$next".{efi,SHA256SUMS,SHA256SUMS.sig} "${images[@]}"; do
    place "$f" "$release/"
done
place "$next_dir/os-channel.json" "$download/os-channel/"
place "$next_dir/os-channel.json.sig" "$download/os-channel/"
place "$next_dir/spike-signing-key.pub.pem" "$tree/lab-signing-key.pub.pem"
find "$tree" -type f | sort
