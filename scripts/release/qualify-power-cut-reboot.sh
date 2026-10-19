#!/usr/bin/env bash
# The Markdown records quote values in backticks, inside single-quoted formats.
# shellcheck disable=SC2016
#
# Release gate step 5, "Pull the plug" (docs/releasing.md), in one command:
# make a clean checkout of the candidate, create a disposable Lima VM from
# power-cut-vm.yaml with only that checkout mounted read-only, check the VM
# from inside, run the three storage power-cut fixtures and the OCI and
# discovery reboot lanes N times each, stop at the first failure, and write
# the per-lane records plus a summary with each lane's rule-of-three bound.
#
# The records go in DIR, and the summary in DIR.md next to it, the shape of
# docs/qualification/2026-10-16-power-cut-reboot.md. The VM and checkout are
# deleted afterwards, unless --keep is given or a lane failed (a failure is a
# release blocker; its guest evidence lives in the VM). It never touches the
# development VM, and never deletes a VM or checkout it didn't make.
set -euo pipefail

usage() {
    echo "usage: $0 --commit SHA --iterations N --record-dir DIR [--vm NAME] [--worktree PATH] [--keep]" >&2
    exit 2
}

refuse() {
    echo "refusing: $*" >&2
    exit 2
}

commit=
iterations=
record_dir=
vm=
worktree=
keep=no
while [[ $# -gt 0 ]]; do
    case $1 in
        --keep) keep=yes; shift; continue ;;
        --commit | --iterations | --record-dir | --vm | --worktree) [[ $# -ge 2 ]] || usage ;;
        *) usage ;;
    esac
    case $1 in
        --commit) commit=$2 ;;
        --iterations) iterations=$2 ;;
        --record-dir) record_dir=${2%/} ;;
        --vm) vm=$2 ;;
        --worktree) worktree=$2 ;;
    esac
    shift 2
done
[[ -n $commit && -n $record_dir ]] || usage
[[ $iterations =~ ^[1-9][0-9]*$ ]] || usage
# The shared development VM carries other people's builds and clusters; this
# script must never name it to limactl, even by mistake.
if [[ $vm == reliaburger-test ]]; then
    refuse "the development VM reliaburger-test is never powered off; leave out --vm for a disposable one"
fi

here=$(cd "$(dirname "$0")" && pwd)
repository=$(git -C "$here" rev-parse --show-toplevel)
template=$here/power-cut-vm.yaml
if ! sha=$(git -C "$repository" rev-parse --verify --quiet "$commit^{commit}"); then
    refuse "no commit $commit in $repository"
fi
short=${sha:0:8}
vm=${vm:-rb-pcut-$short}
[[ $vm =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]] || refuse "VM name $vm isn't a valid Lima instance name"

summary=$record_dir.md
[[ ! -e $record_dir && ! -e $summary ]] || refuse "$record_dir or $summary already exists; records are never overwritten"
[[ -d $(dirname "$record_dir") ]] || refuse "no directory $(dirname "$record_dir") for the records"

if limactl list --format '{{.Name}}' | grep -qx -- "$vm"; then
    refuse "VM $vm already exists; delete it (limactl delete --force $vm) or name another with --vm"
fi

# The checkout. Lima mounts a path at the same path in the guest, so use the
# resolved one: macOS's /var is a symlink.
made_worktree=no
temporary=$(cd "${TMPDIR:-/tmp}" && pwd -P)
worktree=${worktree:-$temporary/reliaburger-pcut-$short}
if [[ -e $worktree ]]; then
    worktree=$(cd "$worktree" && pwd -P)
    head=$(git -C "$worktree" rev-parse HEAD 2>/dev/null) || refuse "$worktree exists and isn't a Git checkout"
    [[ $head == "$sha" ]] || refuse "$worktree is at $head, not the candidate $sha"
else
    git -C "$repository" worktree add --quiet --detach "$worktree" "$sha"
    made_worktree=yes
    worktree=$(cd "$worktree" && pwd -P)
fi
# Everything under test must be the commit's bytes: no edits, no stray files.
# Ignored files (a build directory) can't change what the VM builds from a
# read-only mount, so they're allowed.
if [[ -n $(git -C "$worktree" status --porcelain --untracked-files=all) ]]; then
    git -C "$worktree" status --short >&2
    refuse "$worktree is dirty; the lanes must run on a clean checkout of $sha"
