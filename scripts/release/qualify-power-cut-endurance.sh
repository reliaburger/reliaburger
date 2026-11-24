#!/usr/bin/env bash
# shellcheck disable=SC2016  # single-quoted guest commands expand in the guest
# Power-cut endurance (#2067): build a one-node quickstart cluster from a
# published release, or a staged candidate, run the soak's volume writer and
# Redis on it, then cut its power at random intervals, again and again, and
# after every cut check that it came back by itself with nothing
# acknowledged lost. Step 1 of docs/plans/2026-11-18-plan-power-cut-endurance.md.
#
# After each cut the harness only powers the VM on (`limactl start`) and
# reads; it never repairs anything. The checks (numbered as in the plan):
#   I1  /data/seq holds 1..N; the writer started again after the cut and
#       each start since then resumed at or past the last ACK read before
#       the cut; a start between the cuts resumed at or past the previous
#       cut's ACK
#   I2  the Redis counter never goes backwards; the client started again
#       after the cut and each start since then found it at or past the last
#       INCR read before the cut; a start between the cuts found it at or
#       past the previous cut's INCR
#   I4  Bun started once since boot, never exited, logged no panic or store error
#   I6  Bun answered within 120 s of power-on; the cluster settled within 300 s
#   I7  every workload runs exactly once
#
# The cluster lives in an isolated RELIABURGER_HOME under /tmp; the real
# ~/.reliaburger and ~/.local/bin/relish are never used. It refuses to start,
# and stops between cuts (powering the cluster off cleanly), when
# power_cut/quiet.py says a release gate holds the host; --resume picks the
# run up again. Any other early exit (a harness error, a failed install, an
# interrupt) marks the run aborted and prints where the cluster and the
# evidence are. Between cuts it stops the cluster cleanly, and --resume
# carries on; mid-cut (a cut made but not recorded) it forces the VM off and
# the run can't resume.
#
# Usage:
#   scripts/release/qualify-power-cut-endurance.sh (--release X.Y.Z | --base-url URL --qualified-digest SHA) [options]
#
#   --release X.Y.Z         install this published release with the website's
#                           `curl | sh`
#   --base-url URL          or a staged candidate (the staging pre-release)
#   --qualified-digest SHA  require the candidate's candidate.json to have this SHA-256
#   --tier T                quick (1 h, a cut every 2-10 minutes) or custom
#                           (the default; set --duration)
#   --duration D            how long to keep cutting: 40m, 6h, 2d (default: the tier's)
#   --work MIN-MAX          seconds of work between cuts (default 120-600)
#   --off MIN-MAX           seconds the power stays off (default 5-60)
#   --evidence DIR          evidence directory (default: a new
#                           /var/tmp/reliaburger-power-cut.XXXXXX)
#   --seed N                seed for every random choice (default: a new one)
#   --port-offset N         add N (up to 500) to the cluster's API, ingress
#                           and registry ports, 31000, 31600 and 32200, so
#                           several runs can share a host (default 0)
#   --resume                carry on the run in --evidence: same cluster, seed
#                           and counts; refused once a cut has violated
#                           something, so the kept state stays as it was
#   --rejudge DIR           run today's checker over the cuts recorded in DIR,
#                           offline, print the verdicts and exit; writes nothing
#   --keep                  leave the cluster and the installation afterwards
#   --plan                  print the cut schedule these options give and exit
set -euo pipefail

