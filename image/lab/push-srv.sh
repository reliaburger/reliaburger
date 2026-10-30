#!/bin/bash
# push-srv.sh <dir>: copy <dir>/{tftp,http} into the server's /srv, adding to
# what's there. stage-artefacts.sh builds such a directory for each staging.
set -euo pipefail; . "$(dirname "$0")/lab.env"
tree=${1:?usage: push-srv.sh <dir with tftp/ and/or http/>}
parts=(); for p in tftp http; do [ -d "$tree/$p" ] && parts+=("$p"); done
COPYFILE_DISABLE=1 tar --no-xattrs -C "$tree" -cf - "${parts[@]}" \
    | "$LAB/ssh.sh" 'sudo tar -C /srv -xf - && sudo chown -R root:root /srv && ls -lR /srv/tftp /srv/http'
