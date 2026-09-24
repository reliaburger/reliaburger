#!/usr/bin/env bash
# Build the quickstart guest image for this machine's architecture.
#
#   sudo scripts/release/build_guest_image.sh --output DIR [--source FILE]
#
# Starts from the Ubuntu cloud image pinned in guest-images.json (downloaded,
# or given with --source, and checked against its SHA-256 either way),
# installs the pinned package list from Ubuntu's archive in a chroot, removes everything that must be unique per VM, and writes a
# zlib-compressed qcow2 named after the image's release asset to DIR. Prints a
# JSON build record on standard output; progress goes to standard error.
#
# The build is native: an aarch64 image needs an aarch64 host. Package
# scripts run in the chroot, so a foreign architecture would need emulation,
# and GitHub has native arm64 runners.
#
# Needs root, python3, curl, qemu-img, losetup, lsblk, mount and chroot.
set -euo pipefail

usage() { echo "usage: $0 --output DIR [--source FILE]" >&2; exit 2; }

output="" source=""
while [ $# -gt 0 ]; do
  case "$1" in
    --output) output="${2:?}"; shift 2 ;;
    --source) source="${2:?}"; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$output" ] || usage
[ "$(id -u)" = 0 ] || { echo "run as root: loop devices and chroot need it" >&2; exit 1; }

arch=$(uname -m)
case "$arch" in
  aarch64|arm64) arch=aarch64 ;;
  x86_64|amd64) arch=x86_64 ;;
  *) echo "unsupported build architecture: $arch" >&2; exit 1 ;;
esac

pins="$(dirname "$0")/guest-images.json"
field() {
  python3 -c 'import json, sys
document = json.load(open(sys.argv[1]))
value = document["images"][sys.argv[2]] if sys.argv[3] != "document" else document
for key in sys.argv[4:]:
    value = value[key]
print(" ".join(value) if isinstance(value, list) else value)' "$pins" "$arch" "$@"
}
asset=$(field image asset)
source_url=$(field image source url)
source_sha256=$(field image source sha256)
packages=$(field document packages)

mkdir -p "$output"
work=$(mktemp -d "${output%/}/.guest-build.XXXXXX")
root="$work/root"
loop=""
cleanup() {
  set +e
  for mount in dev/pts dev proc sys var/lib/apt/lists var/cache/apt boot/efi boot ""; do
    mountpoint -q "$root/$mount" 2>/dev/null && umount "$root/$mount"
  done
  [ -n "$loop" ] && losetup -d "$loop"
  rm -rf "$work"
}
trap cleanup EXIT

if [ -z "$source" ]; then
  source="$work/source.img"
  echo "downloading $source_url" >&2
  curl --fail --location --silent --show-error --proto '=https' -o "$source" "$source_url"
fi
echo "$source_sha256  $source" | sha256sum --check --quiet - >&2 \
  || { echo "upstream image does not match its pinned SHA-256" >&2; exit 1; }

# A sparse raw copy that the kernel can loop-mount. Its virtual size stays the
# upstream one; Lima grows the disk and cloud-init the filesystem at boot.
qemu-img convert -O raw "$source" "$work/disk.raw"
loop=$(losetup --find --show --partscan "$work/disk.raw")
udevadm settle 2>/dev/null || true
partition() { lsblk --raw --noheadings --output PATH,LABEL "$loop" | awk -v l="$1" '$2 == l { print $1 }'; }
rootfs=$(partition cloudimg-rootfs)
[ -n "$rootfs" ] || { echo "no cloudimg-rootfs partition in the upstream image" >&2; exit 1; }
mkdir -p "$root"
mount "$rootfs" "$root"
# Package triggers (btrfs-progs' initramfs hook) rewrite /boot, which is its
# own partition in Ubuntu's cloud images.
boot=$(partition BOOT)
[ -n "$boot" ] && mount "$boot" "$root/boot"
efi=$(partition UEFI)
[ -n "$efi" ] && mount "$efi" "$root/boot/efi"
for mount in dev dev/pts proc sys; do
  mount --bind "/$mount" "$root/$mount"
