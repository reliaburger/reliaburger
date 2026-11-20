#!/bin/bash
# os-update.sh <ip> <version> [arm64|x86_64]: stage an OS version on a lab
# node the way bun will (the image's os-stage: signed SHA256SUMS, verified
# downloads, systemd-sysupdate), reboot it, and wait until it's back on that
# version with boot-complete.target reached, which is when systemd-bless-boot
# marks the new UKI good. stage-artefacts.sh --update must have put the
# version on the server. NOWAIT=1 stages and reboots without waiting, for
# the broken build, which should never come back blessed.
set -euo pipefail; . "$(dirname "$0")/lab.env"
ip=${1:?usage: os-update.sh <ip> <version> [arm64|x86_64]}; v=${2:?version}; arch=${3:-arm64}
node() { ssh -F "$WORK/ssh_config" "$@"; }
node -o ConnectTimeout=20 root@"$ip" "set -e
curl -fsS -o /run/os.pem http://192.168.105.2:8080/$arch/os-$v.pub.pem
/usr/lib/reliaburger/os-stage http://192.168.105.2:8080/$arch $v /run/os.pem >/run/os-stage.log 2>&1 || { tail -5 /run/os-stage.log; exit 1; }
# Images older than the Mode=0644 fix write the new UKI read-only, and a
# read-only file on the FAT ESP stops systemd-boot counting its tries.
chmod 0644 /boot/EFI/Linux/reliaburger-os_${v}+*.efi
ls /boot/EFI/Linux"
# The reboot drops the SSH connection; that's expected.
node -o ConnectTimeout=20 root@"$ip" 'systemctl reboot' 2>/dev/null || true
[ "${NOWAIT:-}" = 1 ] && { echo "$ip rebooting into $v"; exit 0; }
start=$(date +%s)
sleep 20
until node -o ConnectTimeout=5 root@"$ip" "systemctl is-active -q boot-complete.target && . /usr/lib/os-release && [ \"\$IMAGE_VERSION\" = $v ] && ls /boot/EFI/Linux" 2>/dev/null; do
    [ $(( $(date +%s) - start )) -lt 600 ] || { echo "$ip isn't back on $v and blessed after 600 s" >&2; exit 1; }
    sleep 5
done
echo "$ip back on $v, blessed, after $(( $(date +%s) - start )) s"
