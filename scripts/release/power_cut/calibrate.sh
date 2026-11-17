#!/usr/bin/env bash
# What does each kind of power cut lose on this host? Step 0 of
# docs/plans/2026-11-18-plan-power-cut-endurance.md.
#
# Creates a disposable Lima VM from calibrate-vm.yaml (Ubuntu 26.04, 2 vCPU,
# 2 GiB), then N times: starts calibrate.py's writer in the guest (records
# written with fsync, with O_DIRECT and no flush, and buffered), cuts the
# power a random 3-12 seconds in, brings the VM back, scans what survived
# and appends one JSON line to EVIDENCE/cuts.jsonl. Ends with
# EVIDENCE/summary.json from `calibrate.py tally` and deletes the VM.
#
# Usage:
#   scripts/release/power_cut/calibrate.sh --kind KIND --cuts N --evidence DIR [--vm NAME] [--seed N] [--keep]
#   scripts/release/power_cut/calibrate.sh --logwrites --evidence DIR [--vm NAME] [--keep]
#
#   --kind stop     `limactl stop --force` (SIGKILL to the host agent and driver)
#   --kind kill     SIGKILL to the process holding the VM's disk image
#                   (Virtualization.framework's VM process)
#   --kind sysrq    `echo b > /proc/sysrq-trigger` in the guest: reboot with no
#                   sync; records whether VZ reboots the guest or stops the VM
#   --logwrites     instead of cuts, run logwrites-proof.sh in the guest: can
#                   the guest kernel record a dm-log-writes log, and does a
#                   replay to the last flush drop exactly the unflushed writes
#
# Never touches the development VM reliaburger-test, never adopts a VM it
# didn't create, and stops (keeping the evidence) when quiet.py says a
# release gate is using the host.
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 2; }
fail() { printf 'calibrate: %s\n' "$*" >&2; exit 1; }
say() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*"; }

kind=
cuts=
evidence=
vm=
seed=$RANDOM
keep=no
logwrites=no
while [[ $# -gt 0 ]]; do
    case $1 in
        --keep) keep=yes; shift; continue ;;
        --logwrites) logwrites=yes; shift; continue ;;
        --kind | --cuts | --evidence | --vm | --seed) [[ $# -ge 2 ]] || usage ;;
        *) usage ;;
    esac
    case $1 in
        --kind) kind=$2 ;;
        --cuts) cuts=$2 ;;
        --evidence) evidence=$2 ;;
        --vm) vm=$2 ;;
        --seed) seed=$2 ;;
    esac
    shift 2
done
[[ -n $evidence ]] || usage
if [[ $logwrites == no ]]; then
    case $kind in stop | kill | sysrq) ;; *) usage ;; esac
    [[ $cuts =~ ^[1-9][0-9]*$ ]] || usage
fi
vm=${vm:-pc-cal-${kind:-logwrites}}
[[ $vm != reliaburger-test ]] || fail 'the development VM reliaburger-test is never cut'
[[ $vm =~ ^pc-cal-[A-Za-z0-9-]+$ ]] || fail "VM names start with pc-cal- ($vm doesn't)"
[[ $seed =~ ^[0-9]+$ ]] || usage

here=$(cd "$(dirname "$0")" && pwd)
lima_home=${LIMA_HOME:-$HOME/.lima}
quiet() { python3 "$here/quiet.py" check; }
gsh() { limactl shell --workdir / "$vm" sudo "$@"; }
random_between() { python3 -c 'import random,sys; r=random.Random(int(sys.argv[1])); print(r.randint(int(sys.argv[2]), int(sys.argv[3])))' "$1" "$2" "$3"; }
now() { python3 -c 'import time; print(f"{time.time():.3f}")'; }
since() { python3 -c 'import sys,time; print(f"{time.time() - float(sys.argv[1]):.1f}")' "$1"; }
# Does the guest answer a shell? Bounded: ssh to a rebooting guest can hang.
answers() { perl -e 'alarm shift; exec @ARGV or die' 15 limactl shell --workdir / "$vm" true > /dev/null 2>&1; }
status() { limactl list --format '{{.Status}}' "$vm" 2> /dev/null || echo missing; }

