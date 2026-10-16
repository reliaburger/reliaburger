#!/usr/bin/env bash
# Guest memory evidence for the V02 soak (#985), run as root inside a node:
# /proc/meminfo, the processes holding the most memory, Lima's guest agent
# (whose leaked sockets got Bun OOM-killed in #982) and the kernel's OOM
# reports with their per-process RSS tables.
#
# Usage: guest-memory.sh cursor|recent
#   cursor   kernel messages since the last `cursor` run (heavy checks)
#   recent   kernel messages from the last 30 minutes (failure bundles)
# SOAK_PROC replaces /proc, for the tests.
set -u
proc=${SOAK_PROC:-/proc}
case ${1:-recent} in
    cursor) window=(--cursor-file=/var/tmp/soak-kernel.cursor) ;;
    recent) window=(--since=-30min) ;;
    *) echo 'usage: guest-memory.sh cursor|recent' >&2; exit 2 ;;
esac

echo '## meminfo'
cat "$proc/meminfo"
echo '## processes by RSS (KiB)'
ps -eo pid,rss,comm --sort=-rss | head -n 16
echo '## lima-guestagent'
pid=$(systemctl show -p MainPID --value lima-guestagent.service 2>/dev/null)
if [ "${pid:-0}" -gt 0 ] 2>/dev/null && [ -d "$proc/$pid" ]; then
    echo "lima_guestagent_rss_kb $(awk '/^VmRSS/ {print $2}' "$proc/$pid/status")"
    echo "lima_guestagent_fd $(find "$proc/$pid/fd" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')"
else
    echo 'lima_guestagent not running'
fi
echo '## kernel OOM reports'
# From "invoked oom-killer" to "Killed process": the trigger, the memory
# state and the per-process table. A cgroup OOM (the memory chaos fault caps
# a cgroup on purpose) has the same shape. Bounded, in case of a storm.
journalctl -k --no-pager -q "${window[@]}" 2>/dev/null | awk '
    /invoked oom-killer/ { inside = 1 }
    inside || /Out of memory|oom-kill:|oom_reaper/ { print; lines++ }
    /Killed process/ { inside = 0 }
    lines >= 600 { print "(truncated)"; exit }'
exit 0
