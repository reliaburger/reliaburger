#!/usr/bin/env bash
# shellcheck disable=SC2016  # backquotes in printf formats are Markdown
# V04: one cold install of the published release on a clean Linux host, the
# way a user meets it (#288). Pipes https://reliaburger.com/install.sh to
# bash with --laptop --timings in the real home, checks the cluster it
# builds, destroys it, uninstalls, and shows what the host looks like after.
#
# Run it once per fresh host: a new Lima VM on a Mac, or a new GitHub-hosted
# runner. It changes ~/.reliaburger and may change ~/.local/bin, so never run
# it on a machine you care about.
#
# Usage:
#   scripts/release/qualify-cold-install.sh [--prepare] [--evidence DIR]
#
#   --prepare        first install QEMU and give this user /dev/kvm (apt and
#                    sudo; Ubuntu or Debian), the prerequisites the quickstart
#                    expects. Timed apart from the install.
#   --evidence DIR   where the logs, timings.json and summary.json go
#                    (default: a fresh directory under /tmp)
#   --bootstrap URL  the bootstrap to pipe (default: https://reliaburger.com/install.sh)
set -euo pipefail

usage() { sed -n '3,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
fail() { printf 'cold-install: %s\n' "$*" >&2; exit 1; }

prepare=false
evidence=
bootstrap=https://reliaburger.com/install.sh
while [ "$#" -gt 0 ]; do
    case $1 in
        --prepare) prepare=true; shift ;;
        --evidence) evidence=${2:-}; shift 2 ;;
        --bootstrap) bootstrap=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; fail "unknown argument: $1" ;;
    esac
done
[ "$(uname -s)" = Linux ] || fail 'this measures a Linux host'
[ -n "$evidence" ] || evidence=$(mktemp -d "${TMPDIR:-/tmp}/reliaburger-cold-install.XXXXXX")
mkdir -p "$evidence"
relish=$HOME/.reliaburger/bin/relish

# The host as the record names it.
{
    printf 'kernel: %s\n' "$(uname -srm)"
    # shellcheck source=/dev/null
    printf 'os: %s\n' "$(. /etc/os-release && printf '%s' "$PRETTY_NAME")"
    # arm64's /proc/cpuinfo has no model name; lscpu has a vendor at least.
    printf 'cpu: %s\n' "$(lscpu | awk -F': *' '/^Vendor ID/ {v = $2} /^Model name/ {m = $2} END {print v ", " m}')"
    printf 'cores: %s\n' "$(nproc)"
    printf 'memory_gib: %s\n' "$(awk '/^MemTotal/ {print int($2 / 1048576); exit}' /proc/meminfo)"
    printf 'disk_free: %s\n' "$(df -h "$HOME" | awk 'NR == 2 {print $4}')"
} | tee "$evidence/host.txt"

# What a clean host has, and what it must have again after uninstall.
host_state() {
    printf -- '--- ~/.reliaburger\n'; find "$HOME/.reliaburger" -maxdepth 1 2>&1 | sort || true
    printf -- '--- ~/.local/bin/relish\n'; ls -l "$HOME/.local/bin/relish" 2>&1 || true
    printf -- '--- rc files (SHA-256)\n'
    for rc in .bashrc .profile .bash_profile .zshrc; do
        if [ -f "$HOME/$rc" ]; then sha256sum "$HOME/$rc"; else printf 'absent  %s\n' "$rc"; fi
    done
    printf -- '--- processes (qemu, limactl, bun)\n'
    pgrep -a '^(qemu-system|limactl|bun)' || printf 'none\n'
    printf -- '--- listening TCP ports\n'
    ss -Htln | awk '{print $4}' | sort -u
}

