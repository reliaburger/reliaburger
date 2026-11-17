#!/usr/bin/env bash
# Prove the write-cache cut can be built in this guest kernel: run as root in
# a disposable guest, with logwrites.py beside this script. It puts ext4 on a
# dm-log-writes device over two loop files, writes one file and syncs it,
# then writes a second file with O_DIRECT and never flushes it (so its data
# reaches the device but no flush covers it), and replays the log three ways
# onto fresh copies: to the end, to the last flush and to the mark after the
# sync. Pass means the flushed file survives every replay, the unflushed
# bytes are on the disk replayed to the end but not on the one replayed to
# the last flush, and e2fsck finds both clean after journal recovery.
#
# Prints one JSON object. Touches nothing outside WORK (default
# /var/tmp/pcut-logwrites), which it deletes first.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
work=${1:-/var/tmp/pcut-logwrites}
name=pcut-logwrites
flushed_text="FLUSHED-$(date +%s)-$$"
unflushed_text="UNFLUSHED-$(date +%s)-$$"

cleanup() {
    umount "$work/mnt" 2> /dev/null || true
    umount "$work/check" 2> /dev/null || true
    dmsetup remove "$name" 2> /dev/null || true
    for device in ${data:-} ${log:-} ${check_device:-}; do losetup -d "$device" 2> /dev/null || true; done
}
trap cleanup EXIT

rm -rf "$work"
mkdir -p "$work/mnt" "$work/check"
module=loaded
modprobe dm-log-writes 2> "$work/modprobe.err" || module="missing: $(tr '\n' ' ' < "$work/modprobe.err")"
[[ $module == loaded ]] || { printf '{"kernel": "%s", "module": "%s", "pass": false}\n' "$(uname -r)" "$module"; exit 1; }

truncate -s 256M "$work/data.img"
truncate -s 1G "$work/log.img"
data=$(losetup -f --show "$work/data.img")
log=$(losetup -f --show "$work/log.img")
dmsetup create "$name" --table "0 $(blockdev --getsz "$data") log-writes $data $log"
dmsetup message "$name" 0 mark start
mkfs.ext4 -q -E nodiscard "/dev/mapper/$name"
dmsetup message "$name" 0 mark mkfs
# A long commit interval, so jbd2 doesn't flush on its own timer mid-proof.
mount -o commit=600 "/dev/mapper/$name" "$work/mnt"
printf '%s\n' "$flushed_text" > "$work/mnt/flushed"
sync -f "$work/mnt/flushed"
dmsetup message "$name" 0 mark synced
python3 - "$work/mnt/unflushed" "$unflushed_text" <<'PY'
import mmap, os, sys
buffer = mmap.mmap(-1, 4096)
text = sys.argv[2].encode()
buffer.write((text * (4096 // len(text) + 1))[:4096])
descriptor = os.open(sys.argv[1], os.O_WRONLY | os.O_CREAT | os.O_DIRECT, 0o644)
os.pwrite(descriptor, buffer, 0)
os.close(descriptor)
PY
sleep 1
dmsetup message "$name" 0 mark end
entries=$(python3 "$here/logwrites.py" list "$work/log.img" | wc -l)

# One replay: a zeroed copy of the data device, the log replayed onto it to
# POINT, then whether the unflushed bytes are on it, journal recovery by a
# mount, the flushed file's content, and e2fsck.
replay() {
    local point=$1 image=$work/replay-${1//:/-}.img result
    truncate -s 256M "$image"
    result=$(python3 "$here/logwrites.py" replay "$work/log.img" --target "$image" --to "$point")
    local raw=no
    if grep -q -a -- "$unflushed_text" "$image"; then raw=yes; fi
    check_device=$(losetup -f --show "$image")
    local mounted=no flushed=missing unflushed=absent fsck=clean
    if mount "$check_device" "$work/check" 2> /dev/null; then
        mounted=yes
        [[ $(cat "$work/check/flushed" 2> /dev/null) != "$flushed_text" ]] || flushed=intact
        [[ ! -e $work/check/unflushed ]] || unflushed="present ($(stat -c %s "$work/check/unflushed") bytes)"
        umount "$work/check"
    fi
    e2fsck -fn "$check_device" > "$work/fsck-${1//:/-}.log" 2>&1 || fsck="exit $?"
    losetup -d "$check_device"
    check_device=
    printf '"%s": {"replay": %s, "unflushed_bytes_on_disk": "%s", "mounted": "%s", "flushed_file": "%s", "unflushed_file": "%s", "fsck": "%s"}' \
        "$point" "$result" "$raw" "$mounted" "$flushed" "$unflushed" "$fsck"
}

end=$(replay end)
last_flush=$(replay last-flush)
synced=$(replay mark:synced)
summary="{\"kernel\": \"$(uname -r)\", \"module\": \"$module\", \"entries\": $entries, $end, $last_flush, $synced}"
printf '%s\n' "$summary" | python3 -c '
import json, sys
result = json.load(sys.stdin)
end, flush, synced = result["end"], result["last-flush"], result["mark:synced"]
result["pass"] = (
    end["unflushed_bytes_on_disk"] == "yes"
    and flush["unflushed_bytes_on_disk"] == "no"
    and all(r["flushed_file"] == "intact" and r["fsck"] == "clean" for r in (end, flush, synced))
)
print(json.dumps(result, indent=2))
sys.exit(0 if result["pass"] else 1)'
