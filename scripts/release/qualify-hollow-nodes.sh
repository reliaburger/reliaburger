#!/usr/bin/env bash
# Hollow-node measurement (#1401): a real three-member council in one process
# in front of N hollow nodes in another (real gossip, Raft endpoint,
# reporting, agent and placement reconciler; a runtime that runs nothing), on
# this one machine. Each size runs alone, one after another, and writes
# <out>/hollow-<N>.json; a Markdown table of every size follows. The numbers
# are recorded, never gated: write them into a dated record in
# docs/qualification/.
#
# Usage:
#   scripts/release/qualify-hollow-nodes.sh [--sizes 100,500,1000] [--out DIR]
#       [--steady SECS] [--churn FRACTION] [--max-load L]
#
#   --sizes     hollow nodes per run, comma-separated (default 100,500,1000)
#   --out       where the JSON reports go (default target/hollow-nodes)
#   --steady    seconds to watch each fleet with every replica running
#               (default 60)
#   --churn     share of the fleet to stop and restart (default 0.1)
#   --max-load  don't start a size while the one-minute load average is over
#               this, and stop a run that goes over it (default 40). The
#               host is usually shared; a busy host measures itself
#
# The harness builds with the `hollow` Cargo profile: optimised, like a
# release, but with more codegen units, and never shipped.
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
fail() { printf 'qualify-hollow-nodes: %s\n' "$*" >&2; exit 1; }

sizes=100,500,1000
out=target/hollow-nodes
steady=60
churn=0.1
max_load=40
while [ "$#" -gt 0 ]; do
    case $1 in
        --sizes) sizes=${2:?}; shift 2 ;;
        --out) out=${2:?}; shift 2 ;;
        --steady) steady=${2:?}; shift 2 ;;
        --churn) churn=${2:?}; shift 2 ;;
        --max-load) max_load=${2:?}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown option $1" ;;
    esac
done

load_average() {
    if [ -r /proc/loadavg ]; then
        cut -d' ' -f1 /proc/loadavg
    else
        sysctl -n vm.loadavg | tr -d '{}' | awk '{print $1}'
    fi
}

# Wait up to ten minutes for the host to calm down before a size starts.
wait_for_quiet_host() {
    local waited=0
    while awk -v load="$(load_average)" -v max="$max_load" 'BEGIN { exit !(load > max) }'; do
        [ "$waited" -ge 600 ] && fail "load average stayed over $max_load for ten minutes"
        printf 'qualify-hollow-nodes: load %s is over %s; waiting\n' "$(load_average)" "$max_load" >&2
        sleep 30
        waited=$((waited + 30))
    done
}

mkdir -p "$out"
cargo test --profile hollow --test hollow_nodes --no-run

IFS=, read -r -a fleet_sizes <<< "$sizes"
for size in "${fleet_sizes[@]}"; do
    wait_for_quiet_host
    printf 'qualify-hollow-nodes: %s hollow nodes\n' "$size" >&2
    RELIABURGER_HOLLOW_NODES=$size \
    RELIABURGER_HOLLOW_STEADY_SECS=$steady \
    RELIABURGER_HOLLOW_CHURN=$churn \
    RELIABURGER_HOLLOW_MAX_LOAD=$max_load \
    RELIABURGER_HOLLOW_REPORT=$out/hollow-$size.json \
    RELIABURGER_HOLLOW_COUNCIL_LOG=$out/council-$size.log \
        cargo test --profile hollow --test hollow_nodes -- \
            --ignored --exact hollow_fleet_measurement --nocapture \
        > "$out/fleet-$size.log" 2>&1 \
        || fail "the $size-node run failed; see $out/fleet-$size.log"
    if grep -q '"stopped": "' "$out/hollow-$size.json"; then
        printf 'qualify-hollow-nodes: the %s-node run stopped early; not trying bigger fleets\n' \
            "$size" >&2
        break
    fi
done

"$(dirname "$0")/hollow_summary.py" "$out"
