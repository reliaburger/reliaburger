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
#   scripts/release/power_cut/calibrate.sh --kind KIND --cuts N --evidence DIR [--raw] [--vm NAME] [--seed N] [--keep]
#   scripts/release/power_cut/calibrate.sh --logwrites --evidence DIR [--vm NAME] [--keep]
#
#   --kind stop     `limactl stop --force` (SIGKILL to the host agent and driver)
#   --kind kill     SIGKILL to the process holding the VM's disk image
#                   (Virtualization.framework's VM process)
#   --kind sysrq    `echo b > /proc/sysrq-trigger` in the guest: reboot with no
#                   sync; records whether VZ reboots the guest or stops the VM
#   --raw           write only O_DIRECT records, alone, to the VM's second
#                   disk (no filesystem), with the disk's flush count, so a
#                   cut shows whether a completed write no flush covered
#                   survives (fault model 1) or not (model 2)
#   --logwrites     instead of cuts, run logwrites-proof.sh in the guest: can
#                   the guest kernel record a dm-log-writes log, and does a
#                   copy of it taken at a cut, replayed, drop exactly the
#                   writes no flush covered
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
raw=no
while [[ $# -gt 0 ]]; do
    case $1 in
        --keep) keep=yes; shift; continue ;;
        --logwrites) logwrites=yes; shift; continue ;;
        --raw) raw=yes; shift; continue ;;
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
if [[ $raw == yes ]]; then
    vm=${vm:-pc-cal-raw-$kind}
else
    vm=${vm:-pc-cal-${kind:-logwrites}}
fi
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
# bounded SECONDS COMMAND...: run COMMAND, killing it and everything it
# started after SECONDS (status 124). Not `perl -e 'alarm; exec'`: limactl is
# a Go program, and Go catches SIGALRM and ignores it, so an ssh to a guest
# that rebooted under it hung there for good. The command gets its own
# process group, so the ssh it starts dies with it and can't hold a pipe open.
bounded() {
    perl -e '
        my $seconds = shift;
        my $pid = fork() // die "fork: $!\n";
        if ($pid == 0) { setpgrp(0, 0); exec(@ARGV) or die "exec: $!\n"; }
        $SIG{ALRM} = sub { kill("KILL", -$pid); waitpid($pid, 0); exit(124); };
        alarm($seconds);
        waitpid($pid, 0);
        exit($? & 127 ? 128 + ($? & 127) : $? >> 8);' "$@"
}
# "${in_own_group[@]}" COMMAND... &: COMMAND leads a new process group, so
# `kill -- -$!` reaches it and everything under it (macOS has no setsid(1)).
# An array, not a function: bash runs a backgrounded function in a subshell,
# and $! would be that subshell, outside the group.
in_own_group=(perl -e 'setpgrp(0, 0); exec(@ARGV) or die "exec: $!\n";')
# Does the guest answer a shell? Bounded: ssh to a rebooting guest can hang.
answers() { bounded 15 limactl shell --workdir / "$vm" true < /dev/null > /dev/null 2>&1; }
status() { limactl list --format '{{.Status}}' "$vm" 2> /dev/null || echo missing; }

reason=$(quiet) || fail "not starting: $reason"
[[ ! -e $evidence/cuts.jsonl ]] || fail "$evidence already has cuts; evidence is never overwritten"
mkdir -p "$evidence"
# Captured first: `grep -q` exits at its first match, and limactl writing
# the rest of the list into the closed pipe would fail the pipeline.
existing=$(limactl list --format '{{.Name}}')
if grep -qx -- "$vm" <<< "$existing"; then
    fail "VM $vm already exists; delete it (limactl delete --force $vm) or name another with --vm"
fi
disk_name=$vm-raw
if [[ $raw == yes && -e $lima_home/_disks/$disk_name ]]; then
    fail "disk $disk_name already exists; delete it (limactl disk delete $disk_name) or name another VM with --vm"
fi

made_vm=no
made_disk=no
cleanup() {
    if [[ $keep == yes ]]; then
        [[ $made_vm == no ]] || echo "kept VM $vm; delete it with: limactl delete --force $vm" >&2
        [[ $made_disk == no ]] || echo "kept disk $disk_name; delete it with: limactl disk delete $disk_name" >&2
        return 0
    fi
    if [[ $made_vm == yes ]]; then
        limactl delete --force "$vm" > /dev/null 2>&1 || echo "could not delete VM $vm" >&2
    fi
    if [[ $made_disk == yes ]]; then
        limactl disk delete "$disk_name" > /dev/null 2>&1 || echo "could not delete disk $disk_name" >&2
    fi
}
trap cleanup EXIT

say "creating $vm"
# Only a --raw run gets the second disk, so every other run's VM is the one
# the published cuts used. The raw disk: 1 GiB (calibrate.py's RAW_SECTORS
# finds it by that size), raw format for VZ, named after this VM so two
# calibration VMs never share one, and attached with format: false so Lima
# neither formats nor mounts it.
start_options=()
if [[ $raw == yes ]]; then
    limactl disk create "$disk_name" --size 1GiB --format raw > "$evidence/disk.log" 2>&1 \
        || fail "creating disk $disk_name failed; see $evidence/disk.log"
    made_disk=yes
    start_options=(--set ".additionalDisks = [{\"name\": \"$disk_name\", \"format\": false}]")
