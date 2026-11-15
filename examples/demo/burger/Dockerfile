# Built by `relish build burger.toml` on a cluster node, with Buildah, for
# linux/amd64 and linux/arm64.
#
# The build stage runs on the builder's own architecture ($BUILDPLATFORM) and
# cross-compiles for each target ($TARGETARCH). Go does that natively, so
# neither platform needs an emulator, and the final stage has no RUN step to
# emulate either: it only copies the static binary into Alpine.
#
# Alpine rather than an empty image because `relish path` makes its TCP
# connects from inside the source container, with the image's own `sh`, `nc`
# and `date`; the tour walks the path from a counter to the patties.
#
# Both images come from the ECR mirror of Docker Hub's official images (no
# rate limits), pinned by digest: golang 1.27.1-alpine and alpine 3.22.1.
FROM --platform=$BUILDPLATFORM public.ecr.aws/docker/library/golang:1.27.1-alpine@sha256:8a5910f31396cd4d89662f56c68b3ae31d374308270a1c3bd96672ee5ed43414 AS build
ARG TARGETARCH
WORKDIR /src
COPY go.mod *.go ./
# Standard library only: nothing to download, so no network during the build.
RUN --network=none CGO_ENABLED=0 GOOS=linux GOARCH=$TARGETARCH GOTOOLCHAIN=local \
    go build -trimpath -ldflags="-s -w" -o /out/burger .

FROM public.ecr.aws/docker/library/alpine@sha256:4bcff63911fcb4448bd4fdacec207030997caf25e9bea4045fa6c8c44de311d1
COPY --from=build /out/burger /burger
USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["/burger"]
