#!/bin/bash
# ssh.sh [args]: SSH into the lab server (through the slirp port forward).
# For a node: ssh -F work/ssh_config root@192.168.105.10<n>.
. "$(dirname "$0")/lab.env"; exec $SSH "$@"
