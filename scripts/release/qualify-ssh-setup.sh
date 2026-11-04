#!/usr/bin/env bash
# The Markdown record quotes values in backticks, inside single-quoted formats.
# shellcheck disable=SC2016
#
# relish setup --ssh, end to end (#1529): create disposable Lima VMs (three,
# or --machines N) from ssh-setup-vm.yaml, build a cluster on them over SSH exactly as an
# operator would, and check it:
#
#   1. setup --ssh finishes, with hello answering through the ingress;
#   2. relish status shows every node and a full council, and relish wtf
#      finds nothing critical; past seven machines the rest run without a
#      council seat, so every node's Bun must also have kept its pid, with
#      no restarts and its API listening, three minutes on (#1477);
#   3. the same command again resumes and finishes without changing a node;
#   4. a second operator, with a fresh RELIABURGER_HOME, is refused on the
#      same machines before anything is written, and Bun keeps its pid.
#
# It writes a Markdown record with each step's result and the setup's
# timings. The VMs are deleted afterwards unless --keep is given or a step
# failed, so the evidence survives. It never touches a VM it didn't make.
#
# The operator's ssh reaches each VM on Lima's forwarded port with Lima's
# key, through a wrapper ssh first on PATH: the same job a user's
# ~/.ssh/config does. Arguments after -- go to relish setup --ssh, for
# example --development-binaries DIR or --release-mirror URL; without them
# it installs the published release matching RELISH's version.
set -euo pipefail

usage() {
    echo "usage: $0 --relish PATH --record FILE [--machines N] [--keep] [-- SETUP_OPTIONS...]" >&2
    exit 2
}

refuse() {
    echo "refusing: $*" >&2
    exit 2
}

