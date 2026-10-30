#!/bin/bash
# stage-artefacts.sh <run-id> <aarch64|x86_64> <version> [--update]: put one
# CI build (appliance.yml's appliance-<arch> artefact) on the netboot server.
#   TFTP:         ipxe-<arch>.efi (iPXE's snp build, the firmware's NIC drivers,
#                 when the artefact has one; IPXE=full serves ipxe.efi) and
#                 boot.ipxe
#   HTTP /<arch>: installer.efi, the disk image, SHA256SUMS and its signature
#   --update:     also the UKI, both /usr images and this build's throwaway
#                 key as os-<version>.pub.pem, for os-update.sh
# It builds a fresh directory under work/srv each time rather than clearing
# an old one, and adds to what the server already has.
set -euo pipefail; . "$(dirname "$0")/lab.env"
run=${1:?usage: stage-artefacts.sh <run-id> <aarch64|x86_64> <version> [--update]}
arch=${2:?arch}; v=${3:?version}; update=${4:-}
case $arch in
    aarch64) name=arm64 ;;
    x86_64) name=x86_64 ;;
    *) echo "arch is aarch64 or x86_64" >&2; exit 2 ;;
esac
art="$WORK/art/$run-$arch"
if [ ! -f "$art/reliaburger-os_$v.SHA256SUMS" ]; then
    mkdir -p "$art"
    gh run download "$run" -R reliaburger/reliaburger -n "appliance-$arch" -D "$art"
fi
mkdir -p "$WORK/srv"
tree=$(mktemp -d "$WORK/srv/$v-$name.XXXXXX")
mkdir -p "$tree/tftp" "$tree/http/$name"
if [ "${IPXE:-snp}" = snp ] && [ -f "$art/netboot/ipxe-snp-$name.efi" ]; then
    cp "$art/netboot/ipxe-snp-$name.efi" "$tree/tftp/ipxe-$name.efi"
else
    cp "$art/netboot/ipxe-$name.efi" "$tree/tftp/ipxe-$name.efi"
fi
cp "$art/netboot/boot.ipxe" "$tree/tftp/"
# rbnode.sh starts iPXE with -kernel under HVF; that's the full-driver build,
# the one the lab has run that way.
cp "$art/netboot/ipxe-$name.efi" "$WORK/ipxe-kernel-$name.efi"
for f in raw.zst SHA256SUMS SHA256SUMS.sig; do
    cp "$art/reliaburger-os_$v.$f" "$tree/http/$name/"
done
cp "$art/reliaburger-os-installer_$v.efi" "$tree/http/$name/installer.efi"
if [ "$update" = --update ]; then
    cp "$art/reliaburger-os_$v.efi" "$art"/reliaburger-os_"$v".usr.*.raw.zst \
        "$art"/reliaburger-os_"$v".usr-verity.*.raw.zst "$tree/http/$name/"
    cp "$art/spike-signing-key.pub.pem" "$tree/http/$name/os-$v.pub.pem"
fi
listing=$("$LAB/push-srv.sh" "$tree")
echo "$listing" | grep -E "$v|ipxe|installer" || true
echo "staged $v ($arch) from run $run"
