#!/usr/bin/env bash
#
# Run COMMAND (the nextest run `make test-s3` passes in) against an S3 bucket.
#
# The two S3 tests read the bucket from RELIABURGER_TEST_S3_URL and the
# client's endpoint, region and credentials from the standard AWS_*
# variables. This script supplies them in one of three ways:
#
#   1. RELIABURGER_TEST_S3_URL is set: a real bucket. COMMAND runs as is,
#      with whatever AWS_* credentials and region the caller exported.
#   2. RELIABURGER_TEST_S3_ENDPOINT is set: an S3 server someone else
#      started, such as CI's SeaweedFS service container. The bucket is
#      RELIABURGER_TEST_S3_BUCKET (default reliaburger-test), and the
#      credentials are the AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY the
#      server was started with.
#   3. Neither: start a throwaway SeaweedFS server (`weed mini`) on free
#      loopback ports, with a random bucket and random credentials, and
#      remove it and its data when COMMAND ends, pass or fail. The pinned
#      `weed` binary is downloaded once into target/test-s3 and checked
#      against its SHA-256 digest.
#
# Usage: scripts/test-s3.sh COMMAND [ARGUMENT...]

set -euo pipefail

# The same release as CI's service container (.github/workflows/ci.yml).
SEAWEEDFS_VERSION=4.48
REPO=$(cd "$(dirname "$0")/.." && pwd)

die() {
    echo "test-s3: $*" >&2
    exit 1
}

[ $# -gt 0 ] || die "usage: scripts/test-s3.sh COMMAND [ARGUMENT...]"

if [ -n "${RELIABURGER_TEST_S3_URL:-}" ]; then
    echo "test-s3: using the bucket at $RELIABURGER_TEST_S3_URL" >&2
    exec "$@"
fi

# A path-style request to a local server, signed the way object_store signs
# it. Answers 200 once the server is up and the bucket exists.
bucket_status() {
    curl -s -o /dev/null -w '%{http_code}' --max-time 5 \
        --aws-sigv4 "aws:amz:us-east-1:s3" \
        --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY" \
        "$1/$2/" || true
}

# Point the S3 client at ENDPOINT and BUCKET, and nowhere else: a session
# token or profile left in the caller's environment would be sent to the
# local server, or send the client to AWS.
use_local_server() {
    local endpoint=$1 bucket=$2 key=$3 secret=$4 name
    for name in $(compgen -e); do
        case "$name" in AWS_*) unset "$name" ;; esac
    done
    export AWS_ENDPOINT=$endpoint AWS_ALLOW_HTTP=true AWS_REGION=us-east-1
    export AWS_ACCESS_KEY_ID=$key AWS_SECRET_ACCESS_KEY=$secret
    export RELIABURGER_TEST_S3_URL="s3://$bucket/test-s3-$(date +%s)-$$"
}

wait_for_bucket() {
    local endpoint=$1 bucket=$2 seconds=$3 pid=${4:-}
    local deadline=$((SECONDS + seconds))
    while [ "$(bucket_status "$endpoint" "$bucket")" != 200 ]; do
        if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then
            return 1
        fi
        [ "$SECONDS" -lt "$deadline" ] || return 1
        sleep 0.5
    done
}

if [ -n "${RELIABURGER_TEST_S3_ENDPOINT:-}" ]; then
    bucket=${RELIABURGER_TEST_S3_BUCKET:-reliaburger-test}
    [ -n "${AWS_ACCESS_KEY_ID:-}" ] && [ -n "${AWS_SECRET_ACCESS_KEY:-}" ] ||
        die "RELIABURGER_TEST_S3_ENDPOINT needs the server's AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY"
    use_local_server "$RELIABURGER_TEST_S3_ENDPOINT" "$bucket" "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY"
    wait_for_bucket "$AWS_ENDPOINT" "$bucket" 120 ||
        die "no bucket $bucket at $AWS_ENDPOINT after 120 seconds"
    echo "test-s3: using $RELIABURGER_TEST_S3_URL at $AWS_ENDPOINT" >&2
    exec "$@"
fi

# Release tarballs from https://github.com/seaweedfs/seaweedfs/releases/tag/4.48,
# with the SHA-256 digests GitHub records for each asset.
case "$(uname -s)/$(uname -m)" in
    Darwin/arm64) asset=darwin_arm64 digest=faa194e72a346755dc4404288b0561eb3c260351702901d8363024d5099bdd7d ;;
    Darwin/x86_64) asset=darwin_amd64 digest=425ef61d2ee773d5d5175003f38bd6cc891070ae6319370354aaf5378c45d5b3 ;;
    Linux/x86_64) asset=linux_amd64 digest=4a7d108384d044d95212d1342cdda9533fa55842c1c9b41f606ca3c8a9561124 ;;
    Linux/aarch64 | Linux/arm64) asset=linux_arm64 digest=557e92c38c5c7d180748eb8e2b2b45e426f8e5282a4caca8e098a90d09f03a07 ;;
    *) die "no pinned SeaweedFS build for $(uname -s)/$(uname -m); set RELIABURGER_TEST_S3_URL or RELIABURGER_TEST_S3_ENDPOINT" ;;
