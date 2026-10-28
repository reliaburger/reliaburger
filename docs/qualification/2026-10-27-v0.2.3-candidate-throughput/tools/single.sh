#!/bin/bash
# Single-node sessions, each on a fresh boot of rb-bench-1030.
set -u
for s in "caps cand" "caps 022" "rounds cand"; do
  echo "$(date -u +%FT%TZ) reboot for $s"
  limactl stop rb-bench-1030 >/dev/null 2>&1; limactl start rb-bench-1030 >/dev/null 2>&1
  sleep 20
  echo "$(date -u +%FT%TZ) start $s"
  limactl shell rb-bench-1030 -- sudo /home/miko.guest/run023.sh $s > /dev/null 2>&1; rc=$?
  echo "$(date -u +%FT%TZ) end $s rc=$rc"
done
echo "$(date -u +%FT%TZ) ALL DONE"
