# Built by `relish build burger.toml` on a cluster node, with Buildah, for
# linux/amd64 and linux/arm64.
#
# The build stage runs on the builder's own architecture ($BUILDPLATFORM) and
# cross-compiles for each target ($TARGETARCH). Go does that natively, so
# neither platform needs an emulator, and the final stage has no RUN step to
# emulate either: it only copies the static binary into an empty image.
#
# The Go image comes from the ECR mirror of Docker Hub's official image
# (no rate limits), pinned by digest: golang 1.27.1-alpine.
FROM --platform=$BUILDPLATFORM public.ecr.aws/docker/library/golang:1.27.1-alpine@sha256:8a5910f31396cd4d89662f56c68b3ae31d374308270a1c3bd96672ee5ed43414 AS build
ARG TARGETARCH
WORKDIR /src
COPY go.mod main.go batch.go ./
# Standard library only: nothing to download, so no network during the build.
RUN --network=none CGO_ENABLED=0 GOOS=linux GOARCH=$TARGETARCH GOTOOLCHAIN=local \
    go build -trimpath -ldflags="-s -w" -o /out/burger .

FROM scratch
COPY --from=build /out/burger /burger
USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["/burger"]
