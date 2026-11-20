#!/usr/bin/env bash
# Prove the write-cache cut can be built in this guest kernel: run as root in
# a disposable guest, with logwrites.py beside this script.
#
# dm-log-writes passes every write through to its data device at once, but
# logs a plain write only when a later flush completes: until then it's in
# the "cache". So a copy of the log taken at a cut, replayed onto the disk as
# it was when logging began, is the disk a drive with a volatile write cache
# could have kept: fault model 2 in
# docs/plans/2026-11-18-plan-power-cut-endurance.md.
#
# The proof puts ext4 on a dm-log-writes device over two loop files, writes
# one file and syncs it, then writes a second file with O_DIRECT (its data
# reaches the device, but no flush covers it) and copies the log: the cut.
# Then it syncs and copies the log again. Pass means:
#
# - the data device holds the unflushed bytes (the write did reach it);
# - the log copied at the cut, replayed to its end, doesn't, and still holds
#   the synced file, and e2fsck finds it clean after journal recovery;
# - the log copied after the sync, replayed to its end, does: the flush
#   logged the write;
# - that later log replayed to the mark before the sync doesn't.
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
mkfs.ext4 -q -E nodiscard,lazy_itable_init=0,lazy_journal_init=0 "/dev/mapper/$name"
dmsetup message "$name" 0 mark mkfs
# No lazy inode-table init (its background zeroing flushes) and a long commit
# interval, so nothing flushes on its own mid-proof.
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
# A mark updates the log's super block, so a copy taken now counts every
# entry so far.
dmsetup message "$name" 0 mark cut
cp "$work/log.img" "$work/log-at-cut.img"
sync -f "$work/mnt"
dmsetup message "$name" 0 mark after-sync
cp "$work/log.img" "$work/log-after-sync.img"
on_data_device=no
if grep -q -a -- "$unflushed_text" "$work/data.img"; then on_data_device=yes; fi
python3 "$here/logwrites.py" list "$work/log-after-sync.img" > "$work/entries.jsonl"

# One replay: a zeroed copy of the data device, LOG replayed onto it to
# POINT, then whether the unflushed bytes are on it, journal recovery by a
# mount, the flushed file's content, and e2fsck.
replay() {
    local label=$1 log_copy=$2 point=$3 image=$work/replay-$1.img result
    truncate -s 256M "$image"
    result=$(python3 "$here/logwrites.py" replay "$log_copy" --target "$image" --to "$point")
    local raw=no
    if grep -q -a -- "$unflushed_text" "$image"; then raw=yes; fi
    check_device=$(losetup -f --show "$image")
    local mounted=no flushed=missing fsck=clean
    if mount "$check_device" "$work/check" 2> /dev/null; then
        mounted=yes
        [[ $(cat "$work/check/flushed" 2> /dev/null) != "$flushed_text" ]] || flushed=intact
        umount "$work/check"
    fi
    e2fsck -fn "$check_device" > "$work/fsck-$label.log" 2>&1 || fsck="exit $?"
    losetup -d "$check_device"
    check_device=
    printf '"%s": {"log": "%s", "to": "%s", "replay": %s, "unflushed_bytes": "%s", "mounted": "%s", "flushed_file": "%s", "fsck": "%s"}' \
        "$label" "${log_copy##*/}" "$point" "$result" "$raw" "$mounted" "$flushed" "$fsck"
}

at_cut=$(replay at-cut "$work/log-at-cut.img" end)
after_sync=$(replay after-sync "$work/log-after-sync.img" end)
before_sync=$(replay before-sync "$work/log-after-sync.img" mark:cut)
summary="{\"kernel\": \"$(uname -r)\", \"module\": \"$module\", \"entries\": $(wc -l < "$work/entries.jsonl"), \"unflushed_bytes_on_data_device\": \"$on_data_device\", $at_cut, $after_sync, $before_sync}"
printf '%s\n' "$summary" | python3 -c '
import json, sys
result = json.load(sys.stdin)
cut, after, before = result["at-cut"], result["after-sync"], result["before-sync"]
result["pass"] = (
    result["unflushed_bytes_on_data_device"] == "yes"
    and cut["unflushed_bytes"] == "no"
    and after["unflushed_bytes"] == "yes"
    and before["unflushed_bytes"] == "no"
    and all(r["flushed_file"] == "intact" and r["fsck"] == "clean" for r in (cut, after, before))
)
print(json.dumps(result, indent=2))
sys.exit(0 if result["pass"] else 1)'
