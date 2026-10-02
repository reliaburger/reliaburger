#!/bin/bash
# A netboot server for appliance installs on a LAN (preview; `relish
# netboot` replaces it, and the QEMU lab moves over in W7). Run it as root
# on any Linux machine on the same network as the machines to install:
#
#   netboot-server.sh <artefacts> <interface> [port]
#
# <artefacts> holds one or both of the appliance-x86_64 and appliance-aarch64
# CI artefacts, unpacked into x86_64/ and aarch64/ (gh run download -n ...).
# It never hands out addresses: dnsmasq answers PXE requests as a ProxyDHCP,
# next to your router's DHCP, and serves our iPXE and boot.ipxe over TFTP;
# Python serves the installer and the signed disk image over HTTP on [port]
# (8080). Ctrl-C stops both. Nothing it serves is secret.
#
# Needs dnsmasq and python3 (apt install dnsmasq-base python3).
set -euo pipefail

art=${1:?usage: netboot-server.sh <artefacts> <interface> [port]}
iface=${2:?usage: netboot-server.sh <artefacts> <interface> [port]}
port=${3:-8080}
[ "$(id -u)" = 0 ] || { echo "run as root: DHCP and TFTP use ports 67, 69 and 4011" >&2; exit 1; }
command -v dnsmasq >/dev/null || { echo "install dnsmasq first" >&2; exit 1; }
address=$(ip -4 -o addr show dev "$iface" | awk '{ print $4 }' | head -n 1)
[ -n "$address" ] || { echo "$iface has no IPv4 address" >&2; exit 1; }
ip=${address%/*}
network=$(python3 -c "import ipaddress, sys; print(ipaddress.ip_interface(sys.argv[1]).network.network_address)" "$address")

root=$(mktemp -d /tmp/reliaburger-netboot.XXXXXX)
trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$root"' EXIT
mkdir -p "$root/tftp" "$root/http"
# dnsmasq serves TFTP as an unprivileged user.
chmod 0755 "$root" "$root/tftp"
cp "$(dirname "$0")/../netboot/boot.ipxe" "$root/tftp/boot.ipxe"
served=
for arch in x86_64 aarch64; do
    src=$art/$arch
    [ -d "$src" ] || continue
    ipxe=${arch/aarch64/arm64}
    version=$(ls "$src" | sed -n 's/^reliaburger-os-installer_\(.*\)\.efi$/\1/p' | sort -V | tail -n 1)
    [ -n "$version" ] || { echo "$src has no installer UKI" >&2; exit 1; }
    # snp.efi uses the firmware's own NIC driver, the safer default.
    cp "$src/netboot/ipxe-snp-$ipxe.efi" "$root/tftp/ipxe-$ipxe.efi"
    mkdir -p "$root/http/$ipxe"
    ln -s "$(cd "$src" && pwd)/reliaburger-os-installer_$version.efi" "$root/http/$ipxe/installer.efi"
    for file in "reliaburger-os_$version.raw.zst" "reliaburger-os_$version.SHA256SUMS" "reliaburger-os_$version.SHA256SUMS.sig"; do
        ln -s "$(cd "$src" && pwd)/$file" "$root/http/$ipxe/$file"
    done
    served="$served $arch:$version"
done
[ -n "$served" ] || { echo "$art has neither x86_64/ nor aarch64/" >&2; exit 1; }
chmod 0644 "$root"/tftp/*
# boot.ipxe asks for port 8080; follow a different one.
sed -i "s|:8080/|:$port/|" "$root/tftp/boot.ipxe"

cat > "$root/dnsmasq.conf" <<CONF
# ProxyDHCP only: port=0 turns DNS off, and the proxy range hands out no
# addresses. Your router keeps doing DHCP.
port=0
interface=$iface
bind-interfaces
dhcp-range=$network,proxy
enable-tftp
tftp-root=$root/tftp
# iPXE announces itself; send it the boot script, never iPXE again.
dhcp-userclass=set:ipxe,iPXE
pxe-prompt="Reliaburger",0
# x86-64 UEFI shows up as architecture 7 or 9 depending on the firmware.
pxe-service=tag:!ipxe,X86-64_EFI,"Reliaburger iPXE",ipxe-x86_64.efi
pxe-service=tag:!ipxe,BC_EFI,"Reliaburger iPXE",ipxe-x86_64.efi
pxe-service=tag:!ipxe,ARM64_EFI,"Reliaburger iPXE",ipxe-arm64.efi
pxe-service=tag:ipxe,X86-64_EFI,"Reliaburger script",boot.ipxe
pxe-service=tag:ipxe,BC_EFI,"Reliaburger script",boot.ipxe
pxe-service=tag:ipxe,ARM64_EFI,"Reliaburger script",boot.ipxe
log-dhcp
log-facility=-
CONF

echo "serving$served on $ip ($iface): ProxyDHCP and TFTP, HTTP on :$port"
(cd "$root/http" && exec python3 -m http.server --bind "$ip" "$port") &
exec_dnsmasq() { dnsmasq --keep-in-foreground --conf-file="$root/dnsmasq.conf" --pid-file="$root/dnsmasq.pid"; }
exec_dnsmasq