done
# Package indexes and downloaded .debs (about 500 MiB) live in memory, so
# they never touch, and never bloat, the image.
for mount in var/lib/apt/lists var/cache/apt; do
  mount -t tmpfs tmpfs "$root/$mount"
done

# Temporary build-time plumbing, all removed before the image is sealed:
# name resolution through the host, and no service starts inside the chroot.
mv "$root/etc/resolv.conf" "$work/resolv.conf"
cp --dereference /etc/resolv.conf "$root/etc/resolv.conf"
printf '#!/bin/sh\nexit 101\n' >"$root/usr/sbin/policy-rc.d"
chmod 755 "$root/usr/sbin/policy-rc.d"

in_guest() { chroot "$root" env DEBIAN_FRONTEND=noninteractive NEEDRESTART_SUSPEND=1 LC_ALL=C "$@"; }
# Live archive, not a snapshot: snapshot.ubuntu.com doesn't serve ubuntu-ports
# (arm64) anonymously. The versions installed go into the build record.
in_guest apt-get -o Acquire::Retries=3 update -qq >&2
# shellcheck disable=SC2086
in_guest apt-get -o Acquire::Retries=3 install -y -qq $packages >&2

# shellcheck disable=SC2086
versions=$(in_guest dpkg-query -W -f='${Package}=${Version}\n' $packages)
mkdir -p "$root/usr/share/reliaburger"
python3 - "$arch" "$source_sha256" "$versions" >"$root/usr/share/reliaburger/guest-image.json" <<'PYCODE'
import json, sys
arch, source, versions = sys.argv[1:]
packages = dict(line.split("=", 1) for line in versions.split())
print(json.dumps({"schema": 1, "arch": arch, "source_sha256": source,
                  "packages": packages}, indent=2, sort_keys=True))
PYCODE

# Seal: nothing that must differ between VMs, nothing that only served the
# build. An empty machine-id makes systemd generate a fresh one at first
# boot; `cloud-init clean` makes cloud-init treat that boot as the first.
in_guest cloud-init clean --logs --seed
rm -f "$root/usr/sbin/policy-rc.d" "$root/etc/resolv.conf"
mv "$work/resolv.conf" "$root/etc/resolv.conf"
: >"$root/etc/machine-id"
rm -f "$root/var/lib/systemd/random-seed" "$root"/etc/ssh/ssh_host_*
rm -rf "$root"/tmp/* "$root"/var/tmp/* "$root/root/.bash_history"
rm -rf "$root"/var/log/journal/*
find "$root/var/log" -type f -exec truncate --size=0 {} +
find "$root/var/cache/debconf" -name '*-old' -delete

for mount in dev/pts dev proc sys var/lib/apt/lists var/cache/apt; do umount "$root/$mount"; done
# Hand freed blocks back to the sparse file so compression skips them. ext4
# frees deleted blocks only when their journal transaction commits.
sync
fstrim --verbose "$root" >&2
[ -n "$efi" ] && umount "$root/boot/efi"
[ -n "$boot" ] && umount "$root/boot"
umount "$root"
losetup -d "$loop"
loop=""

# zlib, not zstd: Lima 2.1.0 reads zlib-compressed qcow2 natively but has
# no zstd decompressor for qcow2 clusters.
qemu-img convert -c -O qcow2 -o compression_type=zlib "$work/disk.raw" "$work/$asset"
mv "$work/$asset" "${output%/}/$asset"

python3 - "$arch" "${output%/}/$asset" "$source_url" "$source_sha256" "$versions" <<'PYCODE'
import hashlib, json, os, sys
arch, path, url, source, versions = sys.argv[1:]
digest = hashlib.sha256()
with open(path, "rb") as stream:
    for chunk in iter(lambda: stream.read(1024 * 1024), b""):
        digest.update(chunk)
print(json.dumps({"arch": arch, "asset": os.path.basename(path), "sha256": digest.hexdigest(),
                  "size": os.path.getsize(path), "source": {"url": url, "sha256": source},
                  "packages": dict(line.split("=", 1) for line in versions.split())},
                 indent=2, sort_keys=True))
PYCODE