fi

made_vm=no
lane_failed=no
cleanup() {
    if [[ $keep == yes || $lane_failed == yes ]]; then
        if [[ $made_vm == yes || $made_worktree == yes ]]; then
            echo "kept for diagnosis: VM $vm and checkout $worktree. Delete them with:" >&2
            [[ $made_vm == no ]] || echo "  limactl delete --force $vm" >&2
            [[ $made_worktree == no ]] || echo "  git -C $repository worktree remove --force $worktree" >&2
        fi
        return
    fi
    if [[ $made_vm == yes ]]; then
        limactl delete --force "$vm" > /dev/null 2>&1 || echo "could not delete VM $vm" >&2
    fi
    if [[ $made_worktree == yes ]]; then
        git -C "$repository" worktree remove --force "$worktree" || echo "could not remove $worktree" >&2
    fi
}
trap cleanup EXIT

evidence=$(mktemp -d "$temporary/reliaburger-pcut-evidence.XXXXXX")
printf 'Power-cut and reboot lanes for %s: VM %s, checkout %s, %s iterations each; evidence %s\n' \
    "$sha" "$vm" "$worktree" "$iterations" "$evidence"

# The VM: the template with the checkout as its only, read-only mount. awk
# reads the path from the environment, so no character in it needs escaping.
rendered=$evidence/$vm.yaml
WORKTREE=$worktree awk '{ gsub(/__WORKTREE__/, ENVIRON["WORKTREE"]); print }' "$template" > "$rendered"
made_vm=yes
started=$(date '+%Y-%m-%d %H:%M %Z')
echo "creating VM $vm (the first boot provisions packages, swap and Rust)"
limactl create --tty=false --name "$vm" "$rendered" > "$evidence/create.log" 2>&1 ||
    { tail -n 20 "$evidence/create.log" >&2; echo "limactl create failed" >&2; exit 1; }
limactl start --tty=false "$vm" > "$evidence/start.log" 2>&1 ||
    { tail -n 20 "$evidence/start.log" >&2; echo "limactl start failed" >&2; exit 1; }

