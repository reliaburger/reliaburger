# shellcheck shell=bash
# The Wyse exit test's limit on a broken OS update's fallback (S5), sourced
# by os-update.sh: back on the old version within 10 minutes of the reboot
# into the broken one.
FALLBACK_LIMIT_S=600

# fallback_within_limit <seconds>: fail, saying why, if the fallback took
# longer than the exit test allows. A slow fallback fails CI rather than
# warning, so the hardware run is never the first to find it.
fallback_within_limit() {
    local seconds=$1
    if [ "$seconds" -gt "$FALLBACK_LIMIT_S" ]; then
        echo "::error::the fallback took ${seconds} s, more than the 10 minutes the Wyse exit test allows"
        return 1
    fi
}