esac

sha256() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

cache="$REPO/target/test-s3/seaweedfs-$SEAWEEDFS_VERSION-$asset"
weed="$cache/weed"
if [ ! -x "$weed" ]; then
    mkdir -p "$cache"
    download=$(mktemp -d "$cache/download.XXXXXX")
    url="https://github.com/seaweedfs/seaweedfs/releases/download/$SEAWEEDFS_VERSION/$asset.tar.gz"
    echo "test-s3: downloading $url" >&2
    curl -fsSL --retry 3 -o "$download/weed.tar.gz" "$url"
    actual=$(sha256 "$download/weed.tar.gz")
    if [ "$actual" != "$digest" ]; then
        rm -rf "$download"
        die "$url has digest $actual, expected $digest"
    fi
    tar -xzf "$download/weed.tar.gz" -C "$download" weed
    mv "$download/weed" "$weed"
    rm -rf "$download"
fi

# Five free loopback ports for the S3 gateway, master, volume server, filer
# and admin server. SeaweedFS also listens on each port plus 10000 for gRPC,
# so both must be free and the pair must stay below the ephemeral range,
# where an outbound connection could take the gRPC port first.
FREE_PORTS='
import random, socket

def free(port):
    with socket.socket() as probe:
        try:
            probe.bind(("127.0.0.1", port))
        except OSError:
            return False
    return True

chosen = []
while len(chosen) < 5:
    port = random.randrange(10000, 22000)
    if port not in chosen and free(port) and free(port + 10000):
        chosen.append(port)
print(" ".join(map(str, chosen)))
'
ports=$(python3 -I -c "$FREE_PORTS")
read -r s3_port master_port volume_port filer_port admin_port <<<"$ports"

temporary=${TMPDIR:-/tmp}
work=$(mktemp -d "${temporary%/}/rb-test-s3.XXXXXX")
weed_pid=""
cleanup() {
    if [ -n "$weed_pid" ] && kill -0 "$weed_pid" 2>/dev/null; then
        kill "$weed_pid" 2>/dev/null || true
        for _ in $(seq 1 20); do
            kill -0 "$weed_pid" 2>/dev/null || break
            sleep 0.5
        done
        kill -9 "$weed_pid" 2>/dev/null || true
        wait "$weed_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
    echo "test-s3: stopped SeaweedFS and removed $work" >&2
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

random_hex() {
    od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'
}
bucket="reliaburger-test-$(random_hex 4)"
key="rb$(random_hex 8)"
secret=$(random_hex 20)

# `weed mini` reads the admin credentials from these two variables and
# creates the bucket once it's up. Telemetry is off: a test run reports
# nothing to seaweedfs.com.
AWS_ACCESS_KEY_ID=$key AWS_SECRET_ACCESS_KEY=$secret "$weed" mini \
    -dir="$work/data" -ip=127.0.0.1 -ip.bind=127.0.0.1 \
    -s3.port="$s3_port" -master.port="$master_port" -volume.port="$volume_port" \
    -filer.port="$filer_port" -admin.port="$admin_port" \
    -filer.localSocket="$work/filer.sock" -s3.localSocket="$work/s3.sock" \
    -webdav=false -s3.port.iceberg=0 -s3.port.lance=0 -master.telemetry=false \
    -bucket="$bucket" >"$work/weed.log" 2>&1 &
weed_pid=$!

use_local_server "http://127.0.0.1:$s3_port" "$bucket" "$key" "$secret"
if ! wait_for_bucket "$AWS_ENDPOINT" "$bucket" 120 "$weed_pid"; then
    tail -n 40 "$work/weed.log" >&2 || true
    die "SeaweedFS didn't serve bucket $bucket at $AWS_ENDPOINT within 120 seconds"
fi
echo "test-s3: SeaweedFS $SEAWEEDFS_VERSION serving $RELIABURGER_TEST_S3_URL at $AWS_ENDPOINT" >&2

status=0
"$@" || status=$?
if [ "$status" -ne 0 ]; then
    echo "test-s3: the last lines SeaweedFS logged:" >&2
    tail -n 40 "$work/weed.log" >&2 || true
fi
exit "$status"
