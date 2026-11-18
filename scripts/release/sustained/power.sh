# shellcheck shell=bash
# Power a quickstart node's VM off and on, and wait for a cluster to settle.
# Sourced by qualify-sustained.sh (the soak) and
# qualify-power-cut-endurance.sh, so both cut power the same way.
#
# The caller sets:
#   limactl, relish     the isolated home's binaries
#   vm[N]               node N's Lima instance name
#   down[N]             1 while node N is powered off (this file keeps it)
#   power_log_dir       where each limactl and `relish local start` log goes
# and defines with_timeout SECONDS COMMAND... (each harness has its own) and
# these hooks:
#   power_before_off N        just before node N's power is cut
#   power_after_on N          just after `relish local start N` returns
#   settle_probe              one settle check; status 0 when the cluster is clean
#   settle_between            after each check, whatever its result
#   settle_succeeded LABEL S  settled after S seconds
#   settle_timed_out LABEL DEADLINE S
#                             not settled within DEADLINE seconds
# settle_streak (default 2) clean checks in a row settle it; settle_interval
# (default 10) seconds pass between checks.

# Cut node N's power: `limactl stop --force` kills the VM process, with no
# shutdown in the guest.
power_off() {
    local node=$1
    power_before_off "$node"
    down[node]=1
    with_timeout 120 "$limactl" stop --force "${vm[node]}" > "$power_log_dir/power-off-$node-$(date +%s).log" 2>&1 || true
}

# Power node N back on and wait for its API (`relish local start N`). The
# endurance harness doesn't use this: it powers on with `limactl start` alone
# (power_back there), so a slow Bun is I6's to judge rather than a reason to
# cut again.
power_on() {
    local node=$1 result=0
    with_timeout 600 "$relish" local start "$node" > "$power_log_dir/power-on-$node-$(date +%s).log" 2>&1 \
        || result=$?
    down[node]=0
    power_after_on "$node"
    return "$result"
}

# Wait until settle_probe passes settle_streak times in a row, or DEADLINE
# seconds pass: settle LABEL DEADLINE.
settle() {
    local label=$1 deadline=$2 started streak=0 elapsed
    started=$(date +%s)
    while :; do
        elapsed=$(( $(date +%s) - started ))
        if [ "$elapsed" -ge "$deadline" ]; then
            settle_timed_out "$label" "$deadline" "$elapsed"
            return 1
        fi
        if settle_probe; then streak=$(( streak + 1 )); else streak=0; fi
        settle_between
        if [ "$streak" -ge "${settle_streak:-2}" ]; then
            settle_succeeded "$label" "$(( $(date +%s) - started ))"
            return 0
        fi
        sleep "${settle_interval:-10}"
    done
}
