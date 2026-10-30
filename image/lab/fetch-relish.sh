#!/bin/bash
# fetch-relish.sh <release-tag>: fetch relish for the Mac and for the server
# from a release (use the tag the image's bun came from, which appliance.yml's
# step summary names), check them against the release's SHA256SUMS, and
# install the Linux one on the server. relish runs there: the server sits on
# the nodes' network and in their operator_cidrs.
set -euo pipefail; . "$(dirname "$0")/lab.env"
tag=${1:?usage: fetch-relish.sh <release-tag>}
bin="$WORK/bin/$tag"; mkdir -p "$bin"
gh release download "$tag" -R reliaburger/reliaburger --dir "$bin" --clobber \
    -p relish-macos-aarch64 -p relish-linux-aarch64 -p SHA256SUMS
(cd "$bin" && shasum -a 256 -c SHA256SUMS --ignore-missing)
chmod +x "$bin"/relish-*
xattr -d com.apple.quarantine "$bin/relish-macos-aarch64" 2>/dev/null || true
ln -sfn "$bin/relish-macos-aarch64" "$WORK/relish"
"$LAB/ssh.sh" 'sudo install -m 0755 /dev/stdin /usr/local/bin/relish && mkdir -p ~/lab && relish --version' \
    < "$bin/relish-linux-aarch64"
"$WORK/relish" --version
