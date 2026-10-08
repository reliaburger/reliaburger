#!/usr/bin/env bash
#
# The homepage's netboot demo, run for real: three machines with blank disks
# network-boot from `relish netboot`, install the appliance, and become one
# cluster with `relish machines claim`.
#
# The machines are QEMU VMs with OVMF firmware, booting over PXE on a bridge
# that plays the LAN, as image/tests/relish-netboot-install.sh does. A network
# namespace on it plays the home router: its dnsmasq hands out the addresses,
# with a reservation per machine, and knows nothing about booting. The
# recording shows the operator's side only: relish's commands and log.
#
# Usage:
#   scripts/demo/netboot.sh [--record CAST] <artefact dir> <version>
#
#   <artefact dir> is a lab build's appliance-x86_64 artefact (the image,
#   installer, iPXE and SHA256SUMS signed by the run's throwaway key), and
#   <version> its IMAGE_VERSION. With --record, asciinema records the run into
#   CAST (110x32, idle time cut to 2 s); every wait says how long it really
#   took.
#
# Needs Linux x86_64 with /dev/kvm, password-less sudo, qemu-system-x86, OVMF
# (/usr/share/OVMF), dnsmasq, zstd, curl, `relish` on root's PATH as well as
# ours (/usr/local/bin), and asciinema 3 for --record.
# .github/workflows/appliance.yml runs it on a pull request labelled
# record-netboot-demo and uploads the recording.

# wait_for and trap call functions by name.
# shellcheck disable=SC2329
set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
IDLE_LIMIT=2
# The operator's home in the recording, so paths read as a person's would.
# It must not exist yet: the script makes it, and removes it at the end.
DEMO_HOME=/home/operator
MACS=(52:54:00:00:00:01 52:54:00:00:00:02 52:54:00:00:00:03)
# The router reserves these, but nothing below relies on it: relish machines finds them.
IPS=(10.42.0.11 10.42.0.12 10.42.0.13)

CAST=""
if [[ "${1:-}" == "--record" ]]; then
    CAST=${2:?usage: netboot.sh [--record CAST] <artefact dir> <version>}
    CAST="$(cd "$(dirname "${CAST}")" && pwd)/$(basename "${CAST}")"
    shift 2
fi

DIM=$'\033[2m'
BOLD=$'\033[1m'
GREEN=$'\033[32m'
RESET=$'\033[0m'

# A comment line, the way a person would narrate at a shell prompt.
say() {
    printf '%s# %s%s\n' "${DIM}" "$*" "${RESET}"
}