# Check the VM from inside before any lane: what Lima was asked for isn't
# proof of what the guest got (a global Lima override can add mounts).
# --workdir: the host's working directory isn't in the guest.
facts=$(limactl shell --workdir / "$vm" bash -s -- "$worktree" <<'GUEST'
set -u
printf 'swap_bytes=%s\n' "$(swapon --show=SIZE --bytes --noheadings | awk '{ s += $1 } END { print s + 0 }')"
if touch "$1/.reliaburger-write-probe" 2>/dev/null; then
    rm -f "$1/.reliaburger-write-probe"
    echo writable=yes
else
    echo writable=no
fi
printf 'options=%s\n' "$(findmnt -n -o OPTIONS --target "$1")"
printf 'cpus=%s\n' "$(nproc)"
[[ -f $1/Cargo.toml ]] && echo source=yes
[[ -x $HOME/.cargo/bin/cargo ]] && echo cargo=yes
printf 'kernel=%s\n' "$(uname -r)"
findmnt -n -t virtiofs -o TARGET | sed 's/^/mount=/'
GUEST
) || { echo "could not check VM $vm from inside" >&2; exit 1; }
fact() { sed -n "s/^$1=//p" <<< "$facts" | head -n 1; }
problems=()
swap=$(fact swap_bytes)
# fallocate -l 6G makes 6 GiB; the kernel reserves the first page for the header.
[[ ${swap:-0} -ge $((6 * 1024 * 1024 * 1024 - 1024 * 1024)) ]] || problems+=("swap is ${swap:-0} bytes, not 6 GiB")
[[ $(fact writable) == no ]] || problems+=("the guest can write to the checkout")
[[ ,$(fact options), == *,ro,* ]] || problems+=("the checkout is mounted $(fact options), not ro")
[[ $(fact cpus) == 4 ]] || problems+=("the guest has $(fact cpus) vCPUs, not 4")
[[ $(fact source) == yes ]] || problems+=("the checkout isn't visible at $worktree")
[[ $(fact cargo) == yes ]] || problems+=("Rust isn't installed")
mounts=$(sed -n 's/^mount=//p' <<< "$facts")
[[ $mounts == "$worktree" ]] || problems+=("host mounts are [$(tr '\n' ' ' <<< "$mounts")], not only $worktree")
if [[ ${#problems[@]} -gt 0 ]]; then
    printf 'VM %s is not fit for the lanes:\n' "$vm" >&2
    printf '  - %s\n' "${problems[@]}" >&2
    exit 1
fi
kernel=$(fact kernel)
echo "VM $vm checked: 6 GiB swap, 4 vCPUs, only $worktree mounted, read-only"

mkdir "$record_dir"
release=$worktree/scripts/release

# Lanes, in order. Each entry: record file, claim, command shown in the summary.
lane_records=(power-cut-exporter.md power-cut-leases.md power-cut-backups.md oci-reboot.md discovery-reboot.md)
lane_claims=('C03: Exporter checkpoint and lock' 'C11: Lease stores' 'Council backups: Upload, then prune to the newest 3'
    'C47: OCI holds and original execution' 'C47: Bun, kernel and discovery')
lane_commands=("qualify-storage-power-cut.sh --fixture exporter --iterations $iterations"
    "qualify-storage-power-cut.sh --fixture leases --iterations $iterations"
    "qualify-storage-power-cut.sh --fixture backups --iterations $iterations"
    "qualify-oci-reboot.sh --vm $vm, $iterations runs"
    "qualify-discovery-reboot.sh --vm $vm, $iterations runs")
lane_runs=(0 0 0 0 0)
lane_failures=(- - - - -)
retried=0

# One storage fixture: the lane script loops and records by itself.
storage_lane() {
    local index=$1 fixture=$2 record=$record_dir/${lane_records[$1]} passed
    echo "lane: storage power cut, $fixture"
    if "$release/qualify-storage-power-cut.sh" --vm "$vm" --fixture "$fixture" \
        --iterations "$iterations" --record "$record" 2>&1 | tee "$evidence/storage-$fixture.log"; then
        lane_runs[index]=$iterations
        lane_failures[index]=0
        retried=$((retried + $(grep -c 'VM start retried' "$record" || true)))
        return 0
    fi
    passed=$(sed -n 's/^\([0-9][0-9]*\) of [0-9]* iterations passed.*/\1/p' "$record" 2>/dev/null | tail -n 1)
    lane_runs[index]=$((${passed:-0} + 1))
    lane_failures[index]=1
    return 1
}

# One reboot lane: N runs of a script that power-cycles the VM once, each
# run's last lines kept in the record.
reboot_lane() {
    local index=$1 script=$2 title=$3 record=$record_dir/${lane_records[$1]} number log
    echo "lane: $title"
    {
        printf '# %s: %s power cycles\n\n' "$title" "$iterations"
        printf 'Candidate `%s`, VM `%s`, %s. One `%s --vm %s` per iteration, stopping at the first failure. The last lines of each:\n' \
            "$short" "$vm" "$(date '+%-d %B %Y')" "$script" "$vm"
    } > "$record"
    for ((number = 1; number <= iterations; number++)); do
        log=$evidence/${script%.sh}-$number.log
        lane_runs[index]=$number
        if "$release/$script" --vm "$vm" > "$log" 2>&1; then
            printf '\n## Iteration %s\n\n```\n%s\n```\n' "$number" "$(tail -n 3 "$log")" >> "$record"
            printf '%s %s/%s: PASS\n' "$script" "$number" "$iterations"
            continue
        fi
        printf '\n## Iteration %s: FAIL\n\n```\n%s\n```\n' "$number" "$(tail -n 40 "$log")" >> "$record"
        printf '\n%s of %s iterations passed before the first failure; full log `%s`.\n' \
            "$((number - 1))" "$iterations" "$log" >> "$record"
        printf '%s %s/%s: FAIL; log %s\n' "$script" "$number" "$iterations" "$log" >&2
        lane_failures[index]=1
        return 1
    done
    printf '\n%s of %s iterations passed.\n' "$iterations" "$iterations" >> "$record"
    lane_failures[index]=0
}

# Rule of three: no failure in n runs bounds the true rate below 3/n at 95%.
bound() {
    awk -v n="$1" 'BEGIN { b = 300 / n; if (b > 100) b = 100; s = sprintf("%.1f", b); sub(/\.0$/, "", s); print "< " s "%" }'
}

write_summary() {
    local index cell result
    if [[ $lane_failed == yes ]]; then
        result='**Result: FAIL.** A lane failed, which blocks the release; the run stopped there. The VM and checkout were kept for diagnosis.'
    else
        result="**Result: every lane passed $iterations of $iterations.**"
    fi
    {
        printf '# Power cuts and reboots on %s (%s)\n\n' "$short" "$(date '+%-d %b %Y')"
        printf 'Release gate step 5'"'"'s power-cut and reboot lanes ([releasing.md](../releasing.md#staging-a-candidate)), '
        printf 'run by `qualify-power-cut-reboot.sh --commit %s --iterations %s`.\n\n' "$short" "$iterations"
        printf '%s ' "$result"
        case $retried in
            0) printf 'No iteration needed the VM start retry.\n' ;;
            1) printf '1 iteration needed the VM start retry (the known Lima start failure after a forced stop).\n' ;;
            *) printf '%s iterations needed the VM start retry (the known Lima start failure after a forced stop).\n' "$retried" ;;
        esac
        printf '\n## Environment\n\n| Item | Value |\n|---|---|\n'
        printf '| Host | %s, %s |\n' "$(host_description)" "$(limactl --version 2>/dev/null || echo 'Lima version unknown')"
        printf '| VM | `%s`, created for this run from [`power-cut-vm.yaml`](https://github.com/reliaburger/reliaburger/blob/%s/scripts/release/power-cut-vm.yaml) with 4 vCPUs, 6 GiB memory, 6 GiB swap and a 40 GiB disk, and only the candidate checkout mounted, read-only, all checked from inside the guest |\n' "$vm" "$sha"
        printf '| Guest | kernel %s |\n' "$kernel"
        printf '| Source | [`%s`](https://github.com/reliaburger/reliaburger/commit/%s), a clean checkout of it |\n' "$short" "$sha"
        printf '| Time | %s to %s |\n' "$started" "$(date '+%Y-%m-%d %H:%M %Z')"
        printf '\n## Results\n\n'
        printf '| Lane | Claim | Command | Runs | Failures | 95%% bound (rule of three) | Per-iteration record |\n'
        printf '|---|---|---|---|---|---|---|\n'
        for index in 0 1 2 3 4; do
            case ${lane_failures[index]} in
                0) cell=$(bound "${lane_runs[index]}") ;;
                -) cell='not run' ;;
                *) cell=failed ;;
            esac
            printf '| %s | %s | `%s` | %s | %s | %s | ' "${lane_claims[index]%%:*}" "${lane_claims[index]#*: }" \
                "${lane_commands[index]}" "${lane_runs[index]}" "${lane_failures[index]}" "$cell"
            if [[ ${lane_failures[index]} == - ]]; then
                printf -- '- |\n'
            else
                printf '[%s](%s/%s) |\n' "${lane_records[index]%.md}" "$(basename "$record_dir")" "${lane_records[index]}"
            fi
        done
        printf '\nZero failures in *n* independent runs bounds the true failure rate below 3/*n*\n'
        printf 'at 95%% confidence. It says nothing about a rarer race.\n\n'
        printf 'The host evidence directories (under `%s`) are temporary and aren'"'"'t kept.\n' "$evidence"
        printf 'The per-iteration records keep what each run verified.\n'
    } > "$summary"
}

host_description() {
    if command -v sw_vers > /dev/null 2>&1; then
        printf '%s, macOS %s' "$(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m)" "$(sw_vers -productVersion)"
    else
        uname -srm
    fi
}

run_lanes() {
    storage_lane 0 exporter &&
        storage_lane 1 leases &&
        storage_lane 2 backups &&
        reboot_lane 3 qualify-oci-reboot.sh 'OCI reboot' &&
        reboot_lane 4 qualify-discovery-reboot.sh 'Bun, kernel and discovery reboot'
}

if ! run_lanes; then
    lane_failed=yes
fi
write_summary
if [[ $lane_failed == yes ]]; then
    echo "FAIL: a lane failed; records in $record_dir, summary $summary" >&2
    exit 1
fi
echo "PASS: every lane passed $iterations of $iterations; records in $record_dir, summary $summary"
