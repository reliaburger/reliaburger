#!/usr/bin/env bash
# The k3s lab for the Kubernetes side of the agent comparison (#1679).
#
# Three Lima VMs (gymk8s-1, -2, -3) on one user-v2 network, with k3s pinned
# in versions.env. gymk8s-1 is the server; the others are agents. Node names
# are gym-1, gym-2 and gym-3, as on the gym's Reliaburger clusters. The
# solver reaches the API server at https://127.0.0.1:16443 with a read-only
# service account (rbac.yaml); the admin kubeconfig never leaves gymk8s-1.
#
#   lab.sh tools              download and check kubectl, the Kubernetes MCP
#                             server, kubectl-ai, HolmesGPT, k3s and its
#                             images into $GYM_K8S_HOME (no VMs)
#   lab.sh create             create and start the three VMs, and import the
#                             images every node needs
#   lab.sh reset --nodes N    a fresh k3s on the first N VMs (1 to 3): what
#                             the gym runs before every run, as the
#                             Reliaburger side starts a fresh cluster
#   lab.sh versions           what the lab runs, for the record
#   lab.sh delete             delete the three VMs
#
# GYM_LAB_DRY_RUN=1 prints each command instead of running it, which is how
# the portable tests check the steps without a VM. Nothing here touches a VM
# whose name doesn't start with gymk8s-.
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=/dev/null
. "$here/versions.env"

GYM_K8S_HOME=${GYM_K8S_HOME:-$HOME/github/rb-wt/gym-k8s}
LIMACTL=${GYM_LIMACTL:-limactl}
DRY_RUN=${GYM_LAB_DRY_RUN:-}
VMS=(gymk8s-1 gymk8s-2 gymk8s-3)
API_PORT=16443
# The kubelet's grace before the node controller marks a silent node
# NotReady, set rather than left to the default, so the partition
# scenario's settle (70 s) is long enough on any k3s.
GRACE=50s

say() { echo "lab: $*" >&2; }
die() { echo "lab: $*" >&2; exit 1; }

run() {
    if [ -n "$DRY_RUN" ]; then
        printf '+ %s\n' "$*"
        return 0
    fi
    "$@"
}

# A command whose output the script needs. In a dry run, a placeholder.
capture() {
    local placeholder=$1; shift
    if [ -n "$DRY_RUN" ]; then
        printf '+ %s\n' "$*" >&2
        echo "$placeholder"
        return 0
    fi
    "$@"
}

on() {
    local vm=$1; shift
    case "$vm" in gymk8s-*) ;; *) die "refusing to touch VM $vm" ;; esac
    run "$LIMACTL" shell "$vm" sudo sh -c "$*"
}

arch() {
    case "$(uname -m)" in
        arm64|aarch64) echo arm64 ;;
        x86_64|amd64) echo amd64 ;;
        *) die "unsupported architecture $(uname -m)" ;;
    esac
}

sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }

download() {
    local url=$1 out=$2
    [ -s "$out" ] && return 0
    run curl -fsSL --retry 3 -o "$out.part" "$url"
    run mv "$out.part" "$out"
}

# Checks a download against a checksum file that lists it; records the
# digest either way, so the record can quote what ran.
check_sum() {
    local file=$1 sums=$2 name=$3
    if [ -n "$DRY_RUN" ]; then
        printf '+ check %s against %s\n' "$file" "$sums"
        return 0
    fi
    local want got
    want=$(awk -v n="$name" '$2 == n || $2 == "*"n { print $1 }' "$sums")
    got=$(sha256_of "$file")
    [ -n "$want" ] || die "$sums lists no $name"
    [ "$want" = "$got" ] || die "$file: sha256 $got, expected $want"
}

record_sum() {
    [ -n "$DRY_RUN" ] && return 0
    printf '%s  %s\n' "$(sha256_of "$1")" "$(basename "$1")" >> "$GYM_K8S_HOME/SHA256SUMS"
}