usage() { sed -n '3,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
fail() { printf 'power-cut: %s\n' "$*" >&2; exit 1; }
say() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*"; }

repository=$(cd "$(dirname "$0")/../.." && pwd)
here=$repository/scripts/release/power_cut
endurance() { python3 "$here/endurance.py" "$@"; }
quiet() { python3 "$here/quiet.py" check; }
# with_timeout SECONDS COMMAND...: run COMMAND, killing it and everything it
# started after SECONDS (status 124). Not `perl -e 'alarm; exec'`: limactl is
# a Go program, and Go catches SIGALRM and ignores it, so a `limactl shell`
# to a guest that went away under it would hang for good (the calibration's
# sysrq run did, 17 November 2026). The command gets its own process group,
# so the ssh it starts dies with it and can't hold a pipe open. Nothing run
# through it reads standard input.
with_timeout() {
    perl -e '
        my $seconds = shift;
        my $pid = fork() // die "fork: $!\n";
        if ($pid == 0) { setpgrp(0, 0); exec(@ARGV) or die "exec: $!\n"; }
        $SIG{ALRM} = sub { kill("KILL", -$pid); waitpid($pid, 0); exit(124); };
        alarm($seconds);
        waitpid($pid, 0);
        exit($? & 127 ? 128 + ($? & 127) : $? >> 8);' "$@" < /dev/null
}
now() { date +%s; }
seconds() {
    case $1 in
        *m) echo $(( ${1%m} * 60 )) ;;
        *h) echo $(( ${1%h} * 3600 )) ;;
        *d) echo $(( ${1%d} * 86400 )) ;;
        *[!0-9]*|'') fail "a duration is a number of minutes, hours or days, like 40m, 6h or 2d (not $1)" ;;
        *) echo "$1" ;;
    esac
}
range() {
    [[ $1 =~ ^([0-9]+)-([0-9]+)$ ]] && [ "${BASH_REMATCH[1]}" -le "${BASH_REMATCH[2]}" ] \
        || fail "a range is MIN-MAX seconds, like 120-600 (not $1)"
    echo "${BASH_REMATCH[1]} ${BASH_REMATCH[2]}"
}

release=
base_url=
qualified_digest=
tier=custom
duration=
work=120-600
off=5-60
evidence=
seed=
resume=false
keep=false
plan=false
port_offset=0
rejudge=
while [ "$#" -gt 0 ]; do
    case $1 in
        --release) release=${2:-}; shift 2 ;;
        --base-url) base_url=${2:-}; shift 2 ;;
        --qualified-digest) qualified_digest=${2:-}; shift 2 ;;
        --tier) tier=${2:-}; shift 2 ;;
        --duration) duration=${2:-}; shift 2 ;;
        --work) work=${2:-}; shift 2 ;;
        --off) off=${2:-}; shift 2 ;;
        --evidence) evidence=${2:-}; shift 2 ;;
        --seed) seed=${2:-}; shift 2 ;;
        --resume) resume=true; shift ;;
        --keep) keep=true; shift ;;
        --plan) plan=true; shift ;;
        --port-offset) port_offset=${2:-}; shift 2 ;;
        --rejudge) rejudge=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown argument: $1" ;;
    esac
done
if [ -n "$rejudge" ]; then
    [ -f "$rejudge/cuts.jsonl" ] || fail "no cuts.jsonl in $rejudge"
    exec python3 "$here/endurance.py" rejudge --evidence "$rejudge"
fi
case $tier in
    quick) duration=${duration:-60m} ;;
    custom) [ -n "$duration" ] || [ "$resume" = true ] || fail 'a custom tier needs --duration' ;;
    *) fail "unknown tier $tier (quick or custom; the day and week tiers come with step 2)" ;;
esac
read -r work_min work_max < <(range "$work")
read -r off_min off_max < <(range "$off")
[ -z "$seed" ] || [[ $seed =~ ^[0-9]+$ ]] || fail '--seed takes a whole number'
[[ $port_offset =~ ^[0-9]+$ ]] && [ "$port_offset" -le 500 ] || fail '--port-offset takes a whole number up to 500'

if [ "$plan" = true ]; then
    endurance plan --seed "${seed:-0}" --duration "$(seconds "$duration")" --min "$work_min" --max "$work_max"
    exit 0
fi

if [ "$resume" = true ]; then
    [ -f "$evidence/run.json" ] || fail "--resume needs --evidence with a run.json"
    # Carrying on would power the kept VM on and overwrite what the
    # violation left (run 10 did, after cut 13).
    if ! violated=$(endurance violations --evidence "$evidence"); then
        fail "not resuming: $(echo "$violated" | paste -sd ';' - | sed 's/;/; /g'); the cluster and evidence" \
            "stay as the violation left them. Diagnose it (--rejudge $evidence re-checks the recorded cuts) and start a new run"
    fi
