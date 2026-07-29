# syntax=docker/dockerfile:1.7

FROM rust:bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends clang cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --locked --release --all-features --bin rust-backup \
    && install -Dm0755 target/release/rust-backup /out/rust-backup

FROM debian:bookworm-slim AS runtime

ARG VERSION=dev
ARG VCS_REF=unknown
LABEL org.opencontainers.image.title="rust-backup" \
      org.opencontainers.image.description="Streaming source-to-destination backup and coordination server" \
      org.opencontainers.image.source="https://github.com/manprint/rust-backup" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${VCS_REF}" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates netcat-openbsd \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 65532 rust-backup \
    && useradd --system --uid 65532 --gid rust-backup --home-dir /nonexistent \
        --shell /usr/sbin/nologin rust-backup

COPY --from=builder /out/rust-backup /usr/local/bin/rust-backup

USER 65532:65532
EXPOSE 7835/tcp 7835/udp
HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
    CMD ["nc", "-z", "127.0.0.1", "7835"]

ENTRYPOINT ["rust-backup"]
CMD ["server", "--bind-addr", "0.0.0.0", "--control-port", "7835"]
