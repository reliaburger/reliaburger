# The portable development environment: what `make ci` needs on Linux, at
# the versions CI pins (#1584). tests/suite/devcontainer.rs fails when a
# version here drifts from .github/workflows/ci.yml, .config/nextest.toml or
# .github/workflows/mutants.yml.
#
# Only the portable suite runs in here. The gated suites (`make test-linux`,
# `make test-cluster`, `make test-apple` and the rest) need root, runc,
# network namespaces, a kernel that loads eBPF, or a Mac.

# The same Ubuntu release as CI's runners (ubuntu-24.04).
FROM ubuntu:24.04

# The compiler CI's portable, lint and source-bundle jobs install with
# dtolnay/rust-toolchain. Bump it with them.
ARG RUST_VERSION=1.98.0
# .config/nextest.toml's nextest-version, which every CI job installs.
ARG NEXTEST_VERSION=0.9.145
# The cargo-mutants mutants.yml installs, for killing a missed mutant locally.
ARG MUTANTS_VERSION=27.1.0

# build-essential: gcc and libc.a, since build.rs links the job executor
#   helpers with `cc -static` on Linux.
# clang, llvm, libbpf-dev: `make lint` builds with --all-features, so build.rs
#   compiles the eBPF programs in ebpf/.
# python3 (3.12): `make test-ci-scripts` needs 3.11 or later.
# git, openssl, openssh-client, procps, iproute2: portable tests run them.
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential ca-certificates clang curl git iproute2 jq libbpf-dev \
        llvm openssh-client openssl pkg-config procps python3 xz-utils \
    && rm -rf /var/lib/apt/lists/*

# Shared toolchain directories, owned by the image's own non-root user
# `ubuntu` (uid 1000): some portable tests check file permissions, which root
# ignores.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN mkdir -p "$RUSTUP_HOME" "$CARGO_HOME" \
    && chown ubuntu:ubuntu "$RUSTUP_HOME" "$CARGO_HOME"
USER ubuntu

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal \
            --default-toolchain "$RUST_VERSION" --component rustfmt,clippy \
    && rustc --version

# Prebuilt binaries where upstream publishes them. cargo-mutants has no
# aarch64 Linux build, so on an arm64 host (an Apple-silicon Mac) it compiles.
RUN case "$(uname -m)" in \
        x86_64) nextest=linux ;; \
        aarch64) nextest=linux-arm ;; \
        *) echo "unsupported architecture $(uname -m)" >&2; exit 1 ;; \
    esac \
    && curl -LsSf "https://get.nexte.st/$NEXTEST_VERSION/$nextest" | tar zxf - -C "$CARGO_HOME/bin" \
    && cargo nextest --version
RUN if [ "$(uname -m)" = x86_64 ]; then \
        curl -LsSf "https://github.com/sourcefrog/cargo-mutants/releases/download/v$MUTANTS_VERSION/cargo-mutants-x86_64-unknown-linux-gnu.tar.gz" \
            | tar zxf - -C "$CARGO_HOME/bin"; \
    else \
        cargo install --locked "cargo-mutants@$MUTANTS_VERSION" && rm -rf "$CARGO_HOME/registry"; \
    fi \
    && cargo mutants --version