reason=$(quiet) || fail "not starting: $reason"
[[ ! -e $evidence/cuts.jsonl ]] || fail "$evidence already has cuts; evidence is never overwritten"
mkdir -p "$evidence"
if limactl list --format '{{.Name}}' | grep -qx -- "$vm"; then
    fail "VM $vm already exists; delete it (limactl delete --force $vm) or name another with --vm"
fi

made_vm=no
cleanup() {
    [[ $made_vm == yes ]] || return 0
    if [[ $keep == yes ]]; then
        echo "kept VM $vm; delete it with: limactl delete --force $vm" >&2
    else
        limactl delete --force "$vm" > /dev/null 2>&1 || echo "could not delete VM $vm" >&2
    fi
}
trap cleanup EXIT

say "creating $vm"
made_vm=yes
limactl start --tty=false --name "$vm" "$here/calibrate-vm.yaml" > "$evidence/create.log" 2>&1 \
    || fail "creating $vm failed; see $evidence/create.log"
limactl copy "$here/calibrate.py" "$here/logwrites.py" "$here/logwrites-proof.sh" "$vm:/tmp/"
gsh bash -c 'install -d /var/lib/pcut && install -m 755 /tmp/calibrate.py /tmp/logwrites.py /tmp/logwrites-proof.sh /var/lib/pcut/ \
    && python3 /var/lib/pcut/calibrate.py prepare --dir /var/lib/pcut && sync'
{
    printf '{"vm": "%s", "kind": "%s", "seed": %s, "lima": "%s", "host": "%s %s", ' \
        "$vm" "${kind:-logwrites}" "$seed" "$(limactl --version | awk '{print $NF}')" "$(sw_vers -productName 2> /dev/null || uname -s)" "$(sw_vers -productVersion 2> /dev/null || uname -r)"
    printf '"kernel": "%s", "write_cache": "%s", "sysrq": "%s"}\n' \
        "$(gsh uname -r)" "$(gsh cat /sys/block/vda/queue/write_cache)" "$(gsh cat /proc/sys/kernel/sysrq)"
} > "$evidence/run.json"

if [[ $logwrites == yes ]]; then
    say 'proving the dm-log-writes replay in the guest'
    result=0
    gsh bash /var/lib/pcut/logwrites-proof.sh > "$evidence/logwrites.json" 2> "$evidence/logwrites.err" || result=$?
    if [[ $result -ne 0 ]] && grep -q '"module": "missing' "$evidence/logwrites.json"; then
        say 'dm-log-writes is not in the base modules; installing linux-modules-extra'
        gsh bash -c 'apt-get update -qq && apt-get install -y -qq "linux-modules-extra-$(uname -r)"' > "$evidence/modules-extra.log" 2>&1 || true
        result=0
        gsh bash /var/lib/pcut/logwrites-proof.sh > "$evidence/logwrites.json" 2> "$evidence/logwrites.err" || result=$?
    fi
    cat "$evidence/logwrites.json"
    exit "$result"
fi

# Bring the VM back after a cut, with Lima's one known VZ start retry
# ("storage device attachment is invalid" after a forced stop). Prints
# "how" (rebooted-in-place or started), the retries, and the seconds until
# a shell answered.
revive() {
    local cut_at=$1 how=started retries=0 waited=0 state
    # A guest reboot may leave the VM running; give VZ a moment to decide.
    while [[ $waited -lt 90 ]]; do
        state=$(status)
        [[ $state == Running ]] || break
        if answers; then
            how=rebooted-in-place
            break
        fi
        sleep 2
        waited=$(( waited + 2 ))
    done
    if [[ $how == started ]]; then
        [[ $(status) == Stopped ]] || limactl stop --force "$vm" > /dev/null 2>&1 || true
        if ! limactl start --tty=false "$vm" >> "$evidence/start.log" 2>&1; then
            retries=1
            limactl stop --force "$vm" > /dev/null 2>&1 || true
            limactl start --tty=false "$vm" >> "$evidence/start.log" 2>&1 || fail "$vm didn't start twice in a row; see $evidence/start.log"
        fi
    fi
    local deadline=$(( $(date +%s) + 300 ))
    until answers; do
        [[ $(date +%s) -lt $deadline ]] || fail "$vm gave no shell within 300 s of its cut"
        sleep 2
    done
    printf '%s %s %s\n' "$how" "$retries" "$(since "$cut_at")"
}

