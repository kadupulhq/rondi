# syntax=docker/dockerfile:1
#
# Runtime image: the rondi executable plus its RRDtool-compatible names, on a
# distroless base that has glibc and no shell.
#
# The build stage always runs on the build host's architecture and
# cross-compiles for the target, so a multi-platform build needs no emulation.
# The release workflow builds linux/amd64 and linux/arm64 on one amd64 runner.

# rust:1.98.0-slim (Debian 13, multi-arch index); same toolchain as .mise.toml
# and docker/Dockerfile.test.
FROM --platform=$BUILDPLATFORM rust@sha256:17d1ba895198f9934c6314ec5346a0d5115372f3243390c3d731e242f35c2f27 AS build

ARG TARGETARCH
ARG BUILDARCH

RUN set -eu; \
    case "$TARGETARCH" in \
        amd64) triple=x86_64-unknown-linux-gnu; gnu=x86_64-linux-gnu; gcc=gcc-x86-64-linux-gnu; deb=amd64 ;; \
        arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu; gcc=gcc-aarch64-linux-gnu; deb=arm64 ;; \
        *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
        apt-get update; \
        apt-get install --yes --no-install-recommends "$gcc" "libc6-dev-$deb-cross"; \
        rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$triple"; \
    printf '%s\n%s\n' "$triple" "$gnu" > /target

WORKDIR /src
COPY . .

# The linker is the target's gcc driver: the native compiler when the
# architectures match, the cross compiler otherwise.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    set -eu; \
    triple=$(sed -n 1p /target); gnu=$(sed -n 2p /target); \
    linker_var="CARGO_TARGET_$(printf '%s' "$triple" | tr 'a-z-' 'A-Z_')_LINKER"; \
    env "$linker_var=$gnu-gcc" \
        cargo build --release --locked -p rondi-cli --target "$triple"; \
    install -D -m 0755 "target/$triple/release/rondi" /usr/local/bin/rondi

# Lay out the final tree here because the runtime image has no shell.
RUN set -eu; \
    mkdir -p /out/usr/local/bin /out/data; \
    sh scripts/install-compat-links.sh /usr/local/bin/rondi /out/usr/local/bin; \
    cp /usr/local/bin/rondi /out/usr/local/bin/rondi; \
    chown 65532:65532 /out/data

# gcr.io/distroless/cc-debian13:nonroot (multi-arch index). Debian 13 matches
# the build stage's glibc.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:cb87acc574abe719b6e5c547384defa2f7b1f24af9605d3f618cf852495d8be2

COPY --from=build /out/ /

USER nonroot:nonroot
WORKDIR /data
ENTRYPOINT ["/usr/local/bin/rrdtool"]
CMD ["--help"]
