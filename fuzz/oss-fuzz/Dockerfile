# Draft of projects/reliaburger/Dockerfile for the OSS-Fuzz repository (#1414).
#
# base-builder-rust carries nightly Rust, cargo-fuzz and clang. build.rs
# compiles a static C helper on Linux, which the image's libc6-dev covers.
FROM gcr.io/oss-fuzz-base/base-builder-rust
RUN git clone --depth 1 https://github.com/reliaburger/reliaburger reliaburger
WORKDIR $SRC/reliaburger
COPY build.sh $SRC/
