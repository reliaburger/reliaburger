# shellcheck shell=bash
# The disk the QEMU tests give an appliance, sourced by image/tests/*.sh and
# by appliance.yml's boot test.
#
# A Dell Wyse 3040's "8 GB" eMMC is 8 × 10^9 bytes, about 7.3 GiB usable, so
# the tests use 7.25 GiB rather than QEMU's round 8 GiB, which would give the
# data partition ~1.3 GiB the real machine doesn't have. DISK_MIB overrides it
# (the unit tests use a tiny disk).
DISK_MIB=${DISK_MIB:-7424}

# What the first boot adds behind the image: slot B of /usr and its verity
# (image/mkosi.extra/usr/lib/repart.d/50-usr-b.conf, 51-usr-verity-b.conf).
SLOT_B_MIB=$(( 1100 + 64 ))

# disk_fits <image bytes> <name>: fail, saying why, unless an image of that
# size plus slot B fits on the test disk.
disk_fits() {
    local bytes=$1 name=$2
    local need_mib=$(( (bytes + 1048575) / 1048576 + SLOT_B_MIB ))
    if [ "$need_mib" -gt "$DISK_MIB" ]; then
        echo "disk.sh: $name doesn't fit a ${DISK_MIB} MiB disk (a Wyse 3040's eMMC):" \
            "the image is $(( (bytes + 1048575) / 1048576 )) MiB and the first boot adds" \
            "$SLOT_B_MIB MiB for slot B, $need_mib MiB in all" >&2
        return 1
    fi
}

# disk_from_image <raw>: grow a disk image written straight to <raw> (the
# image as dd would write it) to the test disk, refusing one that doesn't
# fit rather than letting truncate cut it short.
disk_from_image() {
    local raw=$1
    disk_fits "$(wc -c <"$raw")" "$(basename "$raw")" || return 1
    truncate -s "${DISK_MIB}M" "$raw"
}

# blank_disk <raw> <image.raw.zst>: an empty test disk for the installer,
# after checking the image it will write fits on it.
blank_disk() {
    local raw=$1 image=$2 bytes
    bytes=$(zstd -q -d -c "$image" | wc -c) || return 1
    disk_fits "$bytes" "$(basename "$image")" || return 1
    rm -f "$raw"
    truncate -s "${DISK_MIB}M" "$raw"
}
