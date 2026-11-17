#!/usr/bin/env bash
# shellcheck disable=SC2016  # single-quoted guest commands expand in the guest
# Power-cut endurance (#2067): build a one-node quickstart cluster from a
# published release, or a staged candidate, run the soak's volume writer and
# Redis on it, then cut its power at random intervals, again and again, and
# after every cut check that it came back by itself with nothing
# acknowledged lost. Step 1 of docs/plans/2026-11-18-plan-power-cut-endurance.md.
#
# After each cut the harness only powers the VM on (`relish local start`)
# and reads; it never repairs anything. The checks (numbered as in the plan):
#   I1  /data/seq holds 1..N, and the writer, started again, resumed at or
#       past the last ACK read before the cut
#   I2  the Redis counter never goes backwards, and the client, started
#       again, found it at or past the last INCR read before the cut
#   I4  Bun started once since boot, never exited, logged no panic or store error
#   I6  Bun answered within 120 s of power-on; the cluster settled within 300 s
#   I7  every workload runs exactly once
#
# The cluster lives in an isolated RELIABURGER_HOME under /tmp; the real
# ~/.reliaburger and ~/.local/bin/relish are never used. It refuses to start,
# and stops between cuts (powering the cluster off cleanly), when
# power_cut/quiet.py says a release gate holds the host; --resume picks the
# run up again.
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
#   --resume                carry on the run in --evidence: same cluster, seed
#                           and counts
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
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown argument: $1" ;;
    esac
done
case $tier in
    quick) duration=${duration:-60m} ;;
    custom) [ -n "$duration" ] || [ "$resume" = true ] || fail 'a custom tier needs --duration' ;;
    *) fail "unknown tier $tier (quick or custom; the day and week tiers come with step 2)" ;;
esac
read -r work_min work_max < <(range "$work")
read -r off_min off_max < <(range "$off")
[ -z "$seed" ] || [[ $seed =~ ^[0-9]+$ ]] || fail '--seed takes a whole number'

if [ "$plan" = true ]; then
    endurance plan --seed "${seed:-0}" --duration "$(seconds "$duration")" --min "$work_min" --max "$work_max"
    exit 0
fi

if [ "$resume" = true ]; then
    [ -f "$evidence/run.json" ] || fail "--resume needs --evidence with a run.json"
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
run_get() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get(sys.argv[2]) or "")' "$evidence/run.json" "$1"; }
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

api_port=29217
ingress_port=28180
registry_port=25150

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
    home=$(run_get home)
    case $home in
        /tmp/rbpc.*) ;;
        *) fail "refusing to use RELIABURGER_HOME=$home: this harness only adopts a home it made" ;;
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
want='soak-writer=1,soak-redis=1,soak-redis-client=1'