if [ "$prepare" = true ]; then
    started=$(date +%s)
    case $(uname -m) in
        aarch64) packages='qemu-system-arm qemu-efi-aarch64 qemu-utils' ;;
        x86_64) packages='qemu-system-x86 ovmf qemu-utils' ;;
        *) fail "no QEMU packages known for $(uname -m)" ;;
    esac
    sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq
    # shellcheck disable=SC2086  # a list of package names
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq $packages >/dev/null
    # A udev rule rather than the kvm group: group membership needs a new
    # login session, and this script runs in the one it was given.
    echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' \
        | sudo tee /etc/udev/rules.d/99-reliaburger-kvm.rules >/dev/null
    sudo udevadm control --reload-rules
    sudo udevadm trigger --name-match=kvm
    printf 'prepare_seconds: %s\npackages: %s\n' "$(( $(date +%s) - started ))" "$packages" \
        | tee "$evidence/prepare.txt"
fi
[ -r /dev/kvm ] && [ -w /dev/kvm ] || fail '/dev/kvm is not readable and writable by this user'

host_state > "$evidence/before.txt"
[ ! -e "$HOME/.reliaburger" ] || fail "$HOME/.reliaburger exists; this isn't a clean host"

curl -fsSL "$bootstrap" -o "$evidence/bootstrap.sh"
bootstrap_digest=$(sha256sum "$evidence/bootstrap.sh" | awk '{print $1}')

# The pipeline a user types. The bootstrap needs a mode, so --laptop; it
# passes --timings on to `relish setup --quickstart`.
started=$(date +%s.%N)
install_status=0
curl -fsSL "$bootstrap" | bash -s -- --laptop --timings 2>&1 | tee "$evidence/install.log" \
    || install_status=$?
install_seconds=$(awk -v a="$started" -v b="$(date +%s.%N)" 'BEGIN {printf "%.1f", b - a}')
cp "$HOME/.reliaburger/clusters/laptop/timings.json" "$evidence/timings.json" 2>/dev/null || true

# The cluster answers and runs the sample app.
checks_status=0
if [ "$install_status" -eq 0 ]; then
    "$relish" --version > "$evidence/version.txt" 2>&1 || checks_status=1
    "$relish" status -o json > "$evidence/status.json" 2>&1 || checks_status=1
    "$relish" apps -o json > "$evidence/apps.json" 2>&1 || checks_status=1
    "$relish" local status --name laptop -o json > "$evidence/local-status.json" 2>&1 || checks_status=1
    curl -fsS --max-time 10 http://localhost:18080/ -o "$evidence/hello.html" || checks_status=1
fi

# Teardown the way the setup output says, then uninstall.
teardown_status=0
if [ -x "$relish" ]; then
    "$relish" local destroy --name laptop --yes -o json > "$evidence/destroy.json" 2>&1 || teardown_status=1
    "$relish" uninstall --yes -o json > "$evidence/uninstall.json" 2>&1 || teardown_status=1
fi
host_state > "$evidence/after.txt"
clean_status=0
diff -u "$evidence/before.txt" "$evidence/after.txt" > "$evidence/state.diff" || clean_status=1

setup_seconds=$(sed -n 's/.*"total_seconds": *\([0-9.]*\).*/\1/p' "$evidence/timings.json" 2>/dev/null | head -n 1)
under_five_minutes=false
awk -v s="$install_seconds" 'BEGIN {exit !(s < 300)}' && under_five_minutes=true
cat > "$evidence/summary.json" <<JSON
{
  "bootstrap": "$bootstrap",
  "bootstrap_sha256": "$bootstrap_digest",
  "install_exit": $install_status,
  "install_wall_seconds": $install_seconds,
  "setup_total_seconds": ${setup_seconds:-null},
  "under_five_minutes": $under_five_minutes,
  "checks_passed": $([ "$checks_status" -eq 0 ] && [ "$install_status" -eq 0 ] && echo true || echo false),
  "teardown_passed": $([ "$teardown_status" -eq 0 ] && echo true || echo false),
  "host_clean_after": $([ "$clean_status" -eq 0 ] && echo true || echo false)
}
JSON
cat "$evidence/summary.json"
printf 'evidence: %s\n' "$evidence"
[ "$install_status" -eq 0 ] && [ "$checks_status" -eq 0 ] && [ "$teardown_status" -eq 0 ] \
    && [ "$clean_status" -eq 0 ] && [ "$under_five_minutes" = true ]
