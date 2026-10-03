#!/bin/bash
# Sample every node of a fleet over SSH, for the spike's 24-hour
# measurements on real hardware (research §9.7, step 6): memory, zram, bun's
# RSS, bytes written to the system disk, disk use, load and temperature.
# Needs root SSH on the nodes, so their seeds must carry a key
# (`relish machines claim --ssh-key`, or seed-fleet.sh init --ssh-key).
#
#   fleet-measure.sh [--relish] <dir> [interval seconds] [samples]
#
# <dir> is where the fleet came from, and where the samples go:
#   - a claim directory (`relish machines claim`): its fleet.json names the
#     nodes and their addresses;
#   - a seed-fleet.sh directory: its "fleet" file, nodes node-01, node-02, …
# With --relish, the nodes come from `relish nodes --output json` instead
# (RELISH names the binary), and <dir> only holds the samples.
# fleet-nodes.py does the reading, again before every sample, so machines
# claimed during the run join it.
# Defaults: every 300 s, forever (Ctrl-C stops). Each node gets
# <dir>/measure/<node>.csv, one row per sample; a node that doesn't answer
# gets a row of its timestamp and nothing else, so gaps stay visible.
set -euo pipefail

usage="usage: fleet-measure.sh [--relish] <dir> [interval seconds] [samples]"
from_relish=
if [ "${1:-}" = --relish ]; then
    from_relish=1
    shift
fi
dir=${1:?$usage}
interval=${2:-300}
samples=${3:-0}
fleet_nodes=$(dirname "$0")/fleet-nodes.py
nodes() {
    if [ -n "$from_relish" ]; then
        "${RELISH:-relish}" nodes --output json | python3 "$fleet_nodes" --relish-json -
    else
        python3 "$fleet_nodes" "$dir"
    fi
}
nodes > /dev/null    # fail now, not at the first sample
out=$dir/measure
mkdir -p "$out"
ssh_opts=(-o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new ${SSH_OPTS:-})

header=time,mem_total_kb,mem_available_kb,swap_used_kb,zram_orig_bytes,zram_compr_bytes,bun_rss_kb,disk,disk_written_bytes,data_used_bytes,data_size_bytes,load1,temp_max_mc,os_version,bun_version

# Runs on the node; prints one CSV row without the time.
probe='
set -u
m() { awk -v k="$1:" "\$1 == k { print \$2 }" /proc/meminfo; }
swap_used=$(( $(m SwapTotal) - $(m SwapFree) ))
zo=; zc=
[ -r /sys/block/zram0/mm_stat ] && read -r zo zc _ < /sys/block/zram0/mm_stat
pid=$(systemctl show -p MainPID --value reliaburger 2>/dev/null || echo 0)
rss=; [ "$pid" != 0 ] && rss=$(awk "/^VmRSS:/ { print \$2 }" /proc/$pid/status 2>/dev/null)
disk=$(lsblk -no pkname "$(findmnt -no source /)" 2>/dev/null | head -n 1)
written=; [ -n "$disk" ] && written=$(awk "{ print \$7 * 512 }" /sys/block/$disk/stat)
set -- $(df -B1 --output=used,size / | tail -n 1)
temp=$(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | sort -n | tail -n 1)
. /usr/lib/os-release
bun=$(/var/lib/reliaburger/bin/bun --version 2>/dev/null | awk "{ print \$2 }")
echo "$(m MemTotal),$(m MemAvailable),$swap_used,$zo,$zc,$rss,$disk,$written,$1,$2,$(cut -d" " -f1 /proc/loadavg),$temp,${IMAGE_VERSION:-},$bun"
'

taken=0
while :; do
    now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    while read -r node ip; do
        csv=$out/$node.csv
        [ -s "$csv" ] || echo "$header" > "$csv"
        if row=$(ssh "${ssh_opts[@]}" "root@$ip" "$probe" < /dev/null 2>/dev/null); then
            echo "$now,$row" >> "$csv"
        else
            echo "$now" >> "$csv"
            echo "$now $node ($ip): no answer" >&2
        fi
    done < <(nodes)
    taken=$((taken + 1))
    [ "$samples" -gt 0 ] && [ "$taken" -ge "$samples" ] && break
    sleep "$interval"
done
echo "samples in $out/"
