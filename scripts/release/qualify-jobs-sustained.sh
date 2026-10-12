#!/usr/bin/env bash
# Job fault qualification (#686, the 0.2.0 exit gate decided as D18-C): the
# V02 sustained soak on its three-node cluster, with the job campaign at the
# #668 throughput target and a node-level fault (inter-node delay, a full disk,
# slow fsync) added to every cycle beside the soak's worker and leader kills,
# power-offs, quorum loss and upgrade walks. The record fails unless every job
# mode overlaps every fault kind, counts nothing twice and loses nothing, and
# the accepted rate averages at least --target-rate. The September plan's M8
# measurements are written to the record and to <record>.jobs.json; they are
# recorded, never gated.
#
# Usage:
#   scripts/release/qualify-jobs-sustained.sh --tier fast|final [--target-rate R]
#       [qualify-sustained.sh options: --base-url, --qualified-digest, --record,
#        --soak-bun, --evidence, --resume, --keep, --plan]
#
#   --tier fast    compressed schedule, 90 minutes: every fault kind in the
#                  first cycles, for the fix-and-rerun loop and CI hosts
#   --tier final   full schedule, 12 hours: the run #668 needs on the final
#                  candidate (after #678, #679 and #680). 12 rather than 24
#                  hours is the maintainer's decision for 0.2.0: a reasonable
#                  approximation of a day
#   --target-rate  accepted successes a second (default 1390: 1,158, which is
#                  100 million a day, plus 20% headroom)
#   --plan         print the fault schedule and exit
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
fail() { printf 'qualify-jobs-sustained: %s\n' "$*" >&2; exit 1; }

tier=
passthrough=()
while [ "$#" -gt 0 ]; do
    case $1 in
        --tier) tier=${2:-}; shift 2 ;;
        --job-load|--node-faults|--duration|--schedule)
            fail "$1 is fixed by the job qualification tier" ;;
        -h|--help) usage; exit 0 ;;
        *) passthrough+=("$1"); shift ;;
    esac
done

case $tier in
    fast) tier_options=(--tier fast) ;;
    final) tier_options=(--tier final --duration 12h) ;;
    *) usage >&2; fail '--tier must be fast or final' ;;
esac

exec "$(dirname "$0")/qualify-sustained.sh" "${tier_options[@]}" --job-load target --node-faults \
    ${passthrough[@]+"${passthrough[@]}"}