command_tools() {
    local a os bin
    a=$(arch)
    os=$(uname -s | tr '[:upper:]' '[:lower:]')
    bin="$GYM_K8S_HOME/bin"
    run mkdir -p "$bin" "$GYM_K8S_HOME/k3s" "$GYM_K8S_HOME/downloads"
    [ -n "$DRY_RUN" ] || : > "$GYM_K8S_HOME/SHA256SUMS"

    # k3s for the VMs (always Linux), its air-gap images and install script.
    local k3s_tag k3s_url suffix
    k3s_tag=${K3S_VERSION//+/%2B}
    k3s_url="https://github.com/k3s-io/k3s/releases/download/$k3s_tag"
    suffix=""; [ "$a" = arm64 ] && suffix="-arm64"
    download "$k3s_url/k3s$suffix" "$GYM_K8S_HOME/k3s/k3s"
    download "$k3s_url/k3s-airgap-images-$a.tar.zst" "$GYM_K8S_HOME/k3s/k3s-airgap-images.tar.zst"
    download "$k3s_url/sha256sum-$a.txt" "$GYM_K8S_HOME/k3s/sha256sum.txt"
    check_sum "$GYM_K8S_HOME/k3s/k3s" "$GYM_K8S_HOME/k3s/sha256sum.txt" "k3s$suffix"
    check_sum "$GYM_K8S_HOME/k3s/k3s-airgap-images.tar.zst" "$GYM_K8S_HOME/k3s/sha256sum.txt" \
        "k3s-airgap-images-$a.tar.zst"
    download "https://raw.githubusercontent.com/k3s-io/k3s/$k3s_tag/install.sh" "$GYM_K8S_HOME/k3s/install.sh"
    record_sum "$GYM_K8S_HOME/k3s/install.sh"

    # kubectl for the host, with its published checksum.
    download "https://dl.k8s.io/release/$KUBECTL_VERSION/bin/$os/$a/kubectl" "$bin/kubectl"
    download "https://dl.k8s.io/release/$KUBECTL_VERSION/bin/$os/$a/kubectl.sha256" \
        "$GYM_K8S_HOME/downloads/kubectl.sha256"
    if [ -z "$DRY_RUN" ]; then
        [ "$(cat "$GYM_K8S_HOME/downloads/kubectl.sha256")" = "$(sha256_of "$bin/kubectl")" ] \
            || die "kubectl: checksum mismatch"
    fi

    # The Kubernetes MCP server. Its releases publish no checksum file, so the
    # digest is recorded on first download and quoted in the record.
    download "https://github.com/containers/kubernetes-mcp-server/releases/download/v$KUBERNETES_MCP_SERVER_VERSION/kubernetes-mcp-server-$os-$a" \
        "$bin/kubernetes-mcp-server"
    record_sum "$bin/kubernetes-mcp-server"

    # kubectl-ai, checked against its release's checksums.
    local kai_os kai_arch kai_tar
    kai_os=$(uname -s); kai_arch=$a; [ "$a" = amd64 ] && kai_arch=x86_64
    kai_tar="kubectl-ai_${kai_os}_${kai_arch}.tar.gz"
    download "https://github.com/GoogleCloudPlatform/kubectl-ai/releases/download/v$KUBECTL_AI_VERSION/$kai_tar" \
        "$GYM_K8S_HOME/downloads/$kai_tar"
    download "https://github.com/GoogleCloudPlatform/kubectl-ai/releases/download/v$KUBECTL_AI_VERSION/kubectl-ai_${KUBECTL_AI_VERSION}_checksums.txt" \
        "$GYM_K8S_HOME/downloads/kubectl-ai-checksums.txt"
    check_sum "$GYM_K8S_HOME/downloads/$kai_tar" "$GYM_K8S_HOME/downloads/kubectl-ai-checksums.txt" "$kai_tar"
    run tar -xzf "$GYM_K8S_HOME/downloads/$kai_tar" -C "$bin" kubectl-ai

    # HolmesGPT from PyPI into its own virtualenv: its release binaries
    # cover darwin-arm64 and linux-amd64 only, and PyPI has every platform.
    run python3 -m venv "$GYM_K8S_HOME/holmes"
    run "$GYM_K8S_HOME/holmes/bin/pip" install --quiet "holmesgpt==$HOLMESGPT_VERSION"
    run ln -sf "$GYM_K8S_HOME/holmes/bin/holmes" "$bin/holmes"

    run chmod 0755 "$bin/kubectl" "$bin/kubernetes-mcp-server"
    say "tools in $bin; digests in $GYM_K8S_HOME/SHA256SUMS"
}

command_create() {
    command -v "$LIMACTL" >/dev/null || [ -n "$DRY_RUN" ] || die "limactl isn't installed"
    [ -s "$GYM_K8S_HOME/k3s/k3s" ] || [ -n "$DRY_RUN" ] || die "run lab.sh tools first"
    local vm
    for vm in "${VMS[@]}"; do
        if [ "$vm" = gymk8s-1 ]; then
            run "$LIMACTL" create --name "$vm" --tty=false \
                --set ".portForwards = [{\"guestPort\": 6443, \"hostIP\": \"127.0.0.1\", \"hostPort\": $API_PORT}] + .portForwards" \
                "$here/lima.yaml"
        else
            run "$LIMACTL" create --name "$vm" --tty=false "$here/lima.yaml"
        fi
    done
    for vm in "${VMS[@]}"; do run "$LIMACTL" start --tty=false --timeout 20m "$vm"; done
    for vm in "${VMS[@]}"; do
        run "$LIMACTL" copy "$GYM_K8S_HOME/k3s/k3s" "$vm:/tmp/k3s"
        run "$LIMACTL" copy "$GYM_K8S_HOME/k3s/install.sh" "$vm:/tmp/k3s-install.sh"
        run "$LIMACTL" copy "$GYM_K8S_HOME/k3s/k3s-airgap-images.tar.zst" "$vm:/tmp/k3s-airgap-images.tar.zst"
        run "$LIMACTL" copy "$here/audit-policy.yaml" "$vm:/tmp/audit-policy.yaml"
        on "$vm" "install -m 0755 /tmp/k3s /usr/local/bin/k3s && install -d /opt/gym/images /etc/gym \
            && install -m 0644 /tmp/k3s-airgap-images.tar.zst /opt/gym/images/ \
            && install -m 0755 /tmp/k3s-install.sh /opt/gym/k3s-install.sh \
            && install -m 0644 /tmp/audit-policy.yaml /etc/gym/audit-policy.yaml"
    done
    # The join token every reset uses, made once.
    on gymk8s-1 "head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' > /etc/gym/token"
    # One k3s install to pull the workload image, exported for every reset.
    command_reset_nodes 1
    on gymk8s-1 "k3s ctr images pull $GYM_IMAGE && k3s ctr images export /opt/gym/images/gym-image.tar $GYM_IMAGE"
    run "$LIMACTL" copy gymk8s-1:/opt/gym/images/gym-image.tar "$GYM_K8S_HOME/k3s/gym-image.tar"
    for vm in gymk8s-2 gymk8s-3; do
        run "$LIMACTL" copy "$GYM_K8S_HOME/k3s/gym-image.tar" "$vm:/tmp/gym-image.tar"
        on "$vm" "install -m 0644 /tmp/gym-image.tar /opt/gym/images/gym-image.tar"
    done
    say "created ${VMS[*]}; the API server is at https://127.0.0.1:$API_PORT"
}

address_of() {
    capture "<address of $1>" "$LIMACTL" shell "$1" sh -c \
        "ip -4 -o addr show scope global | awk 'NR == 1 { split(\$4, a, \"/\"); print a[1] }'"
}

interface_of() {
    capture "<interface of $1>" "$LIMACTL" shell "$1" sh -c \
        "ip -4 -o addr show scope global | awk 'NR == 1 { print \$2 }'"
}

uninstall() {
    on "$1" "iptables -F GYM-PARTITION 2>/dev/null; systemctl stop gym-heal.timer gym-heal.service 2>/dev/null; \
        if [ -x /usr/local/bin/k3s-uninstall.sh ]; then /usr/local/bin/k3s-uninstall.sh; fi; \
        if [ -x /usr/local/bin/k3s-agent-uninstall.sh ]; then /usr/local/bin/k3s-agent-uninstall.sh; fi; \
        install -m 0755 /opt/gym/k3s /usr/local/bin/k3s 2>/dev/null || true; true"
}

# k3s-uninstall.sh removes /usr/local/bin/k3s, so keep a copy to put back.
keep_binary() {
    on "$1" "test -x /opt/gym/k3s || install -m 0755 /usr/local/bin/k3s /opt/gym/k3s"
}

images_in() {
    on "$1" "install -d /var/lib/rancher/k3s/agent/images && cp /opt/gym/images/* /var/lib/rancher/k3s/agent/images/"
}

command_reset_nodes() {
    local nodes=$1 i vm server_ip ip iface
    case "$nodes" in 1|2|3) ;; *) die "--nodes must be 1, 2 or 3" ;; esac
    for vm in "${VMS[@]}"; do keep_binary "$vm"; uninstall "$vm"; done
    server_ip=$(address_of gymk8s-1)
    iface=$(interface_of gymk8s-1)
    images_in gymk8s-1
    on gymk8s-1 "INSTALL_K3S_SKIP_DOWNLOAD=true INSTALL_K3S_VERSION='$K3S_VERSION' \
        K3S_TOKEN=\$(cat /etc/gym/token 2>/dev/null || echo gym-lab) \
        INSTALL_K3S_EXEC='server --node-name gym-1 --node-ip $server_ip --flannel-iface $iface \
        --write-kubeconfig-mode 0600 \
        --kube-apiserver-arg=audit-policy-file=/etc/gym/audit-policy.yaml \
        --kube-apiserver-arg=audit-log-path=/var/lib/rancher/k3s/server/logs/audit.log \
        --kube-apiserver-arg=audit-log-maxage=1 \
        --kube-controller-manager-arg=node-monitor-grace-period=$GRACE' \
        /opt/gym/k3s-install.sh"
    for i in $(seq 2 "$nodes"); do
        vm=gymk8s-$i
        ip=$(address_of "$vm")
        iface=$(interface_of "$vm")
        images_in "$vm"
        on "$vm" "INSTALL_K3S_SKIP_DOWNLOAD=true INSTALL_K3S_VERSION='$K3S_VERSION' \
            K3S_URL=https://$server_ip:6443 K3S_TOKEN='$(capture '<token>' "$LIMACTL" shell gymk8s-1 sudo cat /etc/gym/token 2>/dev/null || echo gym-lab)' \
            INSTALL_K3S_EXEC='agent --node-name gym-$i --node-ip $ip --flannel-iface $iface' \
            /opt/gym/k3s-install.sh"
    done
    on gymk8s-1 "for _ in \$(seq 90); do [ \$(k3s kubectl get nodes --no-headers 2>/dev/null | wc -l) -eq $nodes ] && break; sleep 2; done; \
        k3s kubectl wait --for=condition=Ready node --all --timeout=180s && \
        k3s kubectl -n kube-system rollout status deployment/coredns --timeout=180s"
    run "$LIMACTL" copy "$here/rbac.yaml" gymk8s-1:/tmp/gym-rbac.yaml
    on gymk8s-1 "k3s kubectl apply -f /tmp/gym-rbac.yaml"
    say "k3s $K3S_VERSION on $nodes node(s)"
}

command_versions() {
    echo "k3s: $K3S_VERSION"
    echo "kubectl: $KUBECTL_VERSION"
    echo "lima: $(capture "<limactl --version>" "$LIMACTL" --version)"
    echo "image: $GYM_IMAGE"
    echo "kubernetes-mcp-server: $KUBERNETES_MCP_SERVER_VERSION"
    echo "kubectl-ai: $KUBECTL_AI_VERSION"
    echo "holmesgpt: $HOLMESGPT_VERSION"
    if [ -s "$GYM_K8S_HOME/SHA256SUMS" ]; then sed 's/^/sha256: /' "$GYM_K8S_HOME/SHA256SUMS"; fi
}

command_delete() {
    local vm
    for vm in "${VMS[@]}"; do run "$LIMACTL" delete --force "$vm" || true; done
}

case "${1:-}" in
    tools) command_tools ;;
    create) command_create ;;
    reset)
        [ "${2:-}" = --nodes ] || die "usage: lab.sh reset --nodes N"
        command_reset_nodes "${3:-}" ;;
    versions) command_versions ;;
    delete) command_delete ;;
    *) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