else
    [ -n "$release" ] || [ -n "$base_url" ] || { usage >&2; fail 'give --release or --base-url'; }
    [ -z "$release" ] || [ -z "$base_url" ] || fail 'give --release or --base-url, not both'
    [ -z "$release" ] || [[ $release =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail '--release takes a version like 0.2.8'
    if [ -n "$base_url" ]; then
        [[ $base_url == https://?* ]] || fail 'the base URL must be HTTPS'
        [[ $qualified_digest =~ ^[0-9a-f]{64}$ ]] || fail 'a staged candidate needs --qualified-digest'
    fi
    if [ -n "$evidence" ]; then
        [ ! -e "$evidence/run.json" ] || fail "$evidence already holds a run; use --resume or another directory"
    else
        evidence=$(mktemp -d /var/tmp/reliaburger-power-cut.XXXXXX)
    fi
fi
mkdir -p "$evidence/cuts" "$evidence/logs"
reason=$(quiet) || fail "not starting: $reason"
# A key from run.json, or nothing when it isn't there (a stored 0 prints 0,
# a boolean true or false).
run_get() {
    python3 -c '
import json, sys
value = json.load(open(sys.argv[1])).get(sys.argv[2])
print("" if value is None else json.dumps(value) if isinstance(value, bool) else value)' "$evidence/run.json" "$1"
}
run_set() {
    python3 - "$evidence/run.json" "$1" "$2" <<'PY'
import json, os, sys
path, key, value = sys.argv[1:]
data = json.load(open(path)) if os.path.exists(path) else {}
try:
    data[key] = json.loads(value)
except json.JSONDecodeError:
    data[key] = value
with open(path + ".tmp", "w") as file:
    json.dump(data, file, indent=2)
    file.write("\n")
os.replace(path + ".tmp", path)
PY
}

# --- the cluster -----------------------------------------------------------------

# The ports are fixed at install, and the cluster keeps them, so --resume
# needs no --port-offset. endurance.py's PORT_BASES says why they sit in
# 31000-32767: clear of the test suites' ports and every ephemeral range.
read -r api_port ingress_port registry_port < <(endurance ports --offset "$port_offset") \
    || fail "no ports for --port-offset $port_offset"

install_cluster() {
    home=$(mktemp -d /tmp/rbpc.XXXXXX)
    run_set home "$home"
    export RELIABURGER_HOME=$home RELIABURGER_NO_MODIFY_PATH=1
    local setup=(--laptop --nodes 1 --api-port "$api_port" --ingress-port "$ingress_port" --registry-port "$registry_port")
    say "installing into $home"
    if [ -n "$base_url" ]; then
        curl -fsSL --proto '=https' "$base_url/candidate.json" -o "$evidence/candidate.json"
        local digest
        digest=$(shasum -a 256 "$evidence/candidate.json" | awk '{print $1}')
        [ "$digest" = "$qualified_digest" ] || fail "candidate.json has SHA-256 $digest, not $qualified_digest"
        curl -fsSL https://reliaburger.com/install.sh \
            | RELIABURGER_RELEASE_BASE_URL=$base_url sh -s -- "${setup[@]}" > "$evidence/logs/install.log" 2>&1 \
            || fail "the install failed; see $evidence/logs/install.log"
    else
        curl -fsSL https://reliaburger.com/install.sh \
            | RELIABURGER_VERSION=v$release sh -s -- "${setup[@]}" > "$evidence/logs/install.log" 2>&1 \
            || fail "the install failed; see $evidence/logs/install.log"
    fi
}

discover() {
    local adopted
    adopted=$(run_get home)
    case $adopted in
        /tmp/rbpc.*) home=$adopted ;;
        *) fail "refusing to use RELIABURGER_HOME=$adopted: this harness only adopts a home it made" ;;
    esac
    [ -f "$home/clusters/laptop/state.json" ] || fail "no quickstart cluster in $home"
    export RELIABURGER_HOME=$home RELIABURGER_NO_MODIFY_PATH=1 LIMA_HOME=$home/lima
    relish=$home/bin/relish
    limactl=$(find "$home/tools" -path '*/bin/limactl' -type f 2> /dev/null | head -n 1)
    [ -x "$relish" ] && [ -n "$limactl" ] || fail "no relish or limactl in $home"
    vm=(none "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["nodes"][0]["name"])' "$home/clusters/laptop/state.json")")
    down=(0 0)
    [ "${vm[1]}" != reliaburger-test ] || fail 'refusing to cut the development VM'
}

rel() { with_timeout 330 "$relish" "$@"; }
gsh() { with_timeout 120 "$limactl" shell --workdir / "${vm[1]}" sudo bash -c "$1"; }

workloads=$here/workloads.toml

# Turn off the guest's automatic updates. A cut mid-dpkg can leave the guest
# broken, which isn't Reliaburger's to answer for, and in run 8
# unattended-upgrades upgraded libc and needrestart restarted Bun between
# two cuts. Masked, so nothing starts them again; needrestart only lists what
# it would restart, should anything run apt. Idempotent, so a resume runs it
# again on a cluster made before it existed.
hold_guest_updates() {
    gsh 'for unit in apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service; do
             systemctl cat "$unit" > /dev/null 2>&1 || continue
             systemctl disable --now "$unit" && systemctl mask "$unit" || exit 1
         done
         systemctl stop apt-daily.service apt-daily-upgrade.service 2> /dev/null || true
         if [ -d /etc/needrestart ]; then
             mkdir -p /etc/needrestart/conf.d
             echo "\$nrconf{restart} = \"l\";" > /etc/needrestart/conf.d/90-reliaburger-power-cut.conf
         fi' > "$evidence/logs/guest-updates.log" 2>&1
}

want='soak-writer=1,soak-redis=1,soak-redis-client=1'

apply_workloads() {
    rel apply -f "$workloads" > "$evidence/logs/apply.log" 2>&1 || fail "applying $workloads failed"
    take_baseline
}

# Stop the run for something that isn't the cluster's to answer for: a cut
# the harness can't judge. The EXIT trap (cleanup) powers the VM off.
abort_run() {
    run_set aborted "$*"
    fail "$*"
}

# Wait for every workload to run and acknowledge something, then record how
# many starts each has made so far, so the next cut can tell its own. Before
# the first cut, and again after a resume (whose `relish local start` started
# them again).
take_baseline() {
    local deadline=$(( $(now) + 600 )) starts
    until rel --output json apps --instances > "$evidence/logs/instances.json" 2> /dev/null \
        && endurance instances "$evidence/logs/instances.json" --want "$want" > /dev/null; do
        [ "$(now)" -lt "$deadline" ] || abort_run 'the workloads were not all running after 600 s'
        sleep 10
    done
    deadline=$(( $(now) + 120 ))
    until read_acked writer > /dev/null && read_acked redis > /dev/null; do
        [ "$(now)" -lt "$deadline" ] || abort_run "no usable ACK and INCR after 120 s: $(acked_problems)"
        sleep 5
    done
    read_resumes writer "$evidence/logs/resumes-writer.txt"
    read_resumes redis "$evidence/logs/resumes-redis.txt"
    starts=$(starts_in "$evidence/logs/resumes-writer.txt")
    [ -n "$starts" ] || abort_run "couldn't read where the writer resumed (/data/resumes)"
    run_set writer_starts "$starts"
    starts=$(starts_in "$evidence/logs/resumes-redis.txt")
    [ -n "$starts" ] || abort_run "couldn't read where the Redis client resumed (soak:starts)"
    run_set redis_starts "$starts"
}

# Where the writer resumed (/data/resumes) or the Redis client found the
# counter (soak:starts) at each start, oldest first: read_resumes writer|redis FILE.
read_resumes() {
    if [ "$1" = writer ]; then
        rel exec soak-writer cat /data/resumes > "$2" 2>&1 || true
    else
        rel exec soak-redis redis-cli LRANGE soak:starts 0 -1 > "$2" 2>&1 || true
    fi
}
# How many starts FILE records; nothing when it can't be read or records none.
starts_in() {
    endurance resumed "$1" --acked 0 \
        | python3 -c 'import json,sys; r = json.load(sys.stdin); print(r["starts"] if r["pass"] else "")' || true
}

# The highest ACK (writer) or INCR (redis) in the app's recent log. Fails,
# saying why on standard error, when the log can't be read, holds none, or is
# below the last value read (run.json's writer_acked or redis_acked).
acked_now() {
    local app=soak-writer previous
    [ "$1" = writer ] || app=soak-redis-client
    previous=$(run_get "$1_acked")
    rel logs "$app" --tail 50 > "$evidence/logs/acked-$1.txt" 2>&1 \
        || { echo "power-cut: couldn't read $app's log" >&2; return 1; }
    endurance acked "$1" "$evidence/logs/acked-$1.txt" ${previous:+--previous "$previous"}
}

# acked_now, three tries 5 s apart; prints the value and keeps it in run.json.
# On failure the last try's reason is in logs/acked-KIND.problem and on
# standard error.
read_acked() {
    local value attempt problem=$evidence/logs/acked-$1.problem
    for attempt in 1 2 3; do
        if value=$(acked_now "$1" 2> "$problem"); then
            rm -f "$problem"
            run_set "$1_acked" "$value"
            echo "$value"
            return 0
        fi
        [ "$attempt" -eq 3 ] || sleep 5
    done
    cat "$problem" >&2
    return 1
}

# Why the last read_acked calls failed: "writer: ...; redis: ...".
acked_problems() {
    local kind reasons=()
    for kind in writer redis; do
        [ ! -s "$evidence/logs/acked-$kind.problem" ] \
            || reasons+=("$kind: $(sed 's/^power-cut: //' "$evidence/logs/acked-$kind.problem" | paste -sd ' ' -)")
    done
    local IFS=';'
    echo "${reasons[*]:-no reason recorded}"
}

# --- power (sustained/power.sh) ---------------------------------------------------

# shellcheck source=sustained/power.sh
. "$repository/scripts/release/sustained/power.sh"
power_log_dir=$evidence/logs
power_before_off() { :; }
settle_streak=2
settle_interval=5
settle_probe() {
    rel --output json apps --instances > "$cut_dir/instances.json" 2> /dev/null \
        && endurance instances "$cut_dir/instances.json" --want "$want" > /dev/null
}
settle_between() { :; }
settle_succeeded() { settled_seconds=$(( $(now) - powered_on_at )); }
settle_timed_out() { settled_seconds=; }

# --- one cut ---------------------------------------------------------------------

# I6's bound on Bun answering after power-on (API_BOUND_SECONDS in endurance.py).
api_bound=120

# Power the VM back on with `limactl start` alone, then wait until Bun's API
# answers a read, at most api_bound seconds after power-on. Not `relish local
# start`: it also starts Bun's service, and it fails when Bun isn't ready
# within its own 45 s, which a retry would turn into a second, unrecorded cut
# mid-recovery. The one retry is Lima's known VZ refusal straight after a
# forced stop ("storage device attachment is invalid"), when the VM never
# booted; it counts as an environment event. Any other failure is left to I6
# and I4 to judge.
power_back() {
    local log
    log=$power_log_dir/power-on-1-$(now).log
    start_retries=0
    if ! with_timeout 300 "$limactl" start --tty=false "${vm[1]}" > "$log" 2>&1; then
        grep -q 'storage device attachment is invalid' "$log" || return 1
        start_retries=1
        with_timeout 120 "$limactl" stop --force "${vm[1]}" >> "$log" 2>&1 || true
        with_timeout 300 "$limactl" start --tty=false "${vm[1]}" >> "$log" 2>&1 || return 1
    fi
    down[1]=0
    until with_timeout 15 "$relish" --output json apps --instances > /dev/null 2>&1; do
        [ "$(now)" -lt $(( powered_on_at + api_bound )) ] || return 1
        sleep 2
    done
}

# How long verify waits for each workload's start to reach its resume
# record. "Settled" means every instance runs, but the Redis client still
# has to reach Redis before it can push its start onto soak:starts; reading
# the list straight away found no new start (run 12 cut 4, run 10 cuts 13
# and 33).
start_wait=60

# await_start writer|redis FILE BEFORE: read the resume record into FILE
# until it holds more than BEFORE starts, for at most start_wait seconds.
await_start() {
    local deadline=$(( $(now) + start_wait )) count
    while true; do
        read_resumes "$1" "$2"
        count=$(starts_in "$2")
        [ -z "$3" ] || { [ -n "$count" ] && [ "$count" -gt "$3" ]; } && return 0
        [ "$(now)" -lt "$deadline" ] || return 1
        sleep 3
    done
}

# verify WRITER_ACKED REDIS_ACKED [WRITER_BEFORE REDIS_BEFORE WRITER_PREVIOUS REDIS_PREVIOUS]:
# the checks after a cut. The BEFOREs are the starts each record held just
# before the power-off, and the PREVIOUSes the ACK and INCR read before the
# previous cut: a start between the cuts is held to those.
verify() {
    local writer_acked=$1 redis_acked=$2 writer_before=${3:-} redis_before=${4:-} previous starts
    local writer_previous=${5:-} redis_previous=${6:-}
    rel exec soak-writer awk '$0 != NR && bad == "" { bad = NR ": " $0 } { if (count[$0]++ == 1) { dups++; if (first_dup == "") first_dup = $0 } if ($0 + 0 > max) max = $0 + 0 } END { if (bad != "") print "BAD " bad; missing = 0; for (i = 1; i <= max; i++) if (!(i in count)) { if (!missing) first_missing = i; missing++ } if (dups) print "DUP " dups " first " first_dup; if (missing) print "MISSING " missing " first " first_missing; print "MAX " max + 0; print "LAST " NR }' /data/seq \
        > "$cut_dir/seq.txt" 2>&1 || true
    starts=$(run_get writer_starts)
    await_start writer "$cut_dir/resumes-writer.txt" "${writer_before:-$starts}" || true
    endurance seq "$cut_dir/seq.txt" --acked "$writer_acked" --resumes "$cut_dir/resumes-writer.txt" \
        ${starts:+--starts "$starts"} ${writer_before:+--before-cut "$writer_before"} \
        ${writer_previous:+--previous-acked "$writer_previous"} > "$cut_dir/I1.json" || true
    starts=$(starts_in "$cut_dir/resumes-writer.txt")
    [ -z "$starts" ] || run_set writer_starts "$starts"
    rel exec soak-redis redis-cli GET soak:counter > "$cut_dir/counter.txt" 2>&1 || true
    previous=$(run_get counter)
    starts=$(run_get redis_starts)
    await_start redis "$cut_dir/resumes-redis.txt" "${redis_before:-$starts}" || true
    endurance counter "$cut_dir/counter.txt" --acked "$redis_acked" ${previous:+--previous "$previous"} \
        --resumes "$cut_dir/resumes-redis.txt" ${starts:+--starts "$starts"} \
        ${redis_before:+--before-cut "$redis_before"} ${redis_previous:+--previous-acked "$redis_previous"} \
        > "$cut_dir/I2.json" || true
    starts=$(starts_in "$cut_dir/resumes-redis.txt")
    [ -z "$starts" ] || run_set redis_starts "$starts"
    python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); print(r.get("counter", ""))' "$cut_dir/I2.json" > "$cut_dir/counter-value"
    [ -z "$(cat "$cut_dir/counter-value")" ] || run_set counter "$(cat "$cut_dir/counter-value")"
    gsh 'journalctl -b -u reliaburger.service --no-pager -o short-iso' > "$cut_dir/journal.txt" 2>&1 || true
    endurance journal "$cut_dir/journal.txt" > "$cut_dir/I4.json" || true
    rel --output json apps --instances > "$cut_dir/instances.json" 2> /dev/null || true
    endurance instances "$cut_dir/instances.json" --want "$want" > "$cut_dir/I7.json" || true
}

one_cut() {
    local cut=$1 hold off writer_acked redis_acked cut_at api_seconds result=0
    local writer_previous redis_previous writer_before redis_before environment=
    cut_dir=$evidence/cuts/$(printf '%05d' "$cut")
    mkdir -p "$cut_dir"
    hold=$(endurance hold --seed "$seed" --cut "$cut" --what work --min "$work_min" --max "$work_max")
    off=$(endurance hold --seed "$seed" --cut "$cut" --what off --min "$off_min" --max "$off_max")
    say "cut $cut: working for $hold s"
    sleep "$hold"
    # With no last ACK and INCR, I1 and I2 would hold whatever the cut did,
    # while the cut still counted towards the bound: don't make it.
    writer_previous=$(run_get writer_acked)
    redis_previous=$(run_get redis_acked)
    writer_acked=$(read_acked writer) && redis_acked=$(read_acked redis) \
        || abort_run "cut $cut wasn't made: no last ACK or INCR to hold it to ($(acked_problems))"
    # Each record's starts just before the power-off, read after the ACKs: a
    # start after this point is the cut's and must resume past them. When a
    # record can't be read, every start since the previous check is held to
    # this cut's ACK, as before.
    read_resumes writer "$cut_dir/resumes-writer-before.txt"
    read_resumes redis "$cut_dir/resumes-redis-before.txt"
    writer_before=$(starts_in "$cut_dir/resumes-writer-before.txt")
    redis_before=$(starts_in "$cut_dir/resumes-redis-before.txt")
    cut_at=$(now)
    run_set cuts_started "$cut"
    power_off 1
    say "cut $cut: power off (acked: writer $writer_acked, redis $redis_acked); back in $off s"
    sleep "$off"
    powered_on_at=$(now)
    settled_seconds=
    if power_back; then
        api_seconds=$(( $(now) - powered_on_at ))
        settle "cut-$cut" 300 || true
    else
        api_seconds=
        # Lima's host agent says why a forward failed: in run 6 a test binary
        # held the API port, so Bun "never came back".
        cp "$LIMA_HOME/${vm[1]}/ha.stderr.log" "$cut_dir/ha.stderr.log" 2> /dev/null || true
        ! grep -qi 'address already in use' "$cut_dir/ha.stderr.log" 2> /dev/null || environment='host port taken'
    fi
    verify "$writer_acked" "$redis_acked" "$writer_before" "$redis_before" "$writer_previous" "$redis_previous"
    local detail
    detail=$(python3 -c '
import json, sys
def number(text):
    return int(text) if text.strip() else None
detail = {"topology": "1", "kind": "site", "cut_at": int(sys.argv[1]), "work_seconds": int(sys.argv[2]),
    "off_seconds": int(sys.argv[3]), "acked": {"writer": int(sys.argv[4]), "redis": int(sys.argv[5])},
    "recovery": {"api_seconds": number(sys.argv[6]), "settled_seconds": number(sys.argv[7]),
                 "start_retries": int(sys.argv[8])},
    "cycle_seconds": int(sys.argv[9]),
    "starts_before_cut": {"writer": number(sys.argv[10]), "redis": number(sys.argv[11])},
    "previous_acked": {"writer": number(sys.argv[12]), "redis": number(sys.argv[13])}}
if sys.argv[14]:
    detail["environment"] = [sys.argv[14]]
print(json.dumps(detail))' \
        "$cut_at" "$hold" "$off" "$writer_acked" "$redis_acked" "${api_seconds:-}" "${settled_seconds:-}" \
        "$start_retries" "$(( $(now) - cut_at + hold ))" "${writer_before:-}" "${redis_before:-}" \
        "${writer_previous:-}" "${redis_previous:-}" "$environment")
    endurance record --evidence "$evidence" --cut "$cut" --detail "$detail" > "$cut_dir/verdict.json" || result=$?
    say "cut $cut: Bun back in ${api_seconds:-never} s, settled in ${settled_seconds:-never} s; $(cat "$cut_dir/verdict.json")"
    return "$result"
}

# --- the run ---------------------------------------------------------------------

# A finished run tears the cluster down (unless --keep). A violation and a
# pause keep it as it is (stopping=keep-off). Any other exit once there is a
# cluster marks the run aborted and says where things are. Between cuts
# (every cut made has its line) it stops the cluster cleanly, so --resume can
# carry on: a forced stop there would be a cut nobody recorded. Mid-cut, or
# when the clean stop fails, it forces the VM off and the run can't resume.
cleanup() {
    local status=$?
    [ "${stopping:-}" = keep-off ] && return "$status"
    if [ "${finished:-false}" = true ]; then
        if [ "$keep" = false ] && [ -n "${relish:-}" ]; then
            "$relish" local destroy --yes > "$evidence/logs/teardown.log" 2>&1 \
                && "$relish" uninstall --yes >> "$evidence/logs/teardown.log" 2>&1 \
                && rm -rf "$home" || say "teardown failed; see $evidence/logs/teardown.log"
        fi
        return "$status"
    fi
    [ -n "${home:-}" ] || return "$status"
    local between=false log=$evidence/logs/abort-stop.log
    [ ! -f "$evidence/run.json" ] || [ "$(run_get cuts_started)" != "$(cuts_done)" ] || between=true
    if [ "$between" = true ] && [ -n "${relish:-}" ]; then
        with_timeout 300 "$relish" local stop --yes > "$log" 2>&1 || between=false
    elif [ "$between" = true ]; then
        between=false
    fi
    if [ "$between" = false ] && [ -n "${limactl:-}" ] && [ -n "${vm[1]:-}" ]; then
        with_timeout 120 "$limactl" stop --force "${vm[1]}" >> "$log" 2>&1 || true
    fi
    if [ -f "$evidence/run.json" ]; then
        [ -n "$(run_get aborted)" ] || run_set aborted "the harness exited early with status $status"
        run_set aborted_between_cuts "$between"
        run_set ended "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        endurance summary --evidence "$evidence" > /dev/null 2>&1 || true
    fi
    if [ "$between" = true ]; then
        say "stopped between cuts (status $status) with the cluster stopped cleanly; cluster home: $home;" \
            "evidence: $evidence; carry on with --resume --evidence $evidence" >&2
    else
        say "stopped early (status $status) with the VM powered off; cluster home: $home; evidence: $evidence" >&2
    fi
    return "$status"
}
trap cleanup EXIT

# The lines in cuts.jsonl: the cuts recorded.
cuts_done() {
    if [ -f "$evidence/cuts.jsonl" ]; then grep -c . "$evidence/cuts.jsonl" || true; else echo 0; fi
}

if [ "$resume" = true ]; then
    if [ -n "$(run_get aborted)" ]; then
        [ "$(run_get aborted_between_cuts)" = true ] \
            || fail "this run was aborted mid-cut ($(run_get aborted)); start a new one"
    fi
    cuts_started=$(run_get cuts_started)
    [ "${cuts_started:-0}" = "$(cuts_done)" ] \
        || fail "cut $cuts_started was made but never recorded ($(cuts_done) in cuts.jsonl); start a new run"
    if [ -n "$(run_get aborted)" ]; then
        say "carrying on after an abort between cuts: $(run_get aborted)"
        run_set aborted ''
        run_set aborted_between_cuts false
    fi
    seed=$(run_get seed)
    duration_seconds=$(run_get duration_seconds)
    work_min=$(run_get work_min); work_max=$(run_get work_max)
    off_min=$(run_get off_min); off_max=$(run_get off_max)
    discover
    paused_since=$(run_get paused_at)
    if [ -n "$paused_since" ]; then
        run_set paused_seconds $(( $(run_get paused_seconds || echo 0) + $(now) - paused_since ))
        run_set paused_at ''
    fi
    say "resuming in $evidence"
    with_timeout 600 "$relish" local start > "$evidence/logs/resume-start.log" 2>&1 \
        || abort_run "relish local start failed on resume"
    hold_guest_updates || abort_run "couldn't turn off the guest's automatic updates; see $evidence/logs/guest-updates.log"
    take_baseline
else
    seed=${seed:-$(python3 -c 'import random; print(random.randrange(2**31))')}
    duration_seconds=$(seconds "$duration")
    run_set seed "$seed"
    run_set tier "$tier"
    run_set duration_seconds "$duration_seconds"
    run_set work_min "$work_min"; run_set work_max "$work_max"
    run_set off_min "$off_min"; run_set off_max "$off_max"
    run_set release "${release:-$base_url}"
    run_set started "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    run_set paused_seconds 0
    run_set cuts_started 0
    run_set port_offset "$port_offset"
    run_set host "$(sw_vers -productName 2> /dev/null || uname -s) $(sw_vers -productVersion 2> /dev/null || uname -r), $(sysctl -n machdep.cpu.brand_string 2> /dev/null || uname -m)"
    install_cluster
    discover
    run_set hypervisor "lima $("$limactl" --version | awk '{print $NF}') $("$limactl" list --format '{{.VMType}}' "${vm[1]}")"
    run_set relish "$("$relish" --version)"
    run_set bun "$(gsh '/usr/local/bin/bun --version 2>/dev/null || bun --version 2>/dev/null' | tail -n 1)"
    hold_guest_updates || abort_run "couldn't turn off the guest's automatic updates; see $evidence/logs/guest-updates.log"
    say "applying the workloads"
    apply_workloads
fi
say "evidence: $evidence; seed $seed; $(( duration_seconds / 60 )) minutes of cuts"

elapsed() { python3 -c '
import json, sys
total = 0
try:
    for line in open(sys.argv[1]):
        if line.strip():
            total += json.loads(line).get("cycle_seconds", 0)
except FileNotFoundError:
    pass
print(total)' "$evidence/cuts.jsonl"; }

while [ "$(elapsed)" -lt "$duration_seconds" ]; do
    if ! reason=$(quiet); then
        say "pausing: $reason; resume with --resume --evidence $evidence"
        with_timeout 300 "$relish" local stop --yes > "$evidence/logs/pause-stop.log" 2>&1 || true
        run_set paused_at "$(now)"
        stopping=keep-off
        exit 75
    fi
    cut=$(( $(cuts_done) + 1 ))
    if ! one_cut "$cut"; then
        say "cut $cut violated an invariant; powering the VM off and keeping everything for diagnosis"
        with_timeout 120 "$limactl" stop --force "${vm[1]}" > /dev/null 2>&1 || true
        stopping=keep-off
        run_set ended "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        endurance summary --evidence "$evidence" || true
        fail "violation at cut $cut; evidence in $evidence, cluster in $home (powered off)"
    fi
done
run_set ended "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
finished=true
result=0
endurance summary --evidence "$evidence" || result=$?
say "summary: $evidence/summary.json"
exit "$result"
