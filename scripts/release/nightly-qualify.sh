#!/usr/bin/env bash
# Nightly qualification of a merge train's tip (#1363): the power-cut and
# reboot runs, the V02 and jobs fast soaks and, for the full gate, the 8-hour
# job final, one after another on the quiet host, against the commit at the
# tip of the train when the night starts. Writes a dated record directory
# (nightly.json and a README naming the commit) under --records.
#
# A nightly runs only when a release is close: the evening before a planned
# freeze, arm it for that one night (#1721). It builds and stages the tip at
# the cut-off, runs the tiers overnight and disarms itself, whatever the
# result.
#
# Usage:
#   scripts/release/nightly-qualify.sh --once --train release-0-3-2 [--at 18:00] [options]
#   scripts/release/nightly-qualify.sh --disarm
#   scripts/release/nightly-qualify.sh --train release-0-3-2 [--dispatch] [options]
#
#   --once                  arm the nightly for one night: a launchd job that
#                           fires at --at (default 18:00) today, or on --date
#                           YYYY-MM-DD, runs the night with --dispatch, and
#                           removes itself. --dry-run prints the job instead
#   --disarm                remove every armed night that hasn't started
#   --dispatch              start now: build the tip with build.yml and stage
#                           it with stage.yml (channel nightly) first, then
#                           run the tiers on that staged build. If stage.yml
#                           fails, the night stages it itself with
#                           candidate.py and this host's gh login
#   --train NAME            the merge train whose tip to qualify
#   --previous TAG          the previous release (default: the nearest v* tag
#                           behind the tip); the gate is release_tier.py's
#                           verdict on it and the tip's Cargo.toml version
#   --gate full             run the full gate when the rule says patch. The
#                           patch gate is refused for a release that needs
#                           the full one (#1428)
#   --base-url URL          a staged build of the tip, for the soaks, instead
#                           of --dispatch: the nightly-v... pre-release
#                           stage.yml publishes for a train's nightly build.
#                           Without one only the power-cut runs happen, and
#                           the night isn't green. It must be signed with its
#                           channel's key: a nightly with the nightly key in
#                           src/upgrade/keys.rs
#   --qualified-digest SHA  its candidate.json SHA-256
#   --soak-bun PATH         the signed soak bun for the upgrade walks (the job
#                           final needs it); for a nightly build, built with
#                           the nightly-signing feature and signed with the
#                           nightly key
#   --iterations N          power-cut and reboot runs per lane (default 10)
#   --records DIR           where the nightly records go
#                           (default ~/reliaburger-nightly)
#   --remote NAME           the remote holding the train (default origin)
#   --plan, --dry-run       resolve the tip, print the tiers and exit
#
# Every night first asks power_cut/quiet.py whether a release gate or another
# long run holds the host, and doesn't start if one does; while its tiers run
# it holds the quiet lock, so the power-cut endurance run pauses. At code
# freeze, `scripts/release/nightly.py freeze-check --commit SHA` works out the
# same gate and says whether the latest green nightly ran the candidate's
# code.
set -euo pipefail

if [ "${1:-}" = -h ] || [ "${1:-}" = --help ]; then
    sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
    exit 0
fi

here=$(dirname "$0")
case "${1:-}" in
    --once) shift; exec python3 "$here/nightly.py" arm "$@" ;;
    --disarm) shift; exec python3 "$here/nightly.py" disarm "$@" ;;
esac

# A night of soaks must not sleep on idle.
if [ "$(uname -s)" = Darwin ] && [ -z "${RELIABURGER_NIGHTLY_CAFFEINATED:-}" ] && command -v caffeinate >/dev/null; then
    RELIABURGER_NIGHTLY_CAFFEINATED=1 exec caffeinate -dimsu "$0" "$@"
fi

exec python3 "$here/nightly.py" run "$@"