# Type a command at a prompt, a character at a time. A command may span
# lines, continued with a backslash, as a person would type a long one.
type_command() {
    local text="$1" i
    printf '%s$%s ' "${GREEN}${BOLD}" "${RESET}"
    sleep 0.6
    for ((i = 0; i < ${#text}; i++)); do
        printf '%s' "${text:i:1}"
        sleep "0.0$((RANDOM % 5 + 2))"
    done
    sleep 0.5
    printf '\n'
}

# Type a command, then run exactly what was typed.
show() {
    type_command "$1"
    eval "$1" || true
    printf '\n'
    sleep 1.5
}

# Poll a check until it succeeds, then say how long it took.
# $1: what we're waiting for, $2: timeout in seconds, $3...: the check.
wait_for() {
    local what="$1" limit="$2" start elapsed
    shift 2
    start=$(date +%s)
    say "waiting for ${what}"
    until "$@"; do
        elapsed=$(( $(date +%s) - start ))
        if [[ "${elapsed}" -ge "${limit}" ]]; then
            say "gave up after ${elapsed} s"
            exit 1
        fi
        sleep 2
    done
    elapsed=$(( $(date +%s) - start ))
    say "done after ${elapsed} s"
}

# The scene: what the recording shows. It runs in ${DEMO_HOME}.
scene() {
    export HOME="${DEMO_HOME}"
    cd "${HOME}"
    local netboot started
    netboot="sudo relish netboot os --interface lan0 --key lab.pem \\
    --mac ${MACS[0]} --mac ${MACS[1]} --mac ${MACS[2]} &"

    say "Three machines on one switch (lan0), disks blank, network boot on in their firmware."
    say "The router hands out their addresses. relish adds only the boot part."
    say "os/ holds a lab build of the OS, signed with this run's own key, lab.pem."
    say "From a release, relish image download --dir os fetches it, checked against the release key."
    sleep 1
    type_command "${netboot}"
    # The log goes to the screen and to a file the waits below read.
    eval "${netboot% &} > >(tee netboot.log) 2>&1 &"
    wait_for "relish to start serving" 120 grep -q "serving on lan0" netboot.log
    say "Power on the three machines (three QEMU VMs here)."
    started=$(date +%s)
    power_on
    wait_for "all three to install" 600 installed 3
    say "Each one reboots into Reliaburger, unclaimed, its claim key on its screen."
    wait_for "relish machines to hear all three" 600 listed
    show "relish machines"
    say "--trust-lan: this switch is ours, so skip comparing each claim key on the machine's screen."
    say "--yes: skip the master-key backup prompt. Back up ~/lab/secrets for real clusters."
    show "relish machines claim ~/lab --create --name lab --trust-lan --yes \\
    --operator 10.42.0.1 --network 10.42.0.0/24 ${MACS[0]} ${MACS[1]} ${MACS[2]}"
    wait_for "three nodes alive" 600 alive 3
    show "relish nodes"
    wait_for "the council to reach all three" 300 council_reaches 3
    show "relish council"
    say "From power-on to a three-node cluster in $(( $(date +%s) - started )) s, no USB stick, no OS install."
    sleep 3
}

# The checks below run by name, through wait_for.
installed() {
    [[ "$(grep -c "has the installer" netboot.log 2>/dev/null)" -ge "$1" ]]
}

# All three machines announce themselves as unclaimed over mDNS.
listed() {
    local list mac
    list=$(relish machines --wait 3 2>/dev/null) || return 1
    for mac in "${MACS[@]}"; do
        grep -q "${mac}" <<<"${list}" || return 1
    done
}

alive() {
    [[ "$(relish nodes 2>/dev/null | grep -c alive)" -ge "$1" ]]
}

# `relish council` reaches $1 nodes: its last column, REACHABLE, says yes.
council_reaches() {
    [[ "$(relish council 2>/dev/null | grep -cE ' yes *$')" -ge "$1" ]]
}

# Start the three VMs, quietly: the recording is about the operator's side.
power_on() {
    local i
    for i in 0 1 2; do
        qemu-system-x86_64 -machine q35,accel=kvm -cpu host -smp 2 -m 2048 \
            -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
            -drive if=pflash,format=raw,file="${DEMO_WORK}/vars$i.fd" \
            -drive if=none,id=d0,format=raw,file="${DEMO_WORK}/disk$i.raw" \
            -device virtio-blk-pci,drive=d0,bootindex=1 \
            -netdev "tap,id=n0,ifname=rbdemo$i,script=no,downscript=no" \
            -device "virtio-net-pci,netdev=n0,mac=${MACS[$i]},bootindex=2" \
            -display none -serial "file:${DEMO_WORK}/node$i.log" \
            -pidfile "${DEMO_WORK}/node$i.pid" -daemonize
    done
}

if [[ "${1:-}" == "--scene" ]]; then
    scene
    exit 0
fi

artefacts=$(cd "${1:?usage: netboot.sh [--record CAST] <artefact dir> <version>}" && pwd)
version=${2:?usage: netboot.sh [--record CAST] <artefact dir> <version>}
[[ -f "${artefacts}/reliaburger-os_${version}.raw.zst" ]] || {
    echo "no ${version} image in ${artefacts}" >&2
    exit 1
}
command -v relish >/dev/null || { echo "relish isn't on PATH" >&2; exit 1; }
sudo -n sh -c 'command -v relish' >/dev/null || { echo "relish isn't on root's PATH" >&2; exit 1; }
[[ -z "${CAST}" ]] || command -v asciinema >/dev/null || { echo "asciinema isn't installed" >&2; exit 1; }
[[ ! -e "${DEMO_HOME}" ]] || { echo "${DEMO_HOME} exists already; the demo makes it and removes it" >&2; exit 1; }
# shellcheck source=image/tests/disk.sh
. "${REPO_DIR}/image/tests/disk.sh"

DEMO_WORK=$(mktemp -d)
export DEMO_WORK
cleanup() {
    local i
    for i in 0 1 2; do
        [[ -f "${DEMO_WORK}/node$i.pid" ]] && kill "$(cat "${DEMO_WORK}/node$i.pid")" 2>/dev/null
        sudo ip link del "rbdemo$i" 2>/dev/null || true
    done
    sudo pkill -f "relish netboot os --interface lan0" 2>/dev/null || true
    sudo ip netns pids rbdemo 2>/dev/null | xargs -r sudo kill 2>/dev/null || true
    sudo ip netns del rbdemo 2>/dev/null || true
    sudo ip link del rbdemoveth0 2>/dev/null || true
    sudo ip link del lan0 2>/dev/null || true
    sudo rm -rf "${DEMO_HOME}"
}
trap cleanup EXIT

# The LAN: a bridge, and a router in a namespace on it with a reservation
# per machine. The operator's machine (this one) is 10.42.0.1.
sudo ip link add lan0 type bridge
sudo ip addr add 10.42.0.1/24 dev lan0
sudo ip link set lan0 up
sudo ip netns add rbdemo
sudo ip link add rbdemoveth0 type veth peer name rbdemoveth1
sudo ip link set rbdemoveth1 netns rbdemo
sudo ip link set rbdemoveth0 master lan0 up
sudo ip -n rbdemo addr add 10.42.0.2/24 dev rbdemoveth1
sudo ip -n rbdemo link set rbdemoveth1 up
sudo ip netns exec rbdemo dnsmasq --interface=rbdemoveth1 --bind-interfaces --port=0 \
    --dhcp-range=10.42.0.100,10.42.0.200,12h --dhcp-option=3,10.42.0.1 \
    --dhcp-host="${MACS[0]},${IPS[0]}" --dhcp-host="${MACS[1]},${IPS[1]}" \
    --dhcp-host="${MACS[2]},${IPS[2]}" \
    --dhcp-leasefile="${DEMO_WORK}/dnsmasq.leases" \
    --pid-file="${DEMO_WORK}/dnsmasq.pid" --log-dhcp --log-facility="${DEMO_WORK}/dnsmasq.log"

# What `relish image download --dir os` would have saved, from the lab build.
sudo mkdir -p "${DEMO_HOME}"
sudo chown "$(id -un)" "${DEMO_HOME}"
mkdir -p "${DEMO_HOME}/os"
ln -s "${artefacts}" "${DEMO_HOME}/os/x86_64"
cp "${artefacts}/spike-signing-key.pub.pem" "${DEMO_HOME}/lab.pem"

for i in 0 1 2; do
    sudo ip tuntap add "rbdemo$i" mode tap user "$(id -un)"
    sudo ip link set "rbdemo$i" master lan0 up
    blank_disk "${DEMO_WORK}/disk$i.raw" "${artefacts}/reliaburger-os_${version}.raw.zst"
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "${DEMO_WORK}/vars$i.fd"
done

status=0
if [[ -n "${CAST}" ]]; then
    asciinema rec --headless --overwrite --return \
        --window-size 110x32 --idle-time-limit "${IDLE_LIMIT}" \
        --title "Reliaburger: three machines netboot into a cluster" \
        -c "$0 --scene" "${CAST}" || status=$?
    # The recorder's absolute path and shell say nothing about the demo.
    python3 - "${CAST}" <<'PYTHON'
import json, sys
path = sys.argv[1]
lines = open(path, encoding="utf-8").read().splitlines()
header = json.loads(lines[0])
header.pop("command", None)
header.pop("env", None)
with open(path, "w", encoding="utf-8") as cast:
    cast.write(json.dumps(header) + "\n")
    cast.write("\n".join(lines[1:]) + "\n")
PYTHON
else
    "$0" --scene || status=$?
fi
if [[ "${status}" -ne 0 ]]; then
    for i in 0 1 2; do
        echo "--- machine $((i + 1)) console ---"
        cat "${DEMO_WORK}/node$i.log" 2>/dev/null || true
    done
    echo "--- relish netboot ---"
    cat "${DEMO_HOME}/netboot.log" 2>/dev/null || true
    echo "--- relish machines ---"
    relish machines --wait 5 2>&1 || true
    echo "--- the router's dnsmasq ---"
    sudo cat "${DEMO_WORK}/dnsmasq.log" 2>/dev/null || true
fi
exit "${status}"