cut() {
    case $kind in
        stop) limactl stop --force "$vm" > /dev/null 2>&1 ;;
        kill)
            local pids
            pids=$(lsof -t "$lima_home/$vm/disk" 2> /dev/null || true)
            [[ -n $pids ]] || fail "no process holds $lima_home/$vm/disk"
            # shellcheck disable=SC2086  # one pid per word
            ps -o pid=,comm= -p $(echo $pids | tr ' ' ',') >> "$evidence/killed.txt"
            # shellcheck disable=SC2086
            kill -9 $pids ;;
        sysrq)
            # The shell never returns from a reboot; don't wait for it.
            (limactl shell --workdir / "$vm" sudo sh -c 'echo 1 > /proc/sys/kernel/sysrq; echo b > /proc/sysrq-trigger' \
                > /dev/null 2>&1 &)
            # Wait for the old boot's shell to go, so revive can't mistake it
            # for the new one.
            local deadline=$(( $(date +%s) + 30 ))
            while [[ $(date +%s) -lt $deadline && $(status) == Running ]] \
                && answers; do
                sleep 0.5
            done ;;
    esac
}

say "$cuts $kind cuts on $vm, seed $seed"
for (( i = 1; i <= cuts; i++ )); do
    reason=$(quiet) || fail "stopping after $(( i - 1 )) cuts: $reason"
    run=$(printf 'c%04d' "$i")
    acks=$evidence/$run.acks
    gsh python3 /var/lib/pcut/calibrate.py write --dir /var/lib/pcut --run "$run" --seconds 60 > "$acks" 2> "$evidence/$run.err" &
    writer=$!
    deadline=$(( $(date +%s) + 60 ))
    until grep -q '^ACK fsync' "$acks" 2> /dev/null && grep -q '^ACK direct' "$acks" && grep -q '^ACK buffered' "$acks"; do
        [[ $(date +%s) -lt $deadline ]] || fail "the writer for $run never acknowledged in every mode"
        sleep 0.2
    done
    hold=$(random_between $(( seed + i )) 3 12)
    sleep "$hold"
    cut_at=$(now)
    cut
    cut_seconds=$(since "$cut_at")
    read -r how retries back_seconds < <(revive "$cut_at")
    kill "$writer" 2> /dev/null || true
    wait "$writer" 2> /dev/null || true
    gsh python3 /var/lib/pcut/calibrate.py scan --dir /var/lib/pcut --run "$run" > "$evidence/$run.scan.json"
    detail=$(printf '{"hold_seconds": %s, "cut_seconds": %s, "back_seconds": %s, "came_back": "%s", "start_retries": %s}' \
        "$hold" "$cut_seconds" "$back_seconds" "$how" "$retries")
    python3 "$here/calibrate.py" record --kind "$kind" --cut "$i" --acks "$acks" --scan "$evidence/$run.scan.json" \
        --detail "$detail" >> "$evidence/cuts.jsonl"
    say "cut $i/$cuts: held $hold s, $how in $back_seconds s; $(tail -n 1 "$evidence/cuts.jsonl" | python3 -c '
import json, sys
line = json.load(sys.stdin)
print(", ".join(f"{m} acked {r[\"acked\"]} lost {r[\"lost_acked\"]}" for m, r in line["modes"].items()))')"
done
python3 "$here/calibrate.py" tally "$evidence/cuts.jsonl" > "$evidence/summary.json"
say "summary: $evidence/summary.json"