apply_workloads() {
    rel apply -f "$workloads" > "$evidence/logs/apply.log" 2>&1 || fail "applying $workloads failed"
    local deadline=$(( $(now) + 600 ))
    until rel --output json apps --instances > "$evidence/logs/instances.json" 2> /dev/null \
        && endurance instances "$evidence/logs/instances.json" --want "$want" > /dev/null; do
        [ "$(now)" -lt "$deadline" ] || fail 'the workloads were not all running after 600 s'
        sleep 10
    done
    # The first ACK and INCR before the first cut, so I1 and I2 have something to hold.
    deadline=$(( $(now) + 120 ))
    until [ "$(acked_now writer)" -gt 0 ] && [ "$(acked_now redis)" -gt 0 ]; do
        [ "$(now)" -lt "$deadline" ] || fail 'the writer or the Redis client acknowledged nothing in 120 s'
        sleep 5
    done
    # Each start's resume point so far, so the first cut can tell a new one.
    read_resumes writer "$evidence/logs/resumes-writer.txt"
    read_resumes redis "$evidence/logs/resumes-redis.txt"
    run_set writer_starts "$(starts_in "$evidence/logs/resumes-writer.txt")"
    run_set redis_starts "$(starts_in "$evidence/logs/resumes-redis.txt")"
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
starts_in() {
    endurance resumed "$1" --acked 0 | python3 -c 'import json,sys; print(json.load(sys.stdin).get("starts", ""))' || true
}

# The highest ACK (writer) or INCR (redis) in the app's recent log.
acked_now() {
    local app=soak-writer
    [ "$1" = writer ] || app=soak-redis-client
    rel logs "$app" --tail 50 > "$evidence/logs/acked-$1.txt" 2>&1 || true
    endurance acked "$1" "$evidence/logs/acked-$1.txt"
}

# --- power (sustained/power.sh) ---------------------------------------------------

# shellcheck source=sustained/power.sh
. "$repository/scripts/release/sustained/power.sh"
power_log_dir=$evidence/logs
power_before_off() { :; }
power_after_on() { :; }
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

# Power on, with Lima's one known VZ start retry ("storage device attachment
# is invalid" after a forced stop), counted as an environment event.
power_back() {
    start_retries=0
    if ! power_on 1; then
        start_retries=1
        "$limactl" stop --force "${vm[1]}" > /dev/null 2>&1 || true
        power_on 1 || return 1
    fi
}

verify() {
    local writer_acked=$1 redis_acked=$2 previous starts
    rel exec soak-writer awk '$0 != NR && bad == "" { bad = NR ": " $0 } { if (count[$0]++ == 1) { dups++; if (first_dup == "") first_dup = $0 } if ($0 + 0 > max) max = $0 + 0 } END { if (bad != "") print "BAD " bad; missing = 0; for (i = 1; i <= max; i++) if (!(i in count)) { if (!missing) first_missing = i; missing++ } if (dups) print "DUP " dups " first " first_dup; if (missing) print "MISSING " missing " first " first_missing; print "MAX " max + 0; print "LAST " NR }' /data/seq \
        > "$cut_dir/seq.txt" 2>&1 || true
    read_resumes writer "$cut_dir/resumes-writer.txt"
    starts=$(run_get writer_starts)
    endurance seq "$cut_dir/seq.txt" --acked "$writer_acked" --resumes "$cut_dir/resumes-writer.txt" \
        ${starts:+--starts "$starts"} > "$cut_dir/I1.json" || true
    starts=$(starts_in "$cut_dir/resumes-writer.txt")
    [ -z "$starts" ] || run_set writer_starts "$starts"
    rel exec soak-redis redis-cli GET soak:counter > "$cut_dir/counter.txt" 2>&1 || true
    read_resumes redis "$cut_dir/resumes-redis.txt"
    previous=$(run_get counter)
    starts=$(run_get redis_starts)
    endurance counter "$cut_dir/counter.txt" --acked "$redis_acked" ${previous:+--previous "$previous"} \
        --resumes "$cut_dir/resumes-redis.txt" ${starts:+--starts "$starts"} > "$cut_dir/I2.json" || true
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
    cut_dir=$evidence/cuts/$(printf '%05d' "$cut")
    mkdir -p "$cut_dir"
    hold=$(endurance hold --seed "$seed" --cut "$cut" --what work --min "$work_min" --max "$work_max")
    off=$(endurance hold --seed "$seed" --cut "$cut" --what off --min "$off_min" --max "$off_max")
    say "cut $cut: working for $hold s"
    sleep "$hold"
    writer_acked=$(acked_now writer)
    redis_acked=$(acked_now redis)
    cut_at=$(now)
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
    fi
    verify "$writer_acked" "$redis_acked"
    local detail
    detail=$(python3 -c '
import json, sys
def number(text):
    return int(text) if text.strip() else None
print(json.dumps({"topology": "1", "kind": "site", "cut_at": int(sys.argv[1]), "work_seconds": int(sys.argv[2]),
    "off_seconds": int(sys.argv[3]), "acked": {"writer": int(sys.argv[4]), "redis": int(sys.argv[5])},
    "recovery": {"api_seconds": number(sys.argv[6]), "settled_seconds": number(sys.argv[7]),
                 "start_retries": int(sys.argv[8])},
    "cycle_seconds": int(sys.argv[9])}))' \
        "$cut_at" "$hold" "$off" "$writer_acked" "$redis_acked" "${api_seconds:-}" "${settled_seconds:-}" \
        "$start_retries" "$(( $(now) - cut_at + hold ))")
    endurance record --evidence "$evidence" --cut "$cut" --detail "$detail" > "$cut_dir/verdict.json" || result=$?
    say "cut $cut: Bun back in ${api_seconds:-never} s, settled in ${settled_seconds:-never} s; $(cat "$cut_dir/verdict.json")"
    return "$result"
}

# --- the run ---------------------------------------------------------------------

cleanup() {
    local status=$?
    [ "${stopping:-}" = keep-off ] && return "$status"
    if [ "$keep" = false ] && [ -n "${relish:-}" ] && [ "${finished:-false}" = true ]; then
        "$relish" local destroy --yes > "$evidence/logs/teardown.log" 2>&1 \
            && "$relish" uninstall --yes >> "$evidence/logs/teardown.log" 2>&1 \
            && rm -rf "$home" || say "teardown failed; see $evidence/logs/teardown.log"
    fi
    return "$status"
}
trap cleanup EXIT

if [ "$resume" = true ]; then
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
    "$relish" local start > "$evidence/logs/resume-start.log" 2>&1 || fail "relish local start failed on resume"
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
    run_set host "$(sw_vers -productName 2> /dev/null || uname -s) $(sw_vers -productVersion 2> /dev/null || uname -r), $(sysctl -n machdep.cpu.brand_string 2> /dev/null || uname -m)"
    install_cluster
    discover
    run_set hypervisor "lima $("$limactl" --version | awk '{print $NF}') $("$limactl" list --format '{{.VMType}}' "${vm[1]}")"
    run_set relish "$("$relish" --version)"
    run_set bun "$(gsh '/usr/local/bin/bun --version 2>/dev/null || bun --version 2>/dev/null' | tail -n 1)"
    say "applying the workloads"
    apply_workloads
fi
say "evidence: $evidence; seed $seed; $(( duration_seconds / 60 )) minutes of cuts"

cuts_done() { [ -f "$evidence/cuts.jsonl" ] && grep -c . "$evidence/cuts.jsonl" || echo 0; }
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
        "$relish" local stop --yes > "$evidence/logs/pause-stop.log" 2>&1 || true
        run_set paused_at "$(now)"
        stopping=keep-off
        exit 75
    fi
    cut=$(( $(cuts_done) + 1 ))
    if ! one_cut "$cut"; then
        say "cut $cut violated an invariant; powering the VM off and keeping everything for diagnosis"
        "$limactl" stop --force "${vm[1]}" > /dev/null 2>&1 || true
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
