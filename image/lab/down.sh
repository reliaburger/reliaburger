#!/bin/bash
# down.sh: stop the lab. Nodes and virtual Wyses are killed (they hold nothing
# the lab needs across restarts that their disks don't); the server shuts
# down cleanly; lab SSH tunnels close.
. "$(dirname "$0")/lab.env"; cd "$WORK"
for p in rb*.pid wyse*.pid; do [ -f "$p" ] && kill "$(cat "$p")" 2>/dev/null; rm -f "$p"; done
"$LAB/ssh.sh" -o ConnectTimeout=3 'sudo systemctl poweroff' 2>/dev/null
for i in $(seq 1 30); do kill -0 "$(cat server.pid 2>/dev/null)" 2>/dev/null || break; sleep 1; done
kill "$(cat server.pid 2>/dev/null)" 2>/dev/null; rm -f server.pid
pkill -f "ssh -F $WORK/ssh_config" 2>/dev/null
pgrep -fl "qemu-system-.* -name (l2lab|rb|wyse)" || echo "all lab VMs down"
