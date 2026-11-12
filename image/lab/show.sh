#!/bin/bash
# show.sh <log>: the Reliaburger lines of a serial log in work/ (for example
# rb1.serial.log, rb1.install.log, wyse1.serial.log), with ANSI codes stripped.
. "$(dirname "$0")/lab.env"
LC_ALL=C sed -e 's/\x1b\[[0-9;=?]*[a-zA-Z]//g' -e 's/\r//g' "$WORK/${1:?usage: show.sh <log>}" \
    | grep -a -E 'reliaburger|iPXE|Could not|Already started|FAILED' | uniq
