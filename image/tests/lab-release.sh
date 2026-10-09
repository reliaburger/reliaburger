#!/bin/bash
# Lay out a CI lab build's next version the way GitHub lays out a release,
# so any web server can play GitHub for `relish os upgrade`:
#
#   lab-release.sh <next dir> <next version> <tree> [<broken dir> <broken version>]
#
#   <tree>/releases/download/os-channel/os-channel.json{,.sig}
#   <tree>/releases/download/os-<next version>-x86_64/reliaburger-os_<v>.efi,
#       .usr.*.raw.zst, .usr-verity.*.raw.zst, .SHA256SUMS{,.sig}
#   <tree>/releases/download/os-<broken version>-x86_64/…, the same files,
#       when a broken version is given
#   <tree>/lab-signing-key.pub.pem
#
# <next dir> is the second build's output, with its SHA256SUMS signed and
# os-channel.json written (os_release.py lab-channel) by the run's throwaway
# key, whose public half is spike-signing-key.pub.pem. <broken dir> is the
# third build, whose bun never starts (image/tests/broken-bun), its
# SHA256SUMS signed by the same key. The channel doesn't name it, so only an
# update that names its version reaches it: the fallback test, or os-stage
# in the Wyse lab. appliance.yml uploads <tree> as appliance-x86_64-next,
# and image/tests/os-update.sh serves it.
# Files are hard-linked where they can be, else copied.
set -euo pipefail
usage="usage: lab-release.sh <next dir> <next version> <tree> [<broken dir> <broken version>]"
next_dir=$(cd "${1:?$usage}" && pwd)
next=${2:?$usage}
tree=${3:?$usage}
broken_dir=${4:-}
broken=''
if [ -n "$broken_dir" ]; then
    broken=${5:?$usage}
    broken_dir=$(cd "$broken_dir" && pwd)
fi
download="$tree/releases/download"
mkdir -p "$download/os-channel"

place() {
    [ -f "$1" ] || { echo "lab-release.sh: $1 is missing" >&2; exit 1; }
    ln -f "$1" "$2" 2>/dev/null || cp -f "$1" "$2"
}
shopt -s nullglob
# release <dir> <version>: what an update to <version> needs, as GitHub's
# os-<version>-x86_64 release.
release() {
    local dir=$1 version=$2 f
    local target="$download/os-$version-x86_64"
    local images=("$dir"/reliaburger-os_"$version".usr*.raw.zst)
    [ "${#images[@]}" -eq 2 ] || {
        echo "lab-release.sh: expected the /usr image and its verity for $version in $dir, found ${#images[@]}" >&2
        exit 1
    }
    mkdir -p "$target"
    for f in "$dir"/reliaburger-os_"$version".{efi,SHA256SUMS,SHA256SUMS.sig} "${images[@]}"; do
        place "$f" "$target/"
    done
}
release "$next_dir" "$next"
[ -z "$broken" ] || release "$broken_dir" "$broken"
place "$next_dir/os-channel.json" "$download/os-channel/"
place "$next_dir/os-channel.json.sig" "$download/os-channel/"
place "$next_dir/spike-signing-key.pub.pem" "$tree/lab-signing-key.pub.pem"
find "$tree" -type f | sort