relish=
record=
keep=no
machines=3
setup_options=()
while [[ $# -gt 0 ]]; do
    case $1 in
        --keep) keep=yes; shift ;;
        --machines) [[ $# -ge 2 ]] || usage; machines=$2; shift 2 ;;
        --relish) [[ $# -ge 2 ]] || usage; relish=$2; shift 2 ;;
        --record) [[ $# -ge 2 ]] || usage; record=$2; shift 2 ;;
        --) shift; setup_options=("$@"); break ;;
        *) usage ;;
    esac
done
[[ -n $relish && -n $record ]] || usage
[[ -x $relish ]] || refuse "$relish isn't an executable relish"
[[ $machines =~ ^[1-9][0-9]*$ ]] || refuse "--machines needs a whole number of machines, not $machines"
[[ ! -e $record ]] || refuse "$record already exists; records are never overwritten"
[[ -d $(dirname "$record") ]] || refuse "no directory $(dirname "$record") for the record"
command -v limactl >/dev/null || refuse "limactl isn't installed"

here=$(cd "$(dirname "$0")" && pwd)
template=$here/ssh-setup-vm.yaml
run=$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')
vms=()
for index in $(seq 1 "$machines"); do vms+=("rb-sshq-$run-$index"); done
# The council: one seat per machine up to seven, the largest odd number of
# them voting.
seats=$((machines < 7 ? machines : 7))
voters=$((seats % 2 == 1 ? seats : seats - 1))
work=$(mktemp -d "${TMPDIR:-/tmp}/rb-sshq.XXXXXX")
home=$work/home
mkdir -p "$home" "$work/bin"

# The wrapper: Lima's key, and no host-key prompts for VMs made a minute ago.
cat >"$work/bin/ssh" <<EOF
#!/bin/sh
exec /usr/bin/ssh -i "\$HOME/.lima/_config/user" -o IdentitiesOnly=yes \\
    -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR "\$@"
EOF
chmod +x "$work/bin/ssh"
export PATH=$work/bin:$PATH

failed=no
results=()
pass() { results+=("| $1 | PASS | $2 |"); echo "PASS: $1"; }
fail() { results+=("| $1 | FAIL | $2 |"); echo "FAIL: $1: $2" >&2; failed=yes; }

cleanup() {
    if [[ $keep == no && $failed == no ]]; then
        for vm in "${vms[@]}"; do limactl delete --force "$vm" >/dev/null 2>&1 || true; done
        rm -rf "$work"
    else
        echo "kept: VMs ${vms[*]}, RELIABURGER_HOME $home" >&2
    fi
}
trap cleanup EXIT

started=$(date -u +%Y-%m-%dT%H:%M:%SZ)
for vm in "${vms[@]}"; do
    limactl create --name "$vm" --tty=false "$template" >/dev/null
done
for vm in "${vms[@]}"; do limactl start "$vm" >"$work/lima-$vm.log" 2>&1 & done
wait

targets=()
addresses=()
for vm in "${vms[@]}"; do
    port=$(limactl list --format '{{.SSHLocalPort}}' "$vm")
    target=$USER@127.0.0.1:$port
    targets+=("$target")
    addresses+=("$(ssh -p "$port" "$USER@127.0.0.1" \
        "ip -4 -o addr show scope global | awk 'NR == 1 { split(\$4, a, \"/\"); print a[1] }'")")
done
joined_targets=$(IFS=,; echo "${targets[*]}")
joined_addresses=$(IFS=,; echo "${addresses[*]}")
setup=("$relish" setup --ssh "$joined_targets" --advertise "$joined_addresses"
    ${setup_options[@]+"${setup_options[@]}"})

bun_pid() {
    local port=${1##*:}
    ssh -p "$port" "$USER@127.0.0.1" 'systemctl show -p MainPID --value reliaburger'
}

# 1. Setup.
if RELIABURGER_HOME=$home "${setup[@]}" >"$work/setup.log" 2>&1; then
    pass "setup --ssh" "$(grep -m1 'ready in' "$work/setup.log" || echo finished)"
else
    fail "setup --ssh" "exit $?; see the log below"
fi

# 2. Status and diagnosis.
if [[ $failed == no ]]; then
    status=$(RELIABURGER_HOME=$home "$relish" status 2>&1 || true)
    if grep -q "$voters/$voters voters" <<<"$status"; then
        pass "relish status" "$(head -1 <<<"$status")"
    else
        fail "relish status" "$(head -1 <<<"$status")"
    fi
    wtf=$(RELIABURGER_HOME=$home "$relish" wtf 2>&1 || true)
    summary=$(grep -m1 '^Summary:' <<<"$wtf" || echo "no summary")
    if grep -q '^Summary: 0 critical' <<<"$wtf"; then
        pass "relish wtf" "$summary"
    else
        fail "relish wtf" "$summary"
    fi
fi

# 2b. Every node keeps running. Past seven machines a node has no council
# seat and reads the leader's security view; before #1477 its API never
# opened and its Bun restarted every 30 s.
if [[ $failed == no ]]; then
    pids=()
    for target in "${targets[@]}"; do pids+=("$(bun_pid "$target")"); done
    sleep 180
    unsettled=()
    for index in "${!targets[@]}"; do
        port=${targets[$index]##*:}
        restarts=$(ssh -p "$port" "$USER@127.0.0.1" 'systemctl show -p NRestarts --value reliaburger')
        listening=$(ssh -p "$port" "$USER@127.0.0.1" 'sudo ss -ltnH "sport = :9117" | wc -l')
        if [[ $(bun_pid "${targets[$index]}") != "${pids[$index]}" || $restarts != 0 || $listening -lt 1 ]]; then
            unsettled+=("node $((index + 1)): restarts $restarts, listeners $listening")
        fi
    done
    if [[ ${#unsettled[@]} -eq 0 ]]; then
        pass "every node settled" "$machines Buns kept their pids for 3 minutes, with no restarts and the API listening"
    else
        fail "every node settled" "${unsettled[*]}"
    fi
fi

# 3. The same command resumes without restarting anything.
if [[ $failed == no ]]; then
    before=$(bun_pid "${targets[0]}")
    if RELIABURGER_HOME=$home "${setup[@]}" >"$work/resume.log" 2>&1; then
        after=$(bun_pid "${targets[0]}")
        if [[ $before == "$after" ]]; then
            pass "setup again" "$(grep -m1 'ready in' "$work/resume.log"); node 1's Bun kept pid $before"
        else
            fail "setup again" "node 1's Bun restarted: pid $before became $after"
        fi
    else
        fail "setup again" "exit $?"
    fi
fi

# 4. Another operator is refused before anything is written.
if [[ $failed == no ]]; then
    before=$(bun_pid "${targets[0]}")
    mkdir -p "$work/other"
    if RELIABURGER_HOME=$work/other "${setup[@]}" >"$work/other.log" 2>&1; then
        fail "a second operator" "setup succeeded on machines another cluster holds"
    elif grep -q "refusing to overwrite a machine" "$work/other.log" &&
        [[ $(bun_pid "${targets[0]}") == "$before" ]]; then
        pass "a second operator" "refused at the machine checks; node 1's Bun kept pid $before"
    else
        fail "a second operator" "$(tail -3 "$work/other.log" | tr '\n' ' ')"
    fi
fi

finished=$(date -u +%Y-%m-%dT%H:%M:%SZ)
verdict=PASS
[[ $failed == no ]] || verdict=FAIL
{
    printf '# relish setup --ssh: %s\n\n' "$verdict"
    printf '%s to %s. %s disposable Lima VMs from `scripts/release/ssh-setup-vm.yaml`, built into a cluster by `scripts/release/qualify-ssh-setup.sh`.\n\n' "$started" "$finished" "$machines"
    printf '| | |\n|---|---|\n'
    printf '| relish | `%s` |\n' "$("$relish" --version)"
    printf '| Host | %s |\n' "$(sw_vers -productName 2>/dev/null || uname -s) $(sw_vers -productVersion 2>/dev/null || uname -r), $(uname -m)"
    printf '| Lima | %s |\n' "$(limactl --version)"
    printf '| Setup options | `%s` |\n\n' "${setup_options[*]:-none}"
    printf '| Step | Result | Detail |\n|---|---|---|\n'
    printf '%s\n' "${results[@]}"
    printf '\n## Setup\n\n```\n'
    sed 's/[[:cntrl:]]\[[0-9;]*m//g' "$work/setup.log"
    printf '```\n'
    if [[ -n ${wtf:-} ]]; then
        printf '\n## relish wtf\n\n```\n%s\n```\n' "$wtf"
    fi
} >"$record"
echo "record: $record"
[[ $failed == no ]]