fi
# Only a VM this run created is ever deleted, so made_vm flips only once the
# start has worked; a failed start may leave a half-made $vm to delete by hand.
limactl start --tty=false --name "$vm" ${start_options[@]+"${start_options[@]}"} \
    "$here/calibrate-vm.yaml" > "$evidence/create.log" 2>&1 \
    || fail "creating $vm failed; see $evidence/create.log (delete any half-made VM with: limactl delete --force $vm)"
made_vm=yes
limactl copy "$here/calibrate.py" "$here/logwrites.py" "$here/logwrites-proof.sh" "$vm:/tmp/"
gsh bash -c 'install -d /var/lib/pcut && install -m 755 /tmp/calibrate.py /tmp/logwrites.py /tmp/logwrites-proof.sh /var/lib/pcut/ \
    && python3 /var/lib/pcut/calibrate.py prepare --dir /var/lib/pcut && sync'
{
    printf '{"vm": "%s", "kind": "%s", "raw": "%s", "seed": %s, "lima": "%s", "host": "%s %s", ' \
        "$vm" "${kind:-logwrites}" "$raw" "$seed" "$(limactl --version | awk '{print $NF}')" "$(sw_vers -productName 2> /dev/null || uname -s)" "$(sw_vers -productVersion 2> /dev/null || uname -r)"
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
    # Only a guest reboot can leave the VM running; give it time to boot.
    while [[ $kind == sysrq && $waited -lt 90 ]]; do
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
            # One VM, one Virtualization.framework process: more than one
            # holder means something else has the disk open, and killing it
            # wouldn't be the cut this measures.
            [[ $(wc -w <<< "$pids") -eq 1 ]] || fail "more than one process holds $lima_home/$vm/disk: $(echo $pids)"
            ps -o pid=,comm= -p "$pids" >> "$evidence/killed.txt"
            kill -9 "$pids" ;;
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

if [[ $raw == yes ]]; then
    scan_args=(scan-raw)
    acknowledged() { grep -q '^ACK raw' "$1" 2> /dev/null; }
else
    scan_args=(scan --dir /var/lib/pcut)
    acknowledged() { grep -q '^ACK fsync' "$1" 2> /dev/null && grep -q '^ACK direct' "$1" && grep -q '^ACK buffered' "$1"; }
fi

say "$cuts $kind cuts on $vm, seed $seed$([[ $raw == no ]] || echo ', raw disk')"
for (( i = 1; i <= cuts; i++ )); do
    reason=$(quiet) || fail "stopping after $(( i - 1 )) cuts: $reason"
    run=$(printf 'c%04d' "$i")
    acks=$evidence/$run.acks
    # The writer's limactl and the ssh under it lead their own process
    # group, so the kill after the cut reaches all of them, not just a subshell.
    if [[ $raw == yes ]]; then
        "${in_own_group[@]}" limactl shell --workdir / "$vm" sudo python3 /var/lib/pcut/calibrate.py write-raw --run "$run" --seconds 60 \
            > "$acks" 2> "$evidence/$run.err" &
    else
        "${in_own_group[@]}" limactl shell --workdir / "$vm" sudo python3 /var/lib/pcut/calibrate.py write --dir /var/lib/pcut --run "$run" --seconds 60 \
            > "$acks" 2> "$evidence/$run.err" &
    fi
    writer=$!
    deadline=$(( $(date +%s) + 60 ))
    until acknowledged "$acks"; do
        [[ $(date +%s) -lt $deadline ]] || fail "the writer for $run never acknowledged in every mode"
        sleep 0.2
    done
    hold=$(random_between $(( seed + i )) 3 12)
    sleep "$hold"
    cut_at=$(now)
    cut
    cut_seconds=$(since "$cut_at")
    read -r how retries back_seconds < <(revive "$cut_at")
    kill -KILL -- "-$writer" 2> /dev/null || true
    wait "$writer" 2> /dev/null || true
    # A guest that has only just come back can refuse a shell for a moment.
    scanned=no
    for attempt in 1 2 3 4 5 6 7 8 9 10 11 12; do
        if bounded 120 limactl shell --workdir / "$vm" sudo python3 /var/lib/pcut/calibrate.py "${scan_args[@]}" --run "$run" > "$evidence/$run.scan.json" 2>> "$evidence/$run.err" \
            && [[ -s $evidence/$run.scan.json ]]; then
            scanned=yes
            break
        fi
        sleep 5
    done
    [[ $scanned == yes ]] || fail "could not scan $vm after cut $i; see $evidence/$run.err"
    detail=$(printf '{"hold_seconds": %s, "cut_seconds": %s, "back_seconds": %s, "came_back": "%s", "start_retries": %s}' \
        "$hold" "$cut_seconds" "$back_seconds" "$how" "$retries")
    python3 "$here/calibrate.py" record --kind "$kind" --cut "$i" --acks "$acks" --scan "$evidence/$run.scan.json" \
        --detail "$detail" >> "$evidence/cuts.jsonl"
    say "cut $i/$cuts: held $hold s, $how in $back_seconds s; $(tail -n 1 "$evidence/cuts.jsonl" | python3 -c '
import json, sys
line = json.load(sys.stdin)
print(", ".join("%s acked %d lost %d" % (m, r["acked"], r["lost_acked"]) for m, r in line["modes"].items()))')"
done
python3 "$here/calibrate.py" tally "$evidence/cuts.jsonl" > "$evidence/summary.json"
say "summary: $evidence/summary.json"
