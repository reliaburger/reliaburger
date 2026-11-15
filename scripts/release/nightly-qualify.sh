#!/usr/bin/env bash
# Nightly qualification of a merge train's tip (#1363): the power-cut and
# reboot runs, the V02 and jobs fast soaks and, for the full gate, the 8-hour
# job final, one after another on the quiet host, against the commit at the
# tip of the train when the night starts. Writes a dated record directory
# (nightly.json and a README naming the commit) under --records.
#
# Usage:
#   scripts/release/nightly-qualify.sh --train release-0-2-4 [options]
#
#   --train NAME            the merge train whose tip to qualify
#   --previous TAG          the previous release (default: the nearest v* tag
#                           behind the tip); the gate is release_tier.py's
#                           verdict on it and the tip's Cargo.toml version
#   --gate full             run the full gate when the rule says patch. The
#                           patch gate is refused for a release that needs
#                           the full one (#1428)
#   --base-url URL          a staged build of the tip, for the soaks: the
#                           nightly-v... pre-release stage.yml publishes for a
#                           train's nightly build. Without one only the
#                           power-cut runs happen, and the night isn't green.
#                           It must be signed with its channel's key: a
#                           nightly with the nightly key in src/upgrade/keys.rs
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
# At code freeze, `scripts/release/nightly.py freeze-check --commit SHA`
# works out the same gate and says whether the latest green nightly ran the candidate's
# code. scripts/release/nightly-qualify.plist.example runs this from launchd.
set -euo pipefail

if [ "${1:-}" = -h ] || [ "${1:-}" = --help ]; then
    sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'
    exit 0
fi

# A night of soaks must not sleep on idle.
if [ "$(uname -s)" = Darwin ] && [ -z "${RELIABURGER_NIGHTLY_CAFFEINATED:-}" ] && command -v caffeinate >/dev/null; then
    RELIABURGER_NIGHTLY_CAFFEINATED=1 exec caffeinate -dimsu "$0" "$@"
fi

exec python3 "$(dirname "$0")/nightly.py" run "$@"
